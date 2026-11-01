// SPDX-License-Identifier: MPL-2.0
//! The rolling budget windows over the real store: `set_budgets` validation, `windows`
//! reporting and `check` refusing once a window's cap is spent.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_core::error::Error;
use mintworks_core::prelude::Timestamp;
use mintworks_llm::ledger::{self, BudgetWindow, E_BUDGET};
use mintworks_llm::{LlmState, UsageKind, UsageRow};

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-llm-ledger-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn app(db: &TmpDb) -> mintworks_core::App {
	let config = mintworks_core::config::Config {
		master_key: [0; 32],
		db_path: db.0.join("test.db").to_string_lossy().into_owned(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	let store = mintworks_store_sqlite::SqliteStore::open(&config).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let ledger: std::sync::Arc<dyn mintworks_llm::LlmStore> = std::sync::Arc::new(store.clone());
	mintworks_core::AppBuilder::new()
		.config(config)
		.store(std::sync::Arc::new(store) as std::sync::Arc<dyn mintworks_core::store::CoreStore>)
		.settings(mintworks_llm::SETTINGS)
		.extension(LlmState::default())
		.extension(ledger)
		.build()
		.await
		.unwrap()
}

fn row(at: i64, cost: i64) -> UsageRow {
	UsageRow {
		at: Timestamp(at),
		run: None,
		account_id: None,
		org_id: None,
		subject: Some("s".into()),
		step: "draft".into(),
		kind: UsageKind::Llm,
		provider: "p".into(),
		model: "m".into(),
		tokens_in: 1,
		tokens_out: 1,
		cost_micro_eur: cost,
		retry: false,
	}
}

fn code(err: Error) -> &'static str {
	match err {
		Error::Coded { code, .. } => code,
		other => panic!("not a coded error: {other:?}"),
	}
}

#[tokio::test]
async fn set_budgets_rejects_bad_windows() {
	let db = TmpDb::new("validate");
	let app = app(&db).await;
	for bad in [&[(0, 1)][..], &[(60, -1)], &[(60, 1), (60, 2)]] {
		let err = ledger::set_budgets(&app, "s", bad).await.unwrap_err();
		assert!(matches!(err, Error::Validation(_)), "{bad:?}: {err:?}");
	}
	assert!(ledger::windows(&app, "s").await.unwrap().is_empty());
}

#[tokio::test]
async fn check_refuses_once_a_window_is_spent() {
	let db = TmpDb::new("check");
	let app = app(&db).await;
	let now = Timestamp::now().0;
	ledger::set_budgets(&app, "s", &[(3_600, 10), (60, 100)]).await.unwrap();
	ledger::record(&app, &row(now - 7_200, 50)).await.unwrap();
	ledger::record(&app, &row(now - 30, 4)).await.unwrap();
	ledger::check(&app, Some("s")).await.unwrap();

	ledger::record(&app, &row(now - 10, 6)).await.unwrap();
	assert_eq!(code(ledger::check(&app, Some("s")).await.unwrap_err()), E_BUDGET);
	ledger::check(&app, Some("other")).await.unwrap();
	assert_eq!(
		ledger::windows(&app, "s").await.unwrap(),
		[
			BudgetWindow {
				window_secs: 60,
				cap: 100,
				spent: 10,
				oldest: Some(Timestamp(now - 30))
			},
			BudgetWindow {
				window_secs: 3_600,
				cap: 10,
				spent: 10,
				oldest: Some(Timestamp(now - 30))
			},
		]
	);
}

// vim: ts=4
