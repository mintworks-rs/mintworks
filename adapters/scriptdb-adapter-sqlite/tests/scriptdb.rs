//! The script database's own suite: reconcile, transactions, and what a statement may be.
//!
//! A real file database in a temp dir, never `sqlite::memory:` — an in-memory URL gives each
//! connection its own database, so the writer and the readers would never see one another's rows.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use saas_script::{ScriptDb, TableDef};
use scriptdb_adapter_sqlite::SqliteScriptDb;
use serde_json::{Value as Json, json};

/// `script.db_max_rows`' default.
const MAX: usize = 10_000;

struct TmpDb(PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("scriptdb-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn open(&self) -> SqliteScriptDb {
		SqliteScriptDb::new(self.0.join("script.db"))
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// `app.table("ledger", …)` as `TableDef::parse` hands it over.
fn ledger(extra_col: bool, index: &str) -> Vec<TableDef> {
	let mut cols = json!({ "uid": "TEXT PRIMARY KEY", "day": "TEXT NOT NULL" });
	if extra_col {
		cols["total"] = json!("INTEGER NOT NULL DEFAULT 0");
	}
	let decl = json!({ "cols": cols, "indexes": [[index]] });
	vec![TableDef::parse("ledger", &decl).unwrap()]
}

#[tokio::test]
async fn reconcile_creates_then_adds_a_column() {
	let tmp = TmpDb::new("reconcile");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();
	db.exec("INSERT INTO ledger (uid, day) VALUES (?, ?)", &[json!("a"), json!("2026-01-01")])
		.await
		.unwrap();

	// The second pass adds a column and withdraws the index the declaration no longer lists.
	db.reconcile(&ledger(true, "uid")).await.unwrap();

	let rows = db.query("SELECT uid, day, total FROM ledger", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "uid": "a", "day": "2026-01-01", "total": 0 })]);

	let idx = db
		.query(
			"SELECT name FROM sqlite_master
			  WHERE type = 'index' AND name NOT LIKE 'sqlite_autoindex_%' ORDER BY name",
			&[],
			MAX,
		)
		.await
		.unwrap();
	assert_eq!(idx, vec![json!({ "name": "ix:ledger:uid" })]);
}

#[tokio::test]
async fn a_column_type_change_is_a_hard_error() {
	let tmp = TmpDb::new("coltype");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();

	let decl = json!({ "cols": { "uid": "TEXT PRIMARY KEY", "day": "INTEGER" }, "indexes": [] });
	let changed = vec![TableDef::parse("ledger", &decl).unwrap()];
	assert!(db.reconcile(&changed).await.is_err());
}

#[tokio::test]
async fn a_committed_block_lands_and_a_failed_one_does_not() {
	let tmp = TmpDb::new("commit");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();

	let out = db
		.transaction(Box::pin(async {
			db.exec("INSERT INTO ledger (uid, day) VALUES ('a', 'd')", &[]).await?;
			Ok(json!("done"))
		}))
		.await
		.unwrap();
	assert_eq!(out, json!("done"));

	let failed = db
		.transaction(Box::pin(async {
			db.exec("INSERT INTO ledger (uid, day) VALUES ('b', 'd')", &[]).await?;
			Err(saas_core::error::Error::conflict("boom"))
		}))
		.await;
	assert!(failed.is_err());

	let rows = db.query("SELECT uid FROM ledger ORDER BY uid", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "uid": "a" })]);
}

#[tokio::test]
async fn a_read_inside_a_block_sees_its_own_writes() {
	let tmp = TmpDb::new("readown");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();

	db.transaction(Box::pin(async {
		db.exec("INSERT INTO ledger (uid, day) VALUES ('a', 'd')", &[]).await?;
		let rows = db.query("SELECT uid FROM ledger", &[], MAX).await?;
		assert_eq!(rows, vec![json!({ "uid": "a" })]);
		Ok(Json::Null)
	}))
	.await
	.unwrap();
}

#[tokio::test]
async fn a_null_in_a_typed_column_reads_as_null() {
	let tmp = TmpDb::new("null");
	let db = tmp.open();
	let decl =
		json!({ "cols": { "uid": "TEXT PRIMARY KEY", "n": "INTEGER", "t": "TEXT", "b": "BLOB" } });
	db.reconcile(&[TableDef::parse("nulls", &decl).unwrap()]).await.unwrap();
	db.exec("INSERT INTO nulls (uid) VALUES ('a')", &[]).await.unwrap();

	let rows = db.query("SELECT n, t, b FROM nulls", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "n": null, "t": null, "b": null })]);
}

