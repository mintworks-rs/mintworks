// SPDX-License-Identifier: MPL-2.0
//! One ambient transaction on the script database — what `db::tx` opens.
//!
//! Plain `READ COMMITTED`, no global lock and no automatic re-run: the block may have side
//! effects (`http::`), so a serialization failure or deadlock surfaces as a retryable
//! `E-CORE-UNAVAILABLE` and the script decides. Scripts lock rows with `SELECT … FOR UPDATE`.

use std::sync::Arc;

use mintworks_core::error::ClResult;
use sqlx::{PgConnection, PgPool, Postgres, pool::PoolConnection};
use tokio::sync::Mutex;

use crate::util::DbExt;

/// The block's connection, shared with the statements running inside it. One invocation is one VM
/// on one task, so a task-local *is* the block's scope.
pub(crate) type Held = Arc<Mutex<PoolConnection<Postgres>>>;

tokio::task_local! {
	static OPEN: Held;
}

/// The open block's connection, for a `query` or `exec` on this task — so a read inside a block
/// sees the block's own uncommitted writes.
pub(crate) fn ambient() -> Option<Held> {
	OPEN.try_with(Arc::clone).ok()
}

/// Runs one statement of the open block under a savepoint: an error PostgreSQL raises aborts the
/// whole transaction (25P02), so a script that catches it could otherwise run nothing more.
pub(crate) async fn statement<T>(
	conn: &mut PgConnection,
	run: impl AsyncFnOnce(&mut PgConnection) -> ClResult<T>,
) -> ClResult<T> {
	sqlx::query("SAVEPOINT mintworks_stmt").execute(&mut *conn).await.db()?;
	let out = run(&mut *conn).await;
	if out.is_err() {
		sqlx::query("ROLLBACK TO SAVEPOINT mintworks_stmt")
			.execute(&mut *conn)
			.await
			.db()?;
	}
	sqlx::query("RELEASE SAVEPOINT mintworks_stmt").execute(&mut *conn).await.db()?;
	out
}

/// Holds the connection until the block finishes one way or the other.
///
/// PostgreSQL rolls an open transaction back when the session ends, so closing the connection
/// of a **cancelled** `transaction` future never re-pools it mid-transaction.
struct Guard(Option<Held>);

impl Guard {
	fn finished(&mut self) {
		self.0 = None;
	}
}

impl Drop for Guard {
	fn drop(&mut self) {
		let Some(held) = self.0.take() else { return };
		if let Ok(mut conn) = held.try_lock() {
			conn.close_on_drop();
		} else if let Ok(rt) = tokio::runtime::Handle::try_current() {
			// A clone still holds the lock (an in-flight statement): close once it lets go, rather
			// than re-pool a connection with the transaction open.
			rt.spawn(async move { held.lock().await.close_on_drop() });
		}
	}
}

/// Runs `body` inside `BEGIN ISOLATION LEVEL READ COMMITTED`, committing on `Ok` and rolling
/// back on `Err`.
pub(crate) async fn run(
	writer: &PgPool,
	body: mintworks_script::TxBody<'_>,
) -> ClResult<serde_json::Value> {
	if ambient().is_some() {
		// One level only; the fix in the script is to drop the inner block.
		return Err(mintworks_script::error::db("a db::tx block is already open"));
	}
	let mut conn = writer.acquire().await.db()?;
	// Spelled out: the server's `default_transaction_isolation` may say otherwise.
	sqlx::query("BEGIN ISOLATION LEVEL READ COMMITTED")
		.execute(&mut *conn)
		.await
		.db()?;

	let held: Held = Arc::new(Mutex::new(conn));
	let mut guard = Guard(Some(Arc::clone(&held)));
	let out = OPEN.scope(Arc::clone(&held), body).await;

	let mut conn = held.lock().await;
	let finish = match &out {
		Ok(_) => "COMMIT",
		Err(_) => "ROLLBACK",
	};
	let done = sqlx::query(finish).execute(&mut **conn).await;
	// A failed COMMIT/ROLLBACK leaves the session state unknown; never re-pool it.
	if done.is_err() {
		conn.close_on_drop();
	}
	guard.finished();
	match out {
		Ok(v) => done.db().map(|_| v),
		// `e` is the answer, not the rollback's own failure: losing the block's errCode to a
		// secondary fault turns a 409 the client handles into an opaque 500.
		Err(e) => {
			if let Err(fault) = done {
				tracing::error!(error = %fault, "rolling back a db::tx block failed");
			}
			Err(e)
		}
	}
}

// vim: ts=4
