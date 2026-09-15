//! The migration runner. [`crate::STEPS`] is the framework baseline; the application composes
//! it with any steps of its own and calls [`run`] through `SqliteStore::migrate`.
//!
//! The whole pending tail is applied in **one** transaction with foreign keys off, so a
//! failed startup leaves the database exactly as it was. `PRAGMA foreign_key_check` runs
//! before the commit, so a step that leaves a dangling reference aborts the boot rather
//! than shipping a broken database.
//!
//! The ledger is keyed on a step's **name**, not on its position: `migrations.name` decides
//! whether a step has run, `migrations.idx` is only an insertion order, and `vars.db_version`
//! is the count applied. Editing an applied step, or dropping one off the list, is a hard
//! startup error — but appending a framework step ahead of a consumer's own is not, which is
//! what makes `store.migrate(&[STEPS, &MY_STEPS].concat())` survive a framework upgrade.
//! [`apply`] creates the ledger itself before the first step runs, so a consumer step composed
//! *ahead* of `STEPS` boots too — at the price of one rule: a step's own
//! `CREATE TABLE migrations` has to carry `IF NOT EXISTS`, as 001's does.
//!
//! # The database holds no business rules
//!
//! Not one of these steps creates a trigger, and that is a decision rather than an omission
//! (`claude-docs/adapter-contract.md`). Business rules — which status transitions are legal,
//! which columns freeze at issue, that a foreign-currency invoice needs a HUF rate — have named
//! owners in the feature crates instead. **A trigger defends against an accident, never an
//! adversary**: `DROP TRIGGER` is one statement, so the schema is no place for a rule the
//! application also enforces. Where each rule lives is written beside the table it guards.
//!
//! What stays is everything the application *cannot* enforce without holding a lock across a
//! check and a write: `UNIQUE (tenant_id, request_id)`, `idx_invoice_storno_once`,
//! `idx_tenant_personal`, `idx_billing_party_default`, `idx_nav_submission_live` — concurrency
//! constraints, not business rules — along with the `CHECK`s on enum spellings and every
//! foreign key.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

use saas_core::{
	error::{ClResult, Error},
	types::Timestamp,
};

use crate::util::DbExt;

/// One migration: a stable name and the SQL text applied under it.
///
/// The name is recorded in `migrations.name` and appears in mismatch errors, so it should
/// identify the owning crate — `"saas-core/init"`, `"saas-invoice/init"`.
#[derive(Clone, Copy, Debug)]
pub struct Step {
	pub name: &'static str,
	pub sql: &'static str,
}

/// Build a [`Step`] from a name and a path to an SQL file, relative to the invoking file.
///
/// ```ignore
/// pub const M_INIT: Step = store_adapter_sqlite::step!("myapp/init", "../migrations/001_init.sql");
/// ```
#[macro_export]
macro_rules! step {
	($name:literal, $path:literal) => {
		$crate::migrate::Step { name: $name, sql: include_str!($path) }
	};
}

/// Hex SHA-256 of a step's SQL text.
fn checksum(sql: &str) -> String {
	hex::encode(Sha256::digest(sql.as_bytes()))
}

/// The ledger: every applied step's name to its checksum, plus the highest `idx` in use.
///
/// A database with no `migrations` table has had nothing applied; every other failure is a
/// real error and propagates, because this read decides whether steps re-run.
async fn applied_steps(
	tx: &mut Transaction<'_, Sqlite>,
) -> ClResult<(HashMap<String, String>, Option<i64>)> {
	let has_ledger: Option<String> = sqlx::query_scalar(
		"SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'migrations'",
	)
	.fetch_optional(&mut **tx)
	.await
	.db()?;
	if has_ledger.is_none() {
		return Ok((HashMap::new(), None));
	}
	let rows = sqlx::query("SELECT idx, name, checksum FROM migrations")
		.fetch_all(&mut **tx)
		.await
		.db()?;
	let mut max_idx = None;
	let mut applied = HashMap::with_capacity(rows.len());
	for row in &rows {
		let idx: i64 = row.try_get("idx").db()?;
		max_idx = Some(max_idx.map_or(idx, |m: i64| m.max(idx)));
		applied.insert(row.try_get::<String, _>("name").db()?, row.try_get("checksum").db()?);
	}
	Ok((applied, max_idx))
}

/// Apply every step whose name is not yet in the `migrations` ledger, atomically.
///
/// `steps` must be the application's full ordered list, including the ones already applied
/// — their checksums are verified against `migrations` before anything new runs.
pub async fn run(pool: &SqlitePool, steps: &[Step]) -> ClResult<()> {
	// `PRAGMA foreign_keys` is a no-op inside a transaction, so it is set on the connection
	// first — and `detach`ed, because the writer pool is `max_connections(1)`: a cancelled
	// `migrate()` would otherwise return the process's only writer with foreign keys off for
	// life, and `delete_draft` then orphans `invoice_lines`.
	let mut conn = pool.acquire().await.db()?.detach();
	sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut conn).await.db()?;
	apply(&mut conn, steps).await
}

