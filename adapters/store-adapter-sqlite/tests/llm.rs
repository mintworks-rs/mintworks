//! `LlmStore` conformance tests — what a second store adapter must pass: the ledger's sums by
//! subject and by time, and the budget upsert.
//!
//! Every test opens a real file database, per `tests/objects.rs`.

#![cfg(feature = "ai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_core::config::Config;
use saas_core::prelude::Timestamp;
use saas_llm::store::{LlmStore, UsageKind, UsageRow};
use store_adapter_sqlite::SqliteStore;

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-llm-store-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn setup(db: &TmpDb) -> SqliteStore {
	let store = SqliteStore::open(&Config {
		master_key: [0; 32],
		db_path: db.0.join("test.db").to_string_lossy().into_owned(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	})
	.await
	.unwrap();
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	store
}

fn row(at: i64, subject: Option<&str>, cost: i64) -> UsageRow {
	UsageRow {
		at: Timestamp(at),
		run: None,
		account_id: Some(7),
		org_id: None,
		subject: subject.map(str::to_owned),
		step: "draft".into(),
		kind: UsageKind::Llm,
		provider: "p".into(),
		model: "m".into(),
		tokens_in: 10,
		tokens_out: 20,
		cost_micro_eur: cost,
		retry: false,
	}
}

#[tokio::test]
async fn usage_sums_by_subject_and_since() {
	let db = TmpDb::new("sums");
	let store = setup(&db).await;
	assert_eq!(store.usage_cost_for_subject("s1").await.unwrap(), 0);
	assert_eq!(store.usage_cost_since(Timestamp(0)).await.unwrap(), 0);

	store.usage_insert(&row(100, Some("s1"), 5)).await.unwrap();
	store.usage_insert(&row(200, Some("s1"), 7)).await.unwrap();
	store.usage_insert(&row(300, Some("s2"), 11)).await.unwrap();
	store
		.usage_insert(&UsageRow { kind: UsageKind::Search, retry: true, ..row(400, None, 13) })
		.await
		.unwrap();

	assert_eq!(store.usage_cost_for_subject("s1").await.unwrap(), 12);
	assert_eq!(store.usage_cost_for_subject("s2").await.unwrap(), 11);
	assert_eq!(store.usage_cost_since(Timestamp(200)).await.unwrap(), 31, "`since` is inclusive");
}

#[tokio::test]
async fn budget_is_absent_until_set_and_upserts() {
	let db = TmpDb::new("budget");
	let store = setup(&db).await;
	assert_eq!(store.budget_get("s1").await.unwrap(), None);
	store.budget_set("s1", 50_000).await.unwrap();
	store.budget_set("s1", 500_000).await.unwrap();
	assert_eq!(store.budget_get("s1").await.unwrap(), Some(500_000));
	assert_eq!(store.budget_get("s2").await.unwrap(), None);
}

// vim: ts=4
