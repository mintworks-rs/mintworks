// SPDX-License-Identifier: MPL-2.0
//! One ambient transaction on the script database — what `db::tx` opens.
//!
//! Far smaller than the framework store's `tx.rs`, because there is exactly one entry point: no
//! audit buffering, and no second binding mode for a Rust consumer calling `begin()` by hand.

use std::sync::Arc;

use mintworks_core::error::ClResult;
use sqlx::{Sqlite, SqlitePool, pool::PoolConnection};
use tokio::sync::Mutex;

use crate::util::DbExt;

/// The block's connection, shared with the statements running inside it. One invocation is one VM
/// on one task, so a task-local *is* the block's scope.
type Held = Arc<Mutex<PoolConnection<Sqlite>>>;

tokio::task_local! {
	static OPEN: Held;
}

/// The open block's connection, for a `query` or `exec` on this task — so a read inside a block
/// sees the block's own uncommitted writes.
pub(crate) fn ambient() -> Option<Held> {
	OPEN.try_with(Arc::clone).ok()
}

/// Holds the connection until the block finishes one way or the other.
///
/// SQLite rolls an open transaction back when the connection closes, so a **cancelled**
/// `transaction` future can never hand the one-connection writer pool a connection with a
/// transaction still open. `min_connections(1)` re-creates it.
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

/// Runs `body` inside `BEGIN IMMEDIATE`, committing on `Ok` and rolling back on `Err`.
pub(crate) async fn run(
	writer: &SqlitePool,
	body: mintworks_script::TxBody<'_>,
) -> ClResult<serde_json::Value> {
	if ambient().is_some() {
		// One level only. Nesting would need savepoints, the way
		// `adapters/store-sqlite/src/tx.rs` does it; the fix in the script is to drop the inner
		// block, which covers the same rows anyway.
		return Err(mintworks_script::error::db("a db::tx block is already open"));
	}
	let mut conn = writer.acquire().await.db()?;
	// `BEGIN IMMEDIATE`, not a deferred `BEGIN`: a deferred one that reads first cannot upgrade
	// its lock and fails `SQLITE_BUSY` on the spot, whatever the busy timeout says.
	sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await.db()?;

	let held: Held = Arc::new(Mutex::new(conn));
	let mut guard = Guard(Some(Arc::clone(&held)));
	let out = OPEN.scope(Arc::clone(&held), body).await;

	let mut conn = held.lock().await;
	let finish = match &out {
		Ok(_) => "COMMIT",
		Err(_) => "ROLLBACK",
	};
	let done = sqlx::query(finish).execute(&mut **conn).await;
	// A failed COMMIT (a deferred FK) leaves the transaction open; pooled, it would fail every
	// later `BEGIN IMMEDIATE`.
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

#[cfg(test)]
mod tests {
	use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

	use super::*;

	struct Dir(std::path::PathBuf);

	impl Drop for Dir {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	async fn exec(sql: &'static str) -> ClResult<()> {
		let held = ambient().unwrap();
		sqlx::query(sql).execute(&mut **held.lock().await).await.db()?;
		Ok(())
	}

	/// A deferred FK is only checked at COMMIT, the one way a script can make COMMIT itself fail.
	#[tokio::test]
	async fn a_failed_commit_does_not_poison_the_writer() {
		let dir = Dir(std::env::temp_dir().join(format!("appdb-commit-{}", std::process::id())));
		let _ = std::fs::remove_dir_all(&dir.0);
		std::fs::create_dir_all(&dir.0).unwrap();
		let opts = SqliteConnectOptions::new()
			.filename(dir.0.join("t.db"))
			.create_if_missing(true)
			.foreign_keys(true);
		let writer = SqlitePoolOptions::new()
			.max_connections(1)
			.min_connections(1)
			.connect_with(opts)
			.await
			.unwrap();

		let failed = run(
			&writer,
			Box::pin(async {
				exec("CREATE TABLE p (id INTEGER PRIMARY KEY)").await?;
				exec("CREATE TABLE c (p INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)")
					.await?;
				exec("INSERT INTO c (p) VALUES (42)").await?;
				Ok(serde_json::Value::Null)
			}),
		)
		.await;
		assert!(failed.is_err(), "the orphan row must fail the COMMIT");

		run(
			&writer,
			Box::pin(async { exec("CREATE TABLE ok (x)").await.map(|()| serde_json::Value::Null) }),
		)
		.await
		.unwrap();
	}
}

// vim: ts=4
