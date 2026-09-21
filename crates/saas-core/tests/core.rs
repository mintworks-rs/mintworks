//! The `saas-core` unit tests that need a real database: the job runner, the audit trail,
//! the secret store and the two rate-limit cases that go through `Settings` or a built `App`.
//!
//! An integration test, not an inline `mod tests`: the `#[cfg(test)]` build of a crate is a
//! distinct crate from the one `store-adapter-sqlite` links against, so the store impls would
//! not unify (`E0599`). The adapter is therefore a dev-dependency, a cycle Cargo permits.
//!
//! The harness is a real file database in a temp dir, never `sqlite::memory:` — an in-memory
//! URL gives each connection its own database, so two stores would never contend for the
//! write lock.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use saas_core::auth_mw::ClientIp;
use saas_core::config::Config;
use saas_core::ctx::{Actor, Ctx};
use saas_core::error::{Error, Retry};
use saas_core::job::{DEFAULT_MAX_ATTEMPTS, Job, Next, Runner, backoff_secs, enqueue, has_live};
use saas_core::ratelimit::{RateLimiter, default_mw};
use saas_core::secrets::SecretStore;
use saas_core::settings::{Registry, SettingDef, Settings};
use saas_core::store::{CoreStore, Role};
use saas_core::types::Timestamp;
use saas_core::{AppBuilder, audit};
use store_adapter_sqlite::SqliteStore;

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, and the reader pool opens `create_if_missing(false)`, so
/// the store needs a file.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-core-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [7; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: self.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: String::new(),
			jobs_workers: None,
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// The `TmpDb` comes back with the store: dropping it deletes the database, so the caller
/// has to hold it for the length of the test.
async fn fresh(name: &str) -> (TmpDb, Arc<dyn CoreStore>, SqliteStore) {
	let db = TmpDb::new(name);
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	let store: Arc<dyn CoreStore> = Arc::new(sql.clone());
	(db, store, sql)
}

/// Stand-ins for the feature-crate keys these tests exercise: `saas-core` declares only its
/// own, and may not depend on `saas-email` or `saas-invoice` to borrow theirs.
static TEST_SETTINGS: &[SettingDef] = &[
	SettingDef::text("email.from", "", "Sender address.").required(),
	SettingDef::text("email.smtp.host", "", "SMTP host.").required(),
	SettingDef::int("email.smtp.port", "587", "SMTP port.").range(1, 65535),
	SettingDef::text("currency.base", "HUF", "Accounting currency.").range(3, 3),
];

/// Composed per call, not cached: the environment snapshot is taken here, and the re-exec
/// harness below is what puts a variable in it.
fn registry() -> Arc<Registry> {
	let (registry, errors) = Registry::build(
		&[saas_core::settings::SETTINGS, TEST_SETTINGS],
		&[],
		&["auth.jwt_key", "email.smtp.password", "payment.barion.pos_key"],
	);
	assert!(errors.is_empty(), "{errors:?}");
	Arc::new(registry)
}

fn settings(store: Arc<dyn CoreStore>) -> Settings {
	Settings::new(store, registry())
}

fn secret_store(store: Arc<dyn CoreStore>, master_key: [u8; 32]) -> SecretStore {
	SecretStore::new(store, master_key, registry())
}

/// Re-runs one test in a child process with `var` set, and returns `true` in the parent — which
/// must then `return`. `std::env::set_var` is `unsafe` on edition 2024 and this workspace forbids
/// `unsafe`, and a process-wide variable would reach every other test in this binary.
///
/// The child's other framework and bootstrap variables are cleared.
fn reexec(name: &str, var: &str, value: &str) -> bool {
	reexec_with(name, &[(var, value)])
}

/// [`reexec`] with more than one variable, for a table whose rows resolve different scopes.
fn reexec_with(name: &str, vars: &[(&str, &str)]) -> bool {
	/// The framework's bootstrap variables, which `Config::from_env` reads and which
	/// `example/backend/.env` also exports.
	const BOOTSTRAP: [&str; 5] = ["MASTER_KEY", "DB_PATH", "DATA_DIR", "LISTEN", "BASE_URL"];

	/// Top-level namespaces of the declared keys, which with no prefix on the variable is what
	/// "a framework variable" means. A developer's exported `EMAIL_SMTP_HOST` must not reach a
	/// child asserting a registry default.
	const NAMESPACES: [&str; 14] = [
		"ADMIN_",
		"AUTH_",
		"CURRENCY_",
		"DEPLOYMENT_",
		"DUNNING_",
		"EMAIL_",
		"HTTP_",
		"INVOICE_",
		"JOBS_",
		"NAV_",
		"PAYMENT_",
		"POW_",
		"RATELIMIT_",
		"STORAGE_",
	];

	/// Set on the child only, so the guard cannot be satisfied by a developer's shell: comparing
	/// the *value* meant an exported `CURRENCY_BASE=EUR` skipped the re-exec entirely. Inert as
	/// a setting — `test.reexec` is declared nowhere, so it is never read.
	const MARKER: &str = "TEST_REEXEC";

	if std::env::var(MARKER).is_ok() {
		return false;
	}
	let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
	cmd.args([name, "--exact", "--nocapture"])
		.envs(vars.iter().copied())
		.env(MARKER, name);
	for (k, _) in std::env::vars().filter(|(k, _)| {
		(NAMESPACES.iter().any(|p| k.starts_with(p)) || BOOTSTRAP.contains(&k.as_str()))
			&& !vars.iter().any(|(v, _)| v == k)
			&& k != MARKER
	}) {
		cmd.env_remove(k);
	}
	let out = cmd.output().unwrap();
	let (stdout, stderr) =
		(String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
	// `1 passed`, not just a zero exit: libtest exits 0 when `--exact` matches nothing, so a
	// renamed test went on passing while running nothing at all.
	assert!(out.status.success() && stdout.contains("1 passed"), "{name}: {stdout}{stderr}");
	true
}

/// [`reexec`] with no override: the child gets the cleared environment and nothing else, which is
/// what a test asserting a *registry* default needs.
fn reexec_clean(name: &str) -> bool {
	reexec(name, "TEST_CLEAN_ENV", "1")
}

// ---------------------------------------------------------------- job runner

/// Delegates every `CoreStore` call to a real store except [`CoreStore::account_for_token`],
/// which always answers `E-CORE-UNAVAILABLE` — the reader pool being down. `.1` makes that
/// many [`CoreStore::setting_get`] calls answer the same way, for a *transient* outage, and
/// `.2` makes that many [`CoreStore::job_fail`] calls **commit and then fail**: the lost-answer
/// shape, which the row count alone cannot tell from an operator's cancel.
/// `.3` cancels that `(kind, payload)` on every [`CoreStore::setting_get`], which is how a
/// test lands an operator's cancel inside a tick that has already claimed the row.
struct PoolDown(
	Arc<dyn CoreStore>,
	std::sync::atomic::AtomicI64,
	std::sync::atomic::AtomicI64,
	Option<(String, String)>,
	/// Remaining `job_status` reads to fail.
	std::sync::atomic::AtomicI64,
);

impl PoolDown {
	fn new(inner: Arc<dyn CoreStore>) -> Self {
		Self::of(inner, 0, 0, None, 0)
	}

	fn settings_down(inner: Arc<dyn CoreStore>, reads: i64) -> Self {
		Self::of(inner, reads, 0, None, 0)
	}

	fn fails_lost(inner: Arc<dyn CoreStore>, writes: i64) -> Self {
		Self::of(inner, 0, writes, None, 0)
	}

	fn status_down(inner: Arc<dyn CoreStore>, reads: i64) -> Self {
		Self::of(inner, 0, 0, None, reads)
	}

	fn cancels_on_setting_read(inner: Arc<dyn CoreStore>, kind: &str, payload: &str) -> Self {
		Self::of(inner, 0, 0, Some((kind.to_owned(), payload.to_owned())), 0)
	}

	fn of(
		inner: Arc<dyn CoreStore>,
		reads: i64,
		writes: i64,
		cancel: Option<(String, String)>,
		statuses: i64,
	) -> Self {
		use std::sync::atomic::AtomicI64;
		Self(inner, AtomicI64::new(reads), AtomicI64::new(writes), cancel, AtomicI64::new(statuses))
	}
}

#[async_trait::async_trait]
impl CoreStore for PoolDown {
	async fn account_for_token(
		&self,
		_uid: &str,
	) -> Result<Option<saas_core::store::TokenAccount>, Error> {
		Err(Error::Unavailable("reader pool is down".to_owned()))
	}

	async fn api_key_by_prefix(
		&self,
		prefix: &str,
	) -> Result<Option<saas_core::store::ApiKey>, Error> {
		self.0.api_key_by_prefix(prefix).await
	}

	async fn touch_api_key(&self, id: i64, at: Timestamp) -> Result<(), Error> {
		self.0.touch_api_key(id, at).await
	}

	async fn setting_get(&self, key: &str) -> Result<Option<String>, Error> {
		if self.1.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) > 0 {
			return Err(Error::Unavailable("reader pool is down".to_owned()));
		}
		if let Some((kind, payload)) = &self.3 {
			self.0
				.job_cancel(kind, payload, Timestamp(0), "cancelled by an operator", None)
				.await?;
		}
		self.0.setting_get(key).await
	}
	async fn setting_set(&self, k: &str, raw: &str, by: Option<i64>) -> Result<(), Error> {
		self.0.setting_set(k, raw, by).await
	}
	async fn secret_get(&self, key: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
		self.0.secret_get(key).await
	}
	async fn secret_set(
		&self,
		key: &str,
		nonce: &[u8],
		ct: &[u8],
		by: Option<i64>,
	) -> Result<(), Error> {
		self.0.secret_set(key, nonce, ct, by).await
	}
	async fn secret_put_if_absent(&self, key: &str, nonce: &[u8], ct: &[u8]) -> Result<(), Error> {
		self.0.secret_put_if_absent(key, nonce, ct).await
	}
	async fn secret_updated_at(&self, key: &str) -> Result<Option<Timestamp>, Error> {
		self.0.secret_updated_at(key).await
	}
	async fn audit_log(&self, entry: &saas_core::store::AuditEntry) -> Result<(), Error> {
		self.0.audit_log(entry).await
	}
	async fn audit_detached(&self, entry: &saas_core::store::AuditEntry) -> Result<(), Error> {
		self.0.audit_detached(entry).await
	}
	async fn job_enqueue(
		&self,
		kind: &str,
		payload: &str,
		dedup: Option<&str>,
		run_at: Timestamp,
	) -> Result<Option<i64>, Error> {
		self.0.job_enqueue(kind, payload, dedup, run_at).await
	}
	async fn job_has_live(&self, kind: &str, besides: Option<i64>) -> Result<bool, Error> {
		self.0.job_has_live(kind, besides).await
	}
	async fn job_seed_periodic(&self, kind: &str, run_at: Timestamp) -> Result<Option<i64>, Error> {
		self.0.job_seed_periodic(kind, run_at).await
	}
	async fn job_statuses_by_keys(&self, keys: &[String]) -> Result<Vec<(String, String)>, Error> {
		self.0.job_statuses_by_keys(keys).await
	}
	async fn job_status_by_key(&self, dedup_key: &str) -> Result<Option<String>, Error> {
		self.0.job_status_by_key(dedup_key).await
	}
	async fn job_redrive_done(
		&self,
		dedup_key: &str,
		payload: &str,
		now: Timestamp,
	) -> Result<u64, Error> {
		self.0.job_redrive_done(dedup_key, payload, now).await
	}
	async fn job_status(&self, id: i64) -> Result<Option<String>, Error> {
		if self.4.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) > 0 {
			return Err(Error::Unavailable("pool timed out".to_owned()));
		}
		self.0.job_status(id).await
	}
	async fn job_claim(&self, now: Timestamp) -> Result<Option<Job>, Error> {
		self.0.job_claim(now).await
	}
	async fn job_complete(&self, id: i64, now: Timestamp) -> Result<u64, Error> {
		self.0.job_complete(id, now).await
	}
	async fn job_defer(&self, id: i64, run_at: Timestamp) -> Result<u64, Error> {
		self.0.job_defer(id, run_at).await
	}
	async fn job_fail(
		&self,
		id: i64,
		run_at: Timestamp,
		err: &str,
		code: Option<&str>,
	) -> Result<u64, Error> {
		let done = self.0.job_fail(id, run_at, err, code).await?;
		if self.2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) > 0 {
			// Committed, then the answer lost: exactly what a dropped connection does.
			return Err(Error::Unavailable("the writer went away after the commit".to_owned()));
		}
		Ok(done)
	}
	async fn job_terminate(
		&self,
		id: i64,
		now: Timestamp,
		err: &str,
		code: Option<&str>,
	) -> Result<u64, Error> {
		self.0.job_terminate(id, now, err, code).await
	}
	async fn job_redrive(&self, kind: &str, payload: &str, now: Timestamp) -> Result<u64, Error> {
		self.0.job_redrive(kind, payload, now).await
	}
	async fn job_cancel(
		&self,
		kind: &str,
		payload: &str,
		now: Timestamp,
		err: &str,
		code: Option<&str>,
	) -> Result<u64, Error> {
		self.0.job_cancel(kind, payload, now, err, code).await
	}
	async fn job_reclaim(&self, before: Timestamp) -> Result<u64, Error> {
		self.0.job_reclaim(before).await
	}
	async fn job_sweep(&self, cutoff: Timestamp) -> Result<u64, Error> {
		self.0.job_sweep(cutoff).await
	}
	async fn job_status_counts(&self) -> Result<Vec<(String, i64, Timestamp, Timestamp)>, Error> {
		self.0.job_status_counts().await
	}
	async fn job_retrying_kinds(&self) -> Result<Vec<String>, Error> {
		self.0.job_retrying_kinds().await
	}
	async fn job_stale(
		&self,
		kind: &str,
		before: Timestamp,
	) -> Result<Option<(i64, Timestamp)>, Error> {
		self.0.job_stale(kind, before).await
	}
	async fn var_get(&self, name: &str) -> Result<Option<String>, Error> {
		self.0.var_get(name).await
	}
	async fn var_set(&self, name: &str, value: &str) -> Result<(), Error> {
		self.0.var_set(name, value).await
	}
	async fn db_version(&self) -> Result<i64, Error> {
		self.0.db_version().await
	}
	async fn org_membership_role(&self, id: i64, uid: &str) -> Result<Option<(i64, Role)>, Error> {
		self.0.org_membership_role(id, uid).await
	}
	async fn org_role(&self, account_id: i64, org_id: i64) -> Result<Option<Role>, Error> {
		self.0.org_role(account_id, org_id).await
	}
	async fn root_org_id(&self) -> Result<i64, Error> {
		self.0.root_org_id().await
	}
}

