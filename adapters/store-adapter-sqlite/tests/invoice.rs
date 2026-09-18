//! `InvoiceStore` conformance tests — what a second store adapter must pass: gapless numbering
//! under contention, a rolled-back issue consuming no number, immutability after ISSUE, the
//! terminal-status guards, the storno chain at row level, `request_id` idempotency, and the
//! indexes the sweeps and the audit export are selected by.
//!
//! Every test opens a real file database. `sqlite::memory:` gives each *connection* its own
//! database, so two stores over one in-memory URL would never contend for the write lock.
//!
//! The service-handle half lives in `crates/saas-invoice/tests/invoice_service.rs`: a test that
//! drives `Invoices` goes there, a test that drives `InvoiceStore` goes here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_core::{config::Config, prelude::*};
use saas_invoice::{
	draft::Priced,
	service_api::SELLER_ID,
	store::{
		BuyerSnapshot, Invoice, InvoiceKind, InvoicePatch, InvoiceStatus, InvoiceStore,
		InvoiceVatGroup, IssueInvoice, NewInvoice, NewInvoiceLine, PartyKind, PaymentMethod,
		Seller, SellerVersionPatch, SellerVersionStatus,
	},
	vat::VatCode,
};
use store_adapter_sqlite::SqliteStore;

const TENANT: i64 = 1;

/// The version `seed_seller` publishes. It is the first row `seller_versions` ever gets, so it
/// is `1`; a test that publishes a second one names the id `publish_seller_version` returned.
const SELLER_VER: i64 = 1;

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-invoice-test-{}-{name}", std::process::id()));
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

/// Migrations plus the minimum the foreign keys demand: one account, one tenant, seller 1.
/// `HUF` is already seeded by the `saas-invoice/init` step.
async fn setup(db: &TmpDb) -> SqliteStore {
	let store = open(db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
	)
	.execute(store.writer())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO tenants (id, uid, kind, name, owner_account_id, created_at)
		 VALUES (?, 'tnt_t', 'O', 'Teszt', 1, 0)",
	)
	.bind(TENANT)
	.execute(store.writer())
	.await
	.unwrap();

	seed_seller(&store).await;
	store
}

