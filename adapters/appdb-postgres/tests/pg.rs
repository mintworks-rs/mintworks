// SPDX-License-Identifier: MPL-2.0
//! What only the PostgreSQL app DB has to prove: the value mapping, the read-only `query`, the
//! statement allowlist, and the two migration chains. The shared `AppDb` behaviour is in
//! `conformance.rs`.
//!
//! Needs `PG_TEST_URL` (a `postgres://…/<db>` URL whose role has `CREATEDB` and `CREATEROLE`);
//! unset, every test prints a skip line and passes. Each test creates its own database and drops
//! it afterwards.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use mintworks_appdb_postgres::{Fut, Module, PgAppDb};
use mintworks_script::{AppDb, TableDef, db::Migration};
use serde_json::{Value as Json, json};
use sqlx::{Connection, PgConnection};

/// `script.db_max_rows`' default.
const MAX: usize = 10_000;

/// Returns early from the test when `PG_TEST_URL` is unset.
macro_rules! pg {
	() => {
		match common::TestDb::new().await {
			Some(tmp) => tmp,
			None => return,
		}
	};
}

fn ledger() -> Vec<TableDef> {
	let decl = json!({ "cols": { "uid": "TEXT PRIMARY KEY", "day": "TEXT NOT NULL" } });
	vec![TableDef::parse("ledger", &decl).unwrap()]
}

#[tokio::test]
async fn every_mapped_type_round_trips() {
	let tmp = pg!();
	let db = tmp.open();
	let decl = json!({ "cols": {
		"id": "INTEGER PRIMARY KEY", "u": "UUID", "ts": "TIMESTAMPTZ", "d": "DATE", "j": "JSONB",
		"tags": "TEXT[]", "ns": "INTEGER[]", "x": "REAL", "n": "NUMERIC", "b": "BOOLEAN",
	} });
	db.reconcile(&[], &[TableDef::parse("typed", &decl).unwrap()]).await.unwrap();
	let row = json!({
		"u": "6f1c2a9e-3b4d-4e5f-8a7b-1c2d3e4f5a6b",
		"ts": "2026-10-04T12:30:00Z",
		"d": "2026-10-04",
		"j": { "k": [1, true, null] },
		"tags": ["a", "b"],
		"ns": [1, 2],
		"x": 1.25,
		"n": "12.50",
		"b": true,
	});
	let cols = ["u", "ts", "d", "j", "tags", "ns", "x", "n", "b"];
	let args: Vec<Json> = cols.iter().map(|c| row[c].clone()).collect();
	db.exec(
		"INSERT INTO typed (u, ts, d, j, tags, ns, x, n, b) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
		&args,
	)
	.await
	.unwrap();
	// A NULL binds for any parameter type.
	let nulls = vec![Json::Null; cols.len()];
	db.exec(
		"INSERT INTO typed (u, ts, d, j, tags, ns, x, n, b) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
		&nulls,
	)
	.await
	.unwrap();

	let rows = db
		.query("SELECT u, ts, d, j, tags, ns, x, n, b FROM typed ORDER BY id", &[], MAX)
		.await
		.unwrap();
	let empty: serde_json::Map<String, Json> =
		cols.iter().map(|c| ((*c).into(), Json::Null)).collect();
	assert_eq!(rows, vec![row, Json::Object(empty)]);
}

#[tokio::test]
async fn query_outside_a_tx_is_read_only() {
	let tmp = pg!();
	let db = tmp.open();
	db.reconcile(&[], &ledger()).await.unwrap();
	let cte = "WITH x AS (DELETE FROM ledger RETURNING uid) SELECT uid FROM x";

	let err = db.query(cte, &[], MAX).await.unwrap_err();
	assert_eq!(err.parts().1, "E-SCRIPT-DB", "{err:?}");
}

#[tokio::test]
async fn for_update_and_returning_work_inside_a_tx() {
	let tmp = pg!();
	let db = tmp.open();
	db.reconcile(&[], &ledger()).await.unwrap();
	let insert = "INSERT INTO ledger (uid, day) VALUES ($1, 'd') RETURNING uid";

	assert!(db.query(insert, &[json!("x")], MAX).await.is_err());
	let out = db
		.transaction(Box::pin(async {
			let mut rows = db.query(insert, &[json!("a")], MAX).await?;
			rows.extend(db.query("SELECT uid FROM ledger FOR UPDATE", &[], MAX).await?);
			Ok(Json::Array(rows))
		}))
		.await
		.unwrap();
	assert_eq!(out, json!([{ "uid": "a" }, { "uid": "a" }]));
}

