// SPDX-License-Identifier: MPL-2.0
//! The app DB's migration runner: versioned framework content modules (`memory`, `agent`, …)
//! applied through [`crate::PgAppDb::migrate`] before the script's `app.table` reconcile.
//!
//! A copy of `adapters/appdb-sqlite/src/migrate.rs`, not an import: this crate must not depend on
//! another adapter. Same contract — a `schema_version` table owned by the runner, `from == 0`
//! means never applied, every pending module in **one** transaction, and a database a newer
//! build has written is refused. Unlike SQLite, two processes can migrate at once, so the
//! transaction takes [`MIGRATE_LOCK`] first.

use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};

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
	pub apply: for<'a> fn(&'a mut PgConnection, i64) -> Fut<'a>,
}

/// The `pg_advisory_xact_lock` key both the module runner and the `app` chain take, so two
/// booting processes serialize instead of both applying. ASCII `saasappd`.
pub(crate) const MIGRATE_LOCK: i64 = 0x7361_6173_6170_7064;

/// Apply every module whose recorded version is behind the code's, atomically.
pub(crate) async fn run(pool: &PgPool, modules: &[Module]) -> ClResult<()> {
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
	let mut conn = pool.acquire().await.db()?;
	let mut tx = conn.begin().await.db()?;
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
	tx.commit().await.db()
}

/// The `app.migration` chain, on the reconcile's open transaction: every declared version above
/// the one recorded under `module = 'app'`, in order. `migrations` arrive sorted and `1..=n`.
pub(crate) async fn app_chain(conn: &mut PgConnection, migrations: &[Migration]) -> ClResult<()> {
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

/// Takes [`MIGRATE_LOCK`] for the rest of the transaction, then makes sure the table exists:
/// `CREATE TABLE IF NOT EXISTS` alone races on the catalog when two processes run it at once.
async fn ensure_table(conn: &mut PgConnection) -> ClResult<()> {
	sqlx::query("SELECT pg_advisory_xact_lock($1)")
		.bind(MIGRATE_LOCK)
		.execute(&mut *conn)
		.await
		.db()?;
	sqlx::query(
		"CREATE TABLE IF NOT EXISTS schema_version (
			module		TEXT NOT NULL PRIMARY KEY,
			version		BIGINT NOT NULL,
			updated_at	BIGINT NOT NULL
		)",
	)
	.execute(conn)
	.await
	.db()?;
	Ok(())
}

async fn recorded(conn: &mut PgConnection, module: &str) -> ClResult<i64> {
	let v = sqlx::query_scalar("SELECT version FROM schema_version WHERE module = $1")
		.bind(module)
		.fetch_optional(conn)
		.await
		.db()?;
	Ok(v.unwrap_or(0))
}

async fn stamp(conn: &mut PgConnection, module: &str, version: i64) -> ClResult<()> {
	sqlx::query(
		"INSERT INTO schema_version (module, version, updated_at) VALUES ($1, $2, $3) \
		 ON CONFLICT (module) DO UPDATE SET version = excluded.version, \
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
