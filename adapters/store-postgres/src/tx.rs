//! Write transactions, re-entrant through the store handle or through the task — the model of
//! `adapters/store-sqlite/src/tx.rs`, with one global advisory lock standing in for SQLite's single
//! writer connection.
//!
//! By **handle**: [`crate::PgStore::begin`] returns a handle bound to the transaction, and a write
//! through the original handle takes its own. By **task**: [`crate::PgStore::scope_writes`] joins
//! every *pooled* handle to one transaction for the duration of a future. A bound handle always
//! wins over the ambient scope. A nested write on either opens a `SAVEPOINT` on the transaction's
//! own connection — never a second `BEGIN`, which would wait on [`WRITE_LOCK`] against itself.
//!
//! No audit buffer: `audit_detached` writes on a separate pooled writer connection at once, which
//! the contract permits for an engine with more than one writer connection.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use mintworks_core::prelude::*;
use mintworks_core::store::AuditEntry;
use sqlx::pool::PoolConnection;
use sqlx::{AssertSqlSafe, PgConnection, PgPool, Postgres};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

use crate::ACQUIRE_TIMEOUT;
use crate::util::DbExt;

/// The `pg_advisory_xact_lock` key every outermost write transaction takes. ASCII `saaswrit`.
pub(crate) const WRITE_LOCK: i64 = 0x7361_6173_7772_6974;

/// How long a statement in a write transaction waits for a row lock before failing `55P03`,
/// which `map_db` reports retryable — SQLite's `BUSY_TIMEOUT`, same figure.
const LOCK_TIMEOUT: &str = "5s";

/// How long a write transaction waits for [`WRITE_LOCK`] itself: SQLite's 30 s writer-pool acquire,
/// since both are a wait for the one writer.
const WRITE_LOCK_TIMEOUT: &str = "30s";

/// Where a handle's writes go: the pool, in autocommit, or a transaction it is bound to.
///
/// `Weak`, because a bound clone parked in an `AppState` must not keep the transaction's
/// connection — and the advisory lock it holds — alive past the [`WriteTx`].
#[derive(Clone)]
pub(crate) enum ConnSource {
	Pool,
	Held(Weak<HeldTx>),
}

impl std::fmt::Debug for ConnSource {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Pool => f.write_str("ConnSource::Pool"),
			Self::Held(_) => f.write_str("ConnSource::Held"),
		}
	}
}

impl ConnSource {
	/// The scope this handle is bound to, else its task's ambient scope, else `None`.
	///
	/// # Errors
	/// `Error::Internal` when the handle is bound to a transaction that has already ended.
	pub(crate) fn scope(&self) -> ClResult<Option<Arc<HeldTx>>> {
		match self {
			Self::Pool => Ok(ambient()),
			Self::Held(weak) => weak.upgrade().map(Some).ok_or_else(stale),
		}
	}
}

tokio::task_local! {
	/// The transaction the current task runs inside. A task-local, not a thread-local: an async
	/// task migrates between threads at every await point on a multi-thread runtime.
	static AMBIENT: Arc<HeldTx>;
}

fn ambient() -> Option<Arc<HeldTx>> {
	AMBIENT.try_with(Arc::clone).ok()
}

/// Backs [`crate::PgStore::scope_writes`]; [`HeldTx`] is `pub(crate)`, so the scope can only be
/// established from inside this crate.
pub(crate) async fn scoped<T>(tx: &WriteTx, fut: impl Future<Output = T>) -> T {
	AMBIENT.scope(Arc::clone(&tx.held), fut).await
}

/// What a handle still pointing at a finished transaction raises. The text is asserted by the
/// shared `tx` conformance module.
fn stale() -> Error {
	Error::internal("bound handle used after its transaction ended")
}

