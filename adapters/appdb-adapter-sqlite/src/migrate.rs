//! The app DB's migration runner: versioned framework content modules (`memory`, `agent`, …)
//! applied through [`crate::SqliteAppDb::migrate`] before the script's `app.table` reconcile.
//!
//! Copied from `store-adapter-sqlite/src/migrate.rs`, not imported: this crate must not depend
//! on the framework's store adapter. Same contract — a `schema_version` table owned by the
//! runner, `from == 0` means never applied, every pending module in **one** transaction with
//! foreign keys off and a `PRAGMA foreign_key_check` before the commit, and a database a newer
//! build has written is refused.

use sqlx::{Row, SqliteConnection, SqlitePool};

use saas_core::{
	error::{ClResult, Error},
	types::Timestamp,
};

use crate::util::DbExt;

/// What a [`Module::apply`] returns. A boxed future because a `fn` pointer cannot name an
/// opaque return type.
pub type Fut<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ClResult<()>> + Send + 'a>>;

/// One app-DB schema owner and its version, recorded in the app DB's own `schema_version`.
#[derive(Clone, Copy)]
pub struct Module {
	/// The `schema_version.module` key.
	pub name: &'static str,
	pub version: i64,
	/// Brings the database from `from` to `version`. `from == 0` means the module has never been
	/// applied to this database — create the current schema outright.
	pub apply: for<'a> fn(&'a mut SqliteConnection, i64) -> Fut<'a>,
}

/// Apply every module whose recorded version is behind the code's, atomically.
pub(crate) async fn run(pool: &SqlitePool, modules: &[Module]) -> ClResult<()> {
	// `detach`ed: the writer pool is `max_connections(1)`, so a cancelled migrate would
	// otherwise re-pool the only writer with foreign keys off.
	let mut conn = pool.acquire().await.db()?.detach();
	sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut conn).await.db()?;
	apply(&mut conn, modules).await
}

async fn apply(conn: &mut SqliteConnection, modules: &[Module]) -> ClResult<()> {
	use sqlx::Connection;
	// A duplicate name would be skipped on its second pass as "already at that version".
	let mut seen = std::collections::HashSet::with_capacity(modules.len());
	for m in modules {
		if !seen.insert(m.name) {
			return Err(Error::internal(format!(
				"app database module '{}' is listed twice; a module is identified by its name",
				m.name
			)));
		}
	}
	// `BEGIN IMMEDIATE`: a deferred transaction cannot upgrade its read lock and fails
	// `SQLITE_BUSY` at once.
	let mut tx = conn.begin_with("BEGIN IMMEDIATE").await.db()?;

	sqlx::query(
		"CREATE TABLE IF NOT EXISTS schema_version (
			module		TEXT NOT NULL PRIMARY KEY,
			version		INTEGER NOT NULL,
			updated_at	INTEGER NOT NULL
		)",
	)
	.execute(&mut *tx)
	.await
	.db()?;

	let now = Timestamp::now().0;
	let mut changed = false;
	for m in modules {
		let from: i64 = sqlx::query_scalar("SELECT version FROM schema_version WHERE module = ?")
			.bind(m.name)
			.fetch_optional(&mut *tx)
			.await
			.db()?
			.unwrap_or(0);
		if from == m.version {
			continue;
		}
		if from > m.version {
			return Err(Error::internal(format!(
				"app database module '{}' is at version {from}, this build knows only {}",
				m.name, m.version
			)));
		}
		(m.apply)(&mut tx, from).await?;
		sqlx::query(
			"INSERT INTO schema_version (module, version, updated_at) VALUES (?, ?, ?) \
			 ON CONFLICT(module) DO UPDATE SET version = excluded.version, \
			 updated_at = excluded.updated_at",
		)
		.bind(m.name)
		.bind(m.version)
		.bind(now)
		.execute(&mut *tx)
		.await
		.db()?;
		changed = true;
	}
	if !changed {
		return Ok(());
	}

	let dangling = sqlx::query("PRAGMA foreign_key_check").fetch_all(&mut *tx).await.db()?;
	if let Some(row) = dangling.first() {
		let table: String = row.try_get("table").db()?;
		return Err(Error::Internal(format!(
			"app database migration left {} dangling foreign key row(s), first in table '{table}'",
			dangling.len()
		)));
	}

	tx.commit().await.db()
}

// vim: ts=4
