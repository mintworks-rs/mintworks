//! What only the PostgreSQL store has to prove: queued write transactions leave the holder its
//! pool, a dropped transaction re-pools its connection, and the GDPR export's row order.
//!
//! Skips every test when `PG_TEST_URL` is unset.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use common::TestDb;
use saas_auth::store::{AuthStore, ExportScope, ExportSection};
use saas_core::{store::CoreStore, types::Timestamp};
use serde_json::json;
use store_adapter_postgres::{FRAMEWORK, PgStore};

macro_rules! store {
	() => {
		match TestDb::new().await {
			Some(db) => {
				let store = db.open().await;
				store.migrate(&[FRAMEWORK]).await.unwrap();
				(db, store)
			}
			None => return,
		}
	};
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_write_txs_leave_the_holder_its_pool() {
	let (_db, store) = store!();
	let holder = store.write_tx().await.unwrap();
	let waiters: Vec<_> = (0..12)
		.map(|_| {
			let s = store.clone();
			tokio::spawn(async move { s.write_tx().await.unwrap().commit().await.unwrap() })
		})
		.collect();
	tokio::time::sleep(Duration::from_millis(200)).await;

	let started = Instant::now();
	store.job_enqueue("GATE", "{}", None, Timestamp(0)).await.unwrap();
	assert!(store.job_claim(Timestamp(1)).await.unwrap().is_some());
	assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());

	holder.commit().await.unwrap();
	for w in waiters {
		w.await.unwrap();
	}
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_write_tx_is_rolled_back_not_closed() {
	let (_db, store) = store!();
	let mut pids = BTreeSet::new();
	for _ in 0..20 {
		let tx = store.write_tx().await.unwrap();
		let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
			.fetch_one(&mut *tx.lock().await.unwrap())
			.await
			.unwrap();
		pids.insert(pid);
	}
	// Closing would open a fresh session per iteration.
	assert!(pids.len() <= 10, "{} sessions for 20 dropped txs", pids.len());
	let sessions: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM pg_stat_activity WHERE datname = current_database()",
	)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert!(sessions <= 15, "{sessions} sessions");
	store.write_tx().await.unwrap().commit().await.unwrap();
}

#[tokio::test]
async fn the_export_lists_rows_in_table_order() {
	let (_db, store) = store!();
	sqlx::raw_sql(
		"CREATE TABLE notes_export (id BIGINT PRIMARY KEY, account_id BIGINT NOT NULL, note TEXT);
		 INSERT INTO notes_export VALUES (3, 1, 'c'), (1, 1, 'a'), (2, 1, 'b')",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	let section = ExportSection {
		key: "notes",
		table: "notes_export",
		scope: ExportScope::AccountId,
		columns: &["note"],
		scaled: &[],
		mask: &[],
	};
	let out = PgStore::export_account(&store, 1, &[section]).await.unwrap();
	assert_eq!(out, [json!([{ "note": "a" }, { "note": "b" }, { "note": "c" }])]);
}

// vim: ts=4
