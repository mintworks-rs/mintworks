//! `AppDb` conformance: reconcile, transactions, the row cap and the module runner, as every
//! app-DB adapter must reproduce them. What only one engine can say — its statement allowlist,
//! its column types, its file — stays in that adapter's `tests/appdb.rs`.

use saas_script::{AppDb, TableDef, db::Migration};
use serde_json::{Value as Json, json};

use crate::{AppDbHarness, TestModule, fresh_db};

/// `script.db_max_rows`' default.
const MAX: usize = 10_000;

/// `app.table("ledger", …)` as `TableDef::parse` hands it over.
fn ledger(extra_col: bool, index: &str) -> Vec<TableDef> {
	let mut cols = json!({ "uid": "TEXT PRIMARY KEY", "day": "TEXT NOT NULL" });
	if extra_col {
		cols["total"] = json!("INTEGER NOT NULL DEFAULT 0");
	}
	let decl = json!({ "cols": cols, "indexes": [[index]] });
	vec![TableDef::parse("ledger", &decl).unwrap()]
}

async fn version<H: AppDbHarness>(db: &H::Db, module: &str) -> Vec<Json>
where
	H::Db: AppDb,
{
	let sql = H::sql("SELECT version FROM schema_version WHERE module = ?");
	db.query(&sql, &[json!(module)], MAX).await.unwrap()
}

fn migrations(n: i64) -> Vec<Migration> {
	let sql = [
		"CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes (id, body) VALUES (1, 'one')",
		"ALTER TABLE notes ADD COLUMN score REAL",
	];
	(1..=n).zip(sql).map(|(v, s)| Migration::new(v, s.into()).unwrap()).collect()
}

const NOTES: TestModule = TestModule::Notes { version: 1 };

pub async fn reconcile_creates_then_adds_a_column<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-reconcile");
	let db = h.db();
	db.reconcile(&[], &ledger(false, "day")).await.unwrap();
	let insert = H::sql("INSERT INTO ledger (uid, day) VALUES (?, ?)");
	db.exec(&insert, &[json!("a"), json!("2026-01-01")]).await.unwrap();

	// The second pass adds a column and withdraws the index the declaration no longer lists.
	db.reconcile(&[], &ledger(true, "uid")).await.unwrap();

	let rows = db.query("SELECT uid, day, total FROM ledger", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "uid": "a", "day": "2026-01-01", "total": 0 })]);
	assert_eq!(H::index_names(db).await, ["ix:ledger:uid"]);
}

pub async fn a_column_type_change_is_a_hard_error<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-coltype");
	let db = h.db();
	db.reconcile(&[], &ledger(false, "day")).await.unwrap();

	let decl = json!({ "cols": { "uid": "TEXT PRIMARY KEY", "day": "INTEGER" }, "indexes": [] });
	let changed = vec![TableDef::parse("ledger", &decl).unwrap()];
	assert!(db.reconcile(&[], &changed).await.is_err());
}

pub async fn a_committed_block_lands_and_a_failed_one_does_not<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-commit");
	let db = h.db();
	db.reconcile(&[], &ledger(false, "day")).await.unwrap();

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

pub async fn a_read_inside_a_block_sees_its_own_writes<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-readown");
	let db = h.db();
	db.reconcile(&[], &ledger(false, "day")).await.unwrap();

	db.transaction(Box::pin(async {
		db.exec("INSERT INTO ledger (uid, day) VALUES ('a', 'd')", &[]).await?;
		let rows = db.query("SELECT uid FROM ledger", &[], MAX).await?;
		assert_eq!(rows, vec![json!({ "uid": "a" })]);
		Ok(Json::Null)
	}))
	.await
	.unwrap();
}

pub async fn a_duplicate_key_is_a_conflict<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-dup");
	let db = h.db();
	db.reconcile(&[], &ledger(false, "day")).await.unwrap();
	let insert = "INSERT INTO ledger (uid, day) VALUES ('a', 'd')";
	db.exec(insert, &[]).await.unwrap();

	let dup = db.exec(insert, &[]).await;
	assert!(matches!(dup, Err(saas_core::error::Error::Conflict(_))), "{dup:?}");
}

pub async fn a_nested_block_is_refused<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-nested");
	let db = h.db();
	db.reconcile(&[], &ledger(false, "day")).await.unwrap();

	let out = db
		.transaction(Box::pin(async { db.transaction(Box::pin(async { Ok(Json::Null) })).await }))
		.await;
	assert!(out.is_err());
}

pub async fn index_names_do_not_collide<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-idxnames");
	let db = h.db();
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
	db.reconcile(&[], &defs).await.unwrap();
	// A second pass must keep all four, not withdraw one as undeclared.
	db.reconcile(&[], &defs).await.unwrap();

	assert_eq!(H::index_names(db).await, ["ix:a:b_c", "ix:a_b:c", "ix:t:a,b", "ix:t:a_b"]);
}

