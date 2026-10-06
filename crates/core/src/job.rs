//! The background job runner: one `jobs` table serving as outbox, email queue and cron.
//!
//! A [`Runner`] maps a kind string to an async handler and drains the table in a loop.
//! Claiming is a single `UPDATE … RETURNING` — SQLite serialises writers, so no lock is
//! needed and a row is handed to exactly one worker.
//!
//! `dedup_key` is a permanent once-only record: it survives `DONE` **and** `FAILED`, so
//! `"nav:invoice:inv_…"` still means an invoice is reported once however the job ended.
//! Re-driving a terminally failed job is therefore an explicit reset of that row, not a
//! race between a released key and a fresh insert. Re-execution is additionally stopped at
//! the work itself: `idx_nav_submission_live` plus `mintworks_nav::job::may_send` make a double
//! *filing* impossible even when a double *enqueue* happens.
//!
//! Whether a failure is retried at all comes off the error, not off the call site:
//! [`Error::retry`] answers [`Retry::Backoff`] or [`Retry::Never`], and [`Runner::tick`]
//! terminates a `Never` on its first attempt instead of spending eight backoff steps on a
//! fault that will answer the same way every time.
//!
//! A recurring job folds its period into the key, which is what
//! [`Runner::register_periodic`] does when it reschedules. That key is the one exception to
//! the paragraph above: it is prefixed [`PERIODIC_KEY_PREFIX`] and is a scheduling token with
//! a lifetime of one period, so retention may reclaim a spent one.

use std::{collections::HashMap, pin::Pin, sync::Arc, time::Duration};

use crate::{
	error::{ClResult, Error, Retry},
	settings::Settings,
	store::CoreStore,
	types::Timestamp,
};

/// The default attempt ceiling, mirroring the `jobs.max_attempts.` settings family's own
/// default. The runner reads that family; this is the fallback the tests pin against.
pub const DEFAULT_MAX_ATTEMPTS: i64 = 8;

/// The `jobs.backoff_cap.` family default, mirrored here so [`Runner::tick`] has a ceiling to
/// retry under when the settings read itself is what failed.
pub const DEFAULT_BACKOFF_CAP: i64 = 3600;

/// The daily `jobs` retention tick. Registered and seeded by `AppBuilder::build` for every
/// deployment — `jobs` is this crate's table, so its housekeeping is not the consumer's to
/// remember. It deletes finished rows whose `dedup_key` is absent or a spent
/// [`PERIODIC_KEY_PREFIX`] token; see [`CoreStore::job_sweep`].
pub const KIND_SWEEP: &str = "SWEEP_JOBS";

/// The alert sweep tick, registered and seeded by `AppBuilder::build` alongside
/// [`KIND_SWEEP`]. It fires every minute and mostly does nothing: the real period is
/// `settings['admin.alert_interval_minutes']`, which [`crate::alert::sweep`] reads per tick so
/// that changing it does not need a restart. See [`crate::alert::sweep`].
pub const KIND_ALERT_SWEEP: &str = "ALERT_SWEEP";

/// The `jobs.kind` the alert sweep enqueues. `mintworks-core` cannot depend on `mintworks-email`,
/// so the kind and the payload shape (`mintworks_email::SendEmail`) are the contract between them.
pub const KIND_SEND_EMAIL: &str = "SEND_EMAIL";

/// Prefix on the `dedup_key` of a *periodic* successor. It is a scheduling token with a
/// lifetime of one period, not an idempotency record — so `job_sweep` may reclaim a spent
/// one, while a handler-supplied key (`NAV_REPORT`'s "never file twice") is kept forever.
/// Without the split, `KIND_ALERT_SWEEP`'s 60s period minted one permanently unreclaimable
/// `DONE` row per minute.
pub const PERIODIC_KEY_PREFIX: &str = "periodic:";

/// How long [`Runner::run`] sleeps when it finds nothing to do.
const IDLE: Duration = Duration::from_secs(5);

/// Added to the longest registered `jobs.timeout_secs.<KIND>` to get [`Runner::reclaim`]'s
/// cutoff — it covers the gap between a handler returning and its terminal write landing,
/// including `thrice`'s two 200 ms pauses.
const RECLAIM_GRACE: i64 = 60;

/// [`IDLE`] plus a per-worker offset, so `jobs.workers` — up to 64 of them — do not wake in
/// lockstep on the single writer connection. Off the worker index, not the wall clock's
/// sub-second part: that reads the same for every worker waking at the same instant.
fn idle_sleep(worker: i64) -> Duration {
	IDLE + Duration::from_millis(worker.unsigned_abs() * 997 % 1000)
}

/// What a failed tick left behind, and with it whether a periodic chain wants a successor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
	/// Back to `PENDING` for another attempt, or stuck `RUNNING` until the next
	/// [`Runner::reclaim`]. Either way that row still carries the chain, so no successor.
	Live,
	/// `FAILED` with its attempts spent. This is where a periodic chain mints its successor.
	Terminated,
	/// An operator cancelled the row while the handler ran. Terminal, and the chain stops with
	/// it: a successor here would undo the cancel.
	Cancelled,
}