#[tokio::test]
async fn a_duplicate_key_is_a_conflict() {
	let tmp = TmpDb::new("dup");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();
	let insert = "INSERT INTO ledger (uid, day) VALUES ('a', 'd')";
	db.exec(insert, &[]).await.unwrap();

	let dup = db.exec(insert, &[]).await;
	assert!(matches!(dup, Err(saas_core::error::Error::Conflict(_))), "{dup:?}");
}

#[tokio::test]
async fn a_nested_block_is_refused() {
	let tmp = TmpDb::new("nested");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();

	let out = db
		.transaction(Box::pin(async { db.transaction(Box::pin(async { Ok(Json::Null) })).await }))
		.await;
	assert!(out.is_err());
}

#[tokio::test]
async fn ddl_and_attach_are_refused() {
	let tmp = TmpDb::new("refused");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();

	assert!(db.exec("DROP TABLE ledger", &[]).await.is_err());
	assert!(db.exec("ATTACH DATABASE 'saas.db' AS other", &[]).await.is_err());
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

#[tokio::test]
async fn index_names_do_not_collide() {
	let tmp = TmpDb::new("idxnames");
	let db = tmp.open();
	let tables = [
		("a_b", json!({ "cols": { "c": "TEXT" }, "indexes": [["c"]] })),
		("a", json!({ "cols": { "b_c": "TEXT" }, "indexes": [["b_c"]] })),
		(
			"t",
			json!({ "cols": { "a_b": "TEXT", "a": "TEXT", "b": "TEXT" },
			"indexes": [["a_b"], ["a", "b"]] }),
		),
	];
	let defs: Vec<TableDef> = tables.iter().map(|(n, d)| TableDef::parse(n, d).unwrap()).collect();
	db.reconcile(&defs).await.unwrap();
	// A second pass must keep all four, not withdraw one as undeclared.
	db.reconcile(&defs).await.unwrap();

	let idx = db
		.query(
			"SELECT name FROM sqlite_master
			  WHERE type = 'index' AND name NOT LIKE 'sqlite_autoindex_%' ORDER BY name",
			&[],
			MAX,
		)
		.await
		.unwrap();
	let names: Vec<&str> = idx.iter().map(|r| r["name"].as_str().unwrap()).collect();
	assert_eq!(names, ["ix:a:b_c", "ix:a_b:c", "ix:t:a,b", "ix:t:a_b"]);
}

#[tokio::test]
async fn a_cancelled_tx_does_not_poison_the_writer() {
	let tmp = TmpDb::new("cancelled");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();

	let slow = db.transaction(Box::pin(async {
		db.exec("INSERT INTO ledger (uid, day) VALUES ('lost', 'd')", &[]).await?;
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
		Ok(Json::Null)
	}));
	assert!(tokio::time::timeout(std::time::Duration::from_millis(50), slow).await.is_err());

	db.transaction(Box::pin(async {
		db.exec("INSERT INTO ledger (uid, day) VALUES ('kept', 'd')", &[]).await?;
		Ok(Json::Null)
	}))
	.await
	.unwrap();
	let rows = db.query("SELECT uid FROM ledger ORDER BY uid", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "uid": "kept" })]);
}

#[tokio::test]
async fn a_cte_write_through_query_is_refused() {
	let tmp = TmpDb::new("cte-write");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();
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
async fn a_query_over_max_rows_is_refused() {
	let tmp = TmpDb::new("max-rows");
	let db = tmp.open();
	db.reconcile(&ledger(false, "day")).await.unwrap();
	for uid in ["a", "b", "c"] {
		db.exec("INSERT INTO ledger (uid, day) VALUES (?, 'd')", &[json!(uid)])
			.await
			.unwrap();
	}

	assert_eq!(db.query("SELECT uid FROM ledger", &[], 3).await.unwrap().len(), 3);
	let err = db.query("SELECT uid FROM ledger", &[], 2).await.unwrap_err();
	assert_eq!(err.parts().1, "E-SCRIPT-DB", "{err:?}");
}

// vim: ts=4
