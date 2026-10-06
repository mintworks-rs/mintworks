//! Write transactions, re-entrant through the store handle or through the task.
//!
//! The writer pool holds one connection, so a write that must run inside a transaction has to
//! reuse the connection that transaction already holds. Two mechanisms say which.
//! By **handle**: [`crate::SqliteStore::begin`] returns a handle bound to the transaction, and a
//! write through the original handle takes its own. By **task**:
//! [`crate::SqliteStore::scope_writes`] joins every *pooled* handle to one transaction for the
//! duration of a future, for the caller that cannot hand a bound handle to what runs inside it.
//! A bound handle always wins over the ambient scope.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use mintworks_core::prelude::*;
use mintworks_core::store::AuditEntry;
use sqlx::pool::PoolConnection;
use sqlx::{AssertSqlSafe, Sqlite, SqliteConnection, SqlitePool};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::ACQUIRE_TIMEOUT;
use crate::util::DbExt;

/// Where a handle's writes go: the pool, in autocommit, or a transaction it is bound to.
///
/// `Weak`, because the bound handle must not keep the single writer connection checked out: the
/// [`WriteTx`] holds the only strong reference, so ending the transaction frees the connection
/// even where a bound clone is still parked in an `AppState`.
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
			// A bound handle wins: it names its transaction, while the ambient scope is only the
			// fallback for a caller that could not rebind its handles.
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

/// Backs [`crate::SqliteStore::scope_writes`], which is the public name for it: [`HeldTx`] is
/// `pub(crate)`, so the scope can only be established from inside this crate.
pub(crate) async fn scoped<T>(tx: &WriteTx, fut: impl Future<Output = T>) -> T {
	AMBIENT.scope(Arc::clone(&tx.held), fut).await
}

/// What a handle still pointing at a finished transaction raises, from both paths that reach one:
/// the failed upgrade, and a depth no longer on the open stack.
fn stale() -> Error {
	Error::internal("bound handle used after its transaction ended")
}

/// State belonging to the whole transaction chain, owned by its outermost scope.
struct Root {
	/// Locked for one statement at a time: a guard alive across a re-entrant call makes that
	/// call wait out [`ACQUIRE_TIMEOUT`] and fail.
	conn: Arc<AsyncMutex<PoolConnection<Sqlite>>>,
	/// Savepoint names must nest and stay unique on the connection, so the counter only moves on.
	savepoints: AtomicU64,
	/// The depths of every scope currently open, outermost (`0`) first. A `begin` is legal only
	/// on the scope at the top: a savepoint is a stack, so a second child opened under a live
	/// one would `ROLLBACK TO` over its sibling's statements.
	open: Mutex<Vec<u64>>,
	/// Savepoints a dropped [`WriteTx`] left open, undone before the next statement rather than
	/// from `Drop`: a rollback landing midway through that statement would undo it too.
	pending: Mutex<Vec<(u64, String)>>,
	/// Audit rows raised while this transaction is open: a rollback must not take evidence that
	/// the attempt happened along with the attempt.
	audit: Mutex<Vec<AuditEntry>>,
}

/// One scope of a write transaction: the outermost one, or a savepoint inside it.
pub(crate) struct HeldTx {
	root: Arc<Root>,
	/// This scope's savepoint sequence number plus one; `0` is the outermost transaction. An
	/// issue order, not a nesting level, which is how SQLite stacks savepoints too.
	depth: u64,
}