/// What a handler that schedules its own next run answers.
///
/// `Again` is a **success**: the work is progressing and nothing is wrong with the row, which
/// is what `Err(Unavailable)` could never say — it set `last_error`, made the row count as
/// retrying and fed `A-JOB-STALE`, so a poll doing its job looked like a failing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
	Done,
	Again { at: Timestamp },
}

/// Delay before the retry that follows attempt `attempts`: `2^attempts` seconds, capped at
/// `cap`. The claim has already incremented the counter, so the first failure waits 2s.
///
/// The cap is a parameter because it is per kind: the runner takes it from
/// `jobs.backoff_cap.<KIND>`, and `mintworks_nav::job`'s polls pass their own shorter ceiling.
pub fn backoff_secs(attempts: i64, cap: i64) -> i64 {
	if (0..12).contains(&attempts) { (1i64 << attempts).min(cap) } else { cap }
}

/// The retry delay before jitter. An upstream that named a delay wins when it is longer than
/// ours — retrying inside a throttle window only earns another 429 — but bounded by
/// `jobs.backoff_cap.<KIND>`: a NAV `Retry-After: 86400` silently parked filings for a day
/// despite a cap of 600, and nothing the operator could change took effect.
#[must_use]
pub fn retry_base(attempts: i64, cap: i64, retry_after: Option<i64>) -> i64 {
	let ours = backoff_secs(attempts, cap);
	match retry_after {
		Some(secs) => ours.max(secs.clamp(1, cap)),
		None => ours,
	}
}

/// A claimed job, as handed to its handler.
#[derive(Clone, Debug)]
pub struct Job {
	pub id: i64,
	pub kind: String,
	/// JSON, exactly as passed to [`enqueue`].
	pub payload: String,
	pub attempts: i64,
}

/// Queue a job. Returns the new row's id, or `None` when `dedup_key` is already taken — a
/// collision is a no-op, never an error. A `None` key means no deduplication at all.
///
/// `#[must_use]`: a discarded `None` is a silent no-op, which is how two statutory recovery
/// sweeps re-enqueued nothing for as long as they ran.
///
/// A [`PERIODIC_KEY_PREFIX`] key is refused: `job_sweep` deletes finished rows carrying it, so
/// a consumer key spelled `periodic:billing:cust_x` lost the permanent once-only record this
/// module promises and a replayed enqueue re-ran the work. `Error::internal`, because the key
/// is the programmer's, not the client's.
#[must_use = "a `None` means the dedup key was taken and nothing was enqueued"]
pub async fn enqueue(
	store: &Arc<dyn CoreStore>,
	kind: &str,
	payload: &str,
	dedup_key: Option<&str>,
	run_at: Timestamp,
) -> ClResult<Option<i64>> {
	// Case-insensitive, because `case_sensitive_like` is off by default: `PERIODIC:x` passed this
	// guard as a permanent idempotency record and `job_sweep`'s `LIKE` then deleted it anyway.
	if dedup_key.is_some_and(|k| {
		k.get(..PERIODIC_KEY_PREFIX.len())
			.is_some_and(|p| p.eq_ignore_ascii_case(PERIODIC_KEY_PREFIX))
	}) {
		return Err(Error::internal(format!(
			"'{PERIODIC_KEY_PREFIX}' is reserved for periodic scheduling tokens"
		)));
	}
	store.job_enqueue(kind, payload, dedup_key, run_at).await
}

/// Whether a job of this kind is still able to run. A periodic chain is seeded only when it
/// is not: a `dedup_key` survives `DONE`, so a plain [`enqueue`] under a fixed key can never
/// revive a chain that stopped — the seed row holds the key forever.
///
/// `besides` excludes one row, which the reschedule in [`Runner::tick`] needs: it runs before
/// the job it is rescheduling is marked `DONE`, so that row is still `RUNNING`.
pub async fn has_live(
	store: &Arc<dyn CoreStore>,
	kind: &str,
	besides: Option<i64>,
) -> ClResult<bool> {
	store.job_has_live(kind, besides).await
}

/// Runs `op` up to three times, answering the last error if all three fail. Only a
/// [`Retry::Backoff`] error is retried; anything else is answered on its first attempt.
///
/// Every caller is a terminal status write on a path where a propagated error leaves the row
/// `RUNNING` — invisible to [`Runner::claim`], which selects only `PENDING` — until the next
/// process start reclaims it. The single writer connection can hand back `SQLITE_BUSY` under
/// load, so one transient failure must not decide that.
///
/// Public for one caller outside this module: `mintworks_nav::job::report`'s `set_sent` is the same
/// shape of write — a `transactionId` that cannot be reconstructed if the statement is lost.
pub async fn thrice<T, F, Fut>(op: F) -> Result<T, Option<Error>>
where
	F: Fn() -> Fut,
	Fut: std::future::Future<Output = ClResult<T>>,
{
	let mut last = None;
	for attempt in 0..3 {
		if attempt > 0 {
			// A writer that was busy a microsecond ago is busy now: without the pause all three
			// attempts failed identically and the row was left `RUNNING`, which is the one
			// outcome every caller of this exists to avoid.
			tokio::time::sleep(Duration::from_millis(200)).await;
		}
		match op().await {
			Ok(v) => return Ok(v),
			// A `Never` error answers the same way three times, so retrying it only spent
			// 400 ms of sleep on a failure already decided.
			Err(e) if e.retry() != Retry::Backoff => return Err(Some(e)),
			Err(e) => last = Some(e),
		}
	}
	Err(last)
}