async fn apply(conn: &mut sqlx::SqliteConnection, steps: &[Step]) -> ClResult<()> {
	use sqlx::Connection;
	// Name uniqueness is what replaced positional order as the step identity: a duplicate is
	// filtered out as "already applied" on an upgrade and never runs, while the ledger says it did.
	let mut seen = std::collections::HashSet::with_capacity(steps.len());
	for step in steps {
		if !seen.insert(step.name) {
			return Err(Error::internal(format!(
				"migration '{}' is listed twice; a step is identified by its name",
				step.name
			)));
		}
	}
	// `BEGIN IMMEDIATE`, not a deferred `BEGIN`: this reads the ledger before it writes, and a
	// deferred transaction cannot upgrade its read lock — it fails `SQLITE_BUSY` at once whatever
	// `busy_timeout` says. See the explanation on `SqliteStore::write_tx`.
	let mut tx = conn.begin_with("BEGIN IMMEDIATE").await.db()?;

	let (applied, max_idx) = applied_steps(&mut tx).await?;
	verify_applied(steps, &applied)?;

	// Pending is "not in the ledger by name", in list order — not a tail cut at an index. A
	// consumer's step and a newly appended framework step can therefore interleave however
	// the composed list puts them, which is the whole point.
	let pending: Vec<&Step> = steps.iter().filter(|s| !applied.contains_key(s.name)).collect();
	if pending.is_empty() {
		return Ok(());
	}

	// The ledger, before the first step runs: `migrations` is created by the `saas-core/init`
	// step itself, so a consumer composing `&[MY_STEPS, STEPS].concat()` ran its first step's
	// DDL and then failed the ledger insert with `no such table: migrations`. No checksum
	// moves — this is Rust, and 001's own `CREATE TABLE IF NOT EXISTS` is idempotent.
	sqlx::query(
		"CREATE TABLE IF NOT EXISTS migrations (
			idx		INTEGER NOT NULL PRIMARY KEY,
			name		TEXT NOT NULL UNIQUE,
			checksum	TEXT NOT NULL,
			applied_at	INTEGER NOT NULL
		)",
	)
	.execute(&mut *tx)
	.await
	.db()?;

	let now = Timestamp::now().0;
	// `idx` carries on from the highest one in use rather than tracking list position: it is
	// an `INTEGER PRIMARY KEY` and needs only to be unique, and a consumer's applied step
	// already occupies a position the framework's newest one would otherwise claim.
	let next_idx = max_idx.map_or(0, |m| m + 1);
	for (n, step) in pending.iter().enumerate() {
		sqlx::raw_sql(step.sql)
			.execute(&mut *tx)
			.await
			.map_err(|e| Error::Internal(format!("migration '{}' failed: {e}", step.name)))?;
		sqlx::query("INSERT INTO migrations (idx, name, checksum, applied_at) VALUES (?, ?, ?, ?)")
			.bind(next_idx + i64::try_from(n).unwrap_or(i64::MAX))
			.bind(step.name)
			.bind(checksum(step.sql))
			.bind(now)
			.execute(&mut *tx)
			.await
			.db()?;
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

	// `vars` is a 001-created table too, but needs no pre-creation like `migrations` above:
	// this runs *after* the whole pending tail, by which point `saas-core/init` has applied
	// whatever position it occupied in the composed list.
	sqlx::query(
		"INSERT INTO vars (name, value) VALUES ('db_version', ?) \
		 ON CONFLICT(name) DO UPDATE SET value = excluded.value",
	)
	.bind((applied.len() + pending.len()).to_string())
	.execute(&mut *tx)
	.await
	.db()?;

	tx.commit().await.db()?;
	Ok(())
}

