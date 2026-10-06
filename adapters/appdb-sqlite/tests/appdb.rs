//! The SQLite app DB under `mintworks_store_conformance`'s app-DB suites, plus what only SQLite can
//! say: its statement allowlist, its column types, its file, and the `agent` module's version
//! history.
//!
//! A real file database in a temp dir, never `sqlite::memory:` — an in-memory URL gives each
//! connection its own database, so the writer and the readers would never see one another's rows.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use mintworks_appdb_sqlite::{Fut, Module, SqliteAppDb};
use mintworks_core::ClResult;
use mintworks_script::{AppDb, TableDef, db::Migration};
use mintworks_store_conformance::{AppDbHarness, TestModule};
use serde_json::{Value as Json, json};
use sqlx::SqliteConnection;

/// `script.db_max_rows`' default.
const MAX: usize = 10_000;

struct SqliteHarness {
	dir: PathBuf,
	db: SqliteAppDb,
}

impl AppDbHarness for SqliteHarness {
	type Db = SqliteAppDb;

	async fn fresh(name: &str) -> Option<Self> {
		let dir = std::env::temp_dir().join(format!("appdb-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Some(Self { db: SqliteAppDb::new(dir.join("app.db")), dir })
	}

	fn db(&self) -> &SqliteAppDb {
		&self.db
	}

	fn open(&self) -> SqliteAppDb {
		SqliteAppDb::new(self.dir.join("app.db"))
	}

	fn sql(sql: &str) -> String {
		sql.to_owned()
	}

	async fn migrate(db: &SqliteAppDb, modules: &[TestModule]) -> ClResult<()> {
		let modules: Vec<Module> = modules
			.iter()
			.map(|m| match *m {
				TestModule::Notes { version } => {
					Module { name: "notes", version, apply: create_notes }
				}
				TestModule::Seed => Module { name: "seed", version: 1, apply: seed_notes },
				#[cfg(feature = "ai")]
				TestModule::Memory => mintworks_appdb_sqlite::MEMORY,
				#[cfg(feature = "ai")]
				TestModule::Agent => mintworks_appdb_sqlite::AGENT,
				// Unified features can turn `mintworks-store-conformance/ai` on without this crate's `ai`.
				#[cfg(not(feature = "ai"))]
				#[allow(unreachable_patterns)]
				_ => unreachable!("an ai module needs --features ai"),
			})
			.collect();
		db.migrate(&modules).await
	}

	async fn index_names(db: &SqliteAppDb) -> Vec<String> {
		let sql = "SELECT name FROM sqlite_master
			  WHERE type = 'index' AND name NOT LIKE 'sqlite_autoindex_%' ORDER BY name";
		let rows = db.query(sql, &[], MAX).await.unwrap();
		rows.iter().map(|r| r["name"].as_str().unwrap().to_owned()).collect()
	}
}

impl Drop for SqliteHarness {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

fn create_notes(conn: &mut SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		sqlx::query("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)")
			.execute(conn)
			.await
			.unwrap();
		Ok(())
	})
}

fn seed_notes(conn: &mut SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		sqlx::query("INSERT INTO notes (body) VALUES ('seed')")
			.execute(conn)
			.await
			.unwrap();
		Ok(())
	})
}

mintworks_store_conformance::appdb_tests!(SqliteHarness);

#[cfg(feature = "ai")]
mod memory {
	mintworks_store_conformance::memory_tests!(super::SqliteHarness);
}

#[cfg(feature = "ai")]
mod thread {
	mintworks_store_conformance::thread_tests!(super::SqliteHarness);
}

async fn fresh(name: &str) -> SqliteHarness {
	SqliteHarness::fresh(name).await.unwrap()
}

/// `app.table("ledger", …)` as `TableDef::parse` hands it over.
fn ledger() -> Vec<TableDef> {
	let decl = json!({ "cols": { "uid": "TEXT PRIMARY KEY", "day": "TEXT NOT NULL" },
		"indexes": [["day"]] });
	vec![TableDef::parse("ledger", &decl).unwrap()]
}

/// PostgreSQL refuses a `bytea` column outright, so the `BLOB` half is SQLite's alone.
#[tokio::test]
async fn a_null_in_a_typed_column_reads_as_null() {
	let h = fresh("sqlite-null").await;
	let db = h.db();
	let decl =
		json!({ "cols": { "uid": "TEXT PRIMARY KEY", "n": "INTEGER", "t": "TEXT", "b": "BLOB" } });
	db.reconcile(&[], &[TableDef::parse("nulls", &decl).unwrap()]).await.unwrap();
	db.exec("INSERT INTO nulls (uid) VALUES ('a')", &[]).await.unwrap();

	let rows = db.query("SELECT n, t, b FROM nulls", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "n": null, "t": null, "b": null })]);
}

