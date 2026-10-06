//! The in-process run pool: at most one live run per thread, admitted through a per-org and
//! then a global semaphore behind a bounded wait queue. Runs are not jobs — nothing here survives
//! a restart: each pool heartbeats its live runs, and [`sweep`] marks every live run whose
//! heartbeat stopped `interrupted`.

use std::{
	collections::HashMap,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use mintworks_core::{
	App, ClResult, Error,
	error::StatusCode,
	prelude::{RunId, Timestamp},
};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

use crate::store::{AgentRunStore, EventKind, NewRun, Run, RunEvent, RunStatus, ThreadStore};

pub const E_BUSY: &str = "E-AGENT-BUSY";

/// Broadcast capacity per run; a subscriber that lags past it re-reads from `agent_run_events`.
const EVENT_BUFFER: usize = 256;

fn busy(msg: &str) -> Error {
	Error::coded(StatusCode::TOO_MANY_REQUESTS, E_BUSY, msg)
}

/// # Errors
/// `E-CORE-INTERNAL` when no store was registered.
pub fn store(app: &App) -> ClResult<Arc<dyn AgentRunStore>> {
	app.extensions.get::<Arc<dyn AgentRunStore>>().cloned().ok_or_else(|| {
		Error::internal("mintworks-agent: no AgentRunStore was registered on the app")
	})
}

/// # Errors
/// `E-CORE-INTERNAL` when no store was registered.
pub fn threads(app: &App) -> ClResult<Arc<dyn ThreadStore>> {
	app.extensions
		.get::<Arc<dyn ThreadStore>>()
		.cloned()
		.ok_or_else(|| Error::internal("mintworks-agent: no ThreadStore was registered on the app"))
}

/// A run the pool holds: its event fan-out and its cancel switch.
#[derive(Clone)]
pub struct Live {
	pub run_id: i64,
	pub uid: RunId,
	pub tx: broadcast::Sender<RunEvent>,
	pub cancel: CancellationToken,
	store: Arc<dyn AgentRunStore>,
}

impl Live {
	/// Persist the run's next event, then broadcast it. A subscriber must `subscribe` before it
	/// replays `events_after`, and skip `seq`s it already replayed, or it misses the gap between.
	///
	/// # Errors
	/// The store's.
	pub async fn emit(&self, kind: EventKind, payload: &str) -> ClResult<i64> {
		let seq = self.store.event_append(self.run_id, kind, payload).await?;
		// No receiver is not an error: nobody is watching yet.
		let _ =
			self.tx
				.send(RunEvent { seq, kind, payload: payload.to_owned(), at: Timestamp::now() });
		Ok(seq)
	}
}

/// What the run body receives once admitted; dropping it frees the slots.
pub struct Admitted {
	pub run: Run,
	pub live: Live,
	_org: OwnedSemaphorePermit,
	_global: OwnedSemaphorePermit,
}

/// The global semaphore with the size it was built for, re-created when the setting changes.
// Permits held on a replaced semaphore still run, so a resize briefly allows old + new.
type Sized = (usize, Arc<Semaphore>);

fn sized(slot: &mut Option<Sized>, max: usize) -> Arc<Semaphore> {
	match slot {
		Some((n, sem)) if *n == max => sem.clone(),
		_ => slot.insert((max, Arc::new(Semaphore::new(max)))).1.clone(),
	}
}

/// Each distinct cap is its own semaphore: runs sharing a cap are bounded by it.
fn org_semaphore(
	orgs: &mut HashMap<(i64, usize), Arc<Semaphore>>,
	org: i64,
	max: usize,
) -> Arc<Semaphore> {
	orgs.entry((org, max)).or_insert_with(|| Arc::new(Semaphore::new(max))).clone()
}

#[derive(Default)]
struct Inner {
	/// Thread uid → its live run, queued or running.
	live: Mutex<HashMap<String, Live>>,
	// One entry per (org, cap) ever seen, never evicted; evict idle ones if org count explodes.
	orgs: Mutex<HashMap<(i64, usize), Arc<Semaphore>>>,
	global: Mutex<Option<Sized>>,
	waiting: AtomicUsize,
}

/// One per process, in the `App` extensions.
#[derive(Clone, Default)]
pub struct RunPool(Arc<Inner>);

/// Removes the thread's entry however the run task ends, panics included.
struct Entry(Arc<Inner>, String);

impl Drop for Entry {
	fn drop(&mut self) {
		self.0.live.lock().remove(&self.1);
	}
}

/// Counts a run as waiting until it holds both permits.
struct Waiting(Arc<Inner>);

impl Drop for Waiting {
	fn drop(&mut self) {
		self.0.waiting.fetch_sub(1, Ordering::Relaxed);
	}
}

impl RunPool {
	pub fn new() -> Self {
		Self::default()
	}

	/// Insert the run `queued`, admit it and spawn `body` once it holds its slots. `org_limit`
	/// is the run spec's override of `agent.max_concurrent_per_org`. A waiting run emits
	/// `queued`; one cancelled while waiting ends `cancelled` without `body` running. `body`
	/// owns every status change from `running` on.
	///
	/// # Errors
	/// `E-AGENT-BUSY` (429) when the thread already has a live run or the wait queue is full.
	pub async fn submit<F, Fut>(
		&self,
		app: &App,
		new: &NewRun<'_>,
		org_limit: Option<usize>,
		body: F,
	) -> ClResult<Live>
	where
		F: FnOnce(Admitted) -> Fut + Send + 'static,
		Fut: Future<Output = ()> + Send + 'static,
	{
		let store = store(app)?;
		let settings = &app.settings;
		let org_max = match org_limit {
			Some(n) => n.max(1),
			None => {
				usize::try_from(settings.int("agent.max_concurrent_per_org").await?).unwrap_or(1)
			}
		};
		let global_max = usize::try_from(settings.int("agent.max_concurrent").await?).unwrap_or(1);
		let queue_max = usize::try_from(settings.int("agent.queue_max").await?).unwrap_or(0);
		let thread = new.thread.as_str().to_owned();

		// Reserve the thread and decide wait-or-refuse under one lock, before the insert.
		let (org_sem, global_sem, ready, waiting) = {
			let mut live = self.0.live.lock();
			if live.contains_key(&thread) {
				return Err(busy("the thread already has a live run"));
			}
			let org_sem = org_semaphore(&mut self.0.orgs.lock(), new.org_id, org_max);
			let global_sem = sized(&mut self.0.global.lock(), global_max);
			let ready = match org_sem.clone().try_acquire_owned() {
				Ok(o) => global_sem.clone().try_acquire_owned().ok().map(|g| (o, g)),
				Err(_) => None,
			};
			let waiting = if ready.is_none() {
				if self.0.waiting.load(Ordering::Relaxed) >= queue_max {
					return Err(busy("too many agent runs are waiting"));
				}
				self.0.waiting.fetch_add(1, Ordering::Relaxed);
				Some(Waiting(self.0.clone()))
			} else {
				None
			};
			live.insert(
				thread.clone(),
				Live {
					run_id: 0,
					uid: new.uid.clone(),
					tx: broadcast::channel(EVENT_BUFFER).0,
					cancel: CancellationToken::new(),
					store: store.clone(),
				},
			);
			(org_sem, global_sem, ready, waiting)
		};
		let entry = Entry(self.0.clone(), thread.clone());

		let Some(run) = store.run_insert(new).await? else {
			return Err(busy("the thread already has a live run"));
		};
		let live = {
			let mut map = self.0.live.lock();
			let Some(l) = map.get_mut(&thread) else {
				return Err(Error::internal("agent pool entry vanished"));
			};
			l.run_id = run.id;
			l.clone()
		};

		if waiting.is_some()
			&& let Err(e) = live.emit(EventKind::Queued, "{}").await
		{
			tracing::warn!(run = run.uid.as_str(), "agent: queued event: {e}");
		}
		let task_live = live.clone();
		tokio::spawn(async move {
			let _entry = entry;
			let permits = if let Some(p) = ready {
				Some(p)
			} else {
				let _waiting = waiting;
				tokio::select! {
					() = task_live.cancel.cancelled() => None,
					p = async {
						let o = org_sem.acquire_owned().await.ok()?;
						let g = global_sem.acquire_owned().await.ok()?;
						Some((o, g))
					} => p,
				}
			};
			let Some((org, global)) = permits else {
				let s = &task_live.store;
				if let Err(e) = s.run_set_status(run.id, RunStatus::Cancelled, None).await {
					tracing::error!(run = run.uid.as_str(), "agent: cancel while queued: {e}");
				}
				let _ = task_live.emit(EventKind::Done, r#"{"status":"cancelled"}"#).await;
				return;
			};
			let (run_id, uid, after) = (run.id, run.uid.clone(), task_live.clone());
			// Spawned so a panic surfaces as a JoinError; `_entry` frees the slot only after this.
			let adm = Admitted { run, live: task_live, _org: org, _global: global };
			if let Err(e) = tokio::spawn(body(adm)).await {
				tracing::error!(run = uid.as_str(), "agent run panicked: {e}");
				let s = &after.store;
				if let Err(e) =
					s.run_set_status(run_id, RunStatus::Error, Some("run panicked")).await
				{
					tracing::error!(run = uid.as_str(), "agent: final status: {e}");
				}
				let payload =
					r#"{"status":"error","errCode":"E-CORE-INTERNAL","errStr":"internal error"}"#;
				let _ = after.emit(EventKind::Error, payload).await;
			}
		});
		Ok(live)
	}

	/// The live run `uid`, queued or running.
	pub fn get(&self, uid: &RunId) -> Option<Live> {
		self.0.live.lock().values().find(|l| &l.uid == uid).cloned()
	}

	/// Trip run `uid`'s token; `false` when it is not live in this process.
	pub fn cancel(&self, uid: &RunId) -> bool {
		self.get(uid).map(|l| l.cancel.cancel()).is_some()
	}
}

// Fixed lease, make it a setting if runs legitimately stall >2 min between heartbeats
const HEARTBEAT: Duration = Duration::from_secs(30);
const STALE: i64 = 120;

/// One stale sweep: every live run whose lease expired — its process died, or stopped
/// heartbeating — becomes `interrupted`, with a final `error` event. Never resumed: a Rune tool
/// may not be idempotent.
async fn sweep_stale(store: &dyn AgentRunStore) -> ClResult<usize> {
	let runs = store.runs_interrupt_stale(Timestamp(Timestamp::now().0 - STALE)).await?;
	for run in &runs {
		store
			.event_append(run.id, EventKind::Error, r#"{"status":"interrupted"}"#)
			.await?;
	}
	Ok(runs.len())
}

/// Boot: sweep once, then spawn the lease loop that every [`HEARTBEAT`] renews this pool's live
/// runs and sweeps the stale ones. A sibling process's runs survive while it heartbeats them.
///
/// # Errors
/// The store's, from the boot sweep.
pub async fn sweep(app: &App) -> ClResult<usize> {
	let store = store(app)?;
	let n = sweep_stale(&*store).await?;
	let pool = app.extensions.get::<RunPool>().cloned();
	tokio::spawn(async move {
		let mut tick = tokio::time::interval(HEARTBEAT);
		tick.tick().await;
		loop {
			tick.tick().await;
			if let Some(pool) = &pool {
				let ids: Vec<i64> =
					pool.0.live.lock().values().map(|l| l.run_id).filter(|&id| id > 0).collect();
				if let Err(e) = store.runs_heartbeat(&ids, Timestamp::now()).await {
					tracing::warn!("agent: heartbeat: {e}");
				}
			}
			if let Err(e) = sweep_stale(&*store).await {
				tracing::warn!("agent: stale sweep: {e}");
			}
		}
	});
	Ok(n)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn alternating_caps_do_not_reset_permits() {
		let mut orgs = HashMap::new();
		let one = org_semaphore(&mut orgs, 7, 1);
		let _held = one.clone().try_acquire_owned().unwrap();
		let _two = org_semaphore(&mut orgs, 7, 2);
		let again = org_semaphore(&mut orgs, 7, 1);
		assert!(Arc::ptr_eq(&one, &again));
		assert!(again.try_acquire_owned().is_err());
	}
}

// vim: ts=4