impl HeldTx {
	/// Whether this scope has ended. A scope that closed took every scope nested inside it with
	/// it, so an ancestor's close answers here too.
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
	pub(crate) async fn lock_conn(&self) -> ClResult<OwnedMutexGuard<PoolConnection<Sqlite>>> {
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
			// Savepoint names are ours (`mintworks_sp_{n}` from a counter), never user data.
			let sql = format!("ROLLBACK TO {name}; RELEASE {name}");
			if let Err(err) = sqlx::query(AssertSqlSafe(sql)).execute(&mut **conn).await.db() {
				// The failing entry and the ones still behind it go back: dropped, they would
				// leave statements standing inside a scope that was meant to be undone.
				let mut held =
					self.root.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
				held.push((depth, name));
				held.append(&mut pending);
				return Err(err);
			}
		}
		Ok(conn)
	}

	/// The connection alone: no stale check and no pending drain, for the drop path that has to
	/// reach a connection whose scope is already closed.
	///
	/// # Errors
	/// `Error::Unavailable` when the connection is still held after [`ACQUIRE_TIMEOUT`].
	async fn lock_conn_raw(&self) -> ClResult<OwnedMutexGuard<PoolConnection<Sqlite>>> {
		let conn = self.root.conn.clone().lock_owned();
		tokio::time::timeout(ACQUIRE_TIMEOUT, conn).await.map_err(|_| {
			Error::Unavailable(
				"the bound transaction's connection is still held — a guard is alive across a \
				 re-entrant call"
					.to_owned(),
			)
		})
	}

	/// Marks this scope finished, and every scope nested inside it: the `RELEASE` or
	/// `ROLLBACK TO` that closed this one freed their savepoints too.
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

	/// Buffers one row, written once the transaction it was raised in has ended.
	pub(crate) fn buffer_audit(&self, entry: AuditEntry) {
		self.root
			.audit
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.push(entry);
	}

	/// The buffered rows, taken out — emptied, so a path that logs them instead of writing them
	/// cannot leave them for a later flush to write twice.
	fn take_audit(&self) -> Vec<AuditEntry> {
		let mut audit = self.root.audit.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		std::mem::take(&mut *audit)
	}

	/// Writes the buffered rows, in push order, on the connection the transaction held.
	///
	/// A failed insert is logged and swallowed: auditing is best-effort, so a lost row must not
	/// fail an operation that already succeeded.
	async fn flush_audit(&self, conn: &mut PoolConnection<Sqlite>) {
		for entry in self.take_audit() {
			if let Err(error) = insert_audit(conn, &entry).await {
				tracing::error!(?error, entity = %entry.entity, action = %entry.action, "audit log write failed");
			}
		}
	}
}

/// The last record of rows that can no longer be written. `audit_detached` promises the row
/// survives the transaction either way; on a connection that cannot take it, the guarantee
/// degrades to this line, so it carries every field but the IP, which reconciles nothing.
fn log_lost_audit(entries: Vec<AuditEntry>, reason: &str) {
	for entry in entries {
		tracing::error!(
			reason,
			entity = %entry.entity, entity_id = ?entry.entity_id, action = %entry.action,
			at = entry.at.0, account_id = ?entry.account_id, org_id = ?entry.org_id,
			detail = ?entry.detail,
			"detached audit row lost"
		);
	}
}

