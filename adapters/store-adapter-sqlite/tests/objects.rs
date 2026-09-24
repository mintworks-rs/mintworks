//! `ObjectStore` conformance tests — what a second store adapter must pass: round-trip
//! put/get/delete, `UNIQUE (org_id, type, uid)` scoped per org, a query by one declared indexed
//! path, index maintenance on overwrite and on reconcile, the `orgs` cascade and the
//! entity-delete sweep. Transaction binding is `tests/tx.rs`, an adapter-internal invariant.
//!
//! Every test opens a real file database. `sqlite::memory:` gives each *connection* its own
//! database, so two stores over one in-memory URL would never contend for the write lock.
//!
//! The service-handle half lives in the feature crates: a test that drives `Invoices` goes in
//! `crates/saas-invoice/tests/`, a test that drives a store trait goes here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_core::ids::SellerId;
use saas_core::objects::{ObjectStore, ObjectType};
use saas_core::{config::Config, prelude::*};
use saas_invoice::store::{InvoiceKind, InvoiceStore, NewInvoice, PaymentMethod, Seller};
use serde_json::json;
use store_adapter_sqlite::SqliteStore;

/// The fixture's org. `migrate` seeds the platform root at id 1 and these tests want nothing of
/// it but a live `orgs` row: the root is found by `kind = 'ROOT'`, never by its value.
const ORG: i64 = 1;

/// The second org, for the per-org assertions.
const OTHER: i64 = 2;

/// `put_seller` does not autoincrement, so the id is chosen here.
const SELLER: i64 = 1;

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-objects-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn path(&self) -> String {
		self.0.join("test.db").to_string_lossy().into_owned()
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn open(db: &TmpDb) -> SqliteStore {
	SqliteStore::open(&Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	})
	.await
	.unwrap()
}