#[tokio::test]
async fn a_unique_violation_is_a_409() {
	let tmp = pg!();
	let db = tmp.open();
	db.reconcile(&[], &ledger()).await.unwrap();
	let insert = "INSERT INTO ledger (uid, day) VALUES ('a', 'd')";
	db.exec(insert, &[]).await.unwrap();

	let dup = db.exec(insert, &[]).await;
	assert!(matches!(dup, Err(mintworks_core::error::Error::Conflict(_))), "{dup:?}");
}

#[tokio::test]
async fn session_state_and_chained_statements_are_refused() {
	let tmp = pg!();
	let db = tmp.open();
	db.reconcile(&[], &ledger()).await.unwrap();

	for sql in [
		"SET search_path = public",
		"RESET ALL",
		"DISCARD ALL",
		"LISTEN x",
		"DROP TABLE ledger",
	] {
		assert!(db.exec(sql, &[]).await.is_err(), "{sql}");
	}
	assert!(db.query("SELECT 1; SET role = postgres", &[], MAX).await.is_err());
	assert!(db.exec("DELETE FROM ledger; DROP TABLE ledger", &[]).await.is_err());
	db.exec("DELETE FROM ledger;", &[]).await.unwrap();
}

#[tokio::test]
async fn a_superuser_role_is_refused() {
	let tmp = pg!();
	let mut admin = PgConnection::connect(&tmp.admin).await.unwrap();
	let superuser: bool =
		sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = current_user")
			.fetch_one(&mut admin)
			.await
			.unwrap();
	if !superuser {
		eprintln!("skipped: PG_TEST_URL's role is not a superuser");
		return;
	}
	let err = PgAppDb::new(tmp.admin.clone()).query("SELECT 1", &[], MAX).await.unwrap_err();
	assert!(err.to_string().contains("superuser"), "{err}");
	// The boot path: no module selected must still connect and refuse.
	let err = PgAppDb::new(tmp.admin.clone()).migrate(&[]).await.unwrap_err();
	assert!(err.to_string().contains("superuser"), "{err}");
}

#[tokio::test]
async fn statement_timeout_survives_a_script_override() {
	let tmp = pg!();
	let db = tmp.open();
	let timeout = json!([{ "t": "30s" }]).as_array().unwrap().clone();
	let show = "SELECT current_setting('statement_timeout') AS t";
	assert_eq!(db.query(show, &[], MAX).await.unwrap(), timeout);
	db.query("SELECT set_config('statement_timeout', '0', false)", &[], MAX)
		.await
		.unwrap();
	// Every reader lease, so whichever connection held the override is among them.
	for _ in 0..5 {
		assert_eq!(db.query(show, &[], MAX).await.unwrap(), timeout);
	}
}

#[tokio::test]
async fn a_caught_error_leaves_the_tx_usable() {
	let tmp = pg!();
	let db = tmp.open();
	db.reconcile(&[], &ledger()).await.unwrap();
	let insert = "INSERT INTO ledger (uid, day) VALUES ($1, 'd')";
	db.exec(insert, &[json!("a")]).await.unwrap();

	db.transaction(Box::pin(async {
		assert!(db.exec(insert, &[json!("a")]).await.is_err());
		db.exec(insert, &[json!("b")]).await?;
		Ok(Json::Null)
	}))
	.await
	.unwrap();
	let rows = db.query("SELECT uid FROM ledger ORDER BY uid", &[], MAX).await.unwrap();
	assert_eq!(rows, json!([{ "uid": "a" }, { "uid": "b" }]).as_array().unwrap().clone());
}