async fn row(sql: &SqliteStore, id: i64) -> (String, i64, i64) {
	sqlx::query_as("SELECT status, attempts, run_at FROM jobs WHERE id = ?")
		.bind(id)
		.fetch_one(sql.read_pool())
		.await
		.unwrap()
}

async fn status_and_key(sql: &SqliteStore, id: i64) -> (String, Option<String>) {
	sqlx::query_as("SELECT status, dedup_key FROM jobs WHERE id = ?")
		.bind(id)
		.fetch_one(sql.read_pool())
		.await
		.unwrap()
}

#[tokio::test]
async fn dedup_claim_backoff_and_terminal_failure() {
	let (_db, store, sql) = fresh("job-dedup").await;

	let id = enqueue(&store, "boom", "{}", Some("k"), Timestamp(0)).await.unwrap().unwrap();
	// A dedup collision is a silent no-op — that is the once-only guarantee.
	assert!(enqueue(&store, "boom", "{}", Some("k"), Timestamp(0)).await.unwrap().is_none());

	let mut r = Runner::new(Arc::clone(&store));
	// `Retry::Backoff`, or the first failure would be the last: `Retry::Never` terminates
	// on attempt one, which is what the attempt-spending loop below needs not to happen.
	r.register("boom", |_| async { Err(Error::Unavailable("nope".to_owned())) });

	// Claimed, run, failed: back to PENDING one backoff step into the future. The tick
	// itself is `Ok(true)` — a handler failure is the job's, not the runner's, and
	// `fail_hard` swallows a transient writer error rather than stranding the row `RUNNING`.
	assert!(r.tick(Timestamp(0)).await.unwrap());
	// The literal, not `backoff_secs(1, 3600)`: the function is pinned by its own unit test,
	// and an expectation computed from the code under test asserts nothing.
	assert_eq!(row(&sql, id).await, ("PENDING".to_owned(), 1, 2));
	// And not claimable again until then.
	assert!(!r.tick(Timestamp(0)).await.unwrap());

	// Spend the remaining attempts, stepping `now` by the backoff cap each time so the
	// row is always claimable again.
	for i in 1..DEFAULT_MAX_ATTEMPTS {
		assert!(r.tick(Timestamp((1 << 40) + i * 3600)).await.unwrap());
	}
	let (status, attempts, _) = row(&sql, id).await;
	assert_eq!((status.as_str(), attempts), ("FAILED", DEFAULT_MAX_ATTEMPTS));
	// Terminal means terminal: nothing is claimable any more.
	assert!(!r.tick(Timestamp(1 << 40)).await.unwrap());
}

/// `email.smtp.host` unset used to be a silent success: the `SEND_EMAIL` job reached `DONE`,
/// `job_complete` blanked the payload and the activation link was gone, with no alert. The
/// boot gate names **every** missing key at once — one per restart is the other failure mode.
#[tokio::test]
async fn a_required_setting_left_blank_refuses_to_boot() {
	let (_db, store, _sql) = fresh("settings-required").await;
	let settings = settings(Arc::clone(&store));

	let err = settings.check_required("email.").await.unwrap_err();
	let msg = err.to_string();
	assert!(msg.contains("email.from"), "{msg}");
	assert!(msg.contains("email.smtp.host"), "{msg}");

	// Per feature prefix: a consumer embedding only `saas-auth` is not made to configure NAV.
	assert!(!msg.contains("nav."), "{msg}");

	settings.set("email.from", "noreply@e.st", None).await.unwrap();
	settings.set("email.smtp.host", "smtp.e.st", None).await.unwrap();
	settings.check_required("email.").await.unwrap();
}

/// `Runner::fail` was the only attempts ceiling, and it is reached only when a handler
/// *returns*. A handler that killed the process left its row `PENDING` with the claim's
/// increment already banked, so every boot re-claimed it and `attempts` climbed forever.
#[tokio::test]
async fn a_job_past_its_attempt_ceiling_is_not_claimed_again() {
	let (_db, store, sql) = fresh("job-poison").await;
	let settings = settings(Arc::clone(&store));
	settings.set("jobs.max_attempts.poison", "2", None).await.unwrap();

	// A handler that succeeds: if the ceiling let the job through, the row reads `DONE`.
	let mut r = Runner::new(Arc::clone(&store));
	r.register("poison", |_| async { Ok(()) });

	let stranded = async |attempts: i64, key: &str| {
		let id = enqueue(&store, "poison", "{}", Some(key), Timestamp(0)).await.unwrap().unwrap();
		sqlx::query("UPDATE jobs SET attempts = ? WHERE id = ?")
			.bind(attempts)
			.bind(id)
			.execute(sql.write_pool())
			.await
			.unwrap();
		id
	};

	// The claim increments first, so `attempts = 2` is a job on its second and final
	// permitted attempt: it still runs.
	let last_chance = stranded(1, "poison:ok").await;
	assert!(r.tick(Timestamp(0)).await.unwrap());
	assert_eq!(row(&sql, last_chance).await.0, "DONE");

	let poisoned = stranded(3, "poison:dead").await;
	assert!(r.tick(Timestamp(0)).await.unwrap());
	let (status, err_code): (String, Option<String>) =
		sqlx::query_as("SELECT status, err_code FROM jobs WHERE id = ?")
			.bind(poisoned)
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!((status.as_str(), err_code.as_deref()), ("FAILED", Some("E-CORE-JOB-POISON")));
	assert!(!r.tick(Timestamp(0)).await.unwrap(), "terminal means terminal");
}

/// The poison path's "is the row still live" read was a bare `?` between the claim and
/// `terminate_hard`, so one reader stall left the row `RUNNING` — invisible to `job_claim`,
/// `job_stale` and `job_retrying_kinds` alike — until the next process start.
#[tokio::test]
async fn a_poisoned_job_is_failed_even_when_its_status_read_fails() {
	let (_db, store, sql) = fresh("job-poison-status-down").await;
	let settings = settings(Arc::clone(&store));
	settings.set("jobs.max_attempts.poison", "2", None).await.unwrap();

	let id = enqueue(&store, "poison", "{}", Some("k"), Timestamp(0)).await.unwrap().unwrap();
	sqlx::query("UPDATE jobs SET attempts = 3 WHERE id = ?")
		.bind(id)
		.execute(sql.write_pool())
		.await
		.unwrap();

	// One failing read, and it is the poison guard's: `limits` reads settings, `terminate`'s
	// own fallback read never runs because `job_terminate` moves the row.
	let flaky: Arc<dyn CoreStore> = Arc::new(PoolDown::status_down(Arc::clone(&store), 1));
	let mut r = Runner::new(flaky);
	r.register("poison", |_| async { Ok(()) });
	assert!(r.tick(Timestamp(0)).await.unwrap());

	let (status, err_code): (String, Option<String>) =
		sqlx::query_as("SELECT status, err_code FROM jobs WHERE id = ?")
			.bind(id)
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!((status.as_str(), err_code.as_deref()), ("FAILED", Some("E-CORE-JOB-POISON")));
}

/// A bare `?` on the per-kind ceiling read sat between the claim and every `*_hard` write, so
/// a settings read that failed left the row `RUNNING` — invisible to `job_claim`, `job_stale`
/// and `job_retrying_kinds`, all PENDING-only — until the next process start's `reclaim`.
///
/// `fail` then re-read the two retry settings itself, so a *persistent* outage failed all three
/// `thrice` attempts: nothing was written and `fail_hard`'s `false` skipped the reschedule,
/// killing a periodic chain for the life of the process.
///
/// The chain is carried by the *retry* rather than by a successor: a ceiling that could not be
/// read is not a ceiling, so the fallback is unbounded and even a row past
/// `DEFAULT_MAX_ATTEMPTS` goes back to `PENDING`, keeping the kind at exactly one live row.
#[tokio::test]
async fn a_failed_settings_read_leaves_the_job_pending_not_running() {
	// `reads = 1` is the claim's ceiling lookup alone — `fail`'s own reads succeed, so the
	// test asserts the missing `?` rather than the outage. `99` is the sustained outage.
	for (label, periodic, banked, reads) in [
		("one failing ceiling read", false, 0, 1),
		("one failing read on a periodic chain", true, DEFAULT_MAX_ATTEMPTS, 1),
		("a sustained outage on a periodic chain", true, DEFAULT_MAX_ATTEMPTS, 99),
	] {
		let (_db, store, sql) = fresh(&format!("job-settings-read-{reads}-{periodic}")).await;
		let kind = if periodic { "sweepy" } else { "broken" };
		let key = (!periodic).then_some("k");
		let id = enqueue(&store, kind, "{}", key, Timestamp(0)).await.unwrap().unwrap();
		if banked > 0 {
			// Past the family ceiling, which no longer decides anything here: the read that
			// failed is what would have supplied it.
			sqlx::query("UPDATE jobs SET attempts = ? WHERE id = ?")
				.bind(banked)
				.bind(id)
				.execute(sql.write_pool())
				.await
				.unwrap();
		}

		let flaky: Arc<dyn CoreStore> =
			Arc::new(PoolDown::settings_down(Arc::clone(&store), reads));
		let mut r = Runner::new(flaky);
		let boom = |_| async { panic!("the handler must never be reached") };
		if periodic {
			r.register_periodic(kind, 60, boom);
		} else {
			r.register(kind, boom);
		}

		assert!(r.tick(Timestamp(0)).await.unwrap(), "{label}");
		let (status, attempts, run_at) = row(&sql, id).await;
		assert_eq!(status, "PENDING", "{label}: not stranded RUNNING, and not given up on");
		assert_eq!(attempts, banked + 1, "{label}");
		let base = backoff_secs(banked + 1, 3600);
		let want_run_at = if periodic { base + id.rem_euclid(base / 4 + 1) } else { base };
		assert_eq!(run_at, want_run_at, "{label}: the normal backoff, not a stranded row");
		assert_eq!(successors(&sql, kind).await, 1, "{label}: the retry carries the chain");
	}
}