/// State belonging to the whole transaction chain, owned by its outermost scope.
struct Root {
	/// Locked for one statement at a time: a guard alive across a re-entrant call makes that
	/// call wait out [`ACQUIRE_TIMEOUT`] and fail.
	conn: Arc<AsyncMutex<PoolConnection<Postgres>>>,
	/// Savepoint names must nest and stay unique on the connection, so the counter only moves on.
	savepoints: AtomicU64,
	/// The depths of every scope currently open, outermost (`0`) first. A `begin` is legal only
	/// on the scope at the top: a second child opened under a live one would `ROLLBACK TO` over
	/// its sibling's statements.
	open: Mutex<Vec<u64>>,
	/// Savepoints a dropped [`WriteTx`] left open, undone before the next statement rather than
	/// from `Drop`: a rollback landing midway through that statement would undo it too.
	pending: Mutex<Vec<(u64, String)>>,
	/// The process's write gate, released when the last scope lets go of the root.
	_permit: OwnedSemaphorePermit,
}

/// One scope of a write transaction: the outermost one, or a savepoint inside it.
pub(crate) struct HeldTx {
	root: Arc<Root>,
	/// This scope's savepoint sequence number plus one; `0` is the outermost transaction.
	depth: u64,
}

impl HeldTx {
	/// Whether this scope has ended — itself or through an enclosing scope's close.
	fn ended(&self) -> bool {
		!self
			.root
			.open
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.contains(&self.depth)
	}

	/// Locks the connection, first draining any savepoint a dropped transaction left open.
	///
	/// # Errors
	/// `Error::Internal` when a stale bound handle writes after its transaction has ended, and
	/// when undoing a savepoint a dropped transaction left open fails.
	pub(crate) async fn lock_conn(&self) -> ClResult<OwnedMutexGuard<PoolConnection<Postgres>>> {
		if self.ended() {
			return Err(stale());
		}
		let mut conn = self.lock_conn_raw().await?;
		let mut pending = {
			let mut held =
				self.root.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			std::mem::take(&mut *held)
		};
		// Innermost last, then popped: rolling back to an outer savepoint frees the inner names
		// too, and `ROLLBACK TO` a freed name aborts whatever statement asks for it next.
		pending.sort_unstable_by_key(|(depth, _)| *depth);
		while let Some((depth, name)) = pending.pop() {
			if let Err(err) = undo_savepoint(&mut conn, &name).await {
				let mut held =
					self.root.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
				held.push((depth, name));
				held.append(&mut pending);
				return Err(err);
			}
		}
		Ok(conn)
	}

	/// The connection alone: no stale check and no pending drain.
	///
	/// # Errors
	/// `Error::Unavailable` when the connection is still held after [`ACQUIRE_TIMEOUT`].
	async fn lock_conn_raw(&self) -> ClResult<OwnedMutexGuard<PoolConnection<Postgres>>> {
		let conn = self.root.conn.clone().lock_owned();
		tokio::time::timeout(ACQUIRE_TIMEOUT, conn).await.map_err(|_| {
			Error::Unavailable(
				"the bound transaction's connection is still held — a guard is alive across a \
				 re-entrant call"
					.to_owned(),
			)
		})
	}

	/// Marks this scope finished, and every scope nested inside it.
	fn close(&self) {
		let mut open = self.root.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		if let Some(pos) = open.iter().position(|d| *d == self.depth) {
			open.truncate(pos);
		}
		drop(open);
		self.root
			.pending
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.retain(|(depth, _)| *depth <= self.depth);
	}
}

/// `ROLLBACK TO` then `RELEASE`, as two statements: the extended protocol rejects a
/// multi-statement string, and the savepoint must be freed for its name to stay unique.
async fn undo_savepoint(conn: &mut PgConnection, name: &str) -> ClResult<()> {
	// Savepoint names are ours (`mintworks_sp_{n}` from a counter), never user data.
	sqlx::query(AssertSqlSafe(format!("ROLLBACK TO {name}")))
		.execute(&mut *conn)
		.await
		.db()?;
	sqlx::query(AssertSqlSafe(format!("RELEASE {name}"))).execute(conn).await.db()?;
	Ok(())
}

/// One `audit_logs` insert, for `audit_log` (on the caller's connection) and `audit_detached`
/// (on its own pooled writer connection).
pub(crate) async fn insert_audit(conn: &mut PgConnection, entry: &AuditEntry) -> ClResult<()> {
	sqlx::query(
		"INSERT INTO audit_logs
		 (at, account_id, org_id, ip, entity, entity_id, action, detail, request_id)
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
	)
	.bind(entry.at.0)
	.bind(entry.account_id)
	.bind(entry.org_id)
	.bind(entry.ip.as_deref())
	.bind(entry.entity.as_str())
	.bind(entry.entity_id.as_deref())
	.bind(entry.action.as_str())
	.bind(entry.detail.as_deref())
	.bind(entry.request_id.as_deref())
	.execute(conn)
	.await
	.db()?;
	Ok(())
}

