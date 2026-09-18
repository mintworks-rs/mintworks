//! The migration runner. [`crate::schema::FRAMEWORK`] is the framework's module; the
//! application composes it with any modules of its own and calls [`run`] through
//! `SqliteStore::migrate`.
//!
//! Every pending module is applied in **one** transaction with foreign keys off, so a failed
//! startup leaves the database exactly as it was. `PRAGMA foreign_key_check` runs before the
//! commit, so a module that leaves a dangling reference aborts the boot rather than shipping a
//! broken database.
//!
//! A module is identified by its `name` and carries an integer `version`, both recorded in
//! `schema_version`. The runner compares the recorded version with the code's and hands the
//! module the one it found, so `apply` decides between creating the current schema outright
//! (`from == 0`) and upgrading. A database written by a *newer* build is refused rather than
//! run against older code.
//!
//! # The database holds no business rules
//!
//! Not one line of this schema creates a trigger, and that is a decision rather than an
//! omission. Business rules — which status transitions are legal, which columns freeze at
//! issue, that a foreign-currency invoice needs a HUF rate — have named owners in the feature
//! crates instead. **A trigger defends against an accident, never an adversary**: `DROP TRIGGER`
//! is one statement, so the schema is no place for a rule the application also enforces. Where
//! each rule lives is written beside the table it guards.
//!
//! What stays is everything the application *cannot* enforce without holding a lock across a
//! check and a write: `UNIQUE (tenant_id, request_id)`, `idx_invoice_storno_once`,
//! `idx_tenant_personal`, `idx_billing_party_default`, `idx_nav_submission_live` — concurrency
//! constraints, not business rules — along with the `CHECK`s on enum spellings and every
//! foreign key.

use sqlx::{Row, SqliteConnection, SqlitePool};

use saas_core::{
	error::{ClResult, Error},
	types::Timestamp,
};

use crate::util::DbExt;

/// What a [`Module::apply`] returns. A boxed future rather than an `async fn`, because a `fn`
/// pointer cannot name an opaque return type.
pub type Fut<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ClResult<()>> + Send + 'a>>;

/// One schema owner and its version. The framework ships [`crate::schema::FRAMEWORK`]; a
/// consumer declares its own and passes both to `SqliteStore::migrate`.
///
/// `apply` takes a plain `fn` pointer rather than a trait object so a `Module` stays a `const`,
/// and a `&mut SqliteConnection` rather than a `Transaction` so the signature carries one
/// lifetime — the runner owns the transaction and lends each module `&mut *tx`.
#[derive(Clone, Copy)]
pub struct Module {
	/// The `schema_version.module` key: `"saas"` for the framework, whatever the consumer picks
	/// for its own tables.
	pub name: &'static str,
	pub version: i64,
	/// Brings the database from `from` to `version`. `from == 0` means the module has never been
	/// applied to this database — create the current schema outright.
	pub apply: for<'a> fn(&'a mut SqliteConnection, i64) -> Fut<'a>,
}

/// Apply every module whose recorded version is behind the code's, atomically.
pub async fn run(pool: &SqlitePool, modules: &[Module]) -> ClResult<()> {
	// `PRAGMA foreign_keys` is a no-op inside a transaction, so it is set on the connection
	// first — and `detach`ed, because the writer pool is `max_connections(1)`: a cancelled
	// `migrate()` would otherwise return the process's only writer with foreign keys off for
	// life, and `delete_draft` then orphans `invoice_lines`.
	let mut conn = pool.acquire().await.db()?.detach();
	sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut conn).await.db()?;
	apply(&mut conn, modules).await
}

async fn apply(conn: &mut SqliteConnection, modules: &[Module]) -> ClResult<()> {
	use sqlx::Connection;
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
	// `BEGIN IMMEDIATE`, not a deferred `BEGIN`: this reads `schema_version` before it writes,
	// and a deferred transaction cannot upgrade its read lock — it fails `SQLITE_BUSY` at once
	// whatever `busy_timeout` says. See the explanation on `SqliteStore::write_tx`.
	let mut tx = conn.begin_with("BEGIN IMMEDIATE").await.db()?;

	// The runner's own table, created before any module runs: a consumer module listed *ahead*
	// of the framework's must still find somewhere to record itself.
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
		// A database a newer build has already upgraded must not be run against older code,
		// which would read columns by names the new schema moved.
		if from > m.version {
			return Err(Error::internal(format!(
				"database module '{}' is at version {from}, this build knows only {}",
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
		// Propagated, not defaulted to `'?'`: this message is all an operator gets from a
		// boot that aborts here, so a decode failure must not read as a row naming no table.
		let table: String = row.try_get("table").db()?;
		return Err(Error::Internal(format!(
			"migration left {} dangling foreign key row(s), first in table '{table}'",
			dangling.len()
		)));
	}

	tx.commit().await.db()
}

// vim: ts=4