/// Seed a periodic chain's first occurrence. Call once at boot; idempotent, so a chain that
/// is still live is left alone and one that stopped is restarted.
///
/// Deliberately not a plain [`enqueue`] under a fixed `dedup_key`: a key survives `DONE`, so
/// the seed row would hold the kind forever and a chain that broke — a same-second reschedule
/// collision, or eight failures reaching [`Runner::terminate`] — could never be revived.
/// The liveness guard is inside the insert instead: [`has_live`] plus [`enqueue`] is a
/// check-then-insert, so two processes booting a rolling deploy both seeded and the chain
/// doubled permanently.
pub async fn seed_periodic(store: &Arc<dyn CoreStore>, kind: &str) -> ClResult<()> {
	let _id = store.job_seed_periodic(kind, Timestamp::now()).await?;
	Ok(())
}

type BoxFut = Pin<Box<dyn Future<Output = ClResult<Next>> + Send>>;
type Handler = Box<dyn Fn(Job) -> BoxFut + Send + Sync>;

struct Entry {
	handler: Handler,
	/// `Some(seconds)` re-enqueues the kind that far ahead each time it completes.
	period: Option<i64>,
}

/// The worker: a kind -> handler registry plus the drain loop.
pub struct Runner {
	store: Arc<dyn CoreStore>,
	/// Reads the three per-kind retry families.
	settings: Arc<Settings>,
	handlers: HashMap<&'static str, Entry>,
	/// Flipped once, by [`crate::AppState::shutdown_jobs`]. A `watch` rather than a
	/// `CancellationToken` so `mintworks-core` needs no `tokio-util`.
	stop: tokio::sync::watch::Sender<bool>,
}

impl Runner {
	pub fn new(store: Arc<dyn CoreStore>) -> Self {
		let settings = Arc::new(Settings::core(Arc::clone(&store)));
		Self::with_settings(store, settings)
	}

	/// [`Self::new`] over the app's own [`Settings`], so a consumer's `jobs.*` declarations
	/// reach the retry policy and `Settings::set` invalidates what the runner reads.
	pub fn with_settings(store: Arc<dyn CoreStore>, settings: Arc<Settings>) -> Self {
		let (stop, _) = tokio::sync::watch::channel(false);
		Self { store, settings, handlers: HashMap::new(), stop }
	}

	/// The handle that stops every [`Self::run`] worker. `AppBuilder::build` parks it on
	/// `AppState` so [`crate::AppState::shutdown_jobs`] can signal it at shutdown.
	#[must_use]
	pub fn stopper(&self) -> tokio::sync::watch::Sender<bool> {
		self.stop.clone()
	}

