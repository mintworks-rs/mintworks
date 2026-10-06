//! The migration runner — the PostgreSQL counterpart of `store-adapter-sqlite/src/migrate.rs`,
//! with the same contract: a `schema_version` table owned by the runner, `from == 0` means never
//! applied, every pending module in **one** transaction, a duplicate module name or a database a
//! newer build has written refused.
//!
//! Two processes can boot against one database, so the transaction takes [`MIGRATE_LOCK`]
//! before it reads anything. Foreign keys are checked at the commit rather than per statement:
//! `SET CONSTRAINTS ALL DEFERRED` covers every FK declared `DEFERRABLE`, which the cyclic ones
//! (`tenants.billing_currency` → `currencies`, `invoices.tenant_id` → `tenants`) must be, so a
//! module that leaves a dangling reference fails the commit and the boot.
//!
//! No trigger is created here or in any module (arch-9): business rules live in the feature crates.

use sqlx::{Connection, PgConnection, PgPool};

use saas_core::{
	error::{ClResult, Error},
	types::Timestamp,
};

use crate::util::DbExt;

/// What a [`Module::apply`] returns. A boxed future rather than an `async fn`, because a `fn`
/// pointer cannot name an opaque return type.
pub type Fut<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ClResult<()>> + Send + 'a>>;

/// One schema owner and its version. The framework ships `crate::schema::FRAMEWORK`; a consumer
/// declares its own and passes both to `PgStore::migrate`.
#[derive(Clone, Copy)]
pub struct Module {
	/// The `schema_version.module` key: `"saas"` for the framework, whatever the consumer picks
	/// for its own tables.
	pub name: &'static str,
	pub version: i64,
	/// Brings the database from `from` to `version`. `from == 0` means the module has never been
	/// applied to this database — create the current schema outright.
	pub apply: for<'a> fn(&'a mut PgConnection, i64) -> Fut<'a>,
}

/// The `pg_advisory_xact_lock` key the runner holds for its transaction. ASCII `saasmigr`;
/// distinct from `crate::tx::WRITE_LOCK` so a boot does not queue behind live write traffic.
pub(crate) const MIGRATE_LOCK: i64 = 0x7361_6173_6d69_6772;

/// Apply every module whose recorded version is behind the code's, atomically.
///
/// # Errors
/// `Error::Internal` when a module is listed twice, when the database records a higher version
/// than this build knows, and when a module's DDL fails or leaves a dangling foreign key.
pub async fn run(pool: &PgPool, modules: &[Module]) -> ClResult<()> {
	// A duplicate name reads as "already at that version" on the second pass and is skipped,
	// while its DDL never runs and `schema_version` says it did.
	let mut seen = std::collections::HashSet::with_capacity(modules.len());
	for m in modules {
		if !seen.insert(m.name) {
			return Err(Error::internal(format!(
				"schema module '{}' is listed twice; a module is identified by its name",
				m.name
			)));
		}
	}
	let mut conn = pool.acquire().await.db()?;
	// A `Transaction`, not a raw `BEGIN`: dropped mid-flight it rolls back.
	let mut tx = conn.begin().await.db()?;
	ensure_table(&mut tx).await?;
	sqlx::query("SET CONSTRAINTS ALL DEFERRED").execute(&mut *tx).await.db()?;

	let mut changed = false;
	for m in modules {
		let from = recorded(&mut tx, m.name).await?;
		if from == m.version {
			continue;
		}
		// A database a newer build has already upgraded must not be run against older code,
		// which would read columns by names the new schema moved.
		if from > m.version {
			return Err(Error::internal(format!(
				"database module '{}' is at version {from}, this build knows only {}",
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
	// The deferred FK checks run here; a violation (`23503`) is a defect, so `Internal`.
	tx.commit().await.map_err(|err| match &err {
		sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
			Error::Internal(format!("migration left a dangling foreign key: {db}"))
		}
		_ => crate::util::map_db(&err),
	})
}

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
