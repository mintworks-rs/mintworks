//! `DocumentStore` and the `jobs.result` column `RENDER_DOC` hands its sha256 back through —
//! the conformance a store adapter must pass for `mintworks-pdf`.

use mintworks_core::{
	ids::{AccountId, DocId},
	store::CoreStore,
	types::Timestamp,
};
use mintworks_invoice::store::InvoiceStore;
use mintworks_pdf::DocumentStore;
use serde_json::json;

use crate::invoice::{issued, setup};
use crate::{Harness, fresh};

async fn seed_org<H: Harness>(h: &H, id: i64, uid: &str) {
	h.exec(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)
		 ON CONFLICT DO NOTHING",
		&[],
	)
	.await;
	h.exec(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (?, ?, (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
		&[json!(id), json!(uid)],
	)
	.await;
}

pub async fn a_document_is_pending_until_rendered_and_confined_to_its_org<H: Harness>()
where
	H::Store: DocumentStore + CoreStore,
{
	let h = fresh!(H, "pdf-documents");
	let store = h.store();
	seed_org(&h, 100, "org_a").await;
	seed_org(&h, 101, "org_b").await;

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

pub async fn deleting_the_org_cascades_to_its_documents<H: Harness>()
where
	H::Store: DocumentStore + CoreStore,
{
	let h = fresh!(H, "pdf-documents-cascade");
	let store = h.store();
	seed_org(&h, 100, "org_a").await;
	let uid = DocId::generate();
	store.document_insert(100, &uid, "t.typ", "k").await.unwrap();

	h.exec("DELETE FROM orgs WHERE id = 100", &[]).await;
	assert!(store.document_get(100, &uid).await.unwrap().is_none());
}

/// Files are content-addressed and shared: erasure may hand back only a sha nothing else uses.
pub async fn account_erasure_returns_only_unshared_files<H: Harness>()
where
	H::Store: DocumentStore + CoreStore + InvoiceStore,
{
	let h = fresh!(H, "pdf-documents-erase");
	// The invoice fixture, so `invoice_documents` has a real invoice to name: only its sha matters.
	setup(&h).await;
	let store = h.store();
	let inv = issued(store).await;
	seed_org(&h, 100, "org_a").await;
	h.exec(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (102, 'org_p', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'PERSONAL', 'P', 1, 0)",
		&[],
	)
	.await;
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
	h.exec(
		"INSERT INTO invoice_documents (invoice_id, sha256, bytes, template_version, rendered_at)
		 VALUES (?, 'cc33', 1, 'v', 0)",
		&[json!(inv.id)],
	)
	.await;

	let acc = AccountId::from_trusted("acc_t".to_owned());
	assert_eq!(store.documents_for_account(&acc).await.unwrap().len(), 4);
	assert_eq!(store.documents_orphaned_by_account(&acc).await.unwrap(), ["aa11"]);
	store.documents_erase_account(&acc).await.unwrap();
	assert!(store.documents_for_account(&acc).await.unwrap().is_empty());
	assert!(store.documents_orphaned_by_account(&acc).await.unwrap().is_empty());
	store.documents_erase_account(&acc).await.unwrap();
}

pub async fn a_job_result_is_read_back_by_its_dedup_key<H: Harness>()
where
	H::Store: DocumentStore + CoreStore,
{
	let h = fresh!(H, "pdf-job-result");
	let store = h.store();

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