	/// Register a handler for `kind`. A handler returning a retryable `Err` — see
	/// [`Error::retry`] — is retried with backoff until `jobs.max_attempts.<KIND>` is spent,
	/// after which the row is `FAILED` and left alone. A `Retry::Never` error skips the
	/// backoff entirely and fails the row on its first attempt.
	pub fn register<F, Fut>(&mut self, kind: &'static str, f: F)
	where
		F: Fn(Job) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = ClResult<()>> + Send + 'static,
	{
		self.insert(kind, None, f);
	}

	/// As [`register`](Self::register), but each completion re-enqueues the same payload
	/// `every_secs` later. Seed the first occurrence with [`seed_periodic`] — **not** with a
	/// keyed [`enqueue`], which can never revive a broken chain. Every occurrence's dedup key
	/// folds in its own `run_at`, so a restart cannot double-schedule it.
	pub fn register_periodic<F, Fut>(&mut self, kind: &'static str, every_secs: i64, f: F)
	where
		F: Fn(Job) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = ClResult<()>> + Send + 'static,
	{
		self.insert(kind, Some(every_secs), f);
	}

	/// [`Self::register`] for a handler that decides its own schedule. [`Next::Again`] puts the
	/// row back to `PENDING` at the given time with `last_error` and `err_code` cleared; the
	/// claim's `attempts` increment stands, so `jobs.max_attempts.<KIND>` still bounds a
	/// handler that defers forever and [`Self::tick`]'s poison guard still fires.
	///
	/// A second method rather than one generic over `Into<Next>`: relaxing `Fut::Output`
	/// un-pins the error type of every bare `async { Ok(()) }` handler body, so two `From`
	/// impls make it ambiguous (`E0283`) and all ~12 existing closures need a turbofish.
	pub fn register_next<F, Fut>(&mut self, kind: &'static str, f: F)
	where
		F: Fn(Job) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = ClResult<Next>> + Send + 'static,
	{
		let handler: Handler = Box::new(move |job| Box::pin(f(job)));
		self.handlers.insert(kind, Entry { handler, period: None });
	}

	/// Whether a handler for `kind` is registered.
	#[must_use]
	pub fn contains(&self, kind: &str) -> bool {
		self.handlers.contains_key(kind)
	}

	/// The one site where a handler's future type is bound, so the `()` handlers are adapted
	/// here rather than at each registration.
	fn insert<F, Fut>(&mut self, kind: &'static str, period: Option<i64>, f: F)
	where
		F: Fn(Job) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = ClResult<()>> + Send + 'static,
	{
		let handler: Handler = Box::new(move |job| {
			let fut = f(job);
			Box::pin(async move { fut.await.map(|()| Next::Done) })
		});
		self.handlers.insert(kind, Entry { handler, period });
	}

	/// Drain forever — `tokio::spawn(Arc::clone(&runner).run(i))` at startup, once per worker `i`.
	///
	/// A tick that errors is logged and slept off: the queue is still there next time.
	///
	/// One job at a time *per worker*: several of these over one `Arc<Runner>` is the
	/// supported way to get throughput, because the `UPDATE … RETURNING` claim hands a row to
	/// exactly one of them. It is what keeps `SEND_EMAIL` from starving behind a NAV sweep's
	/// two-minute-per-invoice timeouts. [`Self::reclaim`] must run **before** any worker starts.
	///
	/// Returns once [`Self::stopper`] has been signalled: the job in hand is finished, and no
	/// further one is claimed. The drain is deliberately unbounded — the orchestrator's own
	/// kill timer is the deadline, and that is one fewer knob. A handler still in flight when
	/// the process is killed anyway is the at-least-once case [`Self::reclaim`] already covers
	/// at the next boot.
	pub async fn run(self: std::sync::Arc<Self>, worker: i64) {
		let mut stop = self.stop.subscribe();
		loop {
			// Checked before the claim, never during: a stopping worker owes the row it holds a
			// terminal write, and abandoning it there is exactly what `reclaim` exists to undo.
			if *stop.borrow() {
				return;
			}
			match self.tick(Timestamp::now()).await {
				Ok(true) => continue,
				Ok(false) => {}
				Err(e) => tracing::error!(error = %e, "job runner tick failed"),
			}
			// Woken by the signal too, or a rolling deploy waits out the whole idle sleep per
			// worker before the process can exit.
			tokio::select! {
				() = tokio::time::sleep(idle_sleep(worker)) => {}
				_ = stop.changed() => return,
			}
		}
	}

	/// Put every *stale* `RUNNING` row back to `PENDING`. A crash between [`Self::claim`] and
	/// [`Self::complete`] leaves the row `RUNNING`, and `claim` only ever looks at
	/// `PENDING` — without this the job is stranded forever with no trace.
	///
	/// Stale means claimed longer ago than the longest handler timeout this runner registers
	/// plus [`RECLAIM_GRACE`]. That lease is what makes a rolling deploy safe: this used to
	/// reclaim indiscriminately, so the incoming process flipped the outgoing one's live rows
	/// back to `PENDING` and both delivered the same email.
	///
	/// Call it **once**, at boot, before any [`Self::run`] worker of this process starts.
	pub async fn reclaim(&self) -> ClResult<u64> {
		let mut lease = 0;
		for kind in self.handlers.keys() {
			lease = lease.max(self.settings.int(&format!("jobs.timeout_secs.{kind}")).await?);
		}
		let before = Timestamp(Timestamp::now().0 - lease.saturating_add(RECLAIM_GRACE));
		self.store.job_reclaim(before).await
	}

	/// Claim and run at most one job. `Ok(false)` means nothing was due.
	pub async fn tick(&self, now: Timestamp) -> ClResult<bool> {
		let Some(job) = self.claim(now).await? else { return Ok(false) };
		// The ceiling applies here, not only in `fail`: a handler that kills the process never
		// returns, so its row was re-claimed on every boot forever. `0` is still unbounded — a
		// statutory NAV filing must not be given up on. Looked up before the ceiling logic
		// because both terminal paths below end the chain without ever consulting `period`.
		let entry = self.handlers.get(job.kind.as_str());
		let period = entry.and_then(|e| e.period);
		// Both retry settings are resolved here, once, so every terminal write below is
		// settings-free: `fail` used to re-read them, so a persistent pool-acquire timeout
		// failed all three `thrice` attempts and left the row `RUNNING` with its chain dead.
		let (max, cap, timeout) = match self.limits(&job.kind).await {
			Ok(limits) => limits,
			Err(e) => {
				// Unbounded, not `DEFAULT_MAX_ATTEMPTS`: a ceiling that could not be read is not
				// a ceiling, and `NAV_REPORT` is deliberately `0`. `tick`'s poison guard still
				// catches a genuinely stuck row once the setting is readable.
				let (_, code) = e.parts();
				let err = e.to_string();
				let outcome = self
					.fail_hard(&job, now, 0, 0, DEFAULT_BACKOFF_CAP, &err, Some(code), None)
					.await;
				if outcome == Failure::Terminated {
					self.reschedule_periodic(period, &job, now, 0).await;
				}
				return Ok(true);
			}
		};
		// `>`, where `fail`'s ceiling test is `>=`: the claim pre-increments, so `>=` gives
		// exactly `max` executions and this one only ever fires for a row that came back from
		// `reclaim` without a handler verdict — a handler that killed the process.
		if max > 0 && job.attempts > max {
			let err = format!("attempt {} is past the {max}-attempt ceiling", job.attempts);
			// `false` on a read failure rather than `?`: a propagated error here skips
			// `terminate_hard` and strands the row `RUNNING`, where nothing but a restart finds
			// it, while the only thing lost is a periodic successor that `seed_periodic` remints.
			let live = match self.store.job_status(job.id).await {
				Ok(status) => status.as_deref() == Some("RUNNING"),
				Err(e) => {
					tracing::warn!(job = job.id, error = %e, "job status unreadable; no successor");
					false
				}
			};
			if self.terminate_hard(job.id, now, &err, Some("E-CORE-JOB-POISON")).await && live {
				self.reschedule_periodic(period, &job, now, 0).await;
			}
			return Ok(true);
		}
		let Some(entry) = entry else {
			// No retry will ever find a handler; churning the row helps nobody. No `Error`
			// stands behind this, so the row records no errCode. A kind with no handler has no
			// period to preserve either.
			self.terminate_hard(job.id, now, "no handler registered for this kind", None)
				.await;
			return Ok(true);
		};
		// Handed to a task, or one panicking handler ends `run()` for good and every later job
		// with it. `panicked` carries past the retry class so the panic takes the normal
		// backoff: the `Error::internal` standing in for it classes `Retry::Never`.
		let started = std::time::Instant::now();
		let mut task = tokio::spawn((entry.handler)(job.clone()));
		let joined = if timeout > 0 {
			let secs = u64::try_from(timeout).unwrap_or(u64::MAX);
			if let Ok(joined) = tokio::time::timeout(Duration::from_secs(secs), &mut task).await {
				joined
			} else {
				// Aborted, or the wedged handler keeps its worker: `tick` returning is what
				// frees the worker, and the task would otherwise outlive the row it owns.
				task.abort();
				Ok(Err(Error::Timeout(format!("job handler exceeded {timeout}s"))))
			}
		} else {
			(&mut task).await
		};
		let (outcome, panicked) = match joined {
			Ok(result) => (result, false),
			Err(e) => (Err(Error::internal(format!("job handler panicked: {e}"))), true),
		};
		// How long the handler actually took, for `reschedule`. Measured rather than read off
		// the clock so `now` stays the single injectable time source the tests drive.
		let elapsed = i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX);
		match outcome {
			// A deferral mints **no** periodic successor, exactly like `Failure::Live`: the row
			// still carries the chain.
			Ok(Next::Again { at }) => self.defer_hard(job.id, at).await,
			Ok(Next::Done) => {
				// **Complete first, then reschedule, and never propagate past a success.** The
				// side effect has already happened, and a `?` after it leaves the row `RUNNING`
				// for `reclaim` to run a *second* time. The trade is losing a successor rather
				// than duplicating a side effect. Gated on `DONE` so a `job_cancel` landing
				// mid-handler does not mint a successor anyway.
				if self.complete_hard(job.id, now).await {
					self.reschedule_periodic(period, &job, now, elapsed).await;
				}
			}
			Err(e) => {
				let (_, code) = e.parts();
				// Honoured for every kind, not just NAV's.
				let retry_after = e.retry_after().and_then(|s| i64::try_from(s).ok());
				let err = e.to_string();
				// The one place "should this be retried" is decided. `Retry::Never` means the
				// next attempt gets the same answer, so backoff steps only delay the diagnosis
				// and, for `NAV_REPORT`, add requests at the tax authority.
				let outcome = match (panicked, e.retry()) {
					(false, Retry::Never) => {
						tracing::error!(job = job.id, kind = %job.kind, err_code = code,
							error = %err, "job failed terminally: not retryable");
						if self.terminate_hard(job.id, now, &err, Some(code)).await {
							Failure::Terminated
						} else {
							Failure::Live
						}
					}
					_ => {
						self.fail_hard(&job, now, elapsed, max, cap, &err, Some(code), retry_after)
							.await
					}
				};
				// A terminated chain has no successor and `seed_periodic` runs only from
				// `AppBuilder::on_init`, so eight consecutive failures killed `NAV_SWEEP` for the
				// life of the process. `Cancelled` is the one terminal state that gets none.
				if outcome == Failure::Terminated {
					self.reschedule_periodic(period, &job, now, elapsed).await;
				}
			}
		}
		Ok(true)
	}

	/// Re-enqueue a periodic kind's successor, logging rather than propagating. Every caller is
	/// past the point where the work either happened or terminally failed, so a `?` here would
	/// leave the row `RUNNING`.
	async fn reschedule_periodic(
		&self,
		period: Option<i64>,
		job: &Job,
		now: Timestamp,
		elapsed: i64,
	) {
		if let Some(period) = period
			&& let Err(e) = self.reschedule(job, now, period, elapsed).await
		{
			tracing::error!(job = job.id, kind = %job.kind, error = %e,
				"periodic reschedule failed; the chain is broken until the next process start");
		}
	}

	pub async fn claim(&self, now: Timestamp) -> ClResult<Option<Job>> {
		self.store.job_claim(now).await
	}

	/// `jobs.max_attempts.<KIND>`, `jobs.backoff_cap.<KIND>` and `jobs.timeout_secs.<KIND>` —
	/// prefix families, so a kind that declares none gets the family defaults. Resolved once,
	/// before the handler runs, so every terminal write in [`Runner::tick`] is settings-free.
	async fn limits(&self, kind: &str) -> ClResult<(i64, i64, i64)> {
		Ok((
			self.settings.int(&format!("jobs.max_attempts.{kind}")).await?,
			self.settings.int(&format!("jobs.backoff_cap.{kind}")).await?,
			self.settings.int(&format!("jobs.timeout_secs.{kind}")).await?,
		))
	}

	/// [`Runner::complete`] that will not give up quietly. A row left `RUNNING` after a
	/// successful handler is invisible to `claim` and re-runs the handler at the next
	/// `reclaim`, so one transient `SQLITE_BUSY` on the single writer connection must not
	/// decide it — retry, and if it still fails, say so loudly, because only a restart
	/// recovers the row from there.
	///
	/// `true` when the row is `DONE`. `false` means it was cancelled while running, or is stuck
	/// `RUNNING` — neither wants a periodic successor.
	async fn complete_hard(&self, id: i64, now: Timestamp) -> bool {
		match thrice(|| self.complete(id, now)).await {
			Ok(done) => done,
			Err(last) => {
				tracing::error!(job = id, error = ?last,
					"a job whose handler SUCCEEDED could not be marked DONE; it is stuck RUNNING \
					 and the next process start will run the handler a second time");
				false
			}
		}
	}

	/// [`Runner::complete_hard`]'s retry for a handler that asked to run again, and for the
	/// same reason: a row left `RUNNING` is invisible to `claim` until the next process start.
	async fn defer_hard(&self, id: i64, at: Timestamp) {
		if let Err(last) = thrice(|| self.defer(id, at)).await {
			tracing::error!(job = id, error = ?last,
				"a job that asked to run again could not be rescheduled; it is stuck RUNNING \
				 until the next process start reclaims it");
		}
	}

	/// `true` when the row is back to `PENDING`; `false` means an operator cancelled it while
	/// the handler ran, and it stays cancelled.
	async fn defer(&self, id: i64, at: Timestamp) -> ClResult<bool> {
		if self.store.job_defer(id, at).await? > 0 {
			return Ok(true);
		}
		// Same conflation as `complete`: `PENDING` means a previous [`thrice`] attempt lost its
		// answer, anything else is a mid-handler `job_cancel`.
		if self.store.job_status(id).await?.as_deref() == Some("PENDING") {
			return Ok(true);
		}
		tracing::warn!(job = id, "job was cancelled while running; not deferring it");
		Ok(false)
	}

	/// [`Runner::fail`] with [`Runner::complete_hard`]'s retry, and for the same reason: the
	/// single writer connection can hand back `SQLITE_BUSY` under load, and a propagated error
	/// here leaves the row `RUNNING` — invisible to `claim`, which selects only `PENDING` —
	/// waiting for the next process start's `reclaim`.
	///
	/// A row stuck `RUNNING` answers [`Failure::Live`]: it has not terminated, it is merely
	/// unreachable until a restart.
	#[allow(clippy::too_many_arguments)]
	async fn fail_hard(
		&self,
		job: &Job,
		now: Timestamp,
		elapsed: i64,
		max: i64,
		cap: i64,
		err: &str,
		code: Option<&str>,
		retry_after: Option<i64>,
	) -> Failure {
		match thrice(|| self.fail(job, now, elapsed, max, cap, err, code, retry_after)).await {
			Ok(outcome) => outcome,
			Err(last) => {
				tracing::error!(job = job.id, kind = %job.kind, error = ?last,
					"a job whose handler FAILED could not be rescheduled; it is stuck RUNNING \
					 until the next process start reclaims it");
				Failure::Live
			}
		}
	}

	/// [`Runner::terminate`] with [`Runner::complete_hard`]'s retry, and for the same reason: a
	/// propagated error here leaves the row `RUNNING` — invisible to `claim`, which selects only
	/// `PENDING` — waiting for the next process start's `reclaim`.
	async fn terminate_hard(&self, id: i64, now: Timestamp, err: &str, code: Option<&str>) -> bool {
		match thrice(|| self.terminate(id, now, err, code)).await {
			Ok(terminal) => terminal,
			Err(last) => {
				tracing::error!(job = id, error = ?last,
					"a job that failed terminally could not be marked FAILED; it is stuck RUNNING \
					 until the next process start reclaims it");
				false
			}
		}
	}

	/// The periodic successor. Split out of [`Runner::tick`] so its failure can be logged
	/// rather than propagated — by the time it runs, the job is already `DONE`.
	async fn reschedule(
		&self,
		job: &Job,
		now: Timestamp,
		period: i64,
		elapsed: i64,
	) -> ClResult<()> {
		// Fixed-rate from the tick's *start* so runs do not drift — but never behind the handler
		// that just ran. One that outran its period landed `next` in the past and spun: an
		// hourly `NAV_SWEEP` during an outage became a continuous loop against a government API.
		let next = Timestamp(now.0 + std::cmp::max(period, elapsed.saturating_add(1)));
		let key = format!("{PERIODIC_KEY_PREFIX}{}:{}", job.kind, next.0);
		// Straight to the store: the runner mints the reserved prefix `enqueue` refuses.
		if self
			.store
			.job_enqueue(&job.kind, &job.payload, Some(&key), next)
			.await?
			.is_some()
		{
			return Ok(());
		}
		// `DONE` and `FAILED` rows keep their `dedup_key`, so a replayed same-second key makes
		// the reschedule a no-op — and if the collision was with a spent row the chain just
		// died. Push one second out to get past the key.
		if has_live(&self.store, &job.kind, Some(job.id)).await? {
			tracing::warn!(kind = %job.kind, next = next.0,
				"periodic reschedule collided with a live job; \
				 the existing one carries the chain");
		} else {
			tracing::error!(kind = %job.kind, next = next.0,
				"periodic reschedule collided with a spent job and no successor is live; \
				 retrying one second out");
			let next = Timestamp(next.0 + 1);
			let key = format!("{PERIODIC_KEY_PREFIX}{}:{}", job.kind, next.0);
			if self
				.store
				.job_enqueue(&job.kind, &job.payload, Some(&key), next)
				.await?
				.is_none()
			{
				// A spent row sits on the one-second-out key too. Not retried further, and logged
				// loudly because nothing else would say so: there is no `PENDING` row to miss.
				tracing::error!(kind = %job.kind, key = %key,
					"periodic reschedule collided a second time; the chain is dead \
					 until the next process start");
			}
		}
		Ok(())
	}

	/// Marks the job `DONE` and **blanks its payload**.
	///
	/// A `DONE` job never runs again, so its payload is dead weight — and a `SEND_EMAIL` payload
	/// carries the full `activate?token=…` and `reset-password?token=…` links, so for the 24 h
	/// and 2 h those live, a backup or a read-only operator query was an account-takeover
	/// credential. Nothing else cleared it: `jobs` is outside `export_account`.
	///
	/// `dedup_key` is untouched — it is what makes "an invoice is never reported to NAV
	/// twice" hold across `DONE`. `FAILED` keeps its payload too: it is the diagnostic, and a
	/// failed email job is one that never delivered.
	///
	/// `Runner::reschedule` copies the payload from the in-memory [`Job`], not from the row,
	/// so a periodic chain survives this.
	/// `true` when the row is `DONE`.
	async fn complete(&self, id: i64, now: Timestamp) -> ClResult<bool> {
		if self.store.job_complete(id, now).await? > 0 {
			return Ok(true);
		}
		// Zero rows is two things and the count cannot separate them, because [`thrice`] retries
		// this call as a whole: `DONE` means a previous attempt lost its answer, anything else
		// is a mid-handler `job_cancel` — which must not be reported as having run.
		if self.store.job_status(id).await?.as_deref() == Some("DONE") {
			return Ok(true);
		}
		tracing::warn!(job = id, "job was cancelled while running; leaving it FAILED");
		Ok(false)
	}

	/// Back to `PENDING` one backoff step out, or `FAILED` once the attempts are spent.
	///
	/// `max` and `cap` come from the caller — [`Runner::limits`] resolves them *before* the
	/// handler runs, because everything here is a terminal write and a settings read that fails
	/// inside one leaves the row `RUNNING`, invisible to `claim`. `max == 0` means **unbounded**,
	/// which is what a statutory NAV filing wants: giving up on one is not a recovery.
	#[allow(clippy::too_many_arguments)]
	pub async fn fail(
		&self,
		job: &Job,
		now: Timestamp,
		elapsed: i64,
		max: i64,
		cap: i64,
		err: &str,
		code: Option<&str>,
		retry_after: Option<i64>,
	) -> ClResult<Failure> {
		if max > 0 && job.attempts >= max {
			tracing::error!(job = job.id, kind = %job.kind, err_code = code.unwrap_or(""), error = err,
				"job failed terminally");
			let terminal = self.terminate(job.id, now, err, code).await?;
			return Ok(if terminal { Failure::Terminated } else { Failure::Live });
		}
		tracing::warn!(job = job.id, kind = %job.kind, attempt = job.attempts,
			err_code = code.unwrap_or(""), error = err, "job failed, retrying");
		// From the end of the handler, not the tick's start, as `reschedule` also does: a
		// `NAV_REPORT` spending its 30 s timeout put `now+2`…`now+16` all in the past, so four
		// attempts fired back-to-back with zero wait against the tax authority.
		let base = retry_base(job.attempts, cap, retry_after);
		if let Some(secs) = retry_after.filter(|s| *s > backoff_secs(job.attempts, cap)) {
			tracing::info!(job = job.id, kind = %job.kind, retry_after = secs, capped = base,
				"upstream named a longer delay than our backoff");
		}
		// Spread over the last quarter of the window, keyed on the row so it is stable across
		// attempts and needs no RNG: without it every row that failed against one upstream
		// retried on the same second, through the one writer connection.
		let jitter = job.id.rem_euclid(base / 4 + 1);
		let run_at = Timestamp(now.0 + elapsed + base + jitter);
		if self.store.job_fail(job.id, run_at, err, code).await? == 0 {
			// Same conflation as `complete_hard`: `PENDING` means *this* call lost its answer,
			// anything else is a mid-handler cancel. Calling both "cancelled" minted a periodic
			// successor beside a row already `PENDING`, and every completion minted another.
			if self.store.job_status(job.id).await?.as_deref() == Some("PENDING") {
				return Ok(Failure::Live);
			}
			// Cancelled while running. Rescheduling it would be an unbounded retry loop against
			// NAV on a filing a person explicitly stopped: the row is `FAILED`, which is terminal.
			tracing::warn!(job = job.id, kind = %job.kind,
				"job was cancelled while running; not rescheduling");
			return Ok(Failure::Cancelled);
		}
		Ok(Failure::Live)
	}

	/// Terminal failure. **The dedup key is kept** — see the module docs: it is a permanent
	/// idempotency record, so re-driving a terminal job means resetting that row explicitly
	/// (`CoreStore::job_redrive`).
	///
	/// `true` when the row is `FAILED` — by this call, by an operator's `job_cancel` landing
	/// mid-handler, or by a previous [`thrice`] attempt that committed and lost its answer. All
	/// three are terminal and the reason already on the row is the one to keep; `false` means
	/// the row is not terminal at all, so no periodic successor is minted for it.
	pub async fn terminate(
		&self,
		id: i64,
		now: Timestamp,
		err: &str,
		code: Option<&str>,
	) -> ClResult<bool> {
		if self.store.job_terminate(id, now, err, code).await? > 0 {
			return Ok(true);
		}
		let status = self.store.job_status(id).await?;
		tracing::warn!(
			job = id,
			status = status.as_deref().unwrap_or("gone"),
			"the job left RUNNING without this call; keeping the reason already recorded"
		);
		Ok(status.as_deref() == Some("FAILED"))
	}
}

#[cfg(test)]
mod tests {
	use super::{backoff_secs, retry_base};

	/// `Retry-After` used to be clamped to 24 h rather than to the cap, so one NAV `429` parked
	/// `NAV_REPORT` for a day despite `jobs.backoff_cap.NAV_REPORT = 600`.
	#[test]
	fn an_upstream_retry_after_cannot_exceed_the_operators_cap() {
		assert_eq!(retry_base(3, 600, Some(86_400)), 600);
		// It still wins while it is longer than ours and inside the cap, and never goes below.
		assert_eq!(retry_base(1, 600, Some(120)), 120);
		assert_eq!(retry_base(9, 600, Some(5)), backoff_secs(9, 600));
		assert_eq!(retry_base(3, 600, None), backoff_secs(3, 600));
	}

	/// Literals, not `backoff_secs` against itself: every retry-timing assertion in
	/// `tests/core.rs` computes its expectation with this function, so a regression to `0`
	/// would keep them all green while `NAV_REPORT` — unbounded by design — hot-looped
	/// against the tax authority.
	/// Every worker read the same `SystemTime::subsec_millis()`, so `jobs.workers` of them
	/// woke in lockstep on the single writer connection — the opposite of the spread's point.
	#[test]
	fn two_workers_do_not_compute_the_same_idle_sleep() {
		let sleeps: std::collections::HashSet<_> = (0..8).map(super::idle_sleep).collect();
		assert_eq!(sleeps.len(), 8, "the spread must differ per worker");
	}

	/// `thrice` retried every error, so a `Retry::Never` one — which answers the same way on
	/// every attempt — cost 400 ms of sleeping before the caller was told.
	#[tokio::test]
	async fn thrice_does_not_sleep_through_an_error_that_cannot_succeed() {
		use std::sync::atomic::{AtomicUsize, Ordering};

		let calls = AtomicUsize::new(0);
		let out = super::thrice(|| {
			calls.fetch_add(1, Ordering::SeqCst);
			async { Err::<(), _>(crate::error::Error::validation("no")) }
		})
		.await;
		assert!(out.is_err());
		assert_eq!(calls.load(Ordering::SeqCst), 1);

		let calls = AtomicUsize::new(0);
		let out = super::thrice(|| {
			calls.fetch_add(1, Ordering::SeqCst);
			async { Err::<(), _>(crate::error::Error::Unavailable("busy".into())) }
		})
		.await;
		assert!(out.is_err());
		assert_eq!(calls.load(Ordering::SeqCst), 3);
	}

	#[test]
	fn backoff_doubles_then_holds_at_the_cap() {
		assert_eq!(backoff_secs(0, 3600), 1);
		assert_eq!(backoff_secs(1, 3600), 2);
		assert_eq!(backoff_secs(11, 3600), 2048);
		assert_eq!(backoff_secs(12, 3600), 3600);
		assert_eq!(backoff_secs(-1, 3600), 3600);
		assert_eq!(backoff_secs(11, 100), 100);
	}
}

// vim: ts=4