/// Opens a write transaction: a pooled handle leases a writer connection, begins and takes
/// [`WRITE_LOCK`]; a bound one marks a `SAVEPOINT` in the transaction it is already bound to.
pub(crate) async fn begin(
	source: &ConnSource,
	pool: &PgPool,
	gate: &Arc<Semaphore>,
) -> ClResult<WriteTx> {
	if let Some(parent) = source.scope()? {
		let seq = parent.root.savepoints.fetch_add(1, Ordering::Relaxed);
		let name = format!("mintworks_sp_{seq}");
		{
			let mut open =
				parent.root.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			// LIFO or nothing: a second child opened while the first is live would `ROLLBACK TO`
			// over its sibling's statements. The text is asserted by the shared `tx` module.
			if open.last() != Some(&parent.depth) {
				return Err(if open.contains(&parent.depth) {
					Error::internal("a nested write_tx is already open on this handle")
				} else {
					stale()
				});
			}
			open.push(seq + 1);
		}
		let opened = async {
			let mut guard = parent.lock_conn().await?;
			sqlx::query(AssertSqlSafe(format!("SAVEPOINT {name}")))
				.execute(&mut **guard)
				.await
				.db()
		}
		.await;
		if let Err(err) = opened {
			// No scope was opened, so nothing will `close()` this entry back off.
			let mut open =
				parent.root.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			open.retain(|d| *d != seq + 1);
			return Err(err);
		}
		let held = Arc::new(HeldTx { root: parent.root.clone(), depth: seq + 1 });
		return Ok(WriteTx { held, savepoint: Some(name), finished: false });
	}
	// Gate before `acquire`: waiters on the advisory lock would each hold a writer connection,
	// starving the holder's own autocommit writes and `job_claim` of the pool.
	let permit = tokio::time::timeout(ACQUIRE_TIMEOUT, gate.clone().acquire_owned())
		.await
		.map_err(|_| Error::Unavailable("database pool acquire timed out".to_owned()))?
		.map_err(|_| Error::internal("write gate closed"))?;
	let mut conn = pool.acquire().await.db()?;
	// One write transaction at a time DB-wide (the `BEGIN IMMEDIATE` equivalent, across
	// processes), so every "one writer" guarantee holds unchanged; per-site row locks if throughput matters.
	let sql = format!(
		"BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL lock_timeout = '{WRITE_LOCK_TIMEOUT}'; \
		 SELECT pg_advisory_xact_lock({WRITE_LOCK}); SET LOCAL lock_timeout = '{LOCK_TIMEOUT}'"
	);
	if let Err(err) = sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut *conn).await {
		// The session may be inside an aborted transaction; closing it rolls that back server-side.
		conn.close_on_drop();
		return Err(crate::util::map_db(&err));
	}
	let held = Arc::new(HeldTx {
		root: Arc::new(Root {
			conn: Arc::new(AsyncMutex::new(conn)),
			savepoints: AtomicU64::new(0),
			open: Mutex::new(vec![0]),
			pending: Mutex::new(Vec::new()),
			_permit: permit,
		}),
		depth: 0,
	});
	Ok(WriteTx { held, savepoint: None, finished: false })
}

/// A connection borrowed for the statements about to run.
pub enum ConnGuard {
	/// The bound transaction's own.
	Held(OwnedMutexGuard<PoolConnection<Postgres>>),
	/// Leased from a pool, for a handle with no transaction bound.
	Pooled(PoolConnection<Postgres>),
}

impl ConnGuard {
	fn leased(&self) -> &PoolConnection<Postgres> {
		match self {
			Self::Held(guard) => guard,
			Self::Pooled(conn) => conn,
		}
	}

	fn leased_mut(&mut self) -> &mut PoolConnection<Postgres> {
		match self {
			Self::Held(guard) => guard,
			Self::Pooled(conn) => conn,
		}
	}
}