/// `put_seller` plus one published version — the least a suite needs before an invoice can be
/// issued, since `issue` refuses a seller with no `CURRENT` version.
async fn seed_seller(store: &SqliteStore) {
	store.put_seller(&seller()).await.unwrap();
	store.save_seller_version_draft(SELLER_ID, &seller_version()).await.unwrap();
	store
		.publish_seller_version(SELLER_ID, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();
}

fn seller() -> Seller {
	Seller {
		id: SELLER_ID,
		nav_base_url: "https://api-test.onlineszamla.nav.gov.hu".into(),
		nav_login: None,
		series_code: "A".into(),
		created_at: Timestamp::now(),
	}
}

/// The statutory half, as the draft `seed_seller` publishes. Split from [`seller`] because it
/// is versioned: an invoice freezes the version, `sellers` keeps only what stays live.
fn seller_version() -> SellerVersionPatch {
	SellerVersionPatch {
		name: Some("Teszt Kft.".into()),
		country: Some("HU".into()),
		tax_number: Some("12345678242".into()),
		postcode: Some("1011".into()),
		city: Some("Budapest".into()),
		street: Some("Fo utca 1.".into()),
		vat_scheme: Some("NORMAL".into()),
		..Default::default()
	}
}

fn new_invoice(request_id: Option<&str>, kind: InvoiceKind, original: Option<i64>) -> NewInvoice {
	NewInvoice {
		tenant_id: TENANT,
		seller_id: SELLER_ID,
		billing_party_id: None,
		request_id: request_id.map(str::to_string),
		kind,
		original_invoice_id: original,
		currency: CurrencyCode::parse("HUF").unwrap(),
		rate_e6: 1_000_000,
		payment_method: PaymentMethod::Transfer,
		notes: None,
		discount_kind: None,
		discount_value: None,
	}
}

async fn draft(store: &SqliteStore, request_id: Option<&str>) -> Invoice {
	store
		.create_draft(&new_invoice(request_id, InvoiceKind::Normal, None))
		.await
		.unwrap()
}

/// One 27% group of `net` fillér. HUF, so `huf_rate_e6` and the `*_huf` trio stay NULL.
fn issue_input(invoice_id: i64, net: i64) -> IssueInvoice {
	let vat = net * 2700 / 10000;
	IssueInvoice {
		series_code: "A".into(),
		series_year: 2026,
		issued_at: Timestamp::now(),
		fulfilment_date: "2026-01-31".into(),
		due_date: Some("2026-02-08".into()),
		rate_date: None,
		rate_source: None,
		huf_rate_e6: None,
		rate_e6: None,
		net: Money(net),
		vat: Money(vat),
		gross: Money(net + vat),
		vat_note: None,
		seller_ver: SELLER_VER,
		buyer: BuyerSnapshot {
			kind: PartyKind::Company,
			name: "Vevo Zrt.".into(),
			country: "HU".into(),
			tax_number: Some("87654321242".into()),
			eu_vat_id: None,
			group_tax_no: None,
			postcode: Some("1052".into()),
			city: Some("Budapest".into()),
			street: Some("Deak ter 2.".into()),
			vies_request_id: None,
			vies_checked_at: None,
		},
		lines: vec![NewInvoiceLine {
			service_id: None,
			description: "Tanacsadas".into(),
			unit: "ora".into(),
			qty: Qty(1_000_000),
			unit_price: Money(net),
			discount_kind: None,
			discount_value: None,
			discount_amount: Money(0),
			discount_description: None,
			net: Money(net),
			vat_code: VatCode::Std27,
			vat_rate_bp: 2700,
			vat: Money(vat),
			gross: Money(net + vat),
			note: None,
		}],
		groups: vec![InvoiceVatGroup {
			invoice_id,
			vat_code: VatCode::Std27,
			vat_rate_bp: 2700,
			net: Money(net),
			vat: Money(vat),
			gross: Money(net + vat),
			net_huf: None,
			vat_huf: None,
			gross_huf: None,
		}],
	}
}

/// An issued invoice — `number IS NOT NULL`, which is the predicate both index
/// assertions below turn on. The dates are `issue_input`'s and do not matter: nothing
/// here runs `ANALYZE`, so the plans are data-independent.
async fn issued(store: &SqliteStore) -> Invoice {
	let inv = draft(store, None).await;
	store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();
	inv
}

/// One rejected `CREATE` filing, so `idx_nav_submission_live` has a row to serve.
async fn fail_submission(store: &SqliteStore, invoice_id: i64) {
	use saas_nav::{NavOp, NavVerdict, store::NavStore};

	let id = store
		.create_submission(invoice_id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	store
		.finish(
			id,
			Some(NavVerdict::Rejected),
			Some(("INVOICE_NUMBER_NOT_UNIQUE", "dup")),
			Timestamp::now(),
		)
		.await
		.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_issue_is_unique_and_gapless() {
	const N: i64 = 8;

	let db = TmpDb::new("gapless");
	let store = setup(&db).await;

	let mut ids = Vec::new();
	for _ in 0..N {
		let d = draft(&store, None).await;
		ids.push((d.id, d.version));
	}

	// A second store over the same file. The writer pool is one connection, so two tasks on
	// a single store would queue in the pool instead of racing for the SQLite write lock.
	let other = open(&db).await;

	let mut tasks = Vec::new();
	for (i, (id, version)) in ids.into_iter().enumerate() {
		let s = if i % 2 == 0 { store.clone() } else { other.clone() };
		tasks.push(tokio::spawn(
			async move { s.issue(id, &issue_input(id, 100_000), version).await },
		));
	}

	let mut numbers = Vec::new();
	for t in tasks {
		numbers.push(t.await.unwrap().unwrap().number.unwrap());
	}
	numbers.sort();

	let expected: Vec<String> = (1..=N).map(|n| format!("A2026/{n:06}")).collect();
	assert_eq!(numbers, expected, "numbers must be unique and gapless");
}

#[tokio::test]
async fn rolled_back_issue_consumes_no_number() {
	let db = TmpDb::new("rollback");
	let store = setup(&db).await;

	let first = draft(&store, None).await;
	store
		.issue(first.id, &issue_input(first.id, 100_000), first.version)
		.await
		.unwrap();

	// Rewind the series so the next allocation renders a number that already exists:
	// `idx_invoice_number` rejects the freeze, and the whole issue transaction rolls back.
	sqlx::query("UPDATE doc_series SET next_no = 1")
		.execute(store.writer())
		.await
		.unwrap();

	let second = draft(&store, None).await;
	assert!(
		store
			.issue(second.id, &issue_input(second.id, 100_000), second.version)
			.await
			.is_err()
	);

	let next_no: i64 = sqlx::query_scalar("SELECT next_no FROM doc_series")
		.fetch_one(store.reader())
		.await
		.unwrap();
	assert_eq!(next_no, 1, "a rolled-back issue must not consume a number");

	let again = store.invoice_by_id(second.id).await.unwrap().unwrap();
	assert!(matches!(again.status, InvoiceStatus::Draft));
	assert!(again.number.is_none());
}

#[tokio::test]
async fn issued_invoice_cannot_be_mutated() {
	let db = TmpDb::new("immutable");
	let store = setup(&db).await;

	let inv = draft(&store, None).await;
	let issued = store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();

	assert!(store.update_draft(inv.id, &InvoicePatch::default()).await.unwrap().is_none());
	assert!(
		!store
			.replace_draft_lines(
				inv.id,
				None,
				&Priced {
					lines: vec![],
					groups: vec![],
					net: Money(1),
					vat: Money(1),
					gross: Money(2),
				},
				issued.version,
			)
			.await
			.unwrap()
	);
	assert!(!store.delete_draft(inv.id).await.unwrap());

	let after = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(after.gross, issued.gross);
	assert_eq!(after.number, issued.number);
}

/// `set_invoice_status` took any `InvoiceStatus` and the adapter rejected the illegal ones in
/// SQL — the state machine living in the adapter. `mark_paid` and `mark_stornoed` replace it:
/// both hardcode `ISSUED` as the predecessor, so `-> DRAFT` and `-> ISSUED` are not writable
/// at all. The `trg_invoice_status_transition` trigger this replaces existed because
/// `set_invoice_status` let a caller name `STORNOED -> ISSUED` and only SQL refused it. With
/// the method split no trait call moves a terminal invoice anywhere — pinned here, because a
/// future `mark_*` with a looser predicate would resurrect one silently. Raw SQL against the
/// tables is the consumer's own problem, by contract.
#[tokio::test]
async fn no_trait_path_leads_out_of_a_terminal_status() {
	let db = TmpDb::new("transitions");
	let store = setup(&db).await;

	for (terminal, set_paid_ok) in [(InvoiceStatus::Paid, true), (InvoiceStatus::Stornoed, false)] {
		let inv = draft(&store, None).await;
		let issued = store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();
		// The one legal move in, from ISSUED.
		assert!(if terminal == InvoiceStatus::Paid {
			store.mark_paid(inv.id).await.unwrap()
		} else {
			store.mark_stornoed(inv.id).await.unwrap()
		});

		// Every trait path out, all of which must miss.
		assert!(!store.mark_paid(inv.id).await.unwrap(), "{terminal:?}");
		assert!(!store.mark_stornoed(inv.id).await.unwrap(), "{terminal:?}");
		assert!(
			store.update_draft(inv.id, &InvoicePatch::default()).await.unwrap().is_none(),
			"{terminal:?}"
		);
		assert!(!store.delete_draft(inv.id).await.unwrap(), "{terminal:?}");
		// `set_paid` is the exception: recording an amount against a paid invoice is a
		// permitted post-issue write, against a cancelled one it is not.
		assert_eq!(
			store.set_paid(inv.id, issued.gross, Some(Timestamp::now())).await.unwrap(),
			set_paid_ok,
			"{terminal:?}"
		);

		let after = store.invoice_by_id(inv.id).await.unwrap().unwrap();
		assert_eq!(after.status, terminal);
		assert_eq!(after.number, issued.number);
	}

	// A miss is reported, not swallowed — `set_paid` used to discard `rows_affected`, so a
	// payment allocated against a wrong id reported success while the amount landed nowhere.
	assert!(!store.mark_paid(9_999).await.unwrap());
	assert!(!store.set_paid(9_999, Money(1), None).await.unwrap());
}

/// `sweep_drafts` deletes *abandoned* drafts, and it keyed on `created_at` — so a cart
/// opened a month ago and edited this morning was destroyed under the customer.
#[tokio::test]
async fn the_sweep_spares_a_draft_that_is_still_being_edited() {
	let db = TmpDb::new("sweep-age");
	let store = setup(&db).await;

	let now = Timestamp::now().0;
	let old = now - 40 * 86_400;
	let cutoff = Timestamp(now - 30 * 86_400);

	let live = draft(&store, Some("live")).await;
	let abandoned = draft(&store, Some("abandoned")).await;
	// Both created well before the cutoff; only one has been touched since.
	sqlx::query("UPDATE invoices SET created_at = ?, updated_at = ? WHERE id = ?")
		.bind(old)
		.bind(now)
		.bind(live.id)
		.execute(store.writer())
		.await
		.unwrap();
	sqlx::query("UPDATE invoices SET created_at = ?, updated_at = ? WHERE id = ?")
		.bind(old)
		.bind(old)
		.bind(abandoned.id)
		.execute(store.writer())
		.await
		.unwrap();

	assert_eq!(store.sweep_drafts(cutoff).await.unwrap(), 1);
	assert!(store.invoice_by_id(live.id).await.unwrap().is_some(), "an edited cart survives");
	assert!(store.invoice_by_id(abandoned.id).await.unwrap().is_none());
}

#[tokio::test]
async fn storno_negates_and_happens_once() {
	let db = TmpDb::new("storno");
	let store = setup(&db).await;

	let inv = draft(&store, None).await;
	store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();

	let new = new_invoice(None, InvoiceKind::Storno, Some(inv.id));
	let st = store.storno(inv.id, &new, &issue_input(0, -100_000)).await.unwrap();

	assert!(matches!(st.kind, InvoiceKind::Storno));
	assert_eq!(st.original_invoice_id, Some(inv.id));
	assert_eq!(st.number.as_deref(), Some("A2026/000002"), "same series as the original");
	assert_eq!(st.gross, Money(-127_000));
	assert!(matches!(
		store.invoice_by_id(inv.id).await.unwrap().unwrap().status,
		InvoiceStatus::Stornoed
	));

	// At most once, whichever guard gets there first.
	assert!(store.storno(inv.id, &new, &issue_input(0, -100_000)).await.is_err());
}

/// `insert_draft` mapped *every* unique violation to `duplicate_request_id`, so a caller that
/// sent no `request_id` got a 409 naming a key it never sent. Here the collision is
/// `idx_invoice_storno_once`.
#[tokio::test]
async fn a_conflict_without_a_request_id_does_not_name_one() {
	let db = TmpDb::new("requestid-none");
	let store = setup(&db).await;

	let original = draft(&store, None).await;
	let cancel = new_invoice(None, InvoiceKind::Storno, Some(original.id));
	store.create_draft(&cancel).await.unwrap();

	let err = store.create_draft(&cancel).await.unwrap_err().to_string();
	assert!(!err.contains("request_id"), "{err}");

	// And the idempotency-replay answer is unchanged where a key really was sent.
	let keyed = draft(&store, Some("req-1")).await;
	let err = store
		.create_draft(&new_invoice(Some("req-1"), InvoiceKind::Normal, None))
		.await
		.unwrap_err()
		.to_string();
	assert!(err.contains("request_id"), "{err}");
	assert_eq!(
		store.invoice_by_request_id(TENANT, "req-1").await.unwrap().map(|i| i.id),
		Some(keyed.id)
	);
}

/// The uniqueness used to be global, so one tenant taking `"sub-2026-01"` made every other
/// tenant's subscription job answer `404` on a *create*, permanently, for that key. Any tenant
/// could squat any other tenant's natural idempotency keys.
#[tokio::test]
async fn two_tenants_can_hold_the_same_request_id() {
	let db = TmpDb::new("requestid-tenants");
	let store = setup(&db).await;
	sqlx::query(
		"INSERT INTO tenants (id, uid, kind, name, owner_account_id, created_at)
		 VALUES (2, 'tnt_u', 'O', 'Masik', 1, 0)",
	)
	.execute(store.writer())
	.await
	.unwrap();

	let mine = draft(&store, Some("sub-2026-01")).await;
	let theirs = store
		.create_draft(&NewInvoice {
			tenant_id: 2,
			..new_invoice(Some("sub-2026-01"), InvoiceKind::Normal, None)
		})
		.await
		.expect("another tenant's key is not this tenant's key");

	assert_ne!(mine.id, theirs.id);
	// And each lookup stays inside its own tenant.
	assert_eq!(
		store.invoice_by_request_id(TENANT, "sub-2026-01").await.unwrap().map(|i| i.id),
		Some(mine.id)
	);
	assert_eq!(
		store.invoice_by_request_id(2, "sub-2026-01").await.unwrap().map(|i| i.id),
		Some(theirs.id)
	);
}

/// `doc_series`' primary key includes `year`, so the first invoice of every January took
/// the INSERT path and picked up the column default. Nothing anywhere writes `format`, so an
/// operator's hand-set custom format silently reverted at each year boundary — on numbers that
/// are statutorily immutable once issued. It is carried forward from the previous year now.
#[tokio::test]
async fn a_custom_series_format_survives_the_year_boundary() {
	let db = TmpDb::new("series-format-year");
	let store = setup(&db).await;

	let first = draft(&store, None).await;
	let issued = store
		.issue(first.id, &issue_input(first.id, 100_000), first.version)
		.await
		.unwrap();
	assert_eq!(issued.number.as_deref(), Some("A2026/000001"));

	// What an operator does by hand: there is no endpoint for it.
	sqlx::query("UPDATE doc_series SET format = 'SZLA-{year}-{no:04}'")
		.execute(store.writer())
		.await
		.unwrap();

	let second = draft(&store, None).await;
	let mut input = issue_input(second.id, 100_000);
	input.series_year = 2027;
	let next_year = store.issue(second.id, &input, second.version).await.unwrap();
	assert_eq!(
		next_year.number.as_deref(),
		Some("SZLA-2027-0001"),
		"the new year's series row reverted to the schema default"
	);

	// The carried format is on the row, not only in the rendering.
	let format: String = sqlx::query_scalar("SELECT format FROM doc_series WHERE year = 2027")
		.fetch_one(store.reader())
		.await
		.unwrap();
	assert_eq!(format, "SZLA-{year}-{no:04}");

	// A series that has never been used still starts from the schema default.
	let third = draft(&store, None).await;
	let mut input = issue_input(third.id, 100_000);
	input.series_code = "B".into();
	let fresh_series = store.issue(third.id, &input, third.version).await.unwrap();
	assert_eq!(fresh_series.number.as_deref(), Some("B2026/000001"));
}

/// The PDF sweep took the fifty lowest ids, so fifty issued invoices whose `RENDER_PDF`
/// fails permanently held the whole `invoice.pdf_sweep_batch` cap and every newer invoice with
/// a missing PDF was never re-enqueued. Ordering by the last render attempt is what breaks the
/// fixed set — the same property `NavStore::unfiled_invoices` needs.
#[tokio::test]
async fn the_pdf_sweep_returns_the_least_recently_attempted_first() {
	let db = TmpDb::new("pdf-sweep-order");
	let store = setup(&db).await;

	// A draft is not numbered, so it is not the sweep's business.
	let never_issued = draft(&store, None).await;

	let mut issued = Vec::new();
	for net in [100_000, 200_000, 300_000] {
		let d = draft(&store, None).await;
		store.issue(d.id, &issue_input(d.id, net), d.version).await.unwrap();
		issued.push(d.id);
	}

	// The one with a document is done and drops out.
	let done = store.invoice_by_id(issued[0]).await.unwrap().unwrap();
	store
		.put_invoice_document(
			&saas_invoice::store::InvoiceDocument {
				invoice_id: issued[0],
				kind: "PDF".into(),
				sha256: "deadbeef".into(),
				bytes: 1,
				template_version: "v1".into(),
				rendered_at: Timestamp::now(),
			},
			done.version,
		)
		.await
		.unwrap();

	let found = store.issued_without_document(10).await.unwrap();
	assert_eq!(found, vec![issued[1], issued[2]], "numbered, no document, oldest attempt first");
	assert!(!found.contains(&never_issued.id));

	// A permanently failing render leaves a FAILED job row behind, and the ordering finds it by
	// payload rather than by `dedup_key` — this row has none. That attempt must push the
	// invoice behind the one nothing has tried yet, whatever their ids say.
	sqlx::query(
		r#"INSERT INTO jobs (kind, payload, status, run_at, created_at, done_at, last_error)
		   VALUES ('RENDER_PDF', '{"invoiceId":' || ? || '}', 'FAILED', 0, ?, ?, 'typst blew up')"#,
	)
	.bind(issued[1])
	.bind(Timestamp::now().0)
	.bind(Timestamp::now().0)
	.execute(store.writer())
	.await
	.unwrap();

	assert_eq!(
		store.issued_without_document(10).await.unwrap(),
		vec![issued[2], issued[1]],
		"the just-retried invoice goes to the back, so a full batch of failures cannot starve it"
	);

	// And the cap still applies: one slot, and it goes to the least recently attempted.
	assert_eq!(store.issued_without_document(1).await.unwrap(), vec![issued[2]]);
}

/// The sweep used to `LEFT JOIN (… GROUP BY invoice_id)` with no predicate, so every
/// hourly tick re-aggregated `nav_submissions` whole, and the outer `invoices.seller_id`
/// filter had no index either. Both grew linearly with invoice volume forever.
///
/// No migration was needed: `idx_invoice_number` (`invoices(seller_id, number) WHERE number IS
/// NOT NULL`) is exactly the outer predicate, and `idx_nav_submission_live`
/// (`nav_submissions(invoice_id, op)`) serves the `NOT EXISTS`.
///
/// `UNFILED` is the statement `unfiled_invoices` actually runs, exported for this test so
/// the plan is checked against the query rather than a copy of it that can drift.
#[tokio::test]
async fn the_sweep_selection_is_served_by_indexes() {
	let db = TmpDb::new("sweep-plan");
	let store = setup(&db).await;
	let invoice = issued(&store).await;
	fail_submission(&store, invoice.id).await;

	// `EXPLAIN QUERY PLAN` answers (id, parent, notused, detail); only the last is readable.
	// `sqlx::query` takes `&'static str`; the statement is a compile-time constant with the
	// `EXPLAIN` prefix glued on, so leaking it in a test costs one allocation.
	let sql: &'static str =
		Box::leak(format!("EXPLAIN QUERY PLAN {}", store_adapter_sqlite::UNFILED).into_boxed_str());
	let rows = sqlx::query(sql).fetch_all(store.reader()).await.unwrap();
	let plan = rows
		.iter()
		.map(|r| sqlx::Row::get::<String, _>(r, "detail"))
		.collect::<Vec<_>>()
		.join("\n");

	assert!(plan.contains("idx_invoice_number"), "the seller filter is a full scan:\n{plan}");
	assert!(
		plan.contains("idx_nav_submission_live"),
		"the filing-record lookup is a full scan:\n{plan}"
	);
	assert!(
		!plan.contains("GROUP BY"),
		"the unpredicated aggregate is back; it re-reads every submission row ever \
		 written on every tick:\n{plan}"
	);
}

/// The batch leader's candidate selection runs once per `NAV_REPORT` job, so it is on the hot
/// path the sweep is not. It is `UNFILED`'s shape plus `kind = 'NORMAL'`, `id <> ?` and an
/// optional `invoice_documents` existence test, all of which are row filters over an index
/// seek — none of them may turn the outer selection into a scan.
#[tokio::test]
async fn the_batch_candidate_selection_is_served_by_indexes() {
	let db = TmpDb::new("batch-candidates-plan");
	let store = setup(&db).await;
	let invoice = issued(&store).await;
	fail_submission(&store, invoice.id).await;

	let sql: &'static str = Box::leak(
		format!("EXPLAIN QUERY PLAN {}", store_adapter_sqlite::BATCH_CANDIDATES).into_boxed_str(),
	);
	let rows = sqlx::query(sql).fetch_all(store.reader()).await.unwrap();
	let plan = rows
		.iter()
		.map(|r| sqlx::Row::get::<String, _>(r, "detail"))
		.collect::<Vec<_>>()
		.join("\n");

	assert!(plan.contains("idx_invoice_number"), "the seller filter is a full scan:\n{plan}");
	assert!(
		plan.contains("idx_nav_submission_live"),
		"the filing-record lookup is a full scan:\n{plan}"
	);
	assert!(
		!plan.contains("SCAN invoice_documents"),
		"the document test scans; `invoice_documents` is keyed on (invoice_id, kind):\n{plan}"
	);
}

/// `idx_invoice_issued` was `ON invoices(issued_at) WHERE status <> 'DRAFT'`, and the
/// audit export it exists for filters `number IS NOT NULL` — which does not *imply*
/// `status <> 'DRAFT'`, so SQLite refused the partial index and fell back to
/// `idx_invoice_number(seller_id, number)`, filtering `issued_at` row by row. With
/// `SELLER_ID = 1` everywhere that made a one-month export a scan of every invoice ever
/// issued. `idx_invoice_issued` is shaped `(seller_id, issued_at)
/// WHERE number IS NOT NULL`, which is the query's own predicate.
///
/// The two selections are exported so the plan is checked against the statements the store
/// runs rather than a copy that can drift — the same reason `UNFILED` is.
#[tokio::test]
async fn the_audit_export_selections_are_served_by_indexes() {
	let db = TmpDb::new("export-plan");
	let store = setup(&db).await;
	let _ = issued(&store).await;

	let plan_of = |sql: &str| {
		let leaked: &'static str = Box::leak(format!("EXPLAIN QUERY PLAN {sql}").into_boxed_str());
		let store = store.clone();
		async move {
			sqlx::query(leaked)
				.fetch_all(store.reader())
				.await
				.unwrap()
				.iter()
				.map(|r| sqlx::Row::get::<String, _>(r, "detail"))
				.collect::<Vec<_>>()
				.join("\n")
		}
	};

	// What is asserted is the `sel` CTE — the selection that grows with the seller's history
	// that `idx_invoice_issued` exists to bound. `close_over_storno_pairs!` used to add a
	// `SCAN invoices` on top of it; re-applying `seller_id`/`number IS NOT NULL` there — which
	// it needs for correctness anyway — put the closure on `idx_invoice_number` instead.
	let plan = plan_of(store_adapter_sqlite::BY_DATE).await;
	assert!(
		plan.contains("idx_invoice_issued (seller_id=? AND issued_at>? AND issued_at<?)"),
		"the date range is not served by the index that exists for it:\n{plan}"
	);
	assert_eq!(plan.matches("SCAN invoices").count(), 0, "{plan}");

	// The number export is served by `idx_invoice_number`, asserted here so a later index
	// change cannot quietly cost it.
	let plan = plan_of(store_adapter_sqlite::BY_NUMBER).await;
	assert!(plan.contains("idx_invoice_number"), "the number export is a scan:\n{plan}");
	assert_eq!(plan.matches("SCAN invoices").count(), 0, "{plan}");
}

/// `currency::price_in` is `base * 1e6 * (10_000 + fee_bp) / (rate_e6 * 10_000)`, so a seeded
/// `fee_bp = -10000` zeroes the numerator and a 40 000.00 HUF item issues, files and prints at
/// 0.00. A `price_round_step` of 0 makes `round_to_step` a 500 on every catalogue line while
/// `ensure_price_on_step` waves ad-hoc prices through.
#[tokio::test]
async fn a_currency_refuses_a_markup_or_a_step_that_would_misprice() {
	let db = TmpDb::new("currency-checks");
	let store = setup(&db).await;

	for (code, step, fee_bp) in [
		("AAA", 1, -10000),    // zeroes every converted price
		("BBB", 1, -250),      // a silent 2.5% undercharge
		("CCC", 1, 1_000_001), // past the 100x ceiling the i128 product is sized for
		("DDD", 0, 0),         // the two halves of the rounding rule disagree
	] {
		sqlx::query(
			"INSERT INTO currencies (code, price_round_step, mode, fee_bp) VALUES (?, ?, 'OFFICIAL', ?)",
		)
		.bind(code)
		.bind(step)
		.bind(fee_bp)
		.execute(store.writer())
		.await
		.expect_err("the CHECK has to refuse this");
	}

	// The bounds themselves are legal: no markup, and a 100x ceiling.
	sqlx::query(
		"INSERT INTO currencies (code, price_round_step, mode, fee_bp)
		 VALUES ('EEE', 1, 'OFFICIAL', 0), ('FFF', 100, 'OFFICIAL', 1000000)",
	)
	.execute(store.writer())
	.await
	.unwrap();
}

/// The two CHECKs that make `saas_nav::export::original_number`'s refusals unreachable: an
/// audit export names a storno's original, and a blank `originalInvoiceNumber` is a file the
/// tax authority reads as a plain invoice. A second adapter owes both.
#[tokio::test]
async fn a_storno_always_has_a_numbered_original_to_name() {
	let db = TmpDb::new("storno-original-checks");
	let store = setup(&db).await;

	let original = issued(&store).await;
	let cancel = new_invoice(None, InvoiceKind::Storno, Some(original.id));
	let storno = store.create_draft(&cancel).await.unwrap();

	for (label, sql, id) in [
		(
			"a storno with no original",
			"UPDATE invoices SET original_invoice_id = NULL WHERE id = ?",
			storno.id,
		),
		(
			"an issued invoice with no number",
			"UPDATE invoices SET number = NULL WHERE id = ?",
			original.id,
		),
	] {
		let err = sqlx::query(sql).bind(id).execute(store.writer()).await.unwrap_err();
		assert!(err.to_string().contains("CHECK constraint failed"), "{label}: {err}");
	}
}

/// The module doc claims every `UPDATE invoices` carries `AND status = …`. Nothing enforced
/// it: a new method with a missing predicate compiled, linted and passed the whole suite.
#[test]
fn every_invoice_write_carries_a_status_predicate() {
	const SRC: &str = include_str!("../src/invoice.rs");
	// `update_notes` is the one documented exception — `notes` is the single column an issued
	// invoice may still change.
	const ALLOWED_WITHOUT: &[&str] = &["SET notes = ?"];

	for (i, _) in SRC
		.match_indices("UPDATE invoices")
		.chain(SRC.match_indices("DELETE FROM invoices"))
	{
		let stmt: String = SRC[i..].chars().take_while(|c| *c != '"').collect();
		// After the `WHERE`, not anywhere: `SET status = 'X' WHERE id = ?` carries the word
		// and no predicate at all, and used to pass.
		let predicate = stmt.split_once("WHERE").map(|(_, w)| w).unwrap_or_default();
		assert!(
			predicate.contains("status") || ALLOWED_WITHOUT.iter().any(|a| stmt.contains(a)),
			"an invoice write with no status predicate:\n{stmt}"
		);
	}
}

// ---------------------------------------------------------------- seller versions

/// An edit is **not** a version. Repeated saves rewrite the one DRAFT row, so an operator who
/// corrects a typo four times still publishes once — and the live version the invoices freeze
/// never moves while they are typing.
#[tokio::test]
async fn repeated_draft_saves_rewrite_one_row_and_publish_nothing() {
	let db = TmpDb::new("seller-draft-rewrite");
	let store = setup(&db).await;

	for name in ["Elso", "Masodik", "Harmadik"] {
		store
			.save_seller_version_draft(
				SELLER_ID,
				&SellerVersionPatch { name: Some(name.into()), ..Default::default() },
			)
			.await
			.unwrap();
	}

	let drafts: i64 =
		sqlx::query_scalar("SELECT COUNT(*) FROM seller_versions WHERE status = 'DRAFT'")
			.fetch_one(store.reader())
			.await
			.unwrap();
	assert_eq!(drafts, 1, "each edit made its own draft");

	let draft = store.draft_seller_version(SELLER_ID).await.unwrap().unwrap();
	assert_eq!(draft.name, "Harmadik");
	// Seeded from the CURRENT row, so an untouched field is not blanked.
	assert_eq!(draft.tax_number, "12345678242");
	assert!(draft.valid_from.is_none(), "a draft is not in force");

	let current = store.current_seller_version(SELLER_ID).await.unwrap().unwrap();
	assert_eq!(current.name, "Teszt Kft.", "an unpublished edit changed the live version");
	assert_eq!(store.seller_version_history(SELLER_ID).await.unwrap().len(), 1);
}

/// Archive-then-promote, in one transaction: the old CURRENT row is stamped `superseded_at`
/// and the draft takes its place, and `idx_seller_version_current` means there is never a
/// moment with two.
#[tokio::test]
async fn publishing_archives_the_live_version_and_promotes_the_draft() {
	let db = TmpDb::new("seller-publish");
	let store = setup(&db).await;
	let before = store.current_seller_version(SELLER_ID).await.unwrap().unwrap();

	store
		.save_seller_version_draft(
			SELLER_ID,
			&SellerVersionPatch { name: Some("Uj Nev Kft.".into()), ..Default::default() },
		)
		.await
		.unwrap();
	let ver = store
		.publish_seller_version(SELLER_ID, Timestamp(1_800_000_000), &|_| Ok(()))
		.await
		.unwrap();
	let ver = ver.expect("the draft was promoted");

	let current = store.current_seller_version(SELLER_ID).await.unwrap().unwrap();
	assert_eq!(current.seller_ver, ver);
	assert_eq!(current.name, "Uj Nev Kft.");
	assert_eq!(current.valid_from, Some(Timestamp(1_800_000_000)));
	assert!(store.draft_seller_version(SELLER_ID).await.unwrap().is_none(), "the draft survived");

	let archived = store.seller_version(before.seller_ver).await.unwrap().unwrap();
	assert_eq!(archived.status, SellerVersionStatus::Archived);
	assert_eq!(archived.superseded_at, Some(Timestamp(1_800_000_000)));

	// Newest first, and contiguous: the archived row's `superseded_at` is the live one's
	// `valid_from`, which is what answers "which version was in force on this date".
	let history = store.seller_version_history(SELLER_ID).await.unwrap();
	assert_eq!(history.len(), 2);
	assert_eq!(history[0].seller_ver, ver);
	assert_eq!(history[1].superseded_at, history[0].valid_from);
}

/// Publishing with nothing to publish must leave the live version alone. The archive runs
/// first, so a naive implementation ends with no CURRENT row at all.
#[tokio::test]
async fn publishing_with_no_draft_changes_nothing() {
	let db = TmpDb::new("seller-publish-empty");
	let store = setup(&db).await;
	let before = store.current_seller_version(SELLER_ID).await.unwrap().unwrap();

	assert!(
		store
			.publish_seller_version(SELLER_ID, Timestamp::now(), &|_| Ok(()))
			.await
			.unwrap()
			.is_none()
	);

	let after = store.current_seller_version(SELLER_ID).await.unwrap().unwrap();
	assert_eq!(after.seller_ver, before.seller_ver);
	assert_eq!(after.status, SellerVersionStatus::Current);
	assert!(
		after.superseded_at.is_none(),
		"the live version was archived with nothing to replace it"
	);
}

/// Discarding throws away the edit and nothing else.
#[tokio::test]
async fn discarding_a_draft_leaves_the_live_version_untouched() {
	let db = TmpDb::new("seller-discard");
	let store = setup(&db).await;

	assert!(!store.discard_seller_version_draft(SELLER_ID).await.unwrap(), "there was no draft");
	store
		.save_seller_version_draft(
			SELLER_ID,
			&SellerVersionPatch { name: Some("Elvetve".into()), ..Default::default() },
		)
		.await
		.unwrap();
	assert!(store.discard_seller_version_draft(SELLER_ID).await.unwrap());

	assert!(store.draft_seller_version(SELLER_ID).await.unwrap().is_none());
	let current = store.current_seller_version(SELLER_ID).await.unwrap().unwrap();
	assert_eq!(current.name, "Teszt Kft.");
	assert_eq!(store.seller_version_history(SELLER_ID).await.unwrap().len(), 1);
}

/// `idx_seller_version_current` is integrity, not performance: two publishes racing on one
/// draft must not leave two live versions, whichever order they land in.
#[tokio::test]
async fn two_racing_publishes_cannot_leave_two_live_versions() {
	let db = TmpDb::new("seller-publish-race");
	let store = std::sync::Arc::new(setup(&db).await);
	store
		.save_seller_version_draft(
			SELLER_ID,
			&SellerVersionPatch { name: Some("Verseny Kft.".into()), ..Default::default() },
		)
		.await
		.unwrap();

	let (a, b) = tokio::join!(
		{
			let store = std::sync::Arc::clone(&store);
			async move { store.publish_seller_version(SELLER_ID, Timestamp(1), &|_| Ok(())).await }
		},
		{
			let store = std::sync::Arc::clone(&store);
			async move { store.publish_seller_version(SELLER_ID, Timestamp(2), &|_| Ok(())).await }
		},
	);
	// One promoted the draft; the other found none and said so. Neither may have inserted a
	// second live row.
	let promoted = [a.unwrap(), b.unwrap()].into_iter().flatten().count();
	assert_eq!(promoted, 1, "both publishes claimed the same draft");

	let live: i64 =
		sqlx::query_scalar("SELECT COUNT(*) FROM seller_versions WHERE status = 'CURRENT'")
			.fetch_one(store.reader())
			.await
			.unwrap();
	assert_eq!(live, 1);
}

/// What an invoice's `seller_ver` buys: the frozen row still reads back after the seller has
/// been edited and published over.
#[tokio::test]
async fn an_issued_invoice_resolves_the_version_it_froze() {
	let db = TmpDb::new("seller-frozen");
	let store = setup(&db).await;
	let invoice = store.create_draft(&new_invoice(None, InvoiceKind::Normal, None)).await.unwrap();
	let issued = store
		.issue(invoice.id, &issue_input(invoice.id, 100_000), invoice.version)
		.await
		.unwrap();
	assert_eq!(issued.seller_ver, Some(SELLER_VER));

	store
		.save_seller_version_draft(
			SELLER_ID,
			&SellerVersionPatch { name: Some("Utana Kft.".into()), ..Default::default() },
		)
		.await
		.unwrap();
	store
		.publish_seller_version(SELLER_ID, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();

	let frozen = store.seller_version(issued.seller_ver.unwrap()).await.unwrap().unwrap();
	assert_eq!(frozen.name, "Teszt Kft.");
	assert_eq!(frozen.status, SellerVersionStatus::Archived);
	// And the bulk read the audit export uses answers the same.
	let bulk = store.seller_versions(&[issued.seller_ver.unwrap()]).await.unwrap();
	assert_eq!(bulk.len(), 1);
	assert_eq!(bulk[0].name, "Teszt Kft.");
}

// vim: ts=4
