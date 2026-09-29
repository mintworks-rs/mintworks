//! `DocumentStore` and the `jobs.result` column `RENDER_DOC` hands its sha256 back through —
//! the conformance a second store adapter must pass for `saas-pdf`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_core::{
	config::Config,
	ids::{AccountId, DocId},
	store::CoreStore,
	types::Timestamp,
};
use saas_pdf::DocumentStore;
use store_adapter_sqlite::{FRAMEWORK, SqliteStore};

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("saas-pdf-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [0; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: self.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: String::new(),
			jobs_workers: None,
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn store(db: &TmpDb) -> SqliteStore {
	let store = SqliteStore::open(&db.config()).await.unwrap();
	store.migrate(&[FRAMEWORK]).await.unwrap();
	store
}

async fn seed_org(store: &SqliteStore, id: i64, uid: &str) {
	sqlx::query(
		"INSERT OR IGNORE INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (?, ?, (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
	)
	.bind(id)
	.bind(uid)
	.execute(store.write_pool())
	.await
	.unwrap();
}

#[tokio::test]
async fn a_document_is_pending_until_rendered_and_confined_to_its_org() {
	let db = TmpDb::new("documents");
	let store = store(&db).await;
	seed_org(&store, 100, "org_a").await;
	seed_org(&store, 101, "org_b").await;

	let uid = DocId::generate();
	store
		.document_insert(100, &uid, "templates/demo.typ", "pdf:doc:x")
		.await
		.unwrap();

	let doc = store.document_get(100, &uid).await.unwrap().unwrap();
	assert_eq!(
		(doc.template.as_str(), doc.job_key.as_str(), doc.sha256, doc.bytes),
		("templates/demo.typ", "pdf:doc:x", None, None)
	);
	assert!(store.document_get(101, &uid).await.unwrap().is_none(), "another org's uid");

	store.document_rendered(&uid, "abc123", 42).await.unwrap();
	let doc = store.document_get(100, &uid).await.unwrap().unwrap();
	assert_eq!((doc.sha256.as_deref(), doc.bytes), (Some("abc123"), Some(42)));

	// Unknown uid is a no-op, not an error: the org may have been erased mid-render.
	store.document_rendered(&DocId::generate(), "abc123", 42).await.unwrap();
}

#[tokio::test]
async fn deleting_the_org_cascades_to_its_documents() {
	let db = TmpDb::new("documents-cascade");
	let store = store(&db).await;
	seed_org(&store, 100, "org_a").await;
	let uid = DocId::generate();
	store.document_insert(100, &uid, "t.typ", "k").await.unwrap();

	sqlx::query("DELETE FROM orgs WHERE id = 100")
		.execute(store.write_pool())
		.await
		.unwrap();
	assert!(store.document_get(100, &uid).await.unwrap().is_none());
}

/// Files are content-addressed and shared: erasure may hand back only a sha nothing else uses.
#[tokio::test]
async fn account_erasure_returns_only_unshared_files() {
	let db = TmpDb::new("documents-erase");
	let store = store(&db).await;
	seed_org(&store, 100, "org_a").await;
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (102, 'org_p', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'PERSONAL', 'P', 1, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	for (org, sha) in [(102, Some("aa11")), (102, Some("bb22")), (102, Some("cc33")), (102, None)]
		.into_iter()
		.chain([(100, Some("bb22"))])
	{
		let uid = DocId::generate();
		store.document_insert(org, &uid, "t.typ", "k").await.unwrap();
		if let Some(sha) = sha {
			store.document_rendered(&uid, sha, 1).await.unwrap();
		}
	}
	// No invoice behind it: only the sha matters here, so the FK is lifted for the one insert.
	for sql in [
		"PRAGMA foreign_keys = OFF",
		"INSERT INTO invoice_documents (invoice_id, sha256, bytes, template_version, rendered_at)
		 VALUES (999, 'cc33', 1, 'v', 0)",
		"PRAGMA foreign_keys = ON",
	] {
		sqlx::query(sql).execute(store.write_pool()).await.unwrap();
	}

	let acc = AccountId::from_trusted("acc_t".to_owned());
	assert_eq!(store.documents_for_account(&acc).await.unwrap().len(), 4);
	assert_eq!(store.documents_orphaned_by_account(&acc).await.unwrap(), ["aa11"]);
	store.documents_erase_account(&acc).await.unwrap();
	assert!(store.documents_for_account(&acc).await.unwrap().is_empty());
	assert!(store.documents_orphaned_by_account(&acc).await.unwrap().is_empty());
	store.documents_erase_account(&acc).await.unwrap();
}

#[tokio::test]
async fn a_job_result_is_read_back_by_its_dedup_key() {
	let db = TmpDb::new("job-result");
	let store = store(&db).await;

	assert!(store.job_result_by_key("pdf:doc:x").await.unwrap().is_none());
	let id = store
		.job_enqueue("RENDER_DOC", "{}", Some("pdf:doc:x"), Timestamp(0))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(
		store.job_result_by_key("pdf:doc:x").await.unwrap(),
		Some(("PENDING".to_owned(), None))
	);

	store.job_set_result(id, "deadbeef").await.unwrap();
	assert_eq!(store.job_claim(Timestamp(1)).await.unwrap().unwrap().id, id);
	store.job_complete(id, Timestamp(1)).await.unwrap();
	assert_eq!(
		store.job_result_by_key("pdf:doc:x").await.unwrap(),
		Some(("DONE".to_owned(), Some("deadbeef".to_owned())))
	);

	// A keyed row is the record the caller reads back, so the sweep must not reclaim it.
	store.job_sweep(Timestamp(i64::MAX)).await.unwrap();
	assert!(store.job_result_by_key("pdf:doc:x").await.unwrap().is_some());
}

// vim: ts=4