impl Deref for ConnGuard {
	type Target = PgConnection;

	fn deref(&self) -> &PgConnection {
		self.leased()
	}
}

impl DerefMut for ConnGuard {
	fn deref_mut(&mut self) -> &mut PgConnection {
		self.leased_mut()
	}
}

/// An open write transaction, outermost or nested.
///
/// Take a connection from [`lock`](Self::lock) for the statement it runs and no longer: while
/// one is alive it is out of the held transaction's slot, so a later statement has to wait.
pub struct WriteTx {
	held: Arc<HeldTx>,
	/// `None` on the outermost transaction, where `commit` sends `COMMIT`, and `Some` on a nested
	/// one, where it sends `RELEASE` and leaves the outer transaction open.
	savepoint: Option<String>,
	finished: bool,
}

impl WriteTx {
	/// What a handle bound to this transaction points at, for [`crate::PgStore::begin`].
	pub(crate) fn held(&self) -> Weak<HeldTx> {
		Arc::downgrade(&self.held)
	}

	/// The connection, for the statement about to run.
	///
	/// # Errors
	/// `Error::Internal` when undoing a savepoint a dropped transaction left open fails.
	pub async fn lock(&self) -> ClResult<ConnGuard> {
		Ok(ConnGuard::Held(self.held.lock_conn().await?))
	}

	/// Commits, releasing the savepoint alone when nested.
	///
	/// # Errors
	/// `Error::Internal` when the `COMMIT` or `RELEASE` fails; the transaction is left open for
	/// [`rollback`](Self::rollback), or for the drop below.
	pub async fn commit(mut self) -> ClResult<()> {
		let mut conn = self.held.lock_conn().await?;
		let sql = match &self.savepoint {
			Some(name) => format!("RELEASE {name}"),
			None => "COMMIT".to_string(),
		};
		sqlx::query(AssertSqlSafe(sql)).execute(&mut **conn).await.db()?;
		self.finished = true;
		self.held.close();
		Ok(())
	}

	/// Rolls back, undoing the savepoint's own statements alone when nested.
	///
	/// # Errors
	/// `Error::Internal` when the `ROLLBACK` fails.
	pub async fn rollback(mut self) -> ClResult<()> {
		let mut conn = self.held.lock_conn().await?;
		match &self.savepoint {
			Some(name) => undo_savepoint(&mut conn, name).await?,
			None => sqlx::query("ROLLBACK").execute(&mut **conn).await.db().map(drop)?,
		}
		self.finished = true;
		self.held.close();
		Ok(())
	}
}

impl Drop for WriteTx {
	fn drop(&mut self) {
		if self.finished {
			return;
		}
		let held = self.held.clone();
		if let Some(name) = self.savepoint.clone() {
			// Recorded only while the enclosing scope is still open: an enclosing `RELEASE` frees
			// every savepoint inside it, and `ROLLBACK TO` a freed name aborts the next statement.
			if !held.ended() {
				held.root
					.pending
					.lock()
					.unwrap_or_else(std::sync::PoisonError::into_inner)
					.push((held.depth, name));
			}
			held.close();
			return;
		}
		// Closed here, so a bound clone stops passing `ended()` at once.
		held.close();
		abandon(&held);
	}
}

/// Rolls back a dropped outermost transaction and re-pools its connection. Rollback, not close:
/// an early return before `commit` is the normal path, and each would pay a reconnect.
fn abandon(held: &HeldTx) {
	let root = held.root.clone();
	match tokio::runtime::Handle::try_current() {
		// The task owns `root`, and so the write gate's permit: the next writer waits for the rollback.
		Ok(handle) => {
			handle.spawn(async move {
				let mut conn = root.conn.clone().lock_owned().await;
				if sqlx::query("ROLLBACK").execute(&mut **conn).await.is_err() {
					conn.close_on_drop();
				}
			});
		}
		// Nowhere to await a rollback: ending the session rolls back and frees `WRITE_LOCK`.
		Err(error) => {
			if let Ok(mut conn) = root.conn.clone().try_lock_owned() {
				conn.close_on_drop();
			} else {
				tracing::error!(?error, "an open transaction's connection was left held");
			}
		}
	}
}

// vim: ts=4