pub async fn reserved_words_declare<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-reserved");
	let db = h.db();
	let decl =
		|cols: Json| TableDef::parse("order", &json!({ "cols": cols, "indexes": [["user"]] }));
	db.reconcile(&[], &[decl(json!({ "user": "TEXT" })).unwrap()]).await.unwrap();
	// The second pass reaches `ADD COLUMN` with a reserved column name too.
	let both = decl(json!({ "user": "TEXT", "group": "INTEGER" })).unwrap();
	db.reconcile(&[], &[both]).await.unwrap();

	let insert = H::sql(r#"INSERT INTO "order" ("user", "group") VALUES (?, ?)"#);
	db.exec(&insert, &[json!("u"), json!(1)]).await.unwrap();
	let rows = db.query(r#"SELECT "user", "group" FROM "order""#, &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "user": "u", "group": 1 })]);
}

pub async fn a_cancelled_tx_does_not_poison_the_writer<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-cancelled");
	let db = h.db();
	db.reconcile(&[], &ledger(false, "day")).await.unwrap();

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

pub async fn a_query_over_max_rows_is_refused<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-max-rows");
	let db = h.db();
	db.reconcile(&[], &ledger(false, "day")).await.unwrap();
	let insert = H::sql("INSERT INTO ledger (uid, day) VALUES (?, 'd')");
	for uid in ["a", "b", "c"] {
		db.exec(&insert, &[json!(uid)]).await.unwrap();
	}

	assert_eq!(db.query("SELECT uid FROM ledger", &[], 3).await.unwrap().len(), 3);
	let err = db.query("SELECT uid FROM ledger", &[], 2).await.unwrap_err();
	assert_eq!(err.parts().1, "E-SCRIPT-DB", "{err:?}");
}

pub async fn migrate_stamps_the_version_on_a_fresh_file<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-migrate-fresh");
	let db = h.db();
	H::migrate(db, &[NOTES]).await.unwrap();
	assert_eq!(version::<H>(db, "notes").await, vec![json!({ "version": 1 })]);
	assert!(db.query("SELECT * FROM notes", &[], MAX).await.unwrap().is_empty());
}

pub async fn migrate_again_at_the_same_version_is_a_no_op<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-migrate-rerun");
	let db = h.db();
	H::migrate(db, &[NOTES]).await.unwrap();
	// `notes` is not idempotent: a second apply would fail on the existing table.
	H::migrate(db, &[NOTES]).await.unwrap();
	assert_eq!(version::<H>(db, "notes").await, vec![json!({ "version": 1 })]);
}

pub async fn migrate_refuses_a_version_newer_than_the_build<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-migrate-newer");
	H::migrate(h.db(), &[TestModule::Notes { version: 2 }]).await.unwrap();
	let err = H::migrate(&h.open(), &[NOTES]).await.unwrap_err();
	assert!(err.to_string().contains("version 2"), "{err}");
}

pub async fn migrate_applies_modules_in_list_order<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-migrate-order");
	let db = h.db();
	H::migrate(db, &[NOTES, TestModule::Seed]).await.unwrap();
	let rows = db.query("SELECT body FROM notes", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "body": "seed" })]);
	assert_eq!(version::<H>(db, "seed").await, vec![json!({ "version": 1 })]);
}

pub async fn a_float_round_trips<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-float");
	let db = h.db();
	let decl = json!({ "cols": { "uid": "TEXT PRIMARY KEY", "x": "REAL" } });
	db.reconcile(&[], &[TableDef::parse("floats", &decl).unwrap()]).await.unwrap();
	let insert = H::sql("INSERT INTO floats (uid, x) VALUES ('a', ?)");
	db.exec(&insert, &[json!(1.25)]).await.unwrap();

	let rows = db.query("SELECT x FROM floats", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "x": 1.25 })]);
}

pub async fn app_migrations_apply_once<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-app-chain");
	let db = h.db();
	db.reconcile(&migrations(1), &[]).await.unwrap();
	db.reconcile(&migrations(2), &[]).await.unwrap();
	assert_eq!(version::<H>(db, "app").await, vec![json!({ "version": 2 })]);

	// A reboot re-runs nothing: migration 1's INSERT would add a second row.
	let db = h.open();
	db.reconcile(&migrations(2), &[]).await.unwrap();
	let rows = db.query("SELECT body, score FROM notes", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "body": "one", "score": null })]);
}

pub async fn an_app_version_above_the_declared_fails<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-app-newer");
	h.db().reconcile(&migrations(2), &[]).await.unwrap();
	let err = h.open().reconcile(&migrations(1), &[]).await.unwrap_err();
	assert!(err.to_string().contains("app migration 2"), "{err}");
}

pub async fn a_returning_write_through_query_needs_a_tx<H: AppDbHarness>()
where
	H::Db: AppDb,
{
	let h = fresh_db!(H, "appdb-returning");
	let db = h.db();
	db.reconcile(&[], &ledger(false, "day")).await.unwrap();
	let insert = "INSERT INTO ledger (uid, day) VALUES ('a', 'd') RETURNING uid";

	let err = db.query(insert, &[], MAX).await.unwrap_err();
	assert_eq!(err.parts().1, "E-SCRIPT-DB", "{err:?}");
	let out = db
		.transaction(Box::pin(async { Ok(Json::Array(db.query(insert, &[], MAX).await?)) }))
		.await
		.unwrap();
	assert_eq!(out, json!([{ "uid": "a" }]));
}

// vim: ts=4
