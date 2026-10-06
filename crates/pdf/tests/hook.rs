// SPDX-License-Identifier: MPL-2.0
//! `DocumentHook` over the real store: erasure removes the account's unshared files and nothing
//! else, and a retry is a no-op.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mintworks_core::{
	account_data::AccountDataHook,
	config::Config,
	ids::{AccountId, DocId},
};
use mintworks_pdf::{DocumentHook, DocumentStore, doc_path};
use mintworks_store_sqlite::{FRAMEWORK, SqliteStore};

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("mintworks-pdf-hook-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

#[tokio::test]
async fn erase_removes_only_unshared_files() {
	let db = TmpDb::new("erase");
	let data_dir = db.0.to_string_lossy().into_owned();
	let store = SqliteStore::open(&Config {
		master_key: [0; 32],
		db_path: db.0.join("test.db").to_string_lossy().into_owned(),
		data_dir: data_dir.clone(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	})
	.await
	.unwrap();
	store.migrate(&[FRAMEWORK]).await.unwrap();
	sqlx::raw_sql(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0);
		 INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (100, 'org_p', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'PERSONAL', 'P', 1, 0),
		        (101, 'org_s', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'S', 1, 0);",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	for (org, sha) in [(100, "aa11"), (100, "bb22"), (101, "bb22")] {
		let uid = DocId::generate();
		store.document_insert(org, &uid, "t.typ", "k").await.unwrap();
		store.document_rendered(&uid, sha, 1).await.unwrap();
		let path = doc_path(&data_dir, sha).unwrap();
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, b"%PDF").unwrap();
	}
	let hook = DocumentHook { docs: Arc::new(store) as Arc<dyn DocumentStore>, data_dir };
	let acc = AccountId::from_trusted("acc_t".to_owned());

	assert_eq!(hook.export(&acc).await.unwrap().as_array().unwrap().len(), 2);
	hook.erase(&acc).await.unwrap();
	assert!(!doc_path(&hook.data_dir, "aa11").unwrap().exists());
	assert!(doc_path(&hook.data_dir, "bb22").unwrap().exists(), "the shared org's file");
	hook.erase(&acc).await.unwrap();
	assert!(hook.export(&acc).await.unwrap().as_array().unwrap().is_empty());
}

// vim: ts=4
