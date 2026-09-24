//! Everything `saas-core` persists, as one trait the store adapter implements.
//!
//! `saas-core` runs no SQL and holds no pool: [`crate::AppState`] carries an
//! `Arc<dyn CoreStore>` and every framework read or write goes through it. Domain types
//! cross this boundary — `Timestamp`, `i64`, `String`, `Vec<u8>` and the plain structs
//! below — never database types.
//!
//! Unlike the feature crates' stores, this one is a dedicated `AppState` field rather than
//! an `Extensions` entry: the auth middleware runs on every request and cannot fall back on
//! a runtime `Error::internal` for a store the consumer forgot to register.
//!
//! The `accounts`, `orgs` and `memberships` reads live here — not in `saas-auth` — because
//! `saas-invoice` and `saas-nav` call [`crate::auth_mw::require_operator`], and moving the
//! middleware would force a `saas-invoice -> saas-auth` edge. The adapter already depends on
//! both crates and can implement all three.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ClResult;
use crate::ids::ApiKeyId;
use crate::job::Job;
use crate::types::Timestamp;

/// One `audit_logs` row, exactly the columns the insert binds. `at` is stamped by
/// [`crate::audit::log`] so the caller's clock is the one recorded.
#[derive(Clone, Debug)]
pub struct AuditEntry {
	pub at: Timestamp,
	pub account_id: Option<i64>,
	pub org_id: Option<i64>,
	pub ip: Option<String>,
	pub entity: String,
	pub entity_id: Option<String>,
	pub action: String,
	/// The JSON detail, already serialized.
	pub detail: Option<String>,
	pub request_id: Option<String>,
}

/// What the bearer-token path reads off `accounts`. `status` is the raw column value:
/// `auth_mw` owns the `PENDING`/`SUSPENDED`/anonymized mapping.
#[derive(Clone, Debug)]
pub struct TokenAccount {
	pub id: i64,
	pub token_epoch: i64,
	/// An accepted `ADMIN`-or-`OWNER` membership on the root org, which is what an operator
	/// now is. Derived by the adapter's join, not a column — `accounts.is_operator` is gone.
	pub is_root_admin: bool,
	pub status: String,
}

/// One `api_keys` row — the key **plus** the account, org and membership columns every read
/// carries.
///
/// In `saas-core` rather than `saas-auth` because the per-request bearer path verifies keys
/// here and must not grow a `saas-core -> saas-auth` edge. The caller's account, org and
/// membership travel with every read for the same reason `token_epoch` does: a removed
/// membership or a suspended account has to kill a key on the next request, not wait out the
/// token that named it.
#[derive(Clone, Debug)]
pub struct ApiKey {
	pub id: i64,
	pub uid: ApiKeyId,
	pub org_id: i64,
	pub account_id: i64,
	pub name: String,
	pub prefix: String,
	/// The stored hash of the full key. SHA-256 hex, not a password hash — the key is 256 bits
	/// of server-generated randomness, so there is no low-entropy secret to stretch.
	pub key_hash: String,
	/// The `scopes` column as written: a JSON array of `<bundle>:<read|write>` strings.
	pub scopes: String,
	pub created_at: Timestamp,
	pub last_used_at: Option<Timestamp>,
	pub expires_at: Option<Timestamp>,
	pub revoked_at: Option<Timestamp>,
	/// `accounts.status`, the raw column value.
	pub account_status: String,
	/// `orgs.status`, the raw column value.
	pub org_status: String,
	/// An **accepted** membership on the key's org **or any ancestor of it** links the key's
	/// account to it, at the read.
	pub member: bool,
}

/// API key input; the plaintext key never reaches the store.
#[derive(Clone, Debug)]
pub struct NewApiKey {
	pub org_id: i64,
	pub account_id: i64,
	pub name: String,
	pub prefix: String,
	pub key_hash: String,
	pub scopes: String,
	pub expires_at: Option<Timestamp>,
}

/// `memberships.role`, and the effective role after the ancestor walk. Re-read from the
/// database on privileged routes rather than trusted from the access token.
///
/// In `saas-core` rather than `saas-auth` because [`CoreStore::org_membership_role`] returns
/// it on every authenticated request; `saas-auth` re-exports it as `saas_auth::store::Role`.
/// The `Ord` derive follows declaration order and is what `min: Role` comparisons rely on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Role {
	Member,
	Admin,
	Owner,
}

crate::str_enum!(Role { Member => "MEMBER", Admin => "ADMIN", Owner => "OWNER" });