/// `tick` awaited the handler with no deadline, and the `jobs` table has no lease column, so a
/// relay trickling a byte every 29 s held a worker forever with its row `RUNNING` — invisible
/// to `job_claim`, `job_stale` and every alert until a restart.
#[tokio::test]
async fn a_handler_that_never_returns_is_not_left_running_forever() {
	let (_db, store, sql) = fresh("job-handler-deadline").await;
	let settings = settings(Arc::clone(&store));
	settings.set("jobs.timeout_secs.wedged", "1", None).await.unwrap();
	let id = enqueue(&store, "wedged", "{}", None, Timestamp(0)).await.unwrap().unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	r.register("wedged", |_| async {
		tokio::time::sleep(std::time::Duration::from_secs(600)).await;
		Ok(())
	});

	assert!(r.tick(Timestamp(0)).await.unwrap());
	let (status, attempts, _) = row(&sql, id).await;
	assert_eq!((status.as_str(), attempts), ("PENDING", 1), "back in reach of `job_stale`");
	let code: Option<String> = sqlx::query_scalar("SELECT err_code FROM jobs WHERE id = ?")
		.bind(id)
		.fetch_one(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(code.as_deref(), Some("E-CORE-TIMEOUT"));
}

#[tokio::test]
async fn a_poisoned_periodic_job_still_reschedules_its_successor() {
	let (_db, store, sql) = fresh("job-periodic-poison").await;
	let settings = settings(Arc::clone(&store));
	settings.set("jobs.max_attempts.sweepy", "2", None).await.unwrap();
	let id = enqueue(&store, "sweepy", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	sqlx::query("UPDATE jobs SET attempts = 9 WHERE id = ?")
		.bind(id)
		.execute(sql.write_pool())
		.await
		.unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	r.register_periodic("sweepy", 60, |_| async { panic!("the handler must never be reached") });

	assert!(r.tick(Timestamp(0)).await.unwrap());
	assert_eq!(row(&sql, id).await.0, "FAILED");
	assert_eq!(successors(&sql, "sweepy").await, 2, "the chain must have a successor");
}

/// The poison branch discarded `terminate_hard`'s answer, so a `job_cancel` landing during the
/// ceiling read — which leaves the row `FAILED` by the operator's hand — still minted a
/// periodic successor, undoing the cancel `Failure::Cancelled` exists to honour.
#[tokio::test]
async fn a_cancel_during_a_poisoned_tick_stops_the_periodic_chain() {
	let (_db, store, sql) = fresh("job-poison-cancel").await;
	let settings = settings(Arc::clone(&store));
	settings.set("jobs.max_attempts.sweepy", "2", None).await.unwrap();
	let id = enqueue(&store, "sweepy", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	sqlx::query("UPDATE jobs SET attempts = 9 WHERE id = ?")
		.bind(id)
		.execute(sql.write_pool())
		.await
		.unwrap();

	// The cancel lands after the claim, on the settings read the poison check sits behind.
	let cancelling: Arc<dyn CoreStore> =
		Arc::new(PoolDown::cancels_on_setting_read(Arc::clone(&store), "sweepy", "{}"));
	let mut r = Runner::new(cancelling);
	r.register_periodic("sweepy", 60, |_| async { panic!("the handler must never be reached") });

	assert!(r.tick(Timestamp(0)).await.unwrap());
	assert_eq!(row(&sql, id).await.0, "FAILED");
	assert_eq!(successors(&sql, "sweepy").await, 1, "a successor would undo the cancel");
}

/// How many `jobs` rows the periodic chain for `kind` has, the seed included.
async fn successors(sql: &SqliteStore, kind: &str) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = ?")
		.bind(kind)
		.fetch_one(sql.read_pool())
		.await
		.unwrap()
}

/// `job_sweep`'s `LIKE` folds ASCII case, so it reclaimed a caller's `PERIODIC:…` key — the
/// permanent idempotency record "an invoice is never reported to NAV twice" rests on.
#[tokio::test]
async fn the_sweep_reclaims_only_the_lowercase_periodic_namespace() {
	let (_db, store, sql) = fresh("job-sweep-case").await;
	for key in ["periodic:x", "PERIODIC:x", "nav:inv_1"] {
		sqlx::query(
			"INSERT INTO jobs (kind, payload, dedup_key, status, run_at, created_at, done_at)
			 VALUES ('k', '', ?, 'DONE', 0, 0, 0)",
		)
		.bind(key)
		.execute(sql.write_pool())
		.await
		.unwrap();
	}
	assert_eq!(store.job_sweep(Timestamp(1)).await.unwrap(), 1);

	let left: Vec<String> = sqlx::query_scalar("SELECT dedup_key FROM jobs ORDER BY dedup_key")
		.fetch_all(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(left, vec!["PERIODIC:x".to_owned(), "nav:inv_1".to_owned()]);

	// And the guard that keeps such a key out in the first place.
	enqueue(&store, "k", "{}", Some("PERIODIC:x2"), Timestamp(0)).await.unwrap_err();
}

/// A `SEND_EMAIL` payload carries the full `{base_url}/activate?token=…` link, so a `DONE` row
/// holding it is an account-takeover credential in any backup or read-only operator query for
/// the 24 h (2 h for reset) those tokens live — and `jobs` is outside both `export_account` and
/// `anonymize_account`, so nothing else ever clears it.
#[tokio::test]
async fn a_completed_job_keeps_its_dedup_key_and_loses_its_payload() {
	let (_db, store, sql) = fresh("job-payload").await;
	let secret = r#"{"url":"https://app.example/reset-password?token=live-secret"}"#;
	let done = enqueue(&store, "mail", secret, Some("mail:1"), Timestamp(0))
		.await
		.unwrap()
		.unwrap();
	let doomed = enqueue(&store, "boom", secret, Some("boom:1"), Timestamp(0))
		.await
		.unwrap()
		.unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	r.register("mail", |_| async { Ok(()) });
	r.register("boom", |_| async { Err(Error::internal("nope")) });
	assert!(r.tick(Timestamp(0)).await.unwrap());

	let (status, payload, key): (String, String, Option<String>) =
		sqlx::query_as("SELECT status, payload, dedup_key FROM jobs WHERE id = ?")
			.bind(done)
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!(status, "DONE");
	assert_eq!(payload, "", "a DONE job never runs again and must not hold its link");
	// The once-only guarantee — "an invoice is never reported to NAV twice" — rests on
	// the key surviving `DONE`.
	assert_eq!(key.as_deref(), Some("mail:1"));

	// A FAILED job keeps its payload: it is the diagnostic, and a failed email job is one
	// that never delivered, so the link is the lesser concern next to not knowing why.
	for i in 0..DEFAULT_MAX_ATTEMPTS {
		r.tick(Timestamp((1 << 40) + i * 3600)).await.unwrap();
	}
	let (status, payload): (String, String) =
		sqlx::query_as("SELECT status, payload FROM jobs WHERE id = ?")
			.bind(doomed)
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!(status, "FAILED");
	assert_eq!(payload, secret, "a failed job's payload is its diagnostic");
}

/// Nothing deleted from `jobs`, so the table grew one row per email, NAV report and PDF
/// render forever — and `issued_without_document` correlates over `jobs.payload`. The hazard
/// the retention has to respect: `dedup_key` survives `DONE` **on purpose**, and that is the
/// whole of "an invoice is never reported to NAV twice", so deleting a keyed row would
/// release its key and let a replayed enqueue file the same invoice a second time.
#[tokio::test]
async fn retention_drops_unkeyed_rows_and_never_releases_a_dedup_key() {
	let (_db, store, sql) = fresh("job-retention").await;

	let keyed = enqueue(&store, "nav", "{}", Some("nav:invoice:1"), Timestamp(0))
		.await
		.unwrap()
		.unwrap();
	let unkeyed = enqueue(&store, "mail", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	for id in [keyed, unkeyed] {
		// `job_complete` is `RUNNING`-guarded, so the row has to be claimed first.
		assert_eq!(store.job_claim(Timestamp(0)).await.unwrap().unwrap().id, id);
		store.job_complete(id, Timestamp(1_000)).await.unwrap();
	}
	// A pending row of each kind, which retention must not touch whatever its age.
	let live = enqueue(&store, "mail", "{}", None, Timestamp(0)).await.unwrap().unwrap();

	// Nothing is old enough yet.
	assert_eq!(store.job_sweep(Timestamp(1_000)).await.unwrap(), 0, "done_at < cutoff, strictly");
	assert_eq!(store.job_sweep(Timestamp(1_001)).await.unwrap(), 1);

	let survivors: Vec<i64> = sqlx::query_scalar("SELECT id FROM jobs ORDER BY id")
		.fetch_all(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(survivors, [keyed, live], "the keyed row or the pending one was swept");

	// The once-only guarantee still holds after retention has run.
	assert!(
		enqueue(&store, "nav", "{}", Some("nav:invoice:1"), Timestamp(0))
			.await
			.unwrap()
			.is_none(),
		"retention released a dedup key and the invoice could be filed twice"
	);
}

/// A *periodic* successor's key is a scheduling token, not an idempotency record — see
/// `job::PERIODIC_KEY_PREFIX`. `ALERT_SWEEP` runs every 60s, so before retention could
/// reclaim that namespace the framework minted one permanently undeletable `DONE` row per
/// minute: ~525k rows a year on an idle deployment. A `FAILED` row is swept too, so
/// `A-JOB-FAILED` can eventually clear.
#[tokio::test]
async fn retention_reclaims_a_spent_periodic_key_and_failed_rows() {
	let (_db, store, sql) = fresh("job-retention-periodic").await;

	let periodic = reserved(&store, "ALERT_SWEEP", "periodic:ALERT_SWEEP:60", Timestamp(0)).await;
	let nav = enqueue(&store, "nav", "{}", Some("nav:invoice:1"), Timestamp(0))
		.await
		.unwrap()
		.unwrap();
	let failed = enqueue(&store, "mail", "{}", None, Timestamp(0)).await.unwrap().unwrap();

	for id in [periodic, nav] {
		assert_eq!(store.job_claim(Timestamp(0)).await.unwrap().unwrap().id, id);
		store.job_complete(id, Timestamp(1_000)).await.unwrap();
	}
	assert_eq!(store.job_claim(Timestamp(0)).await.unwrap().unwrap().id, failed);
	store.job_terminate(failed, Timestamp(1_000), "gave up", None).await.unwrap();

	assert_eq!(store.job_sweep(Timestamp(1_001)).await.unwrap(), 2);
	let survivors: Vec<i64> = sqlx::query_scalar("SELECT id FROM jobs ORDER BY id")
		.fetch_all(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(survivors, [nav], "a handler-supplied dedup key was released");

	// The reclaimed namespace is reusable; the NAV one still is not.
	assert!(
		store
			.job_enqueue("ALERT_SWEEP", "{}", Some("periodic:ALERT_SWEEP:60"), Timestamp(0))
			.await
			.unwrap()
			.is_some()
	);
	assert!(
		enqueue(&store, "nav", "{}", Some("nav:invoice:1"), Timestamp(0))
			.await
			.unwrap()
			.is_none()
	);
}

/// `A-JOB-FAILED` had no age bound while nothing ever deleted a `FAILED` row, so one
/// `SEND_EMAIL` that failed a year ago — or one deliberate `Nav::cancel_filing` — pinned
/// `/api/admin/stats` at ERROR forever, and an always-on error indicator is one nobody reads.
#[tokio::test]
async fn a_stale_failure_no_longer_raises_a_job_failed() {
	let (db, store, sql) = fresh("alert-job-failed-window").await;
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::clone(&store))
		.build()
		.await
		.unwrap();

	let now = Timestamp::now();
	let id = enqueue(&store, "mail", "{}", None, now).await.unwrap().unwrap();
	store.job_claim(now).await.unwrap().unwrap();
	store.job_terminate(id, now, "gave up", None).await.unwrap();

	let raised =
		|alerts: &[saas_core::alert::Alert]| alerts.iter().any(|a| a.code == "A-JOB-FAILED");
	assert!(raised(&saas_core::alert::alerts(&app).await.unwrap()), "a fresh failure is silent");

	// `jobs.failed_alert_hours` defaults to 24; age the row past it. **Both** timestamps: the
	// window is on the newest *transition*, not the newest enqueue.
	let age = |created: i64, done: i64| {
		sqlx::query("UPDATE jobs SET created_at = ?, done_at = ? WHERE id = ?")
			.bind(now.0 - created)
			.bind(now.0 - done)
			.bind(id)
			.execute(sql.write_pool())
	};
	age(40 * 3600, 40 * 3600).await.unwrap();
	assert!(
		!raised(&saas_core::alert::alerts(&app).await.unwrap()),
		"a failure older than the window still pins the dashboard at ERROR"
	);

	// The converse, and the reason the window moved off `created_at`: an operator calling
	// `Nav::cancel_filing` today on a job enqueued three days ago fails it *now*. Bounding on
	// the enqueue hid the cancelled statutory filing the alert exists to surface.
	age(72 * 3600, 0).await.unwrap();
	assert!(
		raised(&saas_core::alert::alerts(&app).await.unwrap()),
		"a failure that happened just now was aged out by its enqueue time"
	);
}

/// `/readyz` reports the *framework* module's version, and an unmigrated database is genuinely
/// version 0 rather than an error — the two states an operator most needs to tell apart.
#[tokio::test]
async fn db_version_reports_the_framework_module_and_zero_when_absent() {
	let (_db, store, sql) = fresh("db-version").await;
	assert!(store.db_version().await.unwrap() > 0);

	// A consumer module's row is not the answer, however high its version.
	sqlx::query("INSERT INTO schema_version (module, version, updated_at) VALUES ('myapp', 9, 0)")
		.execute(sql.write_pool())
		.await
		.unwrap();
	assert_eq!(store.db_version().await.unwrap(), store_adapter_sqlite::schema::VERSION);

	sqlx::query("DELETE FROM schema_version WHERE module = 'saas'")
		.execute(sql.write_pool())
		.await
		.unwrap();
	assert_eq!(store.db_version().await.unwrap(), 0, "an absent row is genuinely version 0");
}

/// This branch used a bare `?` on `terminate`, so a transient writer error left the row
/// `RUNNING` — invisible to `claim`, which selects only `PENDING` — with its `dedup_key`
/// still held, neither runnable nor reclaimable until the next process start. It goes through
/// `terminate_hard` now, which is what leaves the row `FAILED` here. The key it keeps is
/// deliberate — see `a_terminal_failure_keeps_the_dedup_key`.
#[tokio::test]
async fn unknown_kind_fails_without_retrying_and_keeps_its_dedup_key() {
	let (_db, store, sql) = fresh("job-unknown-kind").await;
	let id = enqueue(&store, "nobody", "{}", Some("k"), Timestamp(0)).await.unwrap().unwrap();
	assert!(Runner::new(Arc::clone(&store)).tick(Timestamp(0)).await.unwrap());
	// `Error::Unsupported` is `Retry::Never`: terminal on attempt one, no backoff.
	let (status, attempts, _) = row(&sql, id).await;
	assert_eq!((status.as_str(), attempts), ("FAILED", 1));

	let key: Option<String> = sqlx::query_scalar("SELECT dedup_key FROM jobs WHERE id = ?")
		.bind(id)
		.fetch_one(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(key, Some("k".to_owned()));
	assert!(
		enqueue(&store, "nobody", "{}", Some("k"), Timestamp(0))
			.await
			.unwrap()
			.is_none()
	);
}

/// The chain has to survive its own ticks: each occurrence is what schedules the next,
/// so one lost reschedule stops `FETCH_RATES` (and every other cron kind) for good.
#[tokio::test]
async fn a_periodic_chain_survives_two_ticks() {
	let (_db, store, sql) = fresh("job-two-ticks").await;
	enqueue(&store, "sweep", "{}", Some("sweep:0"), Timestamp(0)).await.unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	r.register_periodic("sweep", 60, |_| async { Ok(()) });
	assert!(r.tick(Timestamp(0)).await.unwrap());
	assert!(r.tick(Timestamp(60)).await.unwrap());

	let third: i64 =
		sqlx::query_scalar("SELECT run_at FROM jobs WHERE kind = 'sweep' AND status = 'PENDING'")
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!(third, 120);
}

/// Mints a `periodic:`-prefixed dedup key, which `job::enqueue` reserves for the runner's own
/// reschedule. Tests that simulate a collision on one have to go straight to the store.
async fn reserved(store: &Arc<dyn CoreStore>, kind: &str, key: &str, run_at: Timestamp) -> i64 {
	store.job_enqueue(kind, "{}", Some(key), run_at).await.unwrap().unwrap()
}

/// A periodic chain that spends every attempt must still leave a successor. Nothing but
/// `seed_periodic` re-seeds one, and that runs from `AppBuilder::on_init` only — so eight
/// consecutive failures killed `NAV_SWEEP` for the life of the process.
#[tokio::test]
async fn a_periodic_chain_that_terminates_still_leaves_a_successor() {
	let (_db, store, sql) = fresh("job-terminated-chain").await;
	enqueue(&store, "sweep", "{}", Some("sweep:0"), Timestamp(0)).await.unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	// `Retry::Backoff`: the point of the test is a chain that spends every attempt.
	r.register_periodic("sweep", 60, |_| async { Err(Error::Unavailable("nope".to_owned())) });
	// Step past the backoff cap each time so the row is always claimable again.
	for i in 0..DEFAULT_MAX_ATTEMPTS {
		assert!(r.tick(Timestamp((1 << 40) + i * 3600)).await.unwrap());
	}

	let failed: i64 =
		sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep' AND status='FAILED'")
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!(failed, 1, "the chain did terminate");
	let pending: i64 =
		sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep' AND status = 'PENDING'")
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!(pending, 1, "a terminated periodic chain must re-seed itself");
}

/// The backoff runs from the end of the handler, not the tick's start. A 30 s NAV timeout
/// otherwise put the first four retries in the past, and `claim` fired them back-to-back.
#[tokio::test]
async fn the_backoff_starts_from_the_end_of_the_handler() {
	let (_db, store, sql) = fresh("job-backoff-origin").await;
	let id = enqueue(&store, "boom", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	// `job_fail` only matches a `RUNNING` row, so claim it first.
	store.job_claim(Timestamp(0)).await.unwrap();
	let job = Job { id, kind: "boom".into(), payload: "{}".into(), attempts: 1 };
	Runner::new(Arc::clone(&store))
		.fail(&job, Timestamp(100), 30, DEFAULT_MAX_ATTEMPTS, 3600, "timeout", None, None)
		.await
		.unwrap();
	// The same id-keyed jitter `Runner::fail` adds, so the assertion stays on the origin.
	let base = backoff_secs(1, 3600);
	assert_eq!(row(&sql, id).await.2, 100 + 30 + base + id.rem_euclid(base / 4 + 1));
}

/// `Error::RateLimit(n)` carried NAV's own `Retry-After` and the runner ignored the number, so
/// a throttled job retried on `2^attempts` and earned another 429. The upstream's delay wins
/// when it is longer than ours; the jitter and the `elapsed` offset are unchanged.
#[tokio::test]
async fn a_rate_limited_job_waits_the_delay_the_upstream_named() {
	let (_db, store, sql) = fresh("job-rate-limited").await;
	let id = enqueue(&store, "boom", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	store.job_claim(Timestamp(0)).await.unwrap();
	let job = Job { id, kind: "boom".into(), payload: "{}".into(), attempts: 1 };
	Runner::new(Arc::clone(&store))
		.fail(&job, Timestamp(100), 0, DEFAULT_MAX_ATTEMPTS, 3600, "throttled", None, Some(300))
		.await
		.unwrap();
	let jitter = id.rem_euclid(300 / 4 + 1);
	assert_eq!(row(&sql, id).await.2, 100 + 300 + jitter);
}

/// A poll that NAV has not answered yet used to return `Err(Unavailable)`: it wrote
/// `last_error`, so `job_retrying_kinds` counted it and `A-JOB-STALE` fired on a job doing
/// exactly what it was asked to. `Next::Again` is a success carrying a schedule instead.
#[tokio::test]
async fn a_deferred_job_is_not_a_retrying_one() {
	let (_db, store, sql) = fresh("job-defer").await;
	let id = enqueue(&store, "later", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	// A failure first, so the deferral has a `last_error` to clear.
	let mut r = Runner::new(Arc::clone(&store));
	r.register("later", |_| async { Err(Error::Unavailable("not yet".into())) });
	assert!(r.tick(Timestamp(0)).await.unwrap());
	assert_eq!(store.job_retrying_kinds().await.unwrap(), vec!["later".to_owned()]);

	let mut r = Runner::new(Arc::clone(&store));
	r.register_next("later", |_| async { Ok(Next::Again { at: Timestamp(9_000) }) });
	assert!(r.tick(Timestamp(1_000)).await.unwrap());

	let (status, attempts, run_at) = row(&sql, id).await;
	assert_eq!(status, "PENDING");
	assert_eq!(run_at, 9_000, "the handler's own schedule, not the runner's backoff");
	assert_eq!(attempts, 2, "a deferral is still an execution, so the ceiling still bounds it");
	let last_error: Option<String> = sqlx::query_scalar("SELECT last_error FROM jobs WHERE id = ?")
		.bind(id)
		.fetch_one(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(last_error, None, "nothing is wrong with this row");
	assert!(store.job_retrying_kinds().await.unwrap().is_empty(), "so it is not retrying");
}

/// `Next::Again` mints no periodic successor, exactly like `Failure::Live`: the row it
/// deferred still carries the chain, and a second one would double the schedule permanently.
#[tokio::test]
async fn a_deferred_periodic_job_mints_no_successor() {
	let (_db, store, sql) = fresh("job-defer-periodic").await;
	store.job_seed_periodic("beat", Timestamp(0)).await.unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	r.register_next("beat", |_| async { Ok(Next::Again { at: Timestamp(9_000) }) });
	assert!(r.tick(Timestamp(0)).await.unwrap());

	let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'beat'")
		.fetch_one(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(rows, 1, "the deferred row is the chain");
}

/// An operator's `job_cancel` landing mid-handler is terminal, and a deferral must not undo
/// it: `job_defer` is `RUNNING`-guarded for the reason `job_complete` is.
#[tokio::test]
async fn a_cancelled_row_is_not_revived_by_a_deferral() {
	let (_db, store, sql) = fresh("job-defer-cancelled").await;
	let id = enqueue(&store, "later", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	store.job_claim(Timestamp(0)).await.unwrap();
	store
		.job_cancel("later", "{}", Timestamp(1), "an operator stopped it", Some("E-X"))
		.await
		.unwrap();

	assert_eq!(store.job_defer(id, Timestamp(9_000)).await.unwrap(), 0);
	assert_eq!(row(&sql, id).await.0, "FAILED", "it must stay cancelled");
}

/// `DONE` rows keep their `dedup_key`, so a replayed same-second key makes the reschedule a
/// silent no-op. The tick still succeeds; the collision is the signal.
///
/// Which collision it is decides the chain. A *live* row on the key already carries it. A
/// *spent* one carries nothing, so silently dropping the reschedule there stopped the cron for
/// good — the one-second step is what walks past it. Spent rows on *both* candidate keys is
/// genuinely dead until the next process start, and the second `enqueue`'s return used to be
/// discarded, so it died without even the `error!` its `has_live` sibling logs.
#[tokio::test]
async fn a_taken_dedup_key_makes_the_reschedule_a_no_op() {
	// (label, keys already taken and whether each is spent, run_at of every PENDING row after)
	for (n, (label, taken, want)) in [
		("a live row holds the key", &[("periodic:sweep:60", false)][..], &[999_i64][..]),
		("a spent row holds the key", &[("periodic:sweep:60", true)], &[61]),
		(
			"spent rows hold both candidate keys",
			&[("periodic:sweep:60", true), ("periodic:sweep:61", true)],
			&[],
		),
	]
	.into_iter()
	.enumerate()
	{
		let (_db, store, sql) = fresh(&format!("job-key-collision-{n}")).await;
		enqueue(&store, "sweep", "{}", Some("sweep:0"), Timestamp(0)).await.unwrap();
		for (key, spent) in taken {
			let held = reserved(&store, "sweep", key, Timestamp(999)).await;
			if *spent {
				sqlx::query("UPDATE jobs SET status = 'DONE' WHERE id = ?")
					.bind(held)
					.execute(sql.write_pool())
					.await
					.unwrap();
			}
		}

		let mut r = Runner::new(Arc::clone(&store));
		r.register_periodic("sweep", 60, |_| async { Ok(()) });
		// The tick still succeeds — the handler ran, and its success is not the reschedule's
		// to undo.
		assert!(r.tick(Timestamp(0)).await.unwrap(), "{label}");

		let pending: Vec<i64> = sqlx::query_scalar(
			"SELECT run_at FROM jobs WHERE kind = 'sweep' AND status = 'PENDING' ORDER BY run_at",
		)
		.fetch_all(sql.read_pool())
		.await
		.unwrap();
		assert_eq!(pending, want, "{label}");
		assert!(
			store
				.job_enqueue("sweep", "{}", Some("periodic:sweep:60"), Timestamp(60))
				.await
				.unwrap()
				.is_none(),
			"{label}: the key stays taken, so no second occurrence can be minted",
		);
	}
}

/// A fixed dedup key on the seed made it a one-shot: the key outlives `DONE`, so every
/// later boot collided and a chain that had stopped stayed stopped. `has_live` is the
/// guard instead — this is the shape `saas_nav::job::seed` uses.
#[tokio::test]
async fn a_has_live_guarded_seed_revives_a_dead_chain() {
	let (_db, store, sql) = fresh("job-guarded-seed").await;

	let seed = |store: Arc<dyn CoreStore>| async move {
		if !has_live(&store, "sweep", None).await.unwrap() {
			enqueue(&store, "sweep", "{}", None, Timestamp(0)).await.unwrap();
		}
	};

	seed(Arc::clone(&store)).await;
	// A live chain is not re-seeded.
	seed(Arc::clone(&store)).await;
	let live: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'sweep'")
		.fetch_one(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(live, 1);

	// The chain dies — the last occurrence is `DONE` and nothing succeeded it.
	sqlx::query("UPDATE jobs SET status = 'DONE' WHERE kind = 'sweep'")
		.execute(sql.write_pool())
		.await
		.unwrap();
	seed(Arc::clone(&store)).await;
	let revived: i64 =
		sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'sweep' AND status = 'PENDING'")
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!(revived, 1, "the next boot re-enqueues rather than colliding forever");
}

/// A panic in one handler used to end `run()` permanently: every later job stopped and
/// the claimed row stayed `RUNNING`, still holding its dedup key.
#[tokio::test]
async fn a_panicking_handler_fails_only_its_own_job() {
	let (_db, store, sql) = fresh("job-panicking-handler").await;
	let boom = enqueue(&store, "boom", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	// Due only after the loop below, so the loop's ticks can only pick up `boom`.
	let fine = enqueue(&store, "fine", "{}", None, Timestamp(1 << 41)).await.unwrap().unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	r.register("boom", |_| async { panic!("handler exploded") });
	r.register("fine", |_| async { Ok(()) });

	// Spend every attempt: a handler that panics every time terminates like any other
	// repeatedly failing job instead of looping.
	for i in 0..DEFAULT_MAX_ATTEMPTS {
		assert!(r.tick(Timestamp((1 << 40) + i * 3600)).await.unwrap());
	}
	let (status, attempts, _) = row(&sql, boom).await;
	assert_eq!((status.as_str(), attempts), ("FAILED", DEFAULT_MAX_ATTEMPTS));

	// The runner is still working: the following job runs.
	assert!(r.tick(Timestamp(1 << 41)).await.unwrap());
	assert_eq!(row(&sql, fine).await.0, "DONE");
}

/// A crash between `claim` and `complete` leaves the row `RUNNING`, and `claim` only ever
/// looks at `PENDING`. Without the reclaim the job is stranded for good.
#[tokio::test]
async fn a_running_row_is_reclaimed_at_startup() {
	let (_db, store, _sql) = fresh("job-reclaim").await;
	let id = enqueue(&store, "boom", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	let r = Runner::new(Arc::clone(&store));

	assert_eq!(r.claim(Timestamp(0)).await.unwrap().map(|j| j.id), Some(id));
	assert!(r.claim(Timestamp(0)).await.unwrap().is_none(), "a RUNNING row is not claimable");

	assert_eq!(r.reclaim().await.unwrap(), 1);
	assert_eq!(r.claim(Timestamp(0)).await.unwrap().map(|j| j.id), Some(id));
}

/// Nothing could stop the runner — `run` was an unconditional loop and `AppBuilder::run` served
/// with no graceful shutdown — so every rolling deploy was a crash: the handler in flight died
/// and re-ran from the top at the next boot's `reclaim`.
#[tokio::test]
async fn a_shutdown_lets_the_job_in_hand_finish_and_claims_no_more() {
	let (_db, store, sql) = fresh("job-shutdown").await;
	let running = enqueue(&store, "slow", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	let queued = enqueue(&store, "slow", "{}", None, Timestamp(0)).await.unwrap().unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	r.register("slow", |_| async {
		tokio::time::sleep(std::time::Duration::from_millis(300)).await;
		Ok(())
	});
	let stop = r.stopper();
	let worker = tokio::spawn(Arc::new(r).run(0));

	// Ordered on the row actually being claimed, not on a sleep: signalling before the worker
	// has started would pass without exercising anything.
	while row(&sql, running).await.0 != "RUNNING" {
		tokio::time::sleep(std::time::Duration::from_millis(5)).await;
	}
	stop.send_replace(true);
	worker.await.unwrap();

	assert_eq!(row(&sql, running).await.0, "DONE", "the job in hand is finished, not abandoned");
	assert_eq!(row(&sql, queued).await.0, "PENDING", "a stopping worker claims no more");
}

/// The key is a permanent idempotency record, so a terminal failure keeps it: "an invoice is
/// never reported to NAV twice" has to hold across termination too, or a replayed enqueue
/// files it a second time. Re-driving a terminal job is an explicit reset of that row.
#[tokio::test]
async fn a_terminal_failure_keeps_the_dedup_key() {
	let (_db, store, sql) = fresh("job-terminal-release").await;
	let key = "nav:invoice:1";
	let id = enqueue(&store, "NAV_REPORT", "{}", Some(key), Timestamp(0))
		.await
		.unwrap()
		.unwrap();
	assert!(
		enqueue(&store, "NAV_REPORT", "{}", Some(key), Timestamp(0))
			.await
			.unwrap()
			.is_none(),
		"the key is held while the job can still run"
	);

	// Claimed first: `terminate` is `RUNNING`-guarded, exactly as `job_complete` and `job_fail`
	// are, so that an operator's `job_cancel` landing mid-handler is not overwritten. Every
	// real caller (`Runner::fail`, `Runner::terminate_hard`) holds a claimed row.
	assert_eq!(store.job_claim(Timestamp(0)).await.unwrap().map(|j| j.id), Some(id));
	Runner::new(Arc::clone(&store))
		.terminate(id, Timestamp(0), "nav unreachable", None)
		.await
		.unwrap();

	assert_eq!(status_and_key(&sql, id).await, ("FAILED".to_owned(), Some(key.to_owned())));
	assert!(
		enqueue(&store, "NAV_REPORT", "{}", Some(key), Timestamp(0))
			.await
			.unwrap()
			.is_none(),
		"a terminal row still holds the key, so the filing cannot be enqueued twice"
	);
}

/// The at-most-once side of the queue: once a handler has succeeded, the row must be
/// `DONE` even if the periodic reschedule beside it fails — otherwise it stays `RUNNING`,
/// `claim` never sees it again, and the next process start's `reclaim` runs the handler a
/// **second** time. For `SEND_EMAIL` that is a second delivery.
#[tokio::test]
async fn a_succeeded_handler_never_runs_twice_across_a_reclaim() {
	use std::sync::atomic::{AtomicUsize, Ordering};

	let (_db, store, sql) = fresh("job-at-most-once").await;
	let id = enqueue(&store, "sweep", "{}", Some("sweep:0"), Timestamp(0))
		.await
		.unwrap()
		.unwrap();

	// The key the reschedule will want, already held by a live row, so the reschedule
	// takes its collision branch while the handler has already succeeded. (A period of 0
	// no longer collides with this row's own key: `reschedule` floors the step at one
	// second past the handler, or a periodic job would re-claim itself immediately.)
	enqueue(&store, "sweep", "{}", Some("sweep:1"), Timestamp(999))
		.await
		.unwrap()
		.unwrap();

	let runs = Arc::new(AtomicUsize::new(0));
	let mut r = Runner::new(Arc::clone(&store));
	let counter = Arc::clone(&runs);
	r.register_periodic("sweep", 0, move |_| {
		let counter = Arc::clone(&counter);
		async move {
			counter.fetch_add(1, Ordering::SeqCst);
			Ok(())
		}
	});

	assert!(r.tick(Timestamp(0)).await.unwrap());
	assert_eq!(row(&sql, id).await.0, "DONE", "a succeeded handler must leave DONE");

	// A restart: nothing to reclaim, so nothing re-runs.
	assert_eq!(r.reclaim().await.unwrap(), 0);
	assert_eq!(runs.load(Ordering::SeqCst), 1, "the handler ran exactly once");
}

/// `next` was `now + period` with `now` captured before the handler ran, so a handler
/// that outran its period scheduled its successor into the past — `claim` took it
/// immediately and the "hourly" chain became a continuous loop. `NAV_SWEEP` during a NAV
/// outage is exactly that: a self-inflicted hammering of a government API.
#[tokio::test]
async fn a_handler_that_outruns_its_period_does_not_reschedule_into_the_past() {
	let (_db, store, sql) = fresh("job-outruns-period").await;
	enqueue(&store, "sweep", "{}", Some("sweep:0"), Timestamp(0)).await.unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	// One second of period against a handler that takes longer than that.
	r.register_periodic("sweep", 1, |_| async {
		tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
		Ok(())
	});
	assert!(r.tick(Timestamp(0)).await.unwrap());

	let next: i64 =
		sqlx::query_scalar("SELECT run_at FROM jobs WHERE kind = 'sweep' AND status = 'PENDING'")
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!(next, 2, "the successor must fall strictly after the handler finished");
	// And it is not claimable at the moment the handler ended, which is what spun.
	assert!(!r.tick(Timestamp(1)).await.unwrap());
}

// ---------------------------------------------------------------- audit trail

async fn one_row(sql: &SqliteStore) -> (Option<i64>, Option<i64>, String, Option<String>, String) {
	sqlx::query_as("SELECT account_id, org_id, entity, entity_id, action FROM audit_logs")
		.fetch_one(sql.read_pool())
		.await
		.unwrap()
}

async fn detail_of(sql: &SqliteStore) -> Option<String> {
	sqlx::query_scalar("SELECT detail FROM audit_logs")
		.fetch_one(sql.read_pool())
		.await
		.unwrap()
}

#[tokio::test]
async fn a_row_lands_with_the_actor_and_the_target() {
	let (_db, store, sql) = fresh("audit-actor").await;
	let ctx = Ctx {
		actor: Actor::User { account_id: 42 },
		org_id: Some(7),
		ip: Some("2001:db8::1".parse().unwrap()),
		auth_at: None,
		request_id: "req-1".to_owned(),
		on_behalf_of: None,
	};
	audit::log(&store, &ctx, "invoice", Some("inv_x"), "ISSUE", None).await;

	assert_eq!(
		one_row(&sql).await,
		(Some(42), Some(7), "invoice".to_owned(), Some("inv_x".to_owned()), "ISSUE".to_owned())
	);
	// The audit trail keeps the *full* address; only `ratelimit::bucket_key` masks it.
	let ip: Option<String> = sqlx::query_scalar("SELECT ip FROM audit_logs")
		.fetch_one(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(ip.as_deref(), Some("2001:db8::1"));
}

/// `Ctx::system` builds from scratch, so escalating with it dropped the account, the IP and the
/// request id at once and every row a checkout wrote was unattributable.
#[tokio::test]
async fn an_escalated_call_still_names_the_account_that_made_it() {
	let (_db, store, sql) = fresh("audit-escalated").await;
	let ctx = Ctx {
		actor: Actor::User { account_id: 42 },
		org_id: Some(7),
		ip: Some("2001:db8::1".parse().unwrap()),
		auth_at: None,
		request_id: "req-1".to_owned(),
		on_behalf_of: None,
	}
	.as_system("checkout");
	audit::log(&store, &ctx, "invoice", Some("inv_x"), "ISSUE", None).await;

	assert_eq!(one_row(&sql).await.0, Some(42), "the acting human, not NULL");
	// `detail.source` still says *through what*, so the two are distinguishable.
	assert_eq!(detail_of(&sql).await.as_deref(), Some(r#"{"source":"checkout"}"#));
	let (ip, request_id): (Option<String>, Option<String>) =
		sqlx::query_as("SELECT ip, request_id FROM audit_logs")
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!((ip.as_deref(), request_id.as_deref()), (Some("2001:db8::1"), Some("req-1")));
}

#[tokio::test]
async fn a_second_escalation_keeps_the_account_the_first_one_recorded() {
	let (_db, store, sql) = fresh("audit-escalated-twice").await;
	let ctx = Ctx {
		actor: Actor::User { account_id: 42 },
		org_id: Some(7),
		ip: None,
		auth_at: None,
		request_id: "req-1".to_owned(),
		on_behalf_of: None,
	}
	.as_system("checkout")
	.as_system("payment");
	audit::log(&store, &ctx, "invoice", Some("inv_x"), "ISSUE", None).await;

	assert_eq!(one_row(&sql).await.0, Some(42), "the acting human, not NULL");
	assert_eq!(detail_of(&sql).await.as_deref(), Some(r#"{"source":"payment"}"#));
}

/// The nine unauthenticated handlers used to build `Ctx::system`, and `System` is the
/// *most* privileged actor there is — `require_operator` grants it unconditionally and
/// `require_stepup` exempts it. `Actor::Public` is what they carry now, and both gates must
/// refuse it.
#[tokio::test]
async fn both_authorization_gates_refuse_an_anonymous_caller() {
	use saas_core::auth_mw::{require_operator, require_stepup};

	let db = TmpDb::new("actor-public");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(sql) as Arc<dyn CoreStore>)
		.build()
		.await
		.unwrap();

	let public = Ctx::public("auth.login");
	assert_eq!(require_operator(&app, &public).await.unwrap_err().parts().1, "E-AUTH-FORBIDDEN");
	assert_eq!(
		require_stepup(&app, &public).await.unwrap_err().parts().1,
		"E-AUTH-STEPUP-IMPOSSIBLE"
	);
	assert_eq!(public.actor.account_id(), None, "an anonymous caller names no account");

	// The application's own code is still trusted by both, which is what `System` is for.
	let system = Ctx::system("nav_sweep");
	assert!(require_operator(&app, &system).await.is_ok());
	assert!(require_stepup(&app, &system).await.is_ok());
}

/// `Actor::System` has no `account_id`, so `source` is the only thing that says which of
/// the application's own code paths acted.
#[tokio::test]
async fn a_system_actor_records_its_source_whatever_the_detail_shape_is() {
	let ctx = Ctx::system("nav_sweep");

	let (_db, store, sql) = fresh("audit-system-none").await;
	audit::log(&store, &ctx, "invoice", None, "REPORT", None).await;
	assert_eq!(detail_of(&sql).await.as_deref(), Some(r#"{"source":"nav_sweep"}"#));
	assert_eq!(one_row(&sql).await.0, None, "System has no account_id");

	// An object detail is merged into.
	let (_db, store, sql) = fresh("audit-system-object").await;
	audit::log(&store, &ctx, "invoice", None, "REPORT", Some(serde_json::json!({ "n": 3 }))).await;
	assert_eq!(detail_of(&sql).await.as_deref(), Some(r#"{"n":3,"source":"nav_sweep"}"#));

	// A non-object detail has nowhere to take the key, and used to drop the source
	// silently. It is nested rather than lost.
	let (_db, store, sql) = fresh("audit-system-array").await;
	audit::log(&store, &ctx, "invoice", None, "REPORT", Some(serde_json::json!(["a", "b"]))).await;
	assert_eq!(
		detail_of(&sql).await.as_deref(),
		Some(r#"{"detail":["a","b"],"source":"nav_sweep"}"#)
	);
}

/// Best-effort by design: auditing must never break an operation that already succeeded.
#[tokio::test]
async fn a_failed_insert_is_swallowed() {
	let (_db, store, sql) = fresh("audit-swallowed").await;
	sqlx::query("DROP TABLE audit_logs").execute(sql.write_pool()).await.unwrap();
	// Returns `()`; the point is that it does not panic or propagate.
	audit::log(&store, &Ctx::system("test"), "invoice", None, "ISSUE", None).await;
}

/// The other half of the pair: `ISSUE`, `STORNO` and NAV's `SUBMIT` rows are Számv. tv.
/// evidence, not diagnostics, and the write happens past the caller's commit — so a lost one
/// is unrecoverable and the caller, not a log line nobody reads, decides what it means.
#[tokio::test]
async fn a_statutory_audit_row_that_cannot_be_written_is_an_error_not_a_log_line() {
	let (_db, store, sql) = fresh("audit-statutory").await;
	sqlx::query("DROP TABLE audit_logs").execute(sql.write_pool()).await.unwrap();
	let ctx = Ctx::system("test");

	assert!(
		audit::try_log(&store, &ctx, "invoice", Some("inv_x"), "ISSUE", None)
			.await
			.is_err()
	);
	// And the best-effort sibling over the same store still swallows it.
	audit::log(&store, &ctx, "invoice", Some("inv_x"), "ISSUE", None).await;
}

// ---------------------------------------------------------------- secret store

/// `pow::hmac_key` minted through `set`, which overwrites. Two workers racing on an
/// unseeded `auth.jwt_key` both minted and both stored — and the loser had already signed
/// real login responses with a key that was now gone, killing every one of those sessions
/// on its next request. Concurrent callers must converge on one value.
#[tokio::test]
async fn get_or_create_converges_under_concurrency() {
	let (_db, store, _sql) = fresh("secrets-race").await;
	let secrets = Arc::new(secret_store(store, [7; 32]));

	let racers: Vec<_> = (0..8)
		.map(|_| {
			let secrets = Arc::clone(&secrets);
			tokio::spawn(async move { secrets.get_or_create("auth.jwt_key", 32).await.unwrap() })
		})
		.collect();

	let mut keys = Vec::new();
	for r in racers {
		keys.push(r.await.unwrap());
	}
	assert_eq!(keys[0].len(), 32);
	assert!(keys.iter().all(|k| *k == keys[0]), "the losers minted a key of their own");
	// And a later read still returns it — nothing overwrote what was signed with.
	assert_eq!(secrets.get("auth.jwt_key").await.unwrap().as_ref(), Some(&keys[0]));
}

/// An already-set secret is returned untouched: this is a read first and a mint second.
#[tokio::test]
async fn get_or_create_never_replaces_an_existing_secret() {
	let (_db, store, _sql) = fresh("secrets-no-replace").await;
	let secrets = secret_store(store, [7; 32]);
	secrets.set("auth.jwt_key", b"an operator's own key", None).await.unwrap();
	assert_eq!(secrets.get_or_create("auth.jwt_key", 32).await.unwrap(), b"an operator's own key");
}

/// The module's headline property: `Hkdf::expand(master_key, name)` binds the key
/// material to the secret's *name*, so the name is authenticated without a separate AAD. A
/// refactor that derived from the master key alone would leave every other test green while
/// making `auth.jwt_key` and `nav.signing_key` interchangeable.
#[tokio::test]
async fn a_row_moved_to_another_name_fails_to_authenticate() {
	let (_db, store, sql) = fresh("secrets-moved-name").await;
	secret_store(Arc::clone(&store), [7; 32])
		.set("nav.signing_key", b"the plaintext", None)
		.await
		.unwrap();
	sqlx::query("UPDATE secrets SET key = 'auth.jwt_key' WHERE key = 'nav.signing_key'")
		.execute(sql.write_pool())
		.await
		.unwrap();

	// A fresh store, so the plaintext cache cannot answer from the old name.
	let secrets = secret_store(store, [7; 32]);
	assert!(
		matches!(secrets.get("auth.jwt_key").await, Err(Error::Internal(_))),
		"a row under the wrong name decrypted"
	);
}

/// The wrong `MASTER_KEY` is an authentication failure, not garbage plaintext — which is
/// what tells an operator who mixed up two deployments' env files apart from one whose
/// database is corrupt.
#[tokio::test]
async fn a_wrong_master_key_fails_to_authenticate() {
	let (_db, store, _sql) = fresh("secrets-wrong-master").await;
	secret_store(Arc::clone(&store), [7; 32])
		.set("auth.jwt_key", b"the plaintext", None)
		.await
		.unwrap();

	let wrong = secret_store(store, [8; 32]);
	assert!(matches!(wrong.get("auth.jwt_key").await, Err(Error::Internal(_))));
}

/// Reads are cached process-locally, so a rotation that did not invalidate would keep
/// serving the old key until the next restart — and `auth_mw` reads the JWT signing key
/// on every authenticated request, so the rotation would appear to have done nothing.
#[tokio::test]
async fn a_rotation_is_visible_to_the_next_read() {
	let (_db, store, _sql) = fresh("secrets-rotation").await;
	let secrets = secret_store(store, [7; 32]);
	secrets.set("auth.jwt_key", b"first", None).await.unwrap();
	assert_eq!(secrets.get("auth.jwt_key").await.unwrap().as_deref(), Some(&b"first"[..]));
	secrets.set("auth.jwt_key", b"second", None).await.unwrap();
	assert_eq!(secrets.get("auth.jwt_key").await.unwrap().as_deref(), Some(&b"second"[..]));
}

/// A secret resolves from `the matching variable` exactly as a setting does, so a value cannot be read
/// before a seeding step has written it — the ordering bug that killed a first boot with
/// `email.smtp.username` configured and the `email.smtp.password` row still empty.
///
/// Re-runs itself in a child process with the variables set, for the reason
/// [`an_env_override_is_read_when_settings_is_built`] gives.
#[tokio::test]
async fn a_secret_resolves_from_the_environment() {
	const NAME: &str = "a_secret_resolves_from_the_environment";
	// Blank is absent: `.env.example` ships the credentials empty, and a copied blank must not
	// shadow a real row with an empty secret.
	if reexec_with(NAME, &[("EMAIL_SMTP_PASSWORD", "from-env"), ("PAYMENT_BARION_POS_KEY", "  ")]) {
		return;
	}

	let (_db, store, _sql) = fresh("secrets-env").await;
	let secrets = secret_store(store, [7; 32]);

	assert_eq!(
		secrets.get("email.smtp.password").await.unwrap().as_deref(),
		Some(&b"from-env"[..])
	);
	let status = secrets.status("email.smtp.password").await.unwrap();
	assert!(status.set && status.updated_at.is_none(), "configured, but with no row to date");

	secrets.set("payment.barion.pos_key", b"from-row", None).await.unwrap();
	assert_eq!(
		secrets.get("payment.barion.pos_key").await.unwrap().as_deref(),
		Some(&b"from-row"[..])
	);

	secrets.set("nav.sign_key", b"no variable", None).await.unwrap();
	assert_eq!(secrets.get("nav.sign_key").await.unwrap().as_deref(), Some(&b"no variable"[..]));
}

/// The environment is the source of truth and rotation is a redeploy, so a write is refused
/// rather than landing a row every later read would shadow.
///
/// Re-runs itself in a child process with the variable set, for the reason
/// [`an_env_override_is_read_when_settings_is_built`] gives.
#[tokio::test]
async fn the_environment_beats_a_stored_secret() {
	const NAME: &str = "the_environment_beats_a_stored_secret";
	if reexec(NAME, "EMAIL_SMTP_PASSWORD", "from-env") {
		return;
	}

	let (_db, store, _sql) = fresh("secrets-env-wins").await;
	let secrets = secret_store(store, [7; 32]);

	let err = secrets.set("email.smtp.password", b"from-row", None).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-CONFLICT");
	assert_eq!(
		secrets.get("email.smtp.password").await.unwrap().as_deref(),
		Some(&b"from-env"[..])
	);
}

/// Nothing is persisted: `get_or_create` short-circuits at the `get`, so every replica of a
/// multi-replica deployment shares the one `auth.jwt_key` from the environment instead of each
/// minting its own and invalidating the others' sessions.
///
/// Re-runs itself in a child process with the variable set, for the reason
/// [`an_env_override_is_read_when_settings_is_built`] gives.
#[tokio::test]
async fn an_env_secret_is_never_minted() {
	const NAME: &str = "an_env_secret_is_never_minted";
	const KEY: &str = "0123456789abcdef0123456789abcdef";
	if reexec(NAME, "AUTH_JWT_KEY", KEY) {
		return;
	}

	let (_db, store, _sql) = fresh("secrets-env-no-mint").await;
	let secrets = secret_store(Arc::clone(&store), [7; 32]);

	assert_eq!(secrets.get_or_create("auth.jwt_key", 32).await.unwrap(), KEY.as_bytes());
	assert!(store.secret_updated_at("auth.jwt_key").await.unwrap().is_none(), "no row was written");
}

/// AES-GCM is catastrophically broken by a repeated (key, nonce) pair, and the key is
/// fixed per secret name — so every write has to draw a fresh nonce.
#[tokio::test]
async fn every_write_draws_a_fresh_nonce() {
	let (_db, store, sql) = fresh("secrets-nonce").await;
	let secrets = secret_store(store, [7; 32]);
	let nonce = || async {
		sqlx::query_scalar::<_, Vec<u8>>("SELECT nonce FROM secrets WHERE key = 'k'")
			.fetch_one(sql.read_pool())
			.await
			.unwrap()
	};

	secrets.set("k", b"same value twice", None).await.unwrap();
	let first = nonce().await;
	secrets.set("k", b"same value twice", None).await.unwrap();
	let second = nonce().await;

	assert_eq!(first.len(), 12);
	assert_ne!(first, second, "the nonce was reused across two writes of the same key");
}

// ---------------------------------------------------------------- rate limiting

/// Every route without a scope of its own gets a blanket budget; before `default_mw` existed,
/// `ratelimit.default` was read by nothing at all and `POST /api/auth/refresh` was unthrottled.
#[tokio::test]
async fn the_blanket_layer_denies_past_the_default_budget() {
	use axum::body::Body;
	use axum::http::Request;
	use axum::middleware::Next;
	use http_body_util::BodyExt;
	use tower::ServiceExt;

	// The cleared the environment environment is the point: this asserts the *registry* default, which
	// an operator's `RATELIMIT_DEFAULT` would otherwise answer instead.
	const NAME: &str = "the_blanket_layer_denies_past_the_default_budget";
	if reexec_clean(NAME) {
		return;
	}

	let db = TmpDb::new("ratelimit-blanket");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(sql) as Arc<dyn CoreStore>)
		.build()
		.await
		.unwrap();

	let router = axum::Router::new()
		.route("/x", axum::routing::get(|| async { "ok" }))
		.layer(axum::middleware::from_fn_with_state(app.clone(), default_mw))
		.layer(axum::middleware::from_fn(|mut req: Request<Body>, next: Next| async move {
			req.extensions_mut().insert(ClientIp("1.2.3.4".parse().unwrap()));
			next.run(req).await
		}))
		.with_state(app.clone());

	// `ratelimit.default` ships as 120/min/ip.
	for n in 1..=120 {
		let res = router
			.clone()
			.oneshot(Request::get("/x").body(Body::empty()).unwrap())
			.await
			.unwrap();
		assert_eq!(res.status(), 200, "request {n}");
	}
	let res = router.oneshot(Request::get("/x").body(Body::empty()).unwrap()).await.unwrap();
	assert_eq!(res.status(), 429);
	let bytes = res.into_body().collect().await.unwrap().to_bytes();
	let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
	assert_eq!(body["error"]["errCode"], "E-CORE-RATELIMIT");
}

/// `check` consulted `SCOPES` and `ratelimit.default` only, so the documented
/// `ratelimit.<scope>` family validated, wrote, invalidated the cache — and changed
/// nothing. The naive fix (`settings.text`) is worse than the bug: the family default
/// answers for every unset key, which would *loosen* every entry in `SCOPES`.
#[tokio::test]
async fn an_explicit_override_wins_and_an_unset_one_leaves_scopes_alone() {
	let (_db, store, _sql) = fresh("ratelimit-override").await;
	let settings = settings(store);
	let rl = RateLimiter::new();

	// Nothing set: `login.ip` keeps its 10/5min, not the family's 120/min.
	for _ in 0..10 {
		rl.check(&settings, "login.ip", "1.2.3.4").await.unwrap();
	}
	assert!(matches!(rl.check(&settings, "login.ip", "1.2.3.4").await, Err(Error::RateLimit(_))));

	// An operator tightening it is honoured.
	settings.set("ratelimit.login.ip", "1/h/ip", None).await.unwrap();
	rl.check(&settings, "login.ip", "9.9.9.9").await.unwrap();
	assert!(matches!(rl.check(&settings, "login.ip", "9.9.9.9").await, Err(Error::RateLimit(_))));

	// A malformed override is refused where it is written — it used to be stored happily
	// and then fail on every request in the scope, with nothing said at the point of
	// change. The bucket keeps the last good limit rather than being silently disabled.
	assert!(matches!(
		settings.set("ratelimit.login.ip", "nan/min/ip", None).await,
		Err(Error::Setting(_))
	));
	assert!(matches!(
		rl.check(&settings, "login.ip", "7.7.7.7")
			.await
			.and(rl.check(&settings, "login.ip", "7.7.7.7").await),
		Err(Error::RateLimit(_))
	));
}

/// Resolution is row, then environment, then the registry default — and a value that will not
/// parse falls through to the next, never to unlimited: failing open on a rate limit is worse
/// than the 400 this used to answer with the operator's bad value rendered into the body (400
/// is exempt from the 5xx mask).
///
/// Each of the three paths broke separately. A malformed row reached `parse_limit(&raw)?` on
/// every request in its scope. `default_mw` maps every route but the probes to the scope
/// `"default"`, which is deliberately not in `SCOPES`, so a malformed `RATELIMIT_DEFAULT`
/// 400'd *every* request in the process. And `check` read the row and then went straight to
/// `SCOPES`, so `RATELIMIT_REGISTER` had no effect and no error while `RATELIMIT_DEFAULT`
/// worked, because that one path goes through `Settings::text`.
///
/// Re-runs itself in a child process with both variables set: `std::env::set_var` is `unsafe` on
/// edition 2024 and the workspace forbids `unsafe`, and a process-wide variable would reach
/// every other test in this binary.
#[tokio::test]
async fn a_bad_or_overridden_limit_resolves_to_the_next_source() {
	const NAME: &str = "a_bad_or_overridden_limit_resolves_to_the_next_source";
	if reexec_with(
		NAME,
		&[("RATELIMIT_DEFAULT", "120/fortnight/ip"), ("RATELIMIT_REGISTER", "1/h/ip")],
	) {
		return;
	}

	// (label, scope, a row written straight to the store, the budget that must survive)
	for (label, scope, row, budget) in [
		// Straight to the store: `Settings::set`'s check hook is exactly what refuses this, so
		// a row like it can only predate the hook — which is the case being covered.
		(
			"a malformed row falls back to the scope's 10/5min/ip",
			"login.ip",
			Some("10/5week/ip"),
			10,
		),
		("a malformed default falls back to the registry's 120/min/ip", "default", None, 120),
		("an environment override tightens past the 3/h default", "register", None, 1),
	] {
		let (_db, store, _sql) = fresh(&format!("ratelimit-resolve-{scope}")).await;
		if let Some(raw) = row {
			store.setting_set(&format!("ratelimit.{scope}"), raw, None).await.unwrap();
		}
		let settings = settings(store);
		let rl = RateLimiter::new();
		for n in 0..budget {
			rl.check(&settings, scope, "1.2.3.4")
				.await
				.unwrap_or_else(|e| panic!("{label} at {n}: {e}"));
		}
		let over = rl.check(&settings, scope, "1.2.3.4").await;
		assert!(matches!(over, Err(Error::RateLimit(_))), "{label}: {over:?}");
	}

	// A row still wins over the environment, which is the documented order.
	let (_db, store, _sql) = fresh("ratelimit-row-beats-env").await;
	let settings = settings(store);
	let rl = RateLimiter::new();
	settings.set("ratelimit.register", "5/h/ip", None).await.unwrap();
	for _ in 0..5 {
		rl.check(&settings, "register", "9.9.9.9").await.unwrap();
	}
	assert!(matches!(rl.check(&settings, "register", "9.9.9.9").await, Err(Error::RateLimit(_))));
}

/// `DbExt::db` mapped every driver error to `Error::internal`, so a write that waited out
/// `BUSY_TIMEOUT` behind another process's lock arrived as an un-retryable `500` — the one
/// failure the caller most obviously *should* retry. `Error::Unavailable` and `Error::Timeout`
/// already existed for exactly this and are exempt from the 5xx body mask.
#[tokio::test]
async fn a_write_blocked_past_the_busy_timeout_is_retryable() {
	let (db, store, _sql) = fresh("busy-write").await;

	// A squatter holds the write lock for longer than the store's 5 s `busy_timeout`.
	let holder = sqlx::sqlite::SqlitePoolOptions::new()
		.max_connections(1)
		.connect(&format!("sqlite://{}", db.config().db_path))
		.await
		.unwrap();
	let mut held = holder.begin().await.unwrap();
	sqlx::query("INSERT INTO vars (name, value) VALUES ('squatter', '1')")
		.execute(&mut *held)
		.await
		.unwrap();

	let err = enqueue(&store, "BLOCKED", "{}", None, Timestamp::now())
		.await
		.expect_err("the write cannot land while the lock is held");
	held.rollback().await.unwrap();

	assert!(
		matches!(err, Error::Unavailable(_) | Error::Timeout(_)),
		"a busy database must be retryable, not a 500: {err:?}"
	);
	let (status, code) = err.parts();
	assert!(matches!(code, "E-CORE-UNAVAILABLE" | "E-CORE-TIMEOUT"), "{status} {code}");
	assert!(status.is_server_error() && status.as_u16() != 500);
}

/// `map_db`'s catch-all answered `Error::Unavailable` for every variant it did not recognise,
/// and `Unavailable` is `Retry::Backoff`. A `no such column` after a bad migration, a `CHECK`
/// violation and every decode failure answer identically however many times they are sent, so
/// on a kind whose `jobs.max_attempts` is 0 — `NAV_REPORT` is, deliberately — that was an
/// unbounded retry loop against the tax authority over a defect, and a 503 + `Retry-After`
/// over HTTP for a bug.
///
/// Only the defect half is asserted here: `PoolTimedOut` and `SQLITE_BUSY` must stay
/// `Backoff`, but neither is provokable in-process and `map_db` is `pub(crate)`, so there is
/// nothing this suite can address them through.
#[tokio::test]
async fn a_driver_defect_is_never_retried() {
	let (_db, store, sql) = fresh("driver-defect").await;

	// Exactly the bad-migration case: the column the insert names is no longer there.
	sqlx::query("ALTER TABLE jobs RENAME COLUMN payload TO payload_gone")
		.execute(sql.write_pool())
		.await
		.unwrap();

	let err = enqueue(&store, "boom", "{}", None, Timestamp(0))
		.await
		.expect_err("a missing column is a defect, not a busy database");
	assert!(matches!(err.retry(), Retry::Never), "{err:?}");
	assert_eq!(err.parts().1, "E-CORE-INTERNAL");
}

/// `job_cancel` flips `RUNNING` rows as well as `PENDING` ones, and the count it returns is
/// what `Nav::cancel_filing` reports to the operator who asked. The worker still holding that
/// job then finished and overwrote the cancellation — `job_complete` and `job_fail` carried
/// only `WHERE id = ?`.
#[tokio::test]
async fn a_job_cancelled_while_running_is_not_completed() {
	let (_db, store, sql) = fresh("job-cancel-complete").await;
	let id = enqueue(&store, "boom", "{}", None, Timestamp(0)).await.unwrap().unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	// The handler cancels its own job mid-flight, which is what an operator going through
	// `Nav::cancel_filing` does to a filing that is already running.
	let cancelling = Arc::clone(&store);
	r.register("boom", move |_job| {
		let store = Arc::clone(&cancelling);
		async move {
			let n = store.job_cancel("boom", "{}", Timestamp(1), "stopped", None).await?;
			assert_eq!(n, 1, "the RUNNING row is what gets cancelled");
			Ok(())
		}
	});
	assert!(r.tick(Timestamp(0)).await.unwrap());

	assert_eq!(row(&sql, id).await.0, "FAILED", "a successful handler must not resurrect it");
}

/// The other half: a cancelled job whose handler *fails* must not go back to `PENDING` for
/// another attempt. With `jobs.max_attempts.NAV_REPORT` at 0 — unbounded — that was an endless
/// retry against NAV on a filing a person explicitly stopped.
#[tokio::test]
async fn a_job_cancelled_while_running_is_not_rescheduled() {
	let (_db, store, sql) = fresh("job-cancel-fail").await;
	let id = enqueue(&store, "boom", "{}", None, Timestamp(0)).await.unwrap().unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	let cancelling = Arc::clone(&store);
	r.register("boom", move |_job| {
		let store = Arc::clone(&cancelling);
		async move {
			store.job_cancel("boom", "{}", Timestamp(1), "stopped", None).await?;
			// `Retry::Backoff`, so without the guard this reschedules.
			Err(Error::Unavailable("nope".to_owned()))
		}
	});
	assert!(r.tick(Timestamp(0)).await.unwrap());

	assert_eq!(row(&sql, id).await.0, "FAILED", "a cancelled job must not go back to PENDING");
}

/// The periodic form of the same rule, which neither of the two above covers: `tick`
/// rescheduled a periodic kind on both terminal paths regardless of what the row actually did,
/// so cancelling a `RUNNING` occurrence achieved nothing — the chain carried on and the next
/// occurrence ran the handler the operator had stopped.
#[tokio::test]
async fn a_cancelled_periodic_occurrence_mints_no_successor() {
	for (name, succeeds) in [("periodic-cancel-ok", true), ("periodic-cancel-err", false)] {
		let (_db, store, sql) = fresh(name).await;
		enqueue(&store, "sweep", "{}", Some("sweep:0"), Timestamp(0)).await.unwrap();

		let mut r = Runner::new(Arc::clone(&store));
		let cancelling = Arc::clone(&store);
		r.register_periodic("sweep", 60, move |_job| {
			let store = Arc::clone(&cancelling);
			async move {
				let n = store.job_cancel("sweep", "{}", Timestamp(1), "stopped", None).await?;
				assert_eq!(n, 1, "the RUNNING occurrence is what gets cancelled");
				if succeeds { Ok(()) } else { Err(Error::Unavailable("nope".to_owned())) }
			}
		});
		assert!(r.tick(Timestamp(0)).await.unwrap());

		let live: i64 = sqlx::query_scalar(
			"SELECT COUNT(*) FROM jobs WHERE kind = 'sweep' AND status IN ('PENDING','RUNNING')",
		)
		.fetch_one(sql.read_pool())
		.await
		.unwrap();
		assert_eq!(live, 0, "{name}: a cancelled occurrence must not carry the chain on");
	}
}

/// And the third of the trio. `job_terminate` was a bare `WHERE id = ?`, so a handler that
/// spent its last attempt (or returned a `Retry::Never` error) after an operator's
/// `Nav::cancel_filing` had already flipped the row overwrote `last_error` and `err_code` with
/// its own. The row stayed `FAILED`, but the reason a human deliberately recorded was gone and
/// the `A-JOB-FAILED` alert reported the wrong cause.
#[tokio::test]
async fn a_job_cancelled_while_running_keeps_the_operators_reason() {
	let (_db, store, sql) = fresh("job-cancel-terminate").await;
	let id = enqueue(&store, "boom", "{}", None, Timestamp(0)).await.unwrap().unwrap();

	let mut r = Runner::new(Arc::clone(&store));
	let cancelling = Arc::clone(&store);
	r.register("boom", move |_job| {
		let store = Arc::clone(&cancelling);
		async move {
			store
				.job_cancel(
					"boom",
					"{}",
					Timestamp(1),
					"cancelled by an operator",
					Some("E-NAV-CANCELLED"),
				)
				.await?;
			// `Retry::Never`, so the runner terminates rather than reschedules.
			Err(Error::internal("nope"))
		}
	});
	assert!(r.tick(Timestamp(0)).await.unwrap());

	let (status, err, code): (String, Option<String>, Option<String>) =
		sqlx::query_as("SELECT status, last_error, err_code FROM jobs WHERE id = ?")
			.bind(id)
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!(status, "FAILED");
	assert_eq!(err.as_deref(), Some("cancelled by an operator"));
	assert_eq!(code.as_deref(), Some("E-NAV-CANCELLED"));
}

/// `job_redrive` is the counterpart to `job_cancel`, and the reason `Nav::submit` no longer
/// mints a second `NAV_REPORT` row: the one row keeps its identity — `dedup_key` included — so
/// two workers can never hold two rows for one invoice.
#[tokio::test]
async fn a_redrive_resets_the_failed_row_and_keeps_its_key() {
	let (_db, store, sql) = fresh("job-redrive").await;
	let key = "nav:invoice:7";
	let id = enqueue(&store, "NAV_REPORT", "{\"invoiceId\":7}", Some(key), Timestamp(0))
		.await
		.unwrap()
		.unwrap();

	// Nothing to re-drive while the row can still run.
	assert_eq!(
		store
			.job_redrive("NAV_REPORT", "{\"invoiceId\":7}", Timestamp(5))
			.await
			.unwrap(),
		0
	);

	assert_eq!(store.job_claim(Timestamp(0)).await.unwrap().map(|j| j.id), Some(id));
	Runner::new(Arc::clone(&store))
		.terminate(id, Timestamp(1), "nav said no", None)
		.await
		.unwrap();
	assert_eq!(row(&sql, id).await.0, "FAILED");

	assert_eq!(
		store
			.job_redrive("NAV_REPORT", "{\"invoiceId\":7}", Timestamp(5))
			.await
			.unwrap(),
		1
	);
	assert_eq!(row(&sql, id).await, ("PENDING".to_owned(), 0, 5), "reset, not resumed");
	// The same row, so the key is still spent and no second one can be enqueued behind it.
	assert_eq!(status_and_key(&sql, id).await.1.as_deref(), Some(key));
	assert!(
		enqueue(&store, "NAV_REPORT", "{\"invoiceId\":7}", Some(key), Timestamp(5))
			.await
			.unwrap()
			.is_none()
	);
}

// ---------------------------------------------------------------- alerts

/// A `prev` dated now, written before `build`, is what keeps the seeded `ALERT_SWEEP` from
/// racing the test: its worker starts claiming the moment `build` returns, and the interval
/// gate turns that first run into a no-op. It does not come round again for 60 seconds.
async fn quiet_sweep(store: &Arc<dyn CoreStore>, now: Timestamp) {
	store
		.var_set("alert.prev", &format!("{{\"at\":{},\"codes\":{{}}}}", now.0))
		.await
		.unwrap();
	// A built `App` seeds `ALERT_SWEEP` and spawns workers that run it, so the background tick
	// races anything the test asserts about mails. Parking a live row a day out makes
	// `seed_periodic`'s `has_live` skip: the only sweeps that run are the ones the test calls.
	enqueue(store, saas_core::job::KIND_ALERT_SWEEP, "{}", None, Timestamp(now.0 + 86_400))
		.await
		.unwrap();
}

#[tokio::test]
async fn alerts_are_derived_from_the_job_table() {
	let (db, store, sql) = fresh("alerts-jobs").await;
	let now = Timestamp::now();
	quiet_sweep(&store, now).await;
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::clone(&store))
		.build()
		.await
		.unwrap();
	app.settings.set("jobs.backlog_warn", "1", None).await.unwrap();

	// `run_at` far out for all three: a worker must not claim them out from under the test.
	// `NAV_POLL` is old enough for its own `jobs.alert_after` (86_400); `FRESH` falls to the
	// family default of an hour and is younger than it, which is the case that must not fire.
	// `boom` is fresh because `A-JOB-FAILED` is age-bounded by `jobs.failed_alert_hours` —
	// see `a_stale_failure_no_longer_raises_a_job_failed`.
	for (kind, status, attempts, created) in [
		("boom", "FAILED", 8, now.0),
		("NAV_POLL", "PENDING", 3, now.0 - 90_000),
		("FRESH", "PENDING", 1, now.0),
	] {
		// `last_error` non-NULL on all three: that, not `attempts`, is what marks a job as
		// having actually failed — see `a_reclaimed_job_is_not_a_retrying_one`.
		sqlx::query(
			"INSERT INTO jobs (kind, payload, status, run_at, attempts, created_at, last_error)
			 VALUES (?, '{}', ?, ?, ?, ?, 'boom')",
		)
		.bind(kind)
		.bind(status)
		.bind(1_i64 << 40)
		.bind(attempts)
		.bind(created)
		.execute(sql.write_pool())
		.await
		.unwrap();
	}

	let out = saas_core::alert::alerts(&app).await.unwrap();
	let find = |code: &str| out.iter().find(|a| a.code == code).cloned();

	// Most severe first. `NAV_POLL` is ERROR too — `jobs.max_attempts.NAV_POLL` is 0, see
	// `an_unbounded_kind_goes_stale_at_error_not_warn` — and a stable sort keeps the failed job
	// ahead of it, which is the order it was pushed in.
	assert_eq!(out.first().map(|a| a.code), Some("A-JOB-FAILED"));
	let failed = find("A-JOB-FAILED").unwrap();
	assert_eq!((failed.severity, failed.count), (saas_core::alert::Severity::Error, 1));
	assert_eq!(find("A-JOB-BACKLOG").unwrap().severity, saas_core::alert::Severity::Warn);

	// One stale kind, not two: `FRESH` is retrying but has not passed its threshold.
	assert_eq!(out.iter().filter(|a| a.code == "A-JOB-STALE").count(), 1);
	let stale = find("A-JOB-STALE").unwrap();
	assert_eq!(stale.count, 1);
	assert!(stale.message.contains("NAV_POLL"), "{}", stale.message);
	// An unset `auth.jwt_key` is not a stale one, and `TmpDb`'s empty `data_dir` cannot be
	// stat'd — both are absences, not alerts.
	assert!(find("A-SECRET-STALE").is_none() && find("A-DISK-LOW").is_none());
}

/// A kind whose `max_attempts` is 0 never reaches FAILED, so `A-JOB-STALE` is its only alert —
/// and `admin.alert_min_severity` defaults to ERROR, which a WARN never clears. That set is the
/// statutory kinds (`NAV_REPORT`, `NAV_POLL`, `RENDER_PDF`), so at WARN a filing stuck forever
/// mailed nobody.
#[tokio::test]
async fn an_unbounded_kind_goes_stale_at_error_not_warn() {
	let (db, store, sql) = fresh("alert-stale-severity").await;
	let now = Timestamp::now();
	quiet_sweep(&store, now).await;
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::clone(&store))
		.build()
		.await
		.unwrap();
	app.settings.set("jobs.max_attempts.UNBOUND", "0", None).await.unwrap();
	app.settings.set("jobs.max_attempts.YOUNG", "0", None).await.unwrap();
	for kind in ["UNBOUND", "BOUNDED"] {
		app.settings.set(&format!("jobs.alert_after.{kind}"), "1", None).await.unwrap();
	}

	// `YOUNG` keeps the family's 3600 and is seconds old: the gate `A-JOB-STALE` has always had,
	// which this escalation must not turn into a page on the first failed attempt.
	for (kind, created) in [
		("UNBOUND", now.0 - 60),
		("UNBOUND", now.0 - 60),
		("BOUNDED", now.0 - 60),
		("YOUNG", now.0),
	] {
		sqlx::query(
			"INSERT INTO jobs (kind, payload, status, run_at, attempts, created_at, last_error)
			 VALUES (?, '{}', 'PENDING', ?, 1, ?, 'boom')",
		)
		.bind(kind)
		.bind(1_i64 << 40)
		.bind(created)
		.execute(sql.write_pool())
		.await
		.unwrap();
	}

	let out = saas_core::alert::alerts(&app).await.unwrap();
	let stale = |kind: &str| {
		out.iter()
			.find(|a| a.code == "A-JOB-STALE" && a.message.contains(kind))
			.cloned()
	};

	// One alert per kind, carrying the count — an outage that strands 500 filings is one email.
	let unbound = stale("UNBOUND").expect("the unbounded kind is past its threshold");
	assert_eq!((unbound.severity, unbound.count), (saas_core::alert::Severity::Error, 2));
	assert_eq!(out.iter().filter(|a| a.code == "A-JOB-STALE").count(), 2, "{out:?}");

	// A kind that can reach FAILED raises `A-JOB-FAILED` on its own, so it stays WARN here.
	assert_eq!(stale("BOUNDED").unwrap().severity, saas_core::alert::Severity::Warn);
	assert!(stale("YOUNG").is_none(), "a fresh failure must not alert, whatever its severity");
}

/// `job_claim` increments `attempts` *before* the handler runs and `job_reclaim` returns a
/// crashed `RUNNING` job to `PENDING` without resetting it, so `attempts > 0` reported a job
/// that had never failed as "still retrying". A restart during a long job raised `A-JOB-STALE`
/// on it; `last_error IS NOT NULL` is what distinguishes the two.
#[tokio::test]
async fn a_reclaimed_job_is_not_a_retrying_one() {
	let (db, store, sql) = fresh("alert-job-reclaimed").await;
	let now = Timestamp::now();
	quiet_sweep(&store, now).await;
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::clone(&store))
		.build()
		.await
		.unwrap();

	// Old enough to be past `NAV_POLL`'s own `jobs.alert_after` (86_400), and `run_at` far out
	// so the `App`'s own workers cannot claim it out from under the test — the state a claim
	// leaves is written here instead: `RUNNING`, one attempt, and nothing failed.
	let far = 1_i64 << 40;
	let running = |id: Option<i64>| {
		sqlx::query(
			"INSERT INTO jobs (id, kind, payload, status, run_at, attempts, created_at)
			 VALUES (COALESCE(?, NULL), 'NAV_POLL', '{}', 'RUNNING', ?, 1, ?)
			 ON CONFLICT (id) DO UPDATE SET status = 'RUNNING'",
		)
		.bind(id)
		.bind(far)
		.bind(now.0 - 90_000)
		.execute(sql.write_pool())
	};
	running(None).await.unwrap();
	let id: i64 = sqlx::query_scalar("SELECT id FROM jobs WHERE kind = 'NAV_POLL'")
		.fetch_one(sql.read_pool())
		.await
		.unwrap();

	// The process dies and the next start reclaims it: `PENDING`, `attempts` still 1. The count
	// is not asserted — the `App`'s own periodic seeds may be `RUNNING` alongside it.
	assert!(store.job_reclaim(Timestamp::now()).await.unwrap() >= 1);
	let stale = |alerts: &[saas_core::alert::Alert]| {
		alerts.iter().any(|a| a.code == "A-JOB-STALE" && a.message.contains("NAV_POLL"))
	};
	assert!(
		!stale(&saas_core::alert::alerts(&app).await.unwrap()),
		"a crash-reclaimed job was reported as retrying"
	);

	// And a genuine failure still is one. `job_fail` is `RUNNING`-guarded, hence the round trip.
	running(Some(id)).await.unwrap();
	assert_eq!(store.job_fail(id, Timestamp(far), "boom", None).await.unwrap(), 1);
	assert!(
		stale(&saas_core::alert::alerts(&app).await.unwrap()),
		"a job in backoff after a real failure went unreported"
	);
}

/// The sweep is state-free by design: what it mails is decided by comparing against the previous
/// sweep's set in `vars`, so these three cases are the whole of it — new, unchanged, worsened.
#[tokio::test]
async fn sweep_mails_an_alert_once_and_again_when_it_worsens() {
	let (db, store, sql) = fresh("alert-sweep").await;
	let now = Timestamp::now();
	quiet_sweep(&store, now).await;
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::clone(&store))
		.build()
		.await
		.unwrap();
	app.settings.set("admin.alert_email", "ops@example.test", None).await.unwrap();
	sqlx::query(
		"INSERT INTO jobs (kind, payload, status, run_at, attempts, created_at)
		 VALUES ('boom', '{}', 'FAILED', ?, 8, ?)",
	)
	.bind(1_i64 << 40)
	// Fresh: `A-JOB-FAILED` is age-bounded by `jobs.failed_alert_hours`.
	.bind(now.0)
	.execute(sql.write_pool())
	.await
	.unwrap();

	// `at: 0` opens the interval gate; `codes` is what the sweep compares against.
	let set_prev = |codes: &str| {
		let raw = format!("{{\"at\":0,\"codes\":{codes}}}");
		let store = Arc::clone(&store);
		async move { store.var_set("alert.prev", &raw).await.unwrap() }
	};
	let mails = || async {
		sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'SEND_EMAIL'")
			.fetch_one(sql.read_pool())
			.await
			.unwrap()
	};

	// `memo_key` is the code, a unit separator and the link, so the one alert this raises has
	// a composite memo key, not a bare code.
	let failed = r#""A-JOB-FAILED\u001f/api/admin/jobs?status=FAILED""#;

	// New: nothing was holding before, so it is mailed.
	set_prev("{}").await;
	saas_core::alert::sweep(&app).await.unwrap();
	assert_eq!(mails().await, 1);
	// The stored set is the comparison keys, not a round-tripped `Vec<Alert>`.
	let raw = store.var_get("alert.prev").await.unwrap().unwrap();
	assert!(raw.contains(&format!("{failed}:\"ERROR\"")), "{raw}");

	// Unchanged: same code, same severity, no second mail.
	set_prev(&format!("{{{failed}:\"ERROR\"}}")).await;
	saas_core::alert::sweep(&app).await.unwrap();
	assert_eq!(mails().await, 1);

	// Worsened: the same code at a lower severity last time is mailed again.
	set_prev(&format!("{{{failed}:\"WARN\"}}")).await;
	saas_core::alert::sweep(&app).await.unwrap();
	assert_eq!(mails().await, 2);
}

/// `job::thrice` retries `fail` as a whole, so an attempt that committed and lost its answer
/// made the next one see `rows_affected == 0` — indistinguishable, by the count alone, from an
/// operator's cancel. Calling it a cancel minted a periodic successor beside the row that was
/// already `PENDING` for its own retry, and every completion minted another: the doubling was
/// permanent. The 0-row path reads the row instead.
#[tokio::test]
async fn a_committed_fail_with_a_lost_response_leaves_one_live_row() {
	let db = TmpDb::new("job-fail-lost-answer");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	let inner: Arc<dyn CoreStore> = Arc::new(sql.clone());
	let store: Arc<dyn CoreStore> = Arc::new(PoolDown::fails_lost(inner, 1));

	enqueue(&store, "sweep", "{}", Some("sweep:0"), Timestamp(0)).await.unwrap();
	let mut r = Runner::new(Arc::clone(&store));
	// `Retry::Backoff` and an unspent budget, so the row goes back to `PENDING` and *is* the
	// live chain; a successor beside it is the doubling.
	r.register_periodic("sweep", 60, |_| async { Err(Error::Unavailable("nope".to_owned())) });
	assert!(r.tick(Timestamp(0)).await.unwrap());

	let live: i64 = sqlx::query_scalar(
		"SELECT COUNT(*) FROM jobs WHERE kind = 'sweep' AND status IN ('PENDING','RUNNING')",
	)
	.fetch_one(sql.read_pool())
	.await
	.unwrap();
	assert_eq!(live, 1, "the retrying row carries the chain; a successor beside it doubles it");
}

// ---------------------------------------------------------------- auth middleware

/// `authenticate`'s `Err(e) if required` arm sat above the arms that inspect the error, so a
/// reader-pool 500 was charged to the `AUTH_FAILED` bucket — and once it drained,
/// `charge_auth_failure` answered the 429 *instead of* the 500, laundering a database outage
/// into `E-CORE-RATELIMIT`.
#[tokio::test]
async fn a_reader_pool_outage_is_never_charged_to_the_auth_failed_bucket() {
	use axum::body::Body;
	use axum::http::Request;
	use axum::middleware::Next;
	use saas_core::auth_mw::{Claims, JWT_SECRET_KEY, require_auth};
	use tower::ServiceExt;

	let db = TmpDb::new("auth-pool-down");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	let inner: Arc<dyn CoreStore> = Arc::new(sql);
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(PoolDown::new(inner)) as Arc<dyn CoreStore>)
		.build()
		.await
		.unwrap();

	// A *valid* token: the decode has to succeed, or `verify` never reaches the store.
	let key = app.secrets.get_or_create(JWT_SECRET_KEY, 32).await.unwrap();
	let claims = Claims {
		sub: "acc_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
		org: None,
		rol: None,
		opr: false,
		ep: 0,
		auth_at: None,
		imp: None,
		typ: None,
		iat: 0,
		exp: i64::from(u32::MAX),
	};
	let token = jsonwebtoken::encode(
		&jsonwebtoken::Header::default(),
		&claims,
		&jsonwebtoken::EncodingKey::from_secret(&key),
	)
	.unwrap();

	let router = axum::Router::new()
		.route("/x", axum::routing::get(|| async { "ok" }))
		.layer(axum::middleware::from_fn(require_auth))
		.layer(axum::middleware::from_fn(|mut req: Request<Body>, next: Next| async move {
			req.extensions_mut().insert(ClientIp("9.9.9.9".parse().unwrap()));
			next.run(req).await
		}))
		.layer(axum::Extension(app.clone()));

	// `ratelimit.auth_failed` ships as 20/5min/ip, so the 21st request is the one that used
	// to flip to 429. Every one of them must still be the outage.
	for n in 1..=25 {
		let res = router
			.clone()
			.oneshot(
				Request::get("/x")
					.header("authorization", format!("Bearer {token}"))
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		assert_eq!(res.status(), 503, "request {n} must stay the outage, not become a 429");
	}
}
/// `seed_periodic` skips a kind that has a live row, and `RUNNING` counts as live — while
/// `job_claim` selects `PENDING` only. So a row a crash left `RUNNING` was neither reclaimed
/// nor re-seeded, and the chain was dead until the next process start with `/readyz` green.
/// `reclaim` has to run first, and a failed one has to be fatal.
#[tokio::test]
async fn a_periodic_row_left_running_by_a_crash_is_reclaimed_before_seeding() {
	let (db, store, sql) = fresh("boot-reclaim").await;
	sqlx::query(
		"INSERT INTO jobs (kind, payload, status, run_at, attempts, created_at)
		 VALUES (?, '{}', 'RUNNING', 0, 1, 0)",
	)
	.bind(saas_core::job::KIND_SWEEP)
	.execute(sql.write_pool())
	.await
	.unwrap();

	let _app = AppBuilder::new()
		.config(db.config())
		.store(Arc::clone(&store))
		.build()
		.await
		.unwrap();

	let rows: Vec<(String,)> = sqlx::query_as("SELECT status FROM jobs WHERE kind = ?")
		.bind(saas_core::job::KIND_SWEEP)
		.fetch_all(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(rows.len(), 1, "the reclaimed row is the seed, not a second one");
	assert_eq!(rows[0].0, "PENDING", "a RUNNING row no worker can claim is not 'live'");
}

/// `job_sweep` deletes finished rows whose `dedup_key` carries `PERIODIC_KEY_PREFIX`, so a
/// consumer key spelled `periodic:billing:cust_x` lost the permanent once-only record the
/// module promises and a replayed enqueue re-ran the work. The namespace is reserved now.
#[tokio::test]
async fn the_periodic_dedup_namespace_is_reserved() {
	let (_db, store, sql) = fresh("periodic-namespace").await;

	let err = enqueue(&store, "billing", "{}", Some("periodic:billing:cust_x"), Timestamp(0))
		.await
		.expect_err("a consumer must not mint a periodic scheduling token");
	assert!(matches!(err, Error::Internal(_)), "{err:?}");
	// Far out, so it is never the row `tick` below claims instead of the periodic one.
	assert!(
		enqueue(&store, "billing", "{}", Some("billing:cust_x"), Timestamp(1 << 40))
			.await
			.is_ok()
	);

	// The framework's own chain still seeds, reschedules under the prefix, and sweeps it.
	let mut r = Runner::new(Arc::clone(&store));
	r.register_periodic("sweep", 60, |_| async { Ok(()) });
	saas_core::job::seed_periodic(&store, "sweep").await.unwrap();
	assert!(r.tick(Timestamp(Timestamp::now().0 + 1)).await.unwrap());

	let successor: i64 = sqlx::query_scalar(
		"SELECT COUNT(*) FROM jobs WHERE kind = 'sweep' AND dedup_key LIKE 'periodic:%'",
	)
	.fetch_one(sql.read_pool())
	.await
	.unwrap();
	assert_eq!(successor, 1, "the reschedule still mints its prefixed token");

	// `job_sweep` reclaims a *finished* one, which is what the prefix is for.
	sqlx::query("UPDATE jobs SET status = 'DONE', done_at = 0 WHERE dedup_key LIKE 'periodic:%'")
		.execute(sql.write_pool())
		.await
		.unwrap();
	assert!(store.job_sweep(Timestamp(1 << 40)).await.unwrap() >= 1);
	let left: i64 =
		sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE dedup_key LIKE 'periodic:%'")
			.fetch_one(sql.read_pool())
			.await
			.unwrap();
	assert_eq!(left, 0, "a finished periodic token is reclaimed");
}

/// `build` always constructed a `Runner` and always called `reclaim`, which flips **every**
/// `RUNNING` row back to `PENDING` — and `jobs.workers` is a *setting*, so it could not differ
/// between two processes sharing one database. A second instance (a rolling deploy, a
/// web+worker split) therefore yanked the first's in-flight `NAV_REPORT` and `SEND_EMAIL`
/// rows back to `PENDING` and had them run twice. `JOBS_WORKERS=0` is how a process says it
/// runs none.
#[tokio::test]
async fn a_worker_less_process_reclaims_nothing() {
	let (db, store, sql) = fresh("workers-zero").await;
	let id = enqueue(&store, "mail", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	assert_eq!(store.job_claim(Timestamp(0)).await.unwrap().unwrap().id, id);
	assert_eq!(row(&sql, id).await.0, "RUNNING");

	AppBuilder::new()
		.config(Config { jobs_workers: Some(0), ..db.config() })
		.store(Arc::clone(&store))
		.build()
		.await
		.unwrap();

	assert_eq!(
		row(&sql, id).await.0,
		"RUNNING",
		"another process's in-flight job was reclaimed and will now run twice"
	);
}

/// `Settings::get`'s "no row" arm — the normal case for a key nobody ever set — called
/// `std::env::var` on every read, and `ratelimit`'s blanket limiter and `client_ip_mw` both
/// route through it, so the process-wide environment lock was taken on every HTTP request.
/// The snapshot is taken in `Settings::new`, and the row/env/default order must not change.
///
/// Re-runs itself in a child process with the variable set, for the reason
/// [`a_named_scope_honours_its_environment_override`] gives: `std::env::set_var` is `unsafe`
/// on edition 2024 and this workspace forbids `unsafe`.
#[tokio::test]
async fn an_env_override_is_read_when_settings_is_built() {
	const NAME: &str = "an_env_override_is_read_when_settings_is_built";
	if reexec(NAME, "CURRENCY_BASE", "EUR") {
		return;
	}

	let (_db, store, _sql) = fresh("settings-env-snapshot").await;
	let settings = settings(store);
	assert_eq!(settings.text("currency.base").await.unwrap(), "EUR", "env beats the default");

	settings.set("currency.base", "USD", None).await.unwrap();
	assert_eq!(settings.text("currency.base").await.unwrap(), "USD", "a row beats the env");
}

/// A blank variable is **absent**, not an empty override — for a setting as it already was for
/// a secret. `.env.example` ships every key with an empty value, so a copied one used to
/// shadow the registry default with `""`: here that is a `range(3, 3)` violation on every read
/// of `currency.base`, which no row and no default could have caused.
///
/// Re-runs itself in a child process with the variable set, for the reason
/// [`an_env_override_is_read_when_settings_is_built`] gives.
#[tokio::test]
async fn a_blank_variable_does_not_shadow_a_default() {
	const NAME: &str = "a_blank_variable_does_not_shadow_a_default";
	if reexec(NAME, "CURRENCY_BASE", "   ") {
		return;
	}

	let (_db, store, _sql) = fresh("settings-blank-env").await;
	assert_eq!(settings(store).text("currency.base").await.unwrap(), "HUF");
}

/// `check_required` only tested `required` keys for blankness, so a value that does not *parse*
/// first surfaced inside a job handler — as `Error::Setting`, which is `Retry::Never`, so one
/// operator mistake destroyed every queued activation link instead of delaying it. Every
/// declared key under the prefix goes through `Settings::get` now, which is what resolves the
/// environment fallback, so the process refuses to boot instead.
///
/// Re-runs itself in a child process with the variable set, for the reason
/// [`an_env_override_is_read_when_settings_is_built`] gives.
#[tokio::test]
async fn check_required_refuses_a_setting_that_only_the_environment_breaks() {
	const NAME: &str = "check_required_refuses_a_setting_that_only_the_environment_breaks";
	if reexec(NAME, "EMAIL_SMTP_PORT", "99999") {
		return;
	}

	let (_db, store, _sql) = fresh("settings-check-required-env").await;
	let settings = settings(store);
	// Every `required` email key configured, so blankness is not what this trips on — and no
	// row for `email.smtp.port`, so it resolves through the environment.
	for (key, value) in
		[("email.from", "billing@example.com"), ("email.smtp.host", "smtp.example.com")]
	{
		settings.set(key, value, None).await.unwrap();
	}
	assert!(settings.int("email.smtp.port").await.is_err(), "the override is out of range");

	let err = settings.check_required("email.").await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-SETTING");
	assert!(err.to_string().contains("email.smtp.port"), "{err}");
}

/// `deployment.env` chooses which system every filing and every card charge goes to, and
/// `check_required` was only ever called with `"nav."` and `"email."`. `DEPLOYMENT_ENV=prod`
/// booted clean and first surfaced inside `REPORT_INVOICE` as `Retry::Never` — filing dead.
///
/// Re-runs itself in a child process with the variable set, for the reason
/// [`an_env_override_is_read_when_settings_is_built`] gives.
#[tokio::test]
async fn check_required_refuses_an_unknown_deployment_env() {
	const NAME: &str = "check_required_refuses_an_unknown_deployment_env";
	if reexec(NAME, "DEPLOYMENT_ENV", "prod") {
		return;
	}

	let (_db, store, _sql) = fresh("settings-check-deployment").await;
	let settings = settings(store);

	let err = settings.check_required("deployment.").await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-SETTING");
	assert!(err.to_string().contains("deployment.env"), "{err}");
}

/// A scoped bundle reaches [`saas_core::app::AppState::route_scopes`], the prefix list
/// `GET /api/api-keys/scopes` serves and mint validation checks against.
#[tokio::test]
async fn a_scoped_bundle_keeps_its_prefixes_through_the_builder() {
	use std::collections::BTreeSet;

	use axum::Router;
	use saas_core::app::{App, RouterScopeExt};

	let (db, store, _sql) = fresh("route-scopes").await;
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::clone(&store))
		.routes(
			Router::<App>::new()
				.route("/probe", axum::routing::get(|| async { "ok" }))
				.scope("invoice")
				.with(|r| r.fallback(|| async { "spa" })),
		)
		.build()
		.await
		.unwrap();

	assert_eq!(app.route_scopes, BTreeSet::from(["invoice"]));
}

// vim: ts=4