#[tokio::test]
async fn ddl_and_attach_are_refused() {
	let h = fresh("sqlite-refused").await;
	let db = h.db();
	db.reconcile(&[], &ledger()).await.unwrap();

	assert!(db.exec("DROP TABLE ledger", &[]).await.is_err());
	assert!(db.exec("ATTACH DATABASE 'mintworks.db' AS other", &[]).await.is_err());
	assert!(db.exec("PRAGMA journal_mode", &[]).await.is_err());
	// `query` takes the read keywords only, so a write through it is refused as well.
	assert!(
		db.query("INSERT INTO ledger (uid, day) VALUES ('x', 'd')", &[], MAX)
			.await
			.is_err()
	);
	// A second statement behind an allowed first keyword.
	let attach = "DELETE FROM ledger WHERE 0; ATTACH DATABASE 'x.db' AS f";
	assert!(db.exec(attach, &[]).await.is_err());
	assert!(db.query("SELECT 1; ATTACH DATABASE 'x.db' AS f", &[], MAX).await.is_err());
	db.exec("DELETE FROM ledger WHERE 0;", &[]).await.unwrap();
}

/// SQLite-only: inside `db::tx` PostgreSQL's `query` runs on the writable block connection, so a
/// data-modifying CTE there is a write it was allowed anyway.
#[tokio::test]
async fn a_cte_write_through_query_is_refused() {
	let h = fresh("sqlite-cte-write").await;
	let db = h.db();
	db.reconcile(&[], &ledger()).await.unwrap();
	db.exec("INSERT INTO ledger (uid, day) VALUES ('a', 'd')", &[]).await.unwrap();
	let cte = "WITH x AS (SELECT 1) DELETE FROM ledger";

	let err = db.query(cte, &[], MAX).await.unwrap_err();
	assert_eq!(err.parts().1, "E-SCRIPT-DB", "{err:?}");
	db.transaction(Box::pin(async {
		let err = db.query(cte, &[], MAX).await.unwrap_err();
		assert_eq!(err.parts().1, "E-SCRIPT-DB", "{err:?}");
		// The writer is writable again once the read is done.
		db.exec("INSERT INTO ledger (uid, day) VALUES ('b', 'd')", &[]).await?;
		Ok(Json::Null)
	}))
	.await
	.unwrap();

	let rows = db.query("SELECT uid FROM ledger ORDER BY uid", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "uid": "a" }), json!({ "uid": "b" })]);
}

#[tokio::test]
async fn migrate_with_no_module_creates_no_file() {
	let h = fresh("sqlite-migrate-empty").await;
	h.db().migrate(&[]).await.unwrap();
	assert!(!h.dir.join("app.db").exists());
}

#[test]
fn a_migration_naming_a_framework_table_is_refused() {
	assert!(Migration::new(1, "CREATE INDEX x ON memory_docs (id)".into()).is_err());
}

#[tokio::test]
async fn a_postgres_only_column_type_is_refused() {
	let h = fresh("sqlite-uuid").await;
	let decl = json!({ "cols": { "id": "UUID PRIMARY KEY" } });
	assert!(h.db().reconcile(&[], &[TableDef::parse("t", &decl).unwrap()]).await.is_err());
}

/// The `agent` module as it shipped at version 1: `agent_threads` without `title`.
#[cfg(feature = "ai")]
fn agent_v1(conn: &mut SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		for sql in [
			"CREATE TABLE agent_threads (id INTEGER PRIMARY KEY, uid TEXT NOT NULL UNIQUE, \
			 org TEXT NOT NULL, subject TEXT, summary TEXT, token_estimate INTEGER NOT NULL \
			 DEFAULT 0, last_model TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)",
			"INSERT INTO agent_threads (uid, org, created_at, updated_at) \
			 VALUES ('thr_old', 'tnt_a', 1, 1)",
		] {
			sqlx::query(sql).execute(&mut *conn).await.unwrap();
		}
		Ok(())
	})
}

/// SQLite-only: PostgreSQL's `agent` chain starts at the titled shape.
#[cfg(feature = "ai")]
#[tokio::test]
async fn v1_upgrades_to_titled_threads() {
	use mintworks_agent::store::ThreadStore;
	use mintworks_appdb_sqlite::AGENT;
	use mintworks_core::prelude::ThreadId;

	let h = fresh("sqlite-v1-upgrade").await;
	let db = h.db();
	db.migrate(&[Module { name: "agent", version: 1, apply: agent_v1 }])
		.await
		.unwrap();
	db.migrate(&[AGENT]).await.unwrap();

	let old = db
		.thread_get(&ThreadId::from_trusted("thr_old".to_owned()))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(old.title, None);
	let t = db.thread_create("tnt_a", None, Some("new")).await.unwrap();
	assert_eq!(db.thread_get(&t.uid).await.unwrap().unwrap().title.as_deref(), Some("new"));
}

// vim: ts=4