#[async_trait]
pub trait CoreStore: Send + Sync + 'static {
	// ---- settings ------------------------------------------------------------------

	/// The `settings` row as written, or `None` when the key has no row. No environment or
	/// registry-default fallback — [`crate::settings::Settings`] owns that.
	async fn setting_get(&self, key: &str) -> ClResult<Option<String>>;

	/// Upsert. `raw` is already validated and trimmed by the caller.
	async fn setting_set(&self, key: &str, raw: &str, updated_by: Option<i64>) -> ClResult<()>;

	// ---- secrets -------------------------------------------------------------------

	/// `(nonce, ciphertext)` at `org_id` (0 = the global level), or `None` when the secret has
	/// never been set there. Exact match only — no fallback to org 0 or to an ancestor. Encryption
	/// stays in [`crate::secrets::SecretStore`]; only the blobs cross.
	async fn secret_get(&self, org_id: i64, key: &str) -> ClResult<Option<(Vec<u8>, Vec<u8>)>>;

	/// Upsert on `(org_id, key)`, replacing any earlier value.
	async fn secret_set(
		&self,
		org_id: i64,
		key: &str,
		nonce: &[u8],
		ciphertext: &[u8],
		updated_by: Option<i64>,
	) -> ClResult<()>;

	/// Insert at org 0 only when the key is free; a collision is a no-op, never an error.
	///
	/// Not expressible as `secret_get` + `secret_set`: `SecretStore::get_or_create` relies on
	/// the loser of a race leaving the winner's value in place, and a read-then-write would
	/// let two workers both mint `auth.jwt_key` — the loser having already signed sessions
	/// with a key that is now gone.
	async fn secret_put_if_absent(
		&self,
		key: &str,
		nonce: &[u8],
		ciphertext: &[u8],
	) -> ClResult<()>;

	/// When the secret at `org_id` last changed, or `None` when it is unset. Never the value.
	async fn secret_updated_at(&self, org_id: i64, key: &str) -> ClResult<Option<Timestamp>>;

	// ---- audit ---------------------------------------------------------------------

	/// Append one row, joining the transaction the calling **handle** is bound to, so a rollback
	/// takes it with it; unbound, it writes immediately. This is the method for a record of a
	/// mutation that succeeded — undoing the operation makes the row wrong, not informative.
	///
	/// **Call it on the handle the caller is inside.** A *pooled* handle used within a transaction
	/// goes to the pool for the connection that transaction holds and blocks until
	/// `acquire_timeout` — 30 seconds, then a swallowed `Error::Unavailable` and a missing row.
	/// `app.store` is pooled.
	///
	/// Append-only is a property of the trait's shape, not of a database trigger.
	async fn audit_log(&self, entry: &AuditEntry) -> ClResult<()>;

	/// Append one row **outside every transaction the caller holds**: it must be written whether
	/// that transaction commits or rolls back. This is the method for evidence of something that
	/// happened on a path that fails, where the rollback ending the caller would take the only
	/// record with it.
	///
	/// SQLite has one writer connection, so the adapter buffers the row on the bound handle until
	/// the transaction ends; an engine with a second writer connection writes it immediately.
	///
	/// **Call it on the handle the caller is inside.** A *pooled* handle used within a transaction
	/// cannot buffer — it goes to the pool for the connection the transaction holds and blocks
	/// there until `acquire_timeout`. `app.store` is pooled.
	///
	/// The guarantee degrades in one case: a transaction dropped with no runtime left to send
	/// the insert to cannot write anything, and the buffered rows survive only as `error!` log
	/// lines carrying the whole entry.
	async fn audit_detached(&self, entry: &AuditEntry) -> ClResult<()>;

	// ---- jobs ----------------------------------------------------------------------

	/// Queue a job. The new row's id, or `None` when `dedup_key` is already taken — a
	/// collision is a no-op. A `None` key means no deduplication at all.
	async fn job_enqueue(
		&self,
		kind: &str,
		payload: &str,
		dedup_key: Option<&str>,
		run_at: Timestamp,
	) -> ClResult<Option<i64>>;

	/// Whether a job of this kind is still able to run (`PENDING` or `RUNNING`). `besides`
	/// excludes one row — the reschedule path asks while its own job is still `RUNNING`.
	async fn job_has_live(&self, kind: &str, besides: Option<i64>) -> ClResult<bool>;

	/// Enqueue `kind` at `run_at` only if no `PENDING` or `RUNNING` row of that kind exists.
	/// `Some(id)` when it was inserted, `None` when a live row already held the chain.
	///
	/// One statement, not [`Self::job_has_live`] + [`Self::job_enqueue`]: two processes booting
	/// a rolling deploy both passed the read and both seeded, and the chain doubled permanently.
	async fn job_seed_periodic(&self, kind: &str, run_at: Timestamp) -> ClResult<Option<i64>>;

	/// The `status` of the row holding `dedup_key`, or `None` when the key is free.
	///
	/// What a rejected [`CoreStore::job_enqueue`] cannot say: a key is spent by a `DONE` or
	/// `FAILED` row as much as it is held by a live one, and only the first of those needs a
	/// person. `dedup_key` is unique, so this is one row or none.
	async fn job_status_by_key(&self, dedup_key: &str) -> ClResult<Option<String>>;

	/// [`Self::job_status_by_key`] for many keys at once: `(dedup_key, status)` for the keys that
	/// are taken, in no particular order, a free key simply being absent. `saas_nav`'s batch
	/// filing tests up to `nav.batch_max - 1` candidates per leader. `keys` is unbounded: the
	/// store chunks the `IN (…)` list itself.
	async fn job_statuses_by_keys(&self, keys: &[String]) -> ClResult<Vec<(String, String)>>;

	/// The `status` of one row, or `None` when the id is unknown.
	///
	/// Read only on a terminal write that matched zero rows, to tell "an operator cancelled
	/// this" from "my previous attempt already landed": [`crate::job::thrice`] retries the
	/// whole write, so a committed [`Self::job_fail`] whose answer was lost looks identical to
	/// a cancel in the row count alone — and calling it a cancel minted a second live row for a
	/// periodic chain, permanently.
	async fn job_status(&self, id: i64) -> ClResult<Option<String>>;

	/// Claim at most one due job, incrementing `attempts`. Must hand a row to exactly one
	/// caller.
	async fn job_claim(&self, now: Timestamp) -> ClResult<Option<Job>>;

	/// Mark `DONE` and blank the payload — it carries activation and reset links that a
	/// `DONE` row has no further use for. `dedup_key` is untouched.
	///
	/// Only a `RUNNING` row is touched, and the count of rows changed comes back: `0` means
	/// `job_cancel` flipped the row while the handler was still in flight, and the caller must
	/// leave it cancelled rather than resurrect it.
	async fn job_complete(&self, id: i64, now: Timestamp) -> ClResult<u64>;

	/// Back to `PENDING` at `run_at` with **no failure recorded**: `last_error` and `err_code`
	/// are cleared, so the row does not answer [`Self::job_retrying_kinds`] and does not feed
	/// `A-JOB-STALE`. `attempts` is left alone — a deferral is still an execution.
	///
	/// `RUNNING`-guarded and row-counted like [`Self::job_complete`]: `0` means an operator
	/// cancelled the row while the handler ran, and it must stay cancelled.
	async fn job_defer(&self, id: i64, run_at: Timestamp) -> ClResult<u64>;

	/// Pull the `PENDING` rows keyed by any of `dedup_keys` forward to `now`, for a deferral
	/// whose reason just went away. A row already due, running or finished is left alone.
	/// Returns how many moved.
	async fn job_wake(&self, dedup_keys: &[String], now: Timestamp) -> ClResult<u64>;

	/// Back to `PENDING` at `run_at`, recording `err` and the stable `errCode` behind it
	/// (`None` when no `Error` stands behind the failure). The backoff is computed by the
	/// caller.
	///
	/// `RUNNING`-guarded and counted like [`CoreStore::job_complete`]: `0` rows means the job
	/// was cancelled while running, and it must not be rescheduled.
	async fn job_fail(
		&self,
		id: i64,
		run_at: Timestamp,
		err: &str,
		err_code: Option<&str>,
	) -> ClResult<u64>;

	/// Terminal failure: `FAILED`, recording `err` and its `errCode`. **`dedup_key` is kept** —
	/// it is a permanent idempotency record, so "an invoice is never reported to NAV twice"
	/// holds across a terminal failure too; re-driving such a job means resetting that row
	/// ([`Self::job_redrive`]).
	///
	/// `RUNNING`-guarded and counted like [`Self::job_complete`] and [`Self::job_fail`], and for
	/// the same reason: without the guard this overwrote the `last_error` and `err_code` an
	/// operator's [`Self::job_cancel`] had just written, and the `A-JOB-FAILED` alert then
	/// reported the handler's dying words instead of "cancelled by an operator". `0` rows means
	/// exactly that happened, and the caller must leave the row as the operator left it.
	async fn job_terminate(
		&self,
		id: i64,
		now: Timestamp,
		err: &str,
		err_code: Option<&str>,
	) -> ClResult<u64>;

	/// Put a `FAILED` job of `kind` carrying this exact `payload` back to `PENDING` at `now`,
	/// with its attempt count and its recorded failure cleared; returns how many rows moved.
	///
	/// The operator re-drive, and the counterpart to [`Self::job_cancel`]. Addressed by
	/// `(kind, payload)` for the same reason, and **`dedup_key` is left untouched**, which is
	/// the whole point: the row keeps its identity, so one invoice still has exactly one job of
	/// a kind. Enqueuing a *second* row under a fresh key would re-drive it just as well and is
	/// what `Nav::submit` used to do — at the price of two workers able to claim two rows for
	/// one invoice and both POST `manageInvoice`, which only NAV's own `requestId` dedup then
	/// stopped.
	async fn job_redrive(&self, kind: &str, payload: &str, now: Timestamp) -> ClResult<u64>;

	/// [`Self::job_redrive`] for a job that finished **`DONE`**, so a spent `dedup_key` can be
	/// re-run. Returns how many rows moved.
	///
	/// A re-run-after-success is a double execution by definition, so a caller must first prove
	/// the earlier run settled nothing — `saas-nav`'s reconciliation is the one caller, and it
	/// checks the batch's leader row before asking. Do not reach for this to "retry a job".
	///
	/// Addressed by the unique `dedup_key` and re-supplying `payload`, not by `(kind, payload)`
	/// like [`Self::job_redrive`]: `job_complete` blanks `payload`, so a `DONE` row cannot be
	/// found by it and comes back with nothing for the handler to read.
	async fn job_redrive_done(
		&self,
		dedup_key: &str,
		payload: &str,
		now: Timestamp,
	) -> ClResult<u64>;

	/// Terminate every job of `kind` carrying this exact `payload` that can still run
	/// (`PENDING` or `RUNNING`); returns how many. The operator cancel:
	/// [`crate::job::Runner::terminate`] takes a job id and nothing hands one out, so a
	/// service method that wants to stop a job can only address it by what it was enqueued
	/// with. `(kind, payload)` is what `idx_job_kind_payload` indexes.
	///
	/// `dedup_key` is kept, exactly as [`Self::job_terminate`] keeps it: a cancelled filing
	/// must not become a second filing when something replays the enqueue.
	///
	/// **Cancelling a periodic kind stops the chain.** A `RUNNING` occurrence is handled —
	/// `Runner::complete` and `Runner::fail` both check the row before minting a successor — but
	/// a `PENDING` one leaves nothing to mint one from, and `job::seed_periodic` runs only at
	/// boot, so the next process start is the only revival. There is no `PENDING` row left to
	/// notice missing either; that is the price of addressing a job by `(kind, payload)`.
	async fn job_cancel(
		&self,
		kind: &str,
		payload: &str,
		now: Timestamp,
		err: &str,
		err_code: Option<&str>,
	) -> ClResult<u64>;

	/// Put every `RUNNING` row claimed before `before` back to `PENDING`; returns how many.
	/// Boot only, before any worker claims.
	///
	/// The cutoff is the lease: without it a second process starting during a rolling deploy
	/// stole the live rows of the process it was replacing and both ran the same handler. A row
	/// with no `claimed_at` predates the column and is always reclaimed.
	async fn job_reclaim(&self, before: Timestamp) -> ClResult<u64>;

	/// Delete `DONE` and `FAILED` rows finished before `cutoff` whose `dedup_key` is either
	/// absent or a [`crate::job::PERIODIC_KEY_PREFIX`] token; returns how many. Nothing else
	/// bounds this table, and `issued_without_document` scans it.
	///
	/// The `dedup_key` restriction is load-bearing, not a nicety: a handler-supplied key
	/// survives `DONE` on purpose, and that is the whole of "an invoice is never reported to
	/// NAV twice". Deleting such a row releases its key, so a replayed enqueue would file the
	/// same invoice a second time. `NULL` means no dedup guarantee was ever claimed, and a
	/// `periodic:` key is a scheduling token with a lifetime of one period — every framework
	/// period is at most a day and `jobs.retention_days` is at least one, so a reclaimed one
	/// can never collide with a live successor.
	async fn job_sweep(&self, cutoff: Timestamp) -> ClResult<u64>;

	/// One `(status, count, oldest created_at, newest transition)` row per status actually
	/// present. Feeds [`crate::alert::alerts`]'s `A-JOB-FAILED` and `A-JOB-BACKLOG`, and the
	/// `jobs` counter block of `GET /api/admin/stats`; one grouped read rather than a query
	/// per status.
	///
	/// The fourth column is the newest *transition* into the status — the newest
	/// `COALESCE(done_at, created_at)`, not the newest enqueue — and is what age-bounds
	/// `A-JOB-FAILED`. `MAX(created_at)` hid a filing cancelled today on a job enqueued last
	/// week; the coalesce is because `done_at` is NULL for `PENDING`/`RUNNING`.
	async fn job_status_counts(&self) -> ClResult<Vec<(String, i64, Timestamp, Timestamp)>>;

	/// The distinct kinds holding at least one job that is `PENDING` **after an attempt that
	/// reached a verdict** — i.e. currently in backoff. Bounded by the number of job kinds,
	/// and normally empty; it exists because the staleness threshold is per kind
	/// (`settings['jobs.alert_after.<KIND>']`), which SQL cannot join against.
	///
	/// That test is `last_error IS NOT NULL`, **not** `attempts > 0`: [`Self::job_claim`]
	/// increments `attempts` before the handler runs and [`Self::job_reclaim`] returns a
	/// crashed `RUNNING` job to `PENDING` without resetting it, so a restart made a job that
	/// had never failed raise `A-JOB-STALE`. Every failure path writes `last_error`;
	/// [`Self::job_complete`] and [`Self::job_redrive`] clear it.
	async fn job_retrying_kinds(&self) -> ClResult<Vec<String>>;

	/// `(count, oldest created_at)` of `kind`'s jobs that are `PENDING`, have already failed
	/// at least once (the same `last_error IS NOT NULL` test as [`Self::job_retrying_kinds`],
	/// which the two must agree on), and were enqueued before `before`. `None` when there are
	/// none.
	async fn job_stale(&self, kind: &str, before: Timestamp) -> ClResult<Option<(i64, Timestamp)>>;

	// ---- vars ----------------------------------------------------------------------

	/// A framework-internal scalar out of `vars`, or `None` when nothing has written it.
	/// Not settings: `vars` is the framework's own scratch space, never operator-editable.
	async fn var_get(&self, name: &str) -> ClResult<Option<String>>;

	/// Writes one `vars` row, inserting or replacing. [`crate::alert::sweep`] holds the
	/// previous sweep's computed set here, which is what makes alerting state-free.
	async fn var_set(&self, name: &str, value: &str) -> ClResult<()>;

	// ---- health --------------------------------------------------------------------

	/// The applied schema version, `0` when nothing has been migrated. An `Err` is `/readyz`'s
	/// `db: "fail"`.
	async fn db_version(&self) -> ClResult<i64>;

	// ---- auth middleware -----------------------------------------------------------

	/// The account behind a token's `sub`, or `None` when the uid is unknown.
	async fn account_for_token(&self, uid: &str) -> ClResult<Option<TokenAccount>>;

	/// The API key behind a key's 8-char prefix, with the account, org and membership columns
	/// the caller re-reads on every request. The only key read: [`crate::auth_mw`] verifies
	/// with it and `AuthStore::create_api_key` / `AuthStore::api_keys_for_org` return the same
	/// shape.
	///
	/// The stored hash is returned, not checked: the caller compares it, so a second store
	/// adapter is not the one deciding what constant-time means. `None` when the prefix is
	/// unknown — indistinguishable from a wrong hash on purpose.
	async fn api_key_by_prefix(&self, prefix: &str) -> ClResult<Option<ApiKey>>;

	/// Stamp `last_used_at`. The caller fires it only when the row it already read is stale,
	/// so a busy key does not write once per request against the single writer connection.
	async fn touch_api_key(&self, id: i64, at: Timestamp) -> ClResult<()>;

	/// The org's internal id and the caller's **effective** role on it — the highest role
	/// held on the org itself or on any ancestor of it. `None` when the org is unknown, not
	/// `ACTIVE`, or reached by no accepted membership. That join is the authorization check —
	/// a removed member or a suspended org must lose access without waiting out the token.
	///
	/// One call, returning both, because `auth_mw::verify` runs it on every authenticated
	/// request and must not grow a second round trip.
	async fn org_membership_role(
		&self,
		account_id: i64,
		org_uid: &str,
	) -> ClResult<Option<(i64, Role)>>;

	/// The same ancestor walk keyed by internal id, for [`crate::auth_mw::require_role_on`]
	/// on a route that already resolved the org.
	async fn org_role(&self, account_id: i64, org_id: i64) -> ClResult<Option<Role>>;

	/// The root org — the one row with `kind = 'ROOT'`, which
	/// [`crate::auth_mw::require_operator`] gates on. Never a hardcoded id.
	async fn root_org_id(&self) -> ClResult<i64>;
}

// vim: ts=4