/// One `audit_logs` insert, shared by the immediate and the buffered write path.
pub(crate) async fn insert_audit(conn: &mut SqliteConnection, entry: &AuditEntry) -> ClResult<()> {
	sqlx::query(
		"INSERT INTO audit_logs
		 (at, account_id, org_id, ip, entity, entity_id, action, detail, request_id)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
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

/// Opens a write transaction: a pooled handle takes the writer pool's connection with
/// `BEGIN IMMEDIATE`; a bound one marks a `SAVEPOINT` in the transaction it is already bound to.
pub(crate) async fn begin(source: &ConnSource, pool: &SqlitePool) -> ClResult<WriteTx> {
	if let Some(parent) = source.scope()? {
		// The savepoint's sequence number is its depth: SQLite stacks savepoints in issue order,
		// so ordering by it is ordering by nesting even where two scopes share a parent.
		let seq = parent.root.savepoints.fetch_add(1, Ordering::Relaxed);
		let name = format!("mintworks_sp_{seq}");
		{
			let mut open =
				parent.root.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			// LIFO or nothing: a second child opened while the first is live would `ROLLBACK TO`
			// over its sibling's statements. Off the stack entirely means that scope has ended.
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
	let mut conn = pool.acquire().await.db()?;
	sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await.db()?;
	let held = Arc::new(HeldTx {
		root: Arc::new(Root {
			conn: Arc::new(AsyncMutex::new(conn)),
			savepoints: AtomicU64::new(0),
			open: Mutex::new(vec![0]),
			pending: Mutex::new(Vec::new()),
			audit: Mutex::new(Vec::new()),
		}),
		depth: 0,
	});
	Ok(WriteTx { held, savepoint: None, finished: false })
}

/// A connection borrowed for the statements about to run.
pub enum ConnGuard {
	/// The bound transaction's own.
	Held(OwnedMutexGuard<PoolConnection<Sqlite>>),
	/// Leased from a pool, for a handle with no transaction bound.
	Pooled(PoolConnection<Sqlite>),
}

impl ConnGuard {
	fn leased(&self) -> &PoolConnection<Sqlite> {
		match self {
			Self::Held(guard) => guard,
			Self::Pooled(conn) => conn,
		}
	}

	fn leased_mut(&mut self) -> &mut PoolConnection<Sqlite> {
		match self {
			Self::Held(guard) => guard,
			Self::Pooled(conn) => conn,
		}
	}
}

impl Deref for ConnGuard {
	type Target = SqliteConnection;

	fn deref(&self) -> &SqliteConnection {
		self.leased()
	}
}

impl DerefMut for ConnGuard {
	fn deref_mut(&mut self) -> &mut SqliteConnection {
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
	/// What a handle bound to this transaction points at, for [`crate::SqliteStore::begin`].
	/// Weak: the bound handle must not hold the connection open past this transaction.
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
		self.end(&mut conn).await;
		Ok(())
	}

	/// Rolls back, undoing the savepoint's own statements alone when nested.
	///
	/// # Errors
	/// `Error::Internal` when the `ROLLBACK` fails.
	pub async fn rollback(mut self) -> ClResult<()> {
		let mut conn = self.held.lock_conn().await?;
		let sql = match &self.savepoint {
			Some(name) => format!("ROLLBACK TO {name}; RELEASE {name}"),
			None => "ROLLBACK".to_string(),
		};
		sqlx::query(AssertSqlSafe(sql)).execute(&mut **conn).await.db()?;
		self.finished = true;
		self.end(&mut conn).await;
		Ok(())
	}

	/// Closes this scope, then flushes the buffered audit rows once the whole chain is over —
	/// they have to outlive every savepoint inside it, so only the outermost scope writes them.
	async fn end(&self, conn: &mut PoolConnection<Sqlite>) {
		// Closed before the flush awaits: a cancellation there must not leave the scope open.
		self.held.close();
		if self.held.depth == 0 {
			self.held.flush_audit(conn).await;
		}
	}
}

impl Drop for WriteTx {
	fn drop(&mut self) {
		if self.finished {
			if self.held.depth == 0 {
				// Cancelled between the commit and the flush. Empty on every normal path.
				log_lost_audit(
					self.held.take_audit(),
					"the transaction ended before its audit rows were flushed",
				);
			}
			return;
		}
		// A dropped transaction must not stay open: the next statement would run inside it, and
		// the outer rollback cannot await in `Drop`, so it goes to the runtime.
		let held = self.held.clone();
		if let Some(name) = self.savepoint.clone() {
			// Recorded only while the scope that opened it is still open: an enclosing `RELEASE`
			// frees every savepoint inside it, and `ROLLBACK TO` a freed name aborts whatever
			// unrelated statement drains it next.
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
		// Closed here, not in the spawned task: until it closes a bound clone passes `ended()` and
		// writes into a doomed transaction — or, once the rollback lands, in autocommit.
		held.close();
		match tokio::runtime::Handle::try_current() {
			Ok(handle) => {
				handle.spawn(async move {
					match held.lock_conn_raw().await {
						Ok(mut conn) => {
							if let Err(error) = sqlx::query("ROLLBACK").execute(&mut **conn).await {
								tracing::error!(
									?error,
									"rolling back a dropped write transaction failed"
								);
								// Handed back with the transaction still open, the connection fails
								// every later `BEGIN IMMEDIATE`, so it is closed instead of returned.
								conn.close_on_drop();
								// Not `abandon`: `try_lock_owned` would contend with itself here.
								log_lost_audit(
									held.take_audit(),
									"the transaction could not be rolled back",
								);
								return;
							}
							held.flush_audit(&mut conn).await;
						}
						Err(error) => {
							tracing::error!(
								?error,
								"a dropped write transaction could not be rolled back"
							);
							abandon(&held, "the transaction's connection was unreachable");
						}
					}
				});
			}
			// No runtime to send the rollback to. The connection still carries an open
			// transaction, which sqlx does not undo on release, so it is closed rather than returned.
			Err(error) => {
				tracing::error!(?error, "write transaction dropped outside a runtime");
				abandon(&held, "the transaction ended with no runtime");
			}
		}
	}
}

/// Keeps a connection that still carries an open transaction out of the pool, and turns the
/// audit rows it can no longer take into log lines.
fn abandon(held: &HeldTx, reason: &str) {
	match held.root.conn.clone().try_lock_owned() {
		Ok(mut conn) => conn.close_on_drop(),
		// Contended: returning here left the statement in flight to hand the one-connection
		// writer pool a connection with the transaction still open, wedging every later write.
		// The waiter condemns it on release; with no runtime there is nowhere to wait for one.
		Err(_) => match tokio::runtime::Handle::try_current() {
			Ok(handle) => {
				let conn = held.root.conn.clone();
				handle.spawn(async move { conn.lock_owned().await.close_on_drop() });
			}
			Err(error) => tracing::error!(?error, "an open transaction's connection was left held"),
		},
	}
	log_lost_audit(held.take_audit(), reason);
}

// vim: ts=4
