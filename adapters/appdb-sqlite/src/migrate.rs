// SPDX-License-Identifier: MPL-2.0
//! The app DB's migration runner: versioned framework content modules (`memory`, `agent`, …)
//! applied through [`crate::SqliteAppDb::migrate`] before the script's `app.table` reconcile.
//!
//! Copied from `adapters/store-sqlite/src/migrate.rs`, not imported: this crate must not depend
//! on the framework's store adapter. Same contract — a `schema_version` table owned by the
//! runner, `from == 0` means never applied, every pending module in **one** transaction with
//! foreign keys off and a `PRAGMA foreign_key_check` before the commit, and a database a newer
//! build has written is refused.

use sqlx::{AssertSqlSafe, Row, SqliteConnection, SqlitePool};

use mintworks_core::{
	error::{ClResult, Error},
	types::Timestamp,
};
use mintworks_script::db::Migration;

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

	ensure_table(&mut tx).await?;

	let mut changed = false;
	for m in modules {
		let from = recorded(&mut tx, m.name).await?;
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
		stamp(&mut tx, m.name, m.version).await?;
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

/// The `app.migration` chain, on the reconcile's open transaction: every declared version above
/// the one recorded under `module = 'app'`, in order. `migrations` arrive sorted and `1..=n`.
pub(crate) async fn app_chain(
	conn: &mut SqliteConnection,
	migrations: &[Migration],
) -> ClResult<()> {
	ensure_table(conn).await?;
	let from = recorded(conn, APP).await?;
	let declared = migrations.last().map_or(0, |m| m.version);
	if from > declared {
		return Err(Error::internal(format!(
			"the app database is at app migration {from}, the script declares only {declared}"
		)));
	}
	if from == declared {
		return Ok(());
	}
	for m in migrations.iter().filter(|m| m.version > from) {
		// `raw_sql`: a migration may hold several `;`-separated statements.
		sqlx::raw_sql(AssertSqlSafe(m.sql.clone())).execute(&mut *conn).await.db()?;
	}
	stamp(conn, APP, declared).await
}

/// The `schema_version.module` key the script's own migration chain records itself under.
const APP: &str = "app";

/// Runs inside the reconcile too, which may be the first thing to touch a fresh file.
async fn ensure_table(conn: &mut SqliteConnection) -> ClResult<()> {
	sqlx::query(
		"CREATE TABLE IF NOT EXISTS schema_version (
			module		TEXT NOT NULL PRIMARY KEY,
			version		INTEGER NOT NULL,
			updated_at	INTEGER NOT NULL
		)",
	)
	.execute(conn)
	.await
	.db()?;
	Ok(())
}

async fn recorded(conn: &mut SqliteConnection, module: &str) -> ClResult<i64> {
	let v = sqlx::query_scalar("SELECT version FROM schema_version WHERE module = ?")
		.bind(module)
		.fetch_optional(conn)
		.await
		.db()?;
	Ok(v.unwrap_or(0))
}

async fn stamp(conn: &mut SqliteConnection, module: &str, version: i64) -> ClResult<()> {
	sqlx::query(
		"INSERT INTO schema_version (module, version, updated_at) VALUES (?, ?, ?) \
		 ON CONFLICT(module) DO UPDATE SET version = excluded.version, \
		 updated_at = excluded.updated_at",
	)
	.bind(module)
	.bind(version)
	.bind(Timestamp::now().0)
	.execute(conn)
	.await
	.db()?;
	Ok(())
}

// vim: ts=4