#[tokio::test]
async fn session_state_does_not_outlive_the_lease() {
	let tmp = pg!();
	let db = tmp.open();
	let default = db.query("SELECT current_setting('search_path') AS p", &[], MAX).await.unwrap();
	db.query("SELECT set_config('search_path', 'nowhere', false)", &[], MAX)
		.await
		.unwrap();
	// Every reader lease, so whichever connection held the `set_config` is among them.
	for _ in 0..5 {
		let p = db.query("SELECT current_setting('search_path') AS p", &[], MAX).await.unwrap();
		assert_eq!(p, default);
	}

	db.query("SELECT 1 AS x FROM pg_advisory_lock(42)", &[], MAX).await.unwrap();
	let mut other = PgConnection::connect(&tmp.url).await.unwrap();
	let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(42)")
		.fetch_one(&mut other)
		.await
		.unwrap();
	assert!(free, "the advisory lock outlived the lease");
}

fn migrations(n: i64) -> Vec<Migration> {
	let sql = [
		"CREATE TABLE notes (id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, body TEXT); INSERT INTO notes (body) VALUES ('one')",
		"ALTER TABLE notes ADD COLUMN score DOUBLE PRECISION",
	];
	(1..=n).zip(sql).map(|(v, s)| Migration::new(v, s.into()).unwrap()).collect()
}

async fn version(db: &PgAppDb, module: &str) -> Vec<Json> {
	db.query("SELECT version FROM schema_version WHERE module = $1", &[json!(module)], MAX)
		.await
		.unwrap()
}

#[tokio::test]
async fn app_migrations_apply_once_and_too_new_fails() {
	let tmp = pg!();
	let db = tmp.open();
	db.reconcile(&migrations(1), &[]).await.unwrap();
	db.reconcile(&migrations(2), &[]).await.unwrap();
	assert_eq!(version(&db, "app").await, vec![json!({ "version": 2 })]);

	// A reboot re-runs nothing: migration 1's INSERT would add a second row.
	let db = tmp.open();
	db.reconcile(&migrations(2), &[]).await.unwrap();
	let rows = db.query("SELECT body, score FROM notes", &[], MAX).await.unwrap();
	assert_eq!(rows, vec![json!({ "body": "one", "score": null })]);

	let err = tmp.open().reconcile(&migrations(1), &[]).await.unwrap_err();
	assert!(err.to_string().contains("app migration 2"), "{err}");
}

fn create_notes(conn: &mut PgConnection, from: i64) -> Fut<'_> {
	Box::pin(async move {
		assert_eq!(from, 0);
		sqlx::query("CREATE TABLE notes (id BIGINT PRIMARY KEY, body TEXT)")
			.execute(conn)
			.await
			.unwrap();
		Ok(())
	})
}

const NOTES: Module = Module { name: "notes", version: 1, apply: create_notes };

#[tokio::test]
async fn the_module_runner_applies_once_and_refuses_too_new() {
	let tmp = pg!();
	let db = tmp.open();
	db.migrate(&[NOTES]).await.unwrap();
	// `create_notes` is not idempotent: a second apply would fail on the existing table.
	db.migrate(&[NOTES]).await.unwrap();
	assert_eq!(version(&db, "notes").await, vec![json!({ "version": 1 })]);

	sqlx::raw_sql("UPDATE schema_version SET version = 2 WHERE module = 'notes'")
		.execute(&mut PgConnection::connect(&tmp.url).await.unwrap())
		.await
		.unwrap();
	let err = tmp.open().migrate(&[NOTES]).await.unwrap_err();
	assert!(err.to_string().contains("version 2"), "{err}");
}

#[cfg(feature = "ai")]
#[tokio::test]
async fn memory_writes_and_searches() {
	use mintworks_appdb_postgres::MEMORY;
	use mintworks_memory::{MemoryStore, NewVersion, WriteMode};

	let tmp = pg!();
	let db = tmp.open();
	db.migrate(&[MEMORY]).await.unwrap();
	let v = db
		.version_write(&NewVersion {
			org: "org_a",
			space_key: "project:p1",
			path: "a.md",
			body: "the giraffe is tall",
			author: "acc_author",
			pdf_sha256: None,
			mode: WriteMode::Replace,
		})
		.await
		.unwrap();
	assert_eq!(v.version, 1);

	let hits = db.search("org_a", None, "giraffe", 10).await.unwrap();
	assert_eq!(hits.len(), 1);
	assert!(hits[0].snippet.contains("**giraffe**"), "{}", hits[0].snippet);
	assert!(db.search("org_b", None, "giraffe", 10).await.unwrap().is_empty());
	db.search("org_a", None, "\"giraffe AND (", 10).await.unwrap();
}

// vim: ts=4