/// A step whose SQL changed after it was applied is a hard error: the database no longer
/// matches the code that reads it, and re-applying is not an option. So is one that has been
/// dropped off the list — the schema it created is still there and nothing owns it now.
///
/// Order is deliberately **not** checked: a positional check breaks the documented
/// `store.migrate(&[STEPS, &MY_STEPS].concat())` upgrade path, because the consumer's step holds
/// the index the framework's next appended one wants.
fn verify_applied(steps: &[Step], applied: &HashMap<String, String>) -> ClResult<()> {
	for (name, sum) in applied {
		let Some(step) = steps.iter().find(|s| s.name == name) else {
			return Err(Error::Internal(format!(
				"migration '{name}' is applied but no longer in the step list"
			)));
		};
		if &checksum(step.sql) != sum {
			return Err(Error::Internal(format!(
				"migration '{name}' changed after it was applied"
			)));
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Stands in for `saas-core/init`, the step that creates the ledger — and, like it, does so
	/// with `IF NOT EXISTS`, which is load-bearing: `apply` creates `migrations` itself before
	/// the first step runs, so a step claiming the name unconditionally collides with it.
	const BOOT: Step = Step {
		name: "boot",
		sql: "CREATE TABLE IF NOT EXISTS vars (name TEXT PRIMARY KEY, value TEXT); \
		      CREATE TABLE IF NOT EXISTS migrations (idx INTEGER PRIMARY KEY, \
		        name TEXT NOT NULL UNIQUE, checksum TEXT NOT NULL, applied_at INTEGER NOT NULL);",
	};
	const SECOND: Step = Step { name: "second", sql: "CREATE TABLE b (id INTEGER PRIMARY KEY);" };

	/// A pool over a **file** database in a temp dir. `sqlite::memory:` gives each connection
	/// its own database, and `run` detaches the connection it migrates on, so an in-memory pool
	/// would hand the next query a fresh empty database — the same reason the store's own tests
	/// never use one.
	async fn file_pool(name: &str) -> (SqlitePool, std::path::PathBuf) {
		let dir =
			std::env::temp_dir().join(format!("saas-migrate-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let url = format!("sqlite://{}?mode=rwc", dir.join("test.db").display());
		let pool = sqlx::sqlite::SqlitePoolOptions::new()
			.max_connections(1)
			.connect(&url)
			.await
			.unwrap();
		(pool, dir)
	}

	#[tokio::test]
	async fn applies_tail_once_and_rejects_edited_steps() {
		let (pool, dir) = file_pool("tail-once").await;

		run(&pool, &[BOOT]).await.unwrap();
		run(&pool, &[BOOT, SECOND]).await.unwrap();
		// Re-running the same list is a no-op, not a re-apply.
		run(&pool, &[BOOT, SECOND]).await.unwrap();

		let n: i64 = sqlx::query_scalar("SELECT count(*) FROM migrations")
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(n, 2);
		let v: String = sqlx::query_scalar("SELECT value FROM vars WHERE name = 'db_version'")
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(v, "2");

		// Editing an applied step is a hard error, never a silent re-apply.
		let edited =
			Step { name: "second", sql: "CREATE TABLE b (id INTEGER PRIMARY KEY, x TEXT);" };
		assert!(run(&pool, &[BOOT, edited]).await.is_err());
		// Dropping an applied step off the list is too.
		assert!(run(&pool, &[BOOT]).await.is_err());
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// A positional `migrations.idx` check would make a step's index its version: a consumer
	/// following the documented `store.migrate(&[STEPS, &MY_STEPS].concat())` pattern holds the
	/// index the framework's next appended step claims, and every upgrade would kill the boot
	/// with "steps were reordered" on a database nobody had touched.
	#[tokio::test]
	async fn a_framework_step_appended_ahead_of_a_consumers_own_still_applies() {
		const CONSUMER: Step =
			Step { name: "myapp/init", sql: "CREATE TABLE mine (id INTEGER PRIMARY KEY);" };
		const FRAMEWORK_NEW: Step =
			Step { name: "framework/new", sql: "CREATE TABLE theirs (id INTEGER PRIMARY KEY);" };
		const LATER: Step =
			Step { name: "framework/later", sql: "CREATE TABLE later (id INTEGER PRIMARY KEY);" };

		let (pool, dir) = file_pool("consumer-tail").await;

		// The consumer boots on today's framework: its step lands last.
		run(&pool, &[BOOT, SECOND, CONSUMER]).await.unwrap();
		// The framework ships a new step, which the composed list puts *before* theirs.
		run(&pool, &[BOOT, SECOND, FRAMEWORK_NEW, CONSUMER]).await.unwrap();

		let names: Vec<String> = sqlx::query_scalar("SELECT name FROM migrations ORDER BY idx")
			.fetch_all(&pool)
			.await
			.unwrap();
		assert_eq!(names, ["boot", "second", "myapp/init", "framework/new"]);
		// The consumer's own step did not re-run — `mine` would already exist.
		let v: String = sqlx::query_scalar("SELECT value FROM vars WHERE name = 'db_version'")
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(v, "4", "db_version is the count applied");
		// And a plain append still applies exactly the new one.
		run(&pool, &[BOOT, SECOND, FRAMEWORK_NEW, CONSUMER, LATER]).await.unwrap();
		let n: i64 = sqlx::query_scalar("SELECT count(*) FROM migrations")
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(n, 5);
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// The other direction of the same contract: `store.migrate(&[MY_STEPS, STEPS].concat())`.
	/// Each step is followed immediately by its `INSERT INTO migrations`, and that table is
	/// created by `saas-core/init` — so a consumer step placed ahead of it only boots because
	/// `apply` pre-creates the ledger.
	#[tokio::test]
	async fn a_consumer_step_placed_before_the_framework_baseline_still_boots() {
		const CONSUMER: Step =
			Step { name: "myapp/init", sql: "CREATE TABLE mine (id INTEGER PRIMARY KEY);" };

		let (pool, dir) = file_pool("consumer-first").await;
		run(&pool, &[CONSUMER, BOOT, SECOND]).await.unwrap();

		let names: Vec<String> = sqlx::query_scalar("SELECT name FROM migrations ORDER BY idx")
			.fetch_all(&pool)
			.await
			.unwrap();
		assert_eq!(names, ["myapp/init", "boot", "second"]);
		// `vars` is written after the whole tail, so it is there even though `boot` created it
		// second — the reason it needs no pre-creation of its own.
		let v: String = sqlx::query_scalar("SELECT value FROM vars WHERE name = 'db_version'")
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(v, "3");
		// Idempotent: a second boot applies nothing and does not trip the checksum guard.
		run(&pool, &[CONSUMER, BOOT, SECOND]).await.unwrap();
		let n: i64 = sqlx::query_scalar("SELECT count(*) FROM migrations")
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(n, 3);
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// `apply` reads the ledger before it writes, and a deferred transaction cannot upgrade its
	/// read lock — it fails `SQLITE_BUSY` on the spot, however long `busy_timeout` is. Hence
	/// `BEGIN IMMEDIATE`: two processes booting against one file wait instead of failing.
	///
	/// A **file** database on purpose: `sqlite::memory:` gives each connection its own
	/// database, so two of them never contend.
	#[tokio::test]
	async fn a_boot_waits_for_a_held_write_lock_rather_than_failing() {
		use sqlx::sqlite::SqlitePoolOptions;

		let dir =
			std::env::temp_dir().join(format!("saas-migrate-busy-test-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let url = format!("sqlite://{}?mode=rwc", dir.join("test.db").display());

		let holder = SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap();
		sqlx::query("PRAGMA journal_mode = WAL").execute(&holder).await.unwrap();
		let booting = SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap();
		sqlx::query("PRAGMA busy_timeout = 5000").execute(&booting).await.unwrap();

		// Another writer holds the lock for 300 ms — well inside the 5 s timeout.
		let mut held = holder.begin().await.unwrap();
		sqlx::query("CREATE TABLE squatter (id INTEGER PRIMARY KEY)")
			.execute(&mut *held)
			.await
			.unwrap();
		let release = tokio::spawn(async move {
			tokio::time::sleep(std::time::Duration::from_millis(300)).await;
			held.commit().await.unwrap();
		});

		run(&booting, &[BOOT, SECOND]).await.expect("the boot must wait out the lock");
		release.await.unwrap();
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// The tail applies with `PRAGMA foreign_keys = OFF` — which is what lets a step create
	/// tables in any order — so the `foreign_key_check` before the commit is the *only* thing
	/// standing between a bad step and a database full of dangling references.
	#[tokio::test]
	async fn a_step_that_leaves_a_dangling_reference_aborts_the_whole_tail() {
		const BAD: Step = Step {
			name: "bad",
			sql: "CREATE TABLE parent (id INTEGER PRIMARY KEY); \
			      CREATE TABLE child (id INTEGER PRIMARY KEY, \
			        parent_id INTEGER NOT NULL REFERENCES parent(id)); \
			      INSERT INTO child (id, parent_id) VALUES (1, 999);",
		};

		let (pool, dir) = file_pool("dangling-fk").await;
		run(&pool, &[BOOT]).await.unwrap();

		let err = run(&pool, &[BOOT, BAD]).await.unwrap_err().to_string();
		assert!(err.contains("dangling foreign key"), "{err}");
		assert!(err.contains("child"), "the offending table has to be named: {err}");

		// And the whole transaction rolled back: no schema change, no version bump, no row
		// in `migrations`.
		let tables: i64 = sqlx::query_scalar(
			"SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('parent','child')",
		)
		.fetch_one(&pool)
		.await
		.unwrap();
		assert_eq!(tables, 0);
		let v: String = sqlx::query_scalar("SELECT value FROM vars WHERE name = 'db_version'")
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(v, "1");
		let n: i64 = sqlx::query_scalar("SELECT count(*) FROM migrations")
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(n, 1);
		let _ = std::fs::remove_dir_all(&dir);
	}
}

// vim: ts=4
