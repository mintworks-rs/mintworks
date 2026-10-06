// SPDX-License-Identifier: MPL-2.0
//! The migration runner itself, driven by synthetic [`Module`]s, plus the framework module's
//! fresh create. The PG framework chain starts at `VERSION = 1`, so there is no upgrade fixture
//! yet; one arrives with the first `if from < N` block in `migrations.rs`.
//!
//! Skips every test when `PG_TEST_URL` is unset.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicI64, Ordering};

use common::TestDb;
use mintworks_store_postgres::{FRAMEWORK, Fut, Module, PgStore, schema};
use sqlx::PgConnection;

macro_rules! test_db {
	() => {
		match TestDb::new().await {
			Some(db) => db,
			None => return,
		}
	};
}

async fn has_table(store: &PgStore, name: &str) -> bool {
	sqlx::query_scalar::<_, i64>(
		"SELECT count(*) FROM information_schema.tables
		  WHERE table_schema = current_schema() AND table_name = $1",
	)
	.bind(name)
	.fetch_one(store.read_pool())
	.await
	.unwrap()
		> 0
}

async fn ddl(conn: &mut PgConnection, sql: &'static str) -> mintworks_core::ClResult<()> {
	sqlx::raw_sql(sql)
		.execute(conn)
		.await
		.map_err(|e| mintworks_core::error::Error::internal(e.to_string()))?;
	Ok(())
}

#[tokio::test]
async fn a_fresh_database_gets_the_framework_schema_and_a_rerun_is_a_no_op() {
	let db = test_db!();
	let store = db.open().await;

	store.migrate(&[FRAMEWORK]).await.unwrap();
	assert!(has_table(&store, "jobs").await);
	store.migrate(&[FRAMEWORK]).await.unwrap();

	let version: i64 = sqlx::query_scalar("SELECT version FROM schema_version WHERE module = $1")
		.bind(schema::MODULE_NAME)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(version, schema::VERSION);
}

static SEEN: AtomicI64 = AtomicI64::new(-1);

fn v1(conn: &mut PgConnection, from: i64) -> Fut<'_> {
	Box::pin(async move {
		SEEN.store(from, Ordering::SeqCst);
		ddl(conn, "CREATE TABLE toy (id BIGINT PRIMARY KEY)").await
	})
}

fn v2(conn: &mut PgConnection, from: i64) -> Fut<'_> {
	Box::pin(async move {
		SEEN.store(from, Ordering::SeqCst);
		ddl(conn, "ALTER TABLE toy ADD COLUMN label TEXT").await
	})
}

#[tokio::test]
async fn apply_is_handed_the_recorded_version_and_stamped_only_after_it_returns() {
	let db = test_db!();
	let store = db.open().await;

	store.migrate(&[Module { name: "toy", version: 1, apply: v1 }]).await.unwrap();
	assert_eq!(SEEN.load(Ordering::SeqCst), 0, "a never-applied module must see from == 0");

	store.migrate(&[Module { name: "toy", version: 2, apply: v2 }]).await.unwrap();
	assert_eq!(SEEN.load(Ordering::SeqCst), 1, "an upgrade must see the recorded version");

	let version: i64 =
		sqlx::query_scalar("SELECT version FROM schema_version WHERE module = 'toy'")
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(version, 2);
}

static RUNS: AtomicI64 = AtomicI64::new(0);

fn counting(conn: &mut PgConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		RUNS.fetch_add(1, Ordering::SeqCst);
		ddl(conn, "CREATE TABLE counted (id BIGINT PRIMARY KEY)").await
	})
}

#[tokio::test]
async fn a_module_already_at_its_version_is_skipped_rather_than_reapplied() {
	let db = test_db!();
	let store = db.open().await;
	let m = Module { name: "counted", version: 1, apply: counting };

	store.migrate(&[m]).await.unwrap();
	store.migrate(&[m]).await.unwrap();

	assert_eq!(RUNS.load(Ordering::SeqCst), 1);
}

fn dupe(conn: &mut PgConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move { ddl(conn, "CREATE TABLE dupe (id BIGINT PRIMARY KEY)").await })
}

#[tokio::test]
async fn a_duplicate_module_name_is_refused_rather_than_silently_skipped() {
	let db = test_db!();
	let store = db.open().await;
	let m = Module { name: "dupe", version: 1, apply: dupe };

	let err = store.migrate(&[m, m]).await.unwrap_err().to_string();
	assert!(err.contains("dupe"), "the error must name the module: {err}");
	assert!(!has_table(&store, "dupe").await, "neither pass may have run its DDL");
}

fn noop(_conn: &mut PgConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move { Ok(()) })
}

#[tokio::test]
async fn a_database_from_a_newer_build_is_refused() {
	let db = test_db!();
	let store = db.open().await;
	store
		.migrate(&[Module { name: "ahead", version: 1, apply: noop }])
		.await
		.unwrap();
	sqlx::query("UPDATE schema_version SET version = 7 WHERE module = 'ahead'")
		.execute(store.write_pool())
		.await
		.unwrap();

	let err = store
		.migrate(&[Module { name: "ahead", version: 2, apply: noop }])
		.await
		.unwrap_err()
		.to_string();
	assert!(err.contains('7') && err.contains('2'), "both versions must be named: {err}");
}

/// A deferred FK, so the violation surfaces at the runner's commit rather than at the `INSERT`.
fn dangling(conn: &mut PgConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		ddl(
			conn,
			"CREATE TABLE parent (id BIGINT PRIMARY KEY);
			 CREATE TABLE child (id BIGINT PRIMARY KEY,
			     parent_id BIGINT REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);
			 INSERT INTO child (id, parent_id) VALUES (1, 404);",
		)
		.await
	})
}

#[tokio::test]
async fn a_dangling_foreign_key_rolls_the_whole_migration_back() {
	let db = test_db!();
	let store = db.open().await;

	let err = store
		.migrate(&[Module { name: "fk", version: 1, apply: dangling }])
		.await
		.unwrap_err()
		.to_string();
	assert!(err.contains("child"), "the error must name the offending table: {err}");
	assert!(!has_table(&store, "child").await, "the transaction must have rolled back");
	assert!(!has_table(&store, "schema_version").await);
}

fn first(conn: &mut PgConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move { ddl(conn, "CREATE TABLE ordered_first (id BIGINT PRIMARY KEY)").await })
}

/// Legal only if `first` already ran in the same transaction.
fn second(conn: &mut PgConnection, _from: i64) -> Fut<'_> {
	Box::pin(
		async move { ddl(conn, "CREATE VIEW ordered_view AS SELECT id FROM ordered_first").await },
	)
}

fn boom(_conn: &mut PgConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move { Err(mintworks_core::error::Error::internal("boom")) })
}

#[tokio::test]
async fn modules_apply_in_list_order_inside_one_transaction() {
	let db = test_db!();
	let store = db.open().await;

	store
		.migrate(&[
			Module { name: "first", version: 1, apply: first },
			Module { name: "second", version: 1, apply: second },
		])
		.await
		.unwrap();
	assert!(has_table(&store, "ordered_first").await);

	let rollback_db = test_db!();
	let rollback = rollback_db.open().await;
	rollback
		.migrate(&[
			Module { name: "first", version: 1, apply: first },
			Module { name: "boom", version: 1, apply: boom },
		])
		.await
		.unwrap_err();
	assert!(
		!has_table(&rollback, "ordered_first").await,
		"a later module's failure must roll the earlier one back too"
	);
}

// vim: ts=4