/// Migrations plus the one account the fixture references. No org insert: the root org is
/// already there at id 1, and `objects.org_id` needs nothing more than that.
async fn setup(db: &TmpDb) -> SqliteStore {
	let store = open(db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	store
}

/// A second org, for the per-org and cascade assertions. The parent is the root org, found by
/// `kind = 'ROOT'` and never by its value.
async fn seed_org(store: &SqliteStore, id: i64, uid: &str) {
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

fn seller() -> Seller {
	Seller {
		id: SELLER,
		uid: SellerId::generate(),
		org_id: ORG,
		nav_base_url: "https://api-test.onlineszamla.nav.gov.hu".into(),
		nav_login: None,
		series_code: "A".into(),
		closed_at: None,
		payment_days: None,
		created_at: Timestamp::now(),
	}
}

/// A `DRAFT` invoice, which is all the two deletion paths need — `delete_draft`'s predicate and
/// `sweep_drafts`'s `status IN ('DRAFT','PENDING')` both match it, and no published seller
/// version is required until an invoice is issued.
async fn draft(store: &SqliteStore) -> saas_invoice::store::Invoice {
	store.put_seller(&seller()).await.unwrap();
	store
		.create_draft(&NewInvoice {
			org_id: ORG,
			seller_id: SELLER,
			billing_party_id: None,
			request_id: None,
			kind: InvoiceKind::Normal,
			original_invoice_id: None,
			currency: CurrencyCode::parse("HUF").unwrap(),
			rate_e6: 1_000_000,
			payment_method: PaymentMethod::Transfer,
			notes: None,
			discount_kind: None,
			discount_value: None,
		})
		.await
		.unwrap()
}

#[tokio::test]
async fn put_get_list_and_delete_round_trip() {
	let db = TmpDb::new("round-trip");
	let store = setup(&db).await;

	let put = store
		.object_put(ORG, "booking", "bk_1", &json!({"note": "x"}), &[])
		.await
		.unwrap();
	assert_eq!(put.uid, "bk_1");
	assert_eq!(put.body["note"], "x");

	let got = store.object_get(ORG, "booking", "bk_1").await.unwrap().unwrap();
	assert_eq!(got.id, put.id);
	assert!(store.object_get(ORG, "booking", "missing").await.unwrap().is_none());
	assert_eq!(store.object_list(ORG, "booking", None, 10).await.unwrap().len(), 1);

	assert!(store.object_delete(ORG, "booking", "bk_1").await.unwrap());
	// `false`, not an error: the caller asked for absence and has it.
	assert!(!store.object_delete(ORG, "booking", "bk_1").await.unwrap());
}

#[tokio::test]
async fn one_row_per_org_type_and_uid() {
	let db = TmpDb::new("unique");
	let store = setup(&db).await;
	seed_org(&store, OTHER, "org_other").await;

	let first = store.object_put(ORG, "t", "u", &json!({"a": 1}), &[]).await.unwrap();
	let again = store.object_put(ORG, "t", "u", &json!({"a": 2}), &[]).await.unwrap();
	assert_eq!(again.id, first.id, "the unique constraint is an upsert, not a second row");
	assert_eq!(again.body["a"], 2);
	assert_eq!(again.created_at.0, first.created_at.0, "an overwrite moves updated_at alone");
	assert_eq!(store.object_list(ORG, "t", None, 10).await.unwrap().len(), 1);

	// Per org, never global: the same pair under another org is another object.
	let elsewhere = store.object_put(OTHER, "t", "u", &json!({"a": 3}), &[]).await.unwrap();
	assert_ne!(elsewhere.id, first.id);
	assert_eq!(store.object_list(OTHER, "t", None, 10).await.unwrap().len(), 1);
}

/// The read half of the same rule: another org's `(type, uid)` is a miss, never an error and
/// never the other org's row.
#[tokio::test]
async fn a_read_never_crosses_org() {
	let db = TmpDb::new("cross-org-read");
	let store = setup(&db).await;
	seed_org(&store, OTHER, "org_other").await;

	let indexed = vec!["$.projectUid".to_string()];
	store
		.object_put(ORG, "booking", "bk_1", &json!({"projectUid": "prj_a"}), &indexed)
		.await
		.unwrap();

	assert!(store.object_get(OTHER, "booking", "bk_1").await.unwrap().is_none());
	assert!(
		store
			.object_query(OTHER, "booking", "$.projectUid", "prj_a", None, 10)
			.await
			.unwrap()
			.is_empty()
	);
}

#[tokio::test]
async fn query_matches_one_declared_indexed_path() {
	let db = TmpDb::new("query");
	let store = setup(&db).await;
	let indexed = vec!["$.projectUid".to_string()];

	store
		.object_put(ORG, "booking", "bk_1", &json!({"projectUid": "prj_a"}), &indexed)
		.await
		.unwrap();
	store
		.object_put(ORG, "booking", "bk_2", &json!({"projectUid": "prj_b"}), &indexed)
		.await
		.unwrap();

	let hits = store
		.object_query(ORG, "booking", "$.projectUid", "prj_a", None, 10)
		.await
		.unwrap();
	assert_eq!(hits.len(), 1);
	assert_eq!(hits[0].uid, "bk_1");
	assert!(
		store
			.object_query(ORG, "booking", "$.projectUid", "prj_z", None, 10)
			.await
			.unwrap()
			.is_empty()
	);

	// A declared path the body does not carry indexes as SQL NULL, which `= ?` never matches —
	// not even the string "null".
	store
		.object_put(ORG, "booking", "bk_3", &json!({"other": 1}), &indexed)
		.await
		.unwrap();
	assert!(
		store
			.object_query(ORG, "booking", "$.projectUid", "null", None, 10)
			.await
			.unwrap()
			.is_empty()
	);
}

#[tokio::test]
async fn overwrite_reindexes_the_declared_paths() {
	let db = TmpDb::new("reindex");
	let store = setup(&db).await;
	let indexed = vec!["$.projectUid".to_string()];

	store
		.object_put(ORG, "booking", "bk_1", &json!({"projectUid": "prj_a"}), &indexed)
		.await
		.unwrap();
	store
		.object_put(ORG, "booking", "bk_1", &json!({"projectUid": "prj_b"}), &indexed)
		.await
		.unwrap();
	assert!(
		store
			.object_query(ORG, "booking", "$.projectUid", "prj_a", None, 10)
			.await
			.unwrap()
			.is_empty(),
		"the value the body has dropped must stop answering"
	);

	// A path this write no longer declares loses its row rather than answering from a value the
	// body has dropped.
	store
		.object_put(ORG, "booking", "bk_1", &json!({"projectUid": "prj_b"}), &[])
		.await
		.unwrap();
	assert!(
		store
			.object_query(ORG, "booking", "$.projectUid", "prj_b", None, 10)
			.await
			.unwrap()
			.is_empty()
	);
}

#[tokio::test]
async fn reconcile_adds_and_drops_a_declared_path() {
	let db = TmpDb::new("reconcile");
	let store = setup(&db).await;

	store
		.object_put(
			ORG,
			"booking",
			"bk_1",
			&json!({"a": 1, "b": 2}),
			&["$.a".to_string(), "$.b".to_string()],
		)
		.await
		.unwrap();

	let only_a = [ObjectType { type_name: "booking".into(), paths: vec!["$.a".to_string()] }];
	store.object_index_reconcile(&only_a).await.unwrap();
	assert_eq!(store.object_query(ORG, "booking", "$.a", "1", None, 10).await.unwrap().len(), 1);
	assert!(
		store
			.object_query(ORG, "booking", "$.b", "2", None, 10)
			.await
			.unwrap()
			.is_empty(),
		"a withdrawn declaration loses its index rows"
	);

	// Re-declaring it re-extracts from the body, which the drop did not touch.
	let only_b = [ObjectType { type_name: "booking".into(), paths: vec!["$.b".to_string()] }];
	store.object_index_reconcile(&only_b).await.unwrap();
	assert_eq!(store.object_query(ORG, "booking", "$.b", "2", None, 10).await.unwrap().len(), 1);
	assert!(
		store
			.object_query(ORG, "booking", "$.a", "1", None, 10)
			.await
			.unwrap()
			.is_empty()
	);
}

/// Indexed once, not a primary-key violation: `object_index` is keyed `(object_id, path)`.
#[tokio::test]
async fn a_repeated_indexed_path_is_indexed_once() {
	let db = TmpDb::new("repeated-path");
	let store = setup(&db).await;

	store
		.object_put(
			ORG,
			"booking",
			"bk_1",
			&json!({"a": 1}),
			&["$.a".to_string(), "$.a".to_string()],
		)
		.await
		.unwrap();

	assert_eq!(index_rows(&store).await, 1);
	assert_eq!(store.object_query(ORG, "booking", "$.a", "1", None, 10).await.unwrap().len(), 1);
}

/// One `type_name` declared twice is how a consumer shadows a framework extension type. Merging
/// the two path lists would rewrite both indexes with neither declaration's author knowing.
#[tokio::test]
async fn a_type_declared_twice_is_rejected() {
	let db = TmpDb::new("declared-twice");
	let store = setup(&db).await;
	store
		.object_put(ORG, "booking", "bk_1", &json!({"a": 1}), &["$.a".to_string()])
		.await
		.unwrap();

	let clashing = [
		ObjectType { type_name: "booking".into(), paths: vec!["$.a".to_string()] },
		ObjectType { type_name: "booking".into(), paths: vec!["$.b".to_string()] },
	];
	let err = store.object_index_reconcile(&clashing).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-INTERNAL", "{err:?}");
	assert!(format!("{err:?}").contains("declared twice"), "{err:?}");

	// Nothing ran: the index is what the last `object_put` left.
	assert_eq!(index_rows(&store).await, 1);
	assert_eq!(store.object_query(ORG, "booking", "$.a", "1", None, 10).await.unwrap().len(), 1);
}

/// A reconcile is a full pass over every object of a type, per declared path, and the
/// declaration set almost never changes between boots — so an unchanged one must do nothing.
#[tokio::test]
async fn an_unchanged_declaration_skips_the_reconcile() {
	let db = TmpDb::new("fingerprint");
	let store = setup(&db).await;
	store
		.object_put(ORG, "booking", "bk_1", &json!({"a": 1}), &["$.a".to_string()])
		.await
		.unwrap();

	let declared = [ObjectType { type_name: "booking".into(), paths: vec!["$.a".to_string()] }];
	store.object_index_reconcile(&declared).await.unwrap();

	// A sentinel no re-extraction would leave standing: it survives only if the second call did
	// not touch the row.
	sqlx::query("UPDATE object_index SET value = 'sentinel' WHERE path = '$.a'")
		.execute(store.write_pool())
		.await
		.unwrap();
	store.object_index_reconcile(&declared).await.unwrap();
	assert_eq!(
		store
			.object_query(ORG, "booking", "$.a", "sentinel", None, 10)
			.await
			.unwrap()
			.len(),
		1,
		"the same declaration set reconciled again"
	);

	// And a changed set still lands: the gate is the declaration, not a one-shot.
	let changed =
		[
			ObjectType {
				type_name: "booking".into(),
				paths: vec!["$.a".to_string(), "$.b".into()],
			},
		];
	store.object_index_reconcile(&changed).await.unwrap();
	assert_eq!(store.object_query(ORG, "booking", "$.a", "1", None, 10).await.unwrap().len(), 1);
}

/// `object_index` rows over the whole database, for the assertions about how many a write left.
async fn index_rows(store: &SqliteStore) -> i64 {
	sqlx::query_scalar("SELECT COUNT(*) FROM object_index")
		.fetch_one(store.read_pool())
		.await
		.unwrap()
}

#[tokio::test]
async fn deleting_an_org_cascades_its_objects() {
	let db = TmpDb::new("org-cascade");
	let store = setup(&db).await;
	seed_org(&store, OTHER, "org_other").await;

	store
		.object_put(
			OTHER,
			"booking",
			"bk_1",
			&json!({"projectUid": "prj_a"}),
			&["$.projectUid".to_string()],
		)
		.await
		.unwrap();
	assert!(store.object_get(OTHER, "booking", "bk_1").await.unwrap().is_some());

	// At the SQL level, because `AuthStore::delete_org` refuses an org that still holds `objects`
	// — the FK is the backstop for a consumer deleting an org by its own statement.
	sqlx::query("DELETE FROM orgs WHERE id = ?")
		.bind(OTHER)
		.execute(store.write_pool())
		.await
		.unwrap();
	assert!(store.object_get(OTHER, "booking", "bk_1").await.unwrap().is_none());
	assert!(
		store
			.object_query(OTHER, "booking", "$.projectUid", "prj_a", None, 10)
			.await
			.unwrap()
			.is_empty()
	);
}

#[tokio::test]
async fn deleting_a_draft_takes_its_ext_blob() {
	let db = TmpDb::new("sweep-draft");
	let store = setup(&db).await;
	let inv = draft(&store).await;
	store
		.object_put(ORG, "invoice.ext", inv.uid.as_str(), &json!({"note": "x"}), &[])
		.await
		.unwrap();

	assert!(store.delete_draft(inv.id).await.unwrap());
	assert!(store.object_get(ORG, "invoice.ext", inv.uid.as_str()).await.unwrap().is_none());
}

#[tokio::test]
async fn the_stale_draft_sweep_takes_the_ext_blob_with_it() {
	let db = TmpDb::new("sweep-stale");
	let store = setup(&db).await;
	let inv = draft(&store).await;
	store
		.object_put(ORG, "invoice.ext", inv.uid.as_str(), &json!({"note": "x"}), &[])
		.await
		.unwrap();

	// A horizon one second ahead: `updated_at < ?` matches the draft, and `LIVE` lets it through
	// because no payment row is linked to it.
	let swept = store.sweep_drafts(Timestamp(Timestamp::now().0 + 1)).await.unwrap();
	assert_eq!(swept, 1);
	assert!(store.object_get(ORG, "invoice.ext", inv.uid.as_str()).await.unwrap().is_none());
}

/// The negative half of the sweep: it is keyed to the row that went, not to the type name.
#[tokio::test]
async fn a_party_delete_that_matches_nothing_leaves_the_ext_blob() {
	let db = TmpDb::new("sweep-miss");
	let store = setup(&db).await;
	let uid = PartyId::generate();
	store
		.object_put(ORG, "party.ext", uid.as_str(), &json!({"note": "x"}), &[])
		.await
		.unwrap();

	assert!(!store.delete_party(ORG, &uid).await.unwrap());
	assert!(store.object_get(ORG, "party.ext", uid.as_str()).await.unwrap().is_some());
}

/// The positive half of the sweep: deleting the party row takes the ext blob keyed to its uid.
/// The `(type, uid)` pair is polymorphic and carries no FK, so this is the only thing that does.
#[tokio::test]
async fn deleting_a_party_takes_its_ext_blob() {
	let db = TmpDb::new("sweep-hit");
	let store = setup(&db).await;
	let uid = PartyId::generate();
	store
		.object_put(ORG, "party.ext", uid.as_str(), &json!({"note": "x"}), &[])
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties (uid, org_id, kind, name, country, created_at, updated_at)
		 VALUES (?, ?, 'C', 'Teszt Kft.', 'HU', 0, 0)",
	)
	.bind(uid.as_str())
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();

	assert!(store.delete_party(ORG, &uid).await.unwrap());
	assert!(store.object_get(ORG, "party.ext", uid.as_str()).await.unwrap().is_none());
}

// vim: ts=4
