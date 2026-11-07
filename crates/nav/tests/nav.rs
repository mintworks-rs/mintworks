// SPDX-License-Identifier: MPL-2.0
//! The statutory *adóhatósági ellenőrzési adatszolgáltatás* — its two selection forms and the
//! file `mintworks_nav::Nav::audit_export` writes — plus the `NAV_REPORT` filing job.
//!
//! Both halves need a real `NavStore`, so the suite takes `mintworks-store-sqlite` as a
//! dev-dependency. The selection SQL is the part 23/2014 (VI. 30.) NGM r. is strict about, so
//! it is exercised against a real database rather than a double. NAV itself is a `wiremock`
//! stand-in.
//!
//! Every invoice is issued at mid-day UTC: `export_ids_by_date` selects the Europe/Budapest
//! day (`mintworks_invoice::numbering::utc_span`), so a boundary invoice near midnight would be
//! flaky either way.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;
use std::sync::Arc;

use mintworks_core::{
	App, AppBuilder,
	config::Config,
	ctx::Ctx,
	ids::{NavBatchId, SellerId},
	job::Next,
	prelude::*,
};
use mintworks_invoice::{
	draft::Priced,
	store::{
		BuyerSnapshot, Invoice, InvoiceDocument, InvoiceKind, InvoiceStore, InvoiceVatGroup,
		IssueInvoice, NewInvoice, NewInvoiceLine, PartyKind, PaymentMethod, Seller, SellerVersion,
		SellerVersionPatch, SellerVersionStatus,
	},
	vat::VatCode,
};
use mintworks_nav::{
	BatchState, FilingState, Nav, NavBatch, NavFiling, NavOp, NavSellerHold, NavStore,
	NavSubmission, NavVerdict,
	export::{Selection, range_error, selection},
	job::send_key,
	submission::NavArchive,
};
use mintworks_store_sqlite::SqliteStore;
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{method, path},
};

const ORG: i64 = 1;

/// The fixture's one seller. `put_seller` does not autoincrement, so the id is chosen here.
const SELLER: i64 = 1;

/// The root org, after `setup` renumbers it. The seller belongs to the *root* because the
/// ctx-less NAV paths resolve it through `auth::deployment_seller`, which walks up from there.
const ROOT: i64 = 0;

/// The version `setup`'s `seed_seller` publishes — the first row `seller_versions` ever gets.
const SELLER_VER: i64 = 1;

/// A `software` block `invoiceApi.xsd` accepts: `softwareId` is `[0-9A-Z\-]{18}` exactly, and
/// the other required fields are `…NotBlankType`.
const SOFTWARE_SETTINGS: [(&str, &str); 6] = [
	("nav.software_id", "HU12345678MINTWRKS"),
	("nav.software_name", "Mintworks"),
	("nav.software_operation", "LOCAL_SOFTWARE"),
	("nav.software_main_version", "0.1"),
	("nav.software_dev_name", "Teszt Kft."),
	("nav.software_dev_contact", "dev@e.st"),
];

// Mid-day UTC, so the calendar day the export selects on is never in doubt.
const JAN15: i64 = 1_768_478_400; // 2026-01-15T12:00:00Z
const FEB01: i64 = 1_769_947_200; // 2026-02-01T12:00:00Z — lower boundary
const FEB10: i64 = 1_770_724_800; // 2026-02-10T12:00:00Z
const FEB15: i64 = 1_771_156_800; // 2026-02-15T12:00:00Z
const FEB28: i64 = 1_772_280_000; // 2026-02-28T12:00:00Z — upper boundary
const MAR05: i64 = 1_772_712_000; // 2026-03-05T12:00:00Z

const FROM: &str = "2026-02-01";
const TO: &str = "2026-02-28";

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, so the two pools must be over a file.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-nav-export-test-{}-{name}", std::process::id()));
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

/// Migrations, the two store extensions the `Nav` handle resolves through, and the minimum the
/// foreign keys demand: one account, one org, seller 1. `HUF` is seeded by
/// `schema.rs`'s `INVOICE` block, and the export needs it for the currency's minor unit.
async fn setup(db: &TmpDb) -> (App, SqliteStore) {
	let store = SqliteStore::open(&db.config()).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	// No workers: every test here drives `job::report`/`job::poll` directly, and a live worker
	// claiming a row these tests assert the status of is a race with no upside — it registers
	// no NAV handler, so all it can do is terminate the row out from under the assertion.
	// Registered as defaults rather than rows: `AppBuilder::build` refuses to boot on a blank
	// `.required()` key, and every test here then overwrites them with rows of its own.
	let mut builder = AppBuilder::new()
		.config(Config { jobs_workers: Some(0), ..db.config() })
		.store(Arc::new(store.clone()) as Arc<dyn mintworks_core::store::CoreStore>)
		.settings(mintworks_invoice::SETTINGS)
		.settings(mintworks_nav::SETTINGS)
		.extension(invoices)
		.extension(nav);
	for (key, value) in SOFTWARE_SETTINGS {
		builder = builder.setting_default(key, value);
	}
	let app = builder.build().await.unwrap();

	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	// A fresh install seeds the root org at id 1, which this fixture wants for its own;
	// the framework finds the root by `kind = 'ROOT'`, never by its value.
	sqlx::query("UPDATE orgs SET id = ? WHERE kind = 'ROOT'")
		.bind(ROOT)
		.execute(store.write_pool())
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (?, 'org_t', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	// The org's default billing party, so the tests that go through `Invoices` rather than
	// straight at the store can resolve `Party::OrgDefault`.
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', ?, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 1, 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	seed_seller(&store, &seller(), &seller_version()).await;

	(app, store)
}

/// The operational row plus one published version — `issue` refuses a seller with no `CURRENT`
/// version, and `xml::supplier_info` reads only the version.
async fn seed_seller(store: &SqliteStore, seller: &Seller, version: &SellerVersionPatch) {
	store.put_seller(seller).await.unwrap();
	store.save_seller_version_draft(seller.id, version).await.unwrap();
	store
		.publish_seller_version(seller.id, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();
}

/// The seller row as stored, `uid` included: `put_seller` matches on the `uid` as well as the
/// `id`, so a second call for the same seller has to carry the one the first call wrote.
async fn stored_seller(store: &SqliteStore) -> Seller {
	store.seller_by_id(SELLER).await.unwrap().unwrap()
}

fn seller() -> Seller {
	Seller {
		id: SELLER,
		uid: SellerId::generate(),
		org_id: ROOT,
		nav_base_url: String::new(),
		nav_login: Some("techuser".into()),
		series_code: "A".into(),
		test_mode: false,
		closed_at: None,
		payment_days: None,
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

fn new_invoice(kind: InvoiceKind, original: Option<i64>) -> NewInvoice {
	NewInvoice {
		org_id: ORG,
		seller_id: SELLER,
		billing_party_id: None,
		request_id: None,
		kind,
		original_invoice_id: original,
		currency: CurrencyCode::parse("HUF").unwrap(),
		rate_e6: 1_000_000,
		payment_method: PaymentMethod::Transfer,
		notes: None,
		discount_kind: None,
		discount_value: None,
		series_code: None,
		doc_lang: None,
	}
}

fn line(net: i64) -> NewInvoiceLine {
	NewInvoiceLine {
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
		vat: Money(net * 2700 / 10000),
		gross: Money(net + net * 2700 / 10000),
		note: None,
	}
}

fn group(invoice_id: i64, net: i64) -> InvoiceVatGroup {
	InvoiceVatGroup {
		invoice_id,
		vat_code: VatCode::Std27,
		vat_rate_bp: 2700,
		net: Money(net),
		vat: Money(net * 2700 / 10000),
		gross: Money(net + net * 2700 / 10000),
		net_huf: None,
		vat_huf: None,
		gross_huf: None,
	}
}

/// HUF, so `huf_rate_e6` and the `*_huf` trio stay NULL.
fn issue_input(invoice_id: i64, net: i64, issued_at: i64) -> IssueInvoice {
	let vat = net * 2700 / 10000;
	IssueInvoice {
		series_code: "A".into(),
		series_year: 2026,
		test_mode: false,
		doc_lang: None,
		issued_at: Timestamp(issued_at),
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
		lines: vec![line(net)],
		groups: vec![group(invoice_id, net)],
		period_start: None,
		period_end: None,
		paid: false,
		nav_report_at: None,
	}
}

/// A stand-in `invoice_documents.sha256`, stored lowercase as `pdf::run` writes it.
const PDF_SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// The `invoice_documents` row `job::report` now requires: it files the PDF's hash, so an
/// invoice whose `RENDER_PDF` has not landed is not filable yet.
async fn put_pdf(store: &SqliteStore, invoice: &Invoice) {
	let doc = InvoiceDocument {
		invoice_id: invoice.id,
		kind: "PDF".into(),
		sha256: PDF_SHA256.into(),
		bytes: 1024,
		template_version: "test".into(),
		rendered_at: Timestamp::now(),
	};
	assert!(store.put_invoice_document(&doc, invoice.version).await.unwrap());
}

/// A draft with one line, issued at `issued_at`, with its PDF rendered.
async fn issue_at(store: &SqliteStore, issued_at: i64) -> Invoice {
	issue_at_under(store, issued_at, SELLER_VER).await
}

/// [`issue_at`] freezing a named seller version, for the one thing the constant cannot say: an
/// invoice issued after the seller was edited and published.
async fn issue_at_under(store: &SqliteStore, issued_at: i64, seller_ver: i64) -> Invoice {
	let invoice = issue_with(store, issued_at, seller_ver).await;
	put_pdf(store, &invoice).await;
	invoice
}

/// [`issue_at`] minus the `invoice_documents` row.
async fn issue_without_pdf(store: &SqliteStore, issued_at: i64) -> Invoice {
	issue_with(store, issued_at, SELLER_VER).await
}

async fn issue_with(store: &SqliteStore, issued_at: i64, seller_ver: i64) -> Invoice {
	issue_as(store, SELLER, issued_at, seller_ver).await
}

async fn issue_as(store: &SqliteStore, seller_id: i64, issued_at: i64, seller_ver: i64) -> Invoice {
	issue_mode(store, seller_id, issued_at, seller_ver, false).await
}

async fn issue_mode(
	store: &SqliteStore,
	seller_id: i64,
	issued_at: i64,
	seller_ver: i64,
	test_mode: bool,
) -> Invoice {
	const NET: i64 = 100_000;
	let draft = store
		.create_draft(&NewInvoice { seller_id, ..new_invoice(InvoiceKind::Normal, None) })
		.await
		.unwrap();
	store
		.replace_draft_lines(
			draft.id,
			None,
			&Priced {
				lines: vec![line(NET)],
				groups: vec![group(draft.id, NET)],
				net: Money(NET),
				vat: Money(27_000),
				gross: Money(127_000),
			},
			draft.version,
		)
		.await
		.unwrap();
	// Re-read: `replace_draft_lines` above moved `updated_at`, and `issue` now checks it.
	let draft = store.invoice_by_id(draft.id).await.unwrap().unwrap();
	// Allocation refuses a code the seller has no series for, so use its own default.
	let series_code = store.seller_by_id(seller_id).await.unwrap().unwrap().series_code;
	store
		.issue(
			draft.id,
			&IssueInvoice {
				seller_ver,
				series_code,
				test_mode,
				..issue_input(draft.id, NET, issued_at)
			},
			draft.version,
		)
		.await
		.unwrap()
}

/// As [`issue_at`], but with a buyer NAV can never be filed with: `base:TaxNumberType` takes
/// the first 8 digits as `base:taxpayerId`, so a 7-digit `groupMemberTaxNumber` fails in
/// `xml::invoice_data` — before `report` ever opens a submission row.
async fn issue_unfilable_at(store: &SqliteStore, issued_at: i64) -> Invoice {
	const NET: i64 = 100_000;
	let draft = store.create_draft(&new_invoice(InvoiceKind::Normal, None)).await.unwrap();
	store
		.replace_draft_lines(
			draft.id,
			None,
			&Priced {
				lines: vec![line(NET)],
				groups: vec![group(draft.id, NET)],
				net: Money(NET),
				vat: Money(27_000),
				gross: Money(127_000),
			},
			draft.version,
		)
		.await
		.unwrap();
	let draft = store.invoice_by_id(draft.id).await.unwrap().unwrap();
	let mut input = issue_input(draft.id, NET, issued_at);
	input.buyer.group_tax_no = Some("1234567".into());
	let invoice = store.issue(draft.id, &input, draft.version).await.unwrap();
	// Rendered, so `report` gets past the PDF gate and still fails on the buyer.
	put_pdf(store, &invoice).await;
	invoice
}

/// B1.
async fn claim(store: &SqliteStore, invoices: &[&Invoice]) -> NavBatch {
	let ids: Vec<i64> = invoices.iter().map(|i| i.id).collect();
	let (batch, got) = store
		.claim_batch(invoices[0].seller_id, NavOp::Create, &ids, true)
		.await
		.unwrap()
		.unwrap();
	assert_eq!(got, ids);
	batch
}

async fn row_ids(store: &SqliteStore, batch_id: i64) -> Vec<i64> {
	store.batch_rows(batch_id).await.unwrap().iter().map(|r| r.id).collect()
}

async fn job_status(app: &App, key: &str) -> Option<String> {
	let rows = app.store.job_statuses_by_keys(&[key.to_owned()]).await.unwrap();
	rows.into_iter().next().map(|(_, status)| status)
}

/// B1, B2, B5: with NAV under `transaction_id`, the rows taking `idx` 1.. in order.
async fn sent(store: &SqliteStore, invoices: &[&Invoice], transaction_id: &str) -> NavBatch {
	let batch = claim(store, invoices).await;
	let rows: Vec<(i64, i64)> = row_ids(store, batch.id).await.into_iter().zip(1..).collect();
	assert!(store.begin_post(batch.id, "<x/>", &rows, Timestamp::now()).await.unwrap());
	assert!(store.set_sent(batch.id, transaction_id).await.unwrap());
	store.batch_by_uid(batch.uid.as_str()).await.unwrap().unwrap()
}

/// A filing NAV gave `verdict` on; returns its row.
async fn settled(
	store: &SqliteStore,
	invoice: &Invoice,
	verdict: NavVerdict,
	error: Option<(&str, &str)>,
) -> i64 {
	let batch = sent(store, &[invoice], &format!("TX-{}", invoice.id)).await;
	let row = row_ids(store, batch.id).await[0];
	assert!(store.finish(row, verdict, error, Timestamp::now()).await.unwrap());
	row
}

/// A filing NAV rejected, to prove the export ignores NAV state entirely.
async fn fail_submission(store: &SqliteStore, invoice_id: i64) {
	let invoice = store.invoice_by_id(invoice_id).await.unwrap().unwrap();
	settled(store, &invoice, NavVerdict::Rejected, Some(("X", "nav said no"))).await;
}

/// The batch the invoice's newest row sits in.
async fn batch_of(store: &SqliteStore, invoice_id: i64) -> NavBatch {
	store.filing_by_invoice(invoice_id).await.unwrap().unwrap().batch.unwrap()
}

/// What the workers do for an invoice: its `NAV_REPORT`, then the `NAV_SEND` of the batch it is in.
async fn file(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	invoice_id: i64,
) -> ClResult<Next> {
	mintworks_nav::job::report(app, invoices, nav, invoice_id).await?;
	match nav.filing_by_invoice(invoice_id).await?.and_then(|f| f.batch) {
		Some(batch) => mintworks_nav::job::send(app, invoices, nav, batch.uid.as_str()).await,
		None => Ok(Next::Done),
	}
}

/// `awaiting_operator` past any fault's grace.
async fn waiting(store: &SqliteStore, seller_id: Option<i64>) -> i64 {
	store
		.awaiting_operator(seller_id, Timestamp(Timestamp::now().0 + 60))
		.await
		.unwrap()
}

/// The request `begin_post` archived for the invoice's batch.
async fn archived_request(store: &SqliteStore, invoice_id: i64) -> String {
	let batch = batch_of(store, invoice_id).await;
	store.batch_archive(batch.id).await.unwrap().unwrap().request_xml.unwrap()
}

#[tokio::test]
async fn date_range_is_inclusive_and_ignores_nav_state() {
	let db = TmpDb::new("dates");
	let (_app, store) = setup(&db).await;

	let before = issue_at(&store, JAN15).await;
	let lower = issue_at(&store, FEB01).await;
	let failed = issue_at(&store, FEB10).await;
	let upper = issue_at(&store, FEB28).await;
	let after = issue_at(&store, MAR05).await;

	// Reported to NAV and rejected — still the seller's turnover, so still exportable.
	fail_submission(&store, failed.id).await;

	let ids = store.export_ids_by_date(SELLER, FROM, TO).await.unwrap();
	assert_eq!(ids, vec![lower.id, failed.id, upper.id]);
	assert!(!ids.contains(&before.id), "the day before the range must be excluded");
	assert!(!ids.contains(&after.id), "the day after the range must be excluded");

	// A draft has no number and is not an issued invoice.
	let draft = store.create_draft(&new_invoice(InvoiceKind::Normal, None)).await.unwrap();
	let ids = store.export_ids_by_date(SELLER, "2026-01-01", "2026-12-31").await.unwrap();
	assert!(!ids.contains(&draft.id), "drafts must never be exported");
}

#[tokio::test]
async fn storno_pairs_are_closed_over_in_both_directions() {
	let db = TmpDb::new("storno");
	let (_app, store) = setup(&db).await;

	// Issued in January, cancelled in February: the February range must show both, or it
	// misstates turnover.
	let original = issue_at(&store, JAN15).await;
	let cancel = new_invoice(InvoiceKind::Storno, Some(original.id));
	let storno = store
		.storno(original.id, &cancel, &issue_input(0, -100_000, FEB15))
		.await
		.unwrap();

	let ids = store.export_ids_by_date(SELLER, FROM, TO).await.unwrap();
	assert!(ids.contains(&original.id), "the original of an in-range storno must be pulled in");
	assert!(ids.contains(&storno.id));

	// And the other direction: a January range must pull the February storno in.
	let ids = store.export_ids_by_date(SELLER, "2026-01-01", "2026-01-31").await.unwrap();
	assert!(ids.contains(&original.id));
	assert!(ids.contains(&storno.id), "the storno of an in-range invoice must be pulled in");
}

/// The storno-pair closure used to carry neither `seller_id` nor `number IS NOT NULL`, so any
/// row merely *pointing* at an in-range invoice joined a statutory export it has no business in.
#[tokio::test]
async fn the_pair_closure_pulls_in_neither_another_seller_nor_a_draft() {
	let db = TmpDb::new("pair-filters");
	let (_app, store) = setup(&db).await;

	let original = issue_at(&store, FEB15).await;
	let second = issue_at(&store, FEB15).await;

	// An un-numbered DRAFT storno of the first.
	let draft = store
		.create_draft(&new_invoice(InvoiceKind::Storno, Some(original.id)))
		.await
		.unwrap();

	// And a second seller's issued storno of the second — `idx_invoice_storno_once` allows one
	// cancellation per invoice, so the two cases need one original each.
	let mut other = seller();
	other.id = 2;
	other.series_code = "B".into();
	seed_seller(&store, &other, &seller_version()).await;
	let mut cancel = new_invoice(InvoiceKind::Storno, Some(second.id));
	cancel.seller_id = 2;
	let foreign = store
		.storno(
			second.id,
			&cancel,
			&IssueInvoice { series_code: "B".into(), ..issue_input(0, -100_000, FEB15) },
		)
		.await
		.unwrap();

	let ids = store.export_ids_by_date(SELLER, FROM, TO).await.unwrap();
	assert_eq!(ids, vec![original.id, second.id], "only seller 1's numbered rows belong here");
	assert!(!ids.contains(&draft.id));
	assert!(!ids.contains(&foreign.id));
}

#[tokio::test]
async fn export_wraps_every_selected_invoice() {
	let db = TmpDb::new("file");
	let (app, store) = setup(&db).await;

	for at in [JAN15, FEB01, FEB10, FEB28, MAR05] {
		issue_at(&store, at).await;
	}

	let ctx = Ctx::system("test").with_org(ORG);
	let mut out = Vec::new();
	let count = Nav::new(app.clone())
		.audit_export(&ctx, SELLER, Selection::IssueDate { from: FROM, to: TO }, &mut out)
		.await
		.unwrap();

	assert_eq!(count, 3);
	let xml = String::from_utf8(out).unwrap();
	assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Invoices>\n"), "{xml}");
	assert!(xml.trim_end().ends_with("</Invoices>"), "{xml}");
	assert_eq!(xml.matches("<InvoiceData").count(), 3, "one document per selected invoice");
	// The wrapped documents must not each repeat the XML declaration.
	assert_eq!(xml.matches("<?xml").count(), 1, "{xml}");

	// That an export was produced is itself auditable.
	let logged: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM audit_logs WHERE entity = 'audit_export' AND action = 'EXPORT'",
	)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(logged, 1);
}

/// The export spans every invoice of the seller across all orgs, and `export` took a
/// `&Ctx`, wrote an audit row, and never looked at `ctx.actor`. `mintworks-invoice` gates its
/// operator entry points; `mintworks-nav` had no equivalent.
#[tokio::test]
async fn the_export_is_operator_only() {
	let db = TmpDb::new("export-authz");
	let (app, store) = setup(&db).await;
	issue_at(&store, FEB10).await;
	let range = Selection::IssueDate { from: FROM, to: TO };
	let nav = Nav::new(app.clone());

	let mut user = Ctx::system("test").with_org(ORG);
	user.actor = mintworks_core::ctx::Actor::User { account_id: 1 };
	let err = nav
		.audit_export(&user, SELLER, range, &mut Vec::new())
		.await
		.expect_err("an org user must not read every org's invoices");
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");

	// `System` is the application's own code and is trusted, as in `service_api`.
	let system = Ctx::system("test").with_org(ORG);
	assert_eq!(nav.audit_export(&system, SELLER, range, &mut Vec::new()).await.unwrap(), 1);
}

#[tokio::test]
async fn selection_refuses_both_ranges_and_neither() {
	assert!(matches!(
		selection(Some(FROM), Some(TO), None, None).unwrap(),
		Selection::IssueDate { .. }
	));
	assert!(matches!(
		selection(None, None, Some("A2026/000001"), Some("A2026/000009")).unwrap(),
		Selection::Number { .. }
	));
	// Both, or neither, is the one thing the endpoint must refuse.
	assert!(selection(Some(FROM), Some(TO), Some("A"), Some("B")).is_err());
	assert!(selection(None, None, None, None).is_err());
	assert!(format!("{:?}", range_error()).contains("E-NAV-EXPORT-RANGE"));
}

// A duplicate statutory filing cannot be undone, so `report` consults the invoice's submission
// row before it builds anything: the job runner re-runs a failed handler from the top, and the
// `nav:invoice:{id}` dedup key only stops a double *enqueue*.

/// AES-128-ECB, so exactly 16 bytes (`crypto::decrypt_exchange_token`).
const EXCHANGE_KEY: &[u8; 16] = b"0123456789abcdef";
/// Exactly one AES block, so the reply carries no padding block to strip.
const TOKEN: &str = "TOKENTOKENTOKEN1";

const ENVELOPE: &str = concat!(
	r#" xmlns="http://schemas.nav.gov.hu/OSA/3.0/api""#,
	r#" xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common""#,
);

/// Points the `App` at `base_url` and seeds the three NAV secrets and the `software` settings
/// `NavAuth::load` reads. The `nav.software_*` keys default to `""`, which is schema-invalid —
/// `NavAuth::load` refuses to build a request out of them rather than letting NAV reject it.
async fn point_at_nav(app: &App, base_url: &str) {
	app.settings.set("nav.base_url", base_url, None).await.unwrap();
	for (key, value) in SOFTWARE_SETTINGS {
		app.settings.set(key, value, None).await.unwrap();
	}
	app.secrets.set("nav.tech_password", b"tech-pw", None).await.unwrap();
	app.secrets.set("nav.sign_key", b"sign-key", None).await.unwrap();
	app.secrets.set("nav.exchange_key", EXCHANGE_KEY, None).await.unwrap();
}

async fn mock(server: &MockServer, operation: &str, status: u16, body: String) {
	Mock::given(method("POST"))
		.and(path(format!("/{operation}")))
		.respond_with(ResponseTemplate::new(status).set_body_string(body))
		.mount(server)
		.await;
}

/// What NAV puts in `encodedExchangeToken`: AES-128-ECB under the exchange key, base64'd.
fn token_reply() -> String {
	use aes::cipher::{Block, BlockCipherEncrypt, KeyInit};
	use base64::{Engine, engine::general_purpose::STANDARD as B64};

	let mut block = Block::<aes::Aes128>::try_from(TOKEN.as_bytes()).unwrap();
	aes::Aes128::new(&(*EXCHANGE_KEY).into()).encrypt_block(&mut block);
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <TokenExchangeResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <encodedExchangeToken>{}</encodedExchangeToken>\
		 <tokenValidityFrom>2026-09-05T10:00:00.000Z</tokenValidityFrom>\
		 <tokenValidityTo>2026-09-05T10:05:00.000Z</tokenValidityTo>\
		 </TokenExchangeResponse>",
		B64.encode(block),
	)
}

/// The filing record of an invoice: `(id, verdict)`, and a verdict is NULL until NAV gives one.
async fn submissions(store: &SqliteStore, invoice_id: i64) -> Vec<(i64, Option<String>)> {
	sqlx::query_as("SELECT id, verdict FROM nav_submissions WHERE invoice_id = ? ORDER BY id")
		.bind(invoice_id)
		.fetch_all(store.read_pool())
		.await
		.unwrap()
}

async fn enqueued(store: &SqliteStore, kind: &str) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = ?")
		.bind(kind)
		.fetch_one(store.read_pool())
		.await
		.unwrap()
}

/// A `manageInvoice` that fails on the wire says nothing about the invoice, so the row stays open
/// with no verdict and the batch keeps its one `requestId`. `503` demonstrably never reached NAV,
/// so the next run re-POSTs; `408`, `500` and `502` are indeterminate and `NAV_RECONCILE` owns them,
/// since §1.9.2 forbids the immediate repeat under the same `requestId`.
#[tokio::test]
async fn a_lost_reply_stays_retryable_on_one_row() {
	for (status, name) in [
		(408u16, "report-408"),
		(500, "report-500"),
		(502, "report-502"),
		(503, "report-503"),
	] {
		let indeterminate = status != 503;
		let server = MockServer::builder().start().await;
		mock(&server, "tokenExchange", 200, token_reply()).await;
		mock(&server, "manageInvoice", status, "<html>down</html>".to_owned()).await;

		let db = TmpDb::new(name);
		let (app, store) = setup(&db).await;
		point_at_nav(&app, &server.uri()).await;
		let invoice = issue_at(&store, FEB10).await;

		let first = file(&app, &store, &store, invoice.id).await;
		let first = first.expect_err("a lost reply must fail the job, not be swallowed");
		if indeterminate {
			assert_eq!(first.parts().1, "E-CORE-TIMEOUT", "{status}: {first:?}");
		}
		let rows = submissions(&store, invoice.id).await;
		assert_eq!(rows.len(), 1);
		assert_eq!(rows[0].1, None, "{status}: NAV said nothing, so there is no verdict to record");

		// Whether the re-run reaches NAV again depends on whether NAV may already hold the filing.
		let hits_before = server.received_requests().await.unwrap().len();
		let again = file(&app, &store, &store, invoice.id).await;
		assert_eq!(submissions(&store, invoice.id).await.len(), 1, "one row per (invoice, op)");
		let hits_after = server.received_requests().await.unwrap().len();
		if indeterminate {
			again.expect("the batch is handed to the reconciliation");
			assert_eq!(hits_after, hits_before, "{status}: the reconciliation asks, not a resend");
			assert_eq!(enqueued(&store, "NAV_RECONCILE").await, 1);
		} else {
			again.expect_err("NAV is still down");
			assert!(hits_after > hits_before, "{status}: the retry reaches NAV again");
		}

		// The sweep covers filings that were never enqueued at all, and this one has a row.
		assert!(store.unfiled_invoices(SELLER, 50).await.unwrap().is_empty());
	}
}

/// A `NAV_RECONCILE` that ended `FAILED` must not open the way to a re-POST under a `requestId` NAV
/// may already hold: the batch says `IN_DOUBT`, and `NAV_SEND` revives the job instead.
#[tokio::test]
async fn a_failed_reconciliation_still_blocks_the_resend() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 500, "<html>down</html>".to_owned()).await;

	let db = TmpDb::new("report-failed-reconcile");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	// A 500 is indeterminate, so the first attempt hands the batch to `NAV_RECONCILE`.
	file(&app, &store, &store, invoice.id)
		.await
		.expect_err("a lost reply fails the job");
	let key = mintworks_nav::job::reconcile_key(batch_of(&store, invoice.id).await.uid.as_str());
	assert_eq!(job_status(&app, &key).await.as_deref(), Some("PENDING"));

	sqlx::query("UPDATE jobs SET status = 'FAILED', done_at = 1 WHERE dedup_key = ?")
		.bind(&key)
		.execute(store.write_pool())
		.await
		.unwrap();

	let hits_before = server.received_requests().await.unwrap().len();
	file(&app, &store, &store, invoice.id)
		.await
		.expect("an IN_DOUBT batch is handed on, not resent");
	assert_eq!(server.received_requests().await.unwrap().len(), hits_before, "it resent anyway");
	assert_eq!(
		job_status(&app, &key).await.as_deref(),
		Some("PENDING"),
		"the dead reconciliation must be revived, not just waited on"
	);
}

/// A buyer `xml::invoice_data` refuses fails `NAV_REPORT` before any row, so the sweep still sees
/// the invoice as unfiled. `tokenExchange` answering a 200 that is not a NAV reply is an outage: the
/// batch stays `CLAIMED` with the fault on it, nothing went on the wire, and the sweep owes it a
/// `NAV_SEND`.
#[tokio::test]
async fn a_filing_that_fails_before_the_wire_stays_recoverable() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, "<html><body>maintenance</body></html>".to_owned()).await;

	let db = TmpDb::new("report-preflight");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;
	// A buyer `xml::invoice_data` refuses: the other pre-flight failure, before the network.
	let broken = issue_unfilable_at(&store, FEB15).await;

	for _ in 0..3 {
		for id in [broken.id, invoice.id] {
			assert!(file(&app, &store, &store, id).await.is_err());
		}
	}
	assert!(submissions(&store, broken.id).await.is_empty(), "nothing was claimed for it");
	let batch = batch_of(&store, invoice.id).await;
	assert_eq!(batch.state, BatchState::Claimed);
	assert!(batch.posted_at.is_none(), "the token failed before the write-ahead");
	assert!(batch.error_code.is_some(), "the reason has to be on the batch");
	// It has no row, so however many times it failed it is still unfiled.
	assert_eq!(store.unfiled_invoices(SELLER, 50).await.unwrap(), vec![broken.id]);

	// `report` enqueued the send, so only a dead one shows the sweep supervising the batch.
	let key = send_key(batch.uid.as_str());
	sqlx::query("UPDATE jobs SET status = 'FAILED', done_at = 1 WHERE dedup_key = ?")
		.bind(&key)
		.execute(store.write_pool())
		.await
		.unwrap();
	mintworks_nav::job::sweep(&app, &store).await.unwrap();
	assert_eq!(job_status(&app, &key).await.as_deref(), Some("PENDING"));
}

/// The credential in the envelope is what NAV authenticates on the wire. An archive that kept
/// it would be a replayable credential per row.
#[tokio::test]
async fn the_archived_request_carries_no_credentials() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 503, "<html>down</html>".to_owned()).await;

	let db = TmpDb::new("report-redact");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let _ = file(&app, &store, &store, invoice.id).await;

	let archived = archived_request(&store, invoice.id).await;
	assert!(!archived.contains(TOKEN), "the exchange token was archived");
	assert!(archived.contains("<common:requestId>"), "the archive lost the request's shape");
	assert_eq!(archived.matches("[redacted]").count(), 3, "{archived}");
}

/// `manageInvoice` files an invoice once, so a filing that overtook `RENDER_PDF` would leave
/// the invoice with no hash at NAV and no second chance to send one.
#[tokio::test]
async fn a_filing_waits_for_the_pdf_that_proves_it() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("report-no-pdf");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_without_pdf(&store, FEB10).await;

	let err = file(&app, &store, &store, invoice.id).await.expect_err("no PDF, no filing");
	assert_eq!(err.parts().1, "E-CORE-UNAVAILABLE", "the runner has to retry it: {err:?}");
	assert!(server.received_requests().await.unwrap().is_empty(), "nothing may reach NAV");
	assert!(
		store.filing_by_invoice(invoice.id).await.unwrap().is_none(),
		"and no filing record is burned while it waits"
	);

	// Rendered, and the same invoice files.
	put_pdf(&store, &invoice).await;
	file(&app, &store, &store, invoice.id).await.unwrap();
	assert_eq!(submissions(&store, invoice.id).await.len(), 1);
}

/// NAV never sees the PDF, so the archived hash is the whole proof: it must be the hash of the
/// file the buyer downloads, uppercased as NAV writes hex.
#[tokio::test]
async fn the_filed_hash_is_the_pdf_the_buyer_downloads() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("report-hash");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	file(&app, &store, &store, invoice.id).await.unwrap();

	let archived = archived_request(&store, invoice.id).await;
	assert!(
		archived.contains(&format!(
			"<electronicInvoiceHash cryptoType=\"SHA-256\">{}</electronicInvoiceHash>",
			PDF_SHA256.to_ascii_uppercase()
		)),
		"{archived}"
	);
}

/// The element asserts electronic issuance under Áfa tv. 175. §, which needs the buyer's
/// acceptance — a paper-delivered deployment turns it off, and the filing itself is unaffected.
/// With the flag off the render is not a prerequisite either: nothing hashes the document, so a
/// statutory filing does not wait behind `RENDER_PDF`.
#[tokio::test]
async fn a_paper_deployment_files_without_the_electronic_hash() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("report-no-hash");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.electronic_invoice", "0", None).await.unwrap();
	let invoice = issue_without_pdf(&store, FEB10).await;

	file(&app, &store, &store, invoice.id).await.unwrap();

	let archived = archived_request(&store, invoice.id).await;
	assert!(!archived.contains("electronicInvoiceHash"), "{archived}");
	assert_eq!(submissions(&store, invoice.id).await.len(), 1, "and it still files");
}

fn error_reply(code: &str, message: &str) -> String {
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <GeneralErrorResponse{ENVELOPE}>\
		 <common:result><common:funcCode>ERROR</common:funcCode>\
		 <common:errorCode>{code}</common:errorCode>\
		 <common:message>{message}</common:message></common:result>\
		 </GeneralErrorResponse>"
	)
}

fn manage_ok_reply() -> String {
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <ManageInvoiceResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <transactionId>TX-VERDICT</transactionId>\
		 </ManageInvoiceResponse>"
	)
}

fn aborted_reply() -> String {
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryTransactionStatusResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <processingResults><processingResult><index>1</index>\
		 <invoiceStatus>ABORTED</invoiceStatus>\
		 <technicalValidationMessages>\
		 <validationResultCode>ERROR</validationResultCode>\
		 <validationErrorCode>SCHEMA_VIOLATION</validationErrorCode>\
		 <message>the invoice does not validate</message>\
		 </technicalValidationMessages>\
		 </processingResult></processingResults>\
		 </QueryTransactionStatusResponse>"
	)
}

/// `invoiceStatus = ABORTED` is NAV's verdict on the invoice, not a transport fault: identical data
/// can only earn it again, so nothing resends the invoice and it waits for a person.
#[tokio::test]
async fn a_rejected_invoice_is_terminal_and_is_never_resent() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;
	mock(&server, "queryTransactionStatus", 200, aborted_reply()).await;

	let db = TmpDb::new("report-rejected");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	file(&app, &store, &store, invoice.id).await.unwrap();
	let rows = submissions(&store, invoice.id).await;
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].1, None, "sent, but NAV has not answered about the invoice yet");

	let uid = batch_of(&store, invoice.id).await.uid;
	poll(&app, &store, &store, uid.as_str()).await.unwrap();
	assert_eq!(
		submissions(&store, invoice.id).await[0].1.as_deref(),
		Some("REJECTED"),
		"NAV's verdict is not a technical fault"
	);

	let hits = server.received_requests().await.unwrap().len();
	file(&app, &store, &store, invoice.id).await.unwrap();
	assert_eq!(submissions(&store, invoice.id).await.len(), 1, "no second filing");
	assert_eq!(server.received_requests().await.unwrap().len(), hits, "NAV was contacted again");

	// It is the operator's problem now, and the hourly sweep is what keeps saying so.
	assert!(store.unfiled_invoices(SELLER, 50).await.unwrap().is_empty());
	assert_eq!(waiting(&store, Some(SELLER)).await, 1);
}

/// The sweep's orphan pass: an issued invoice with no filing and no job, which
/// `InvoiceStore::issue` no longer leaves behind. An invoice with a row belongs to its batch.
#[tokio::test]
async fn the_sweep_enqueues_only_filings_that_were_never_made() {
	let db = TmpDb::new("sweep");
	let (app, store) = setup(&db).await;

	let mut issued = Vec::new();
	for i in 0..4 {
		issued.push(issue_at(&store, FEB10 + i * 86_400).await);
	}
	// One is already with NAV and one was rejected: neither is the sweep's business.
	sent(&store, &[&issued[0]], "TX1").await;
	fail_submission(&store, issued[1].id).await;

	assert_eq!(
		store.unfiled_invoices(SELLER, 50).await.unwrap(),
		vec![issued[2].id, issued[3].id],
		"only invoices with no filing record at all, oldest first"
	);

	mintworks_nav::job::sweep(&app, &store).await.unwrap();
	assert_eq!(enqueued(&store, "NAV_REPORT").await, 2);

	// `nav:invoice:{id}` is the dedup key, and it is never released, so a second tick adds
	// nothing however long the filings stay unmade.
	mintworks_nav::job::sweep(&app, &store).await.unwrap();
	assert_eq!(enqueued(&store, "NAV_REPORT").await, 2, "the hourly sweep must not stampede");
}

/// A test-mode invoice and its storno get a PDF but never a `NAV_REPORT`, from issue or sweep.
#[tokio::test]
async fn a_test_mode_invoice_and_its_storno_are_never_reported() {
	let db = TmpDb::new("test-mode-unreported");
	let (app, store) = setup(&db).await;
	let invoice = issue_mode(&store, SELLER, FEB10, SELLER_VER, true).await;
	mintworks_invoice::issue::enqueue_render(&app, &invoice).await;
	let storno = mintworks_invoice::storno::run(&app, &store, &invoice, "teszt").await.unwrap();
	assert!(storno.test_mode);

	for inv in [&invoice, &storno] {
		let pdf = job_status(&app, &format!("pdf:invoice:{}", inv.id)).await;
		assert!(pdf.is_some(), "no RENDER_PDF for {}", inv.id);
	}
	mintworks_nav::job::sweep(&app, &store).await.unwrap();
	assert_eq!(enqueued(&store, "NAV_REPORT").await, 0);
}

/// `service_api::alerts` is what turns that count into the operator-facing `A-NAV-REJECTED`. The
/// link's `state=open` is the one filter that spans every class `awaiting_operator` counts.
#[tokio::test]
async fn a_rejection_awaiting_an_operator_raises_the_nav_alert() {
	let db = TmpDb::new("nav-alert");
	let (app, store) = setup(&db).await;

	// Nothing is wrong yet, so there is nothing to say.
	assert!(mintworks_nav::alerts(app.clone()).await.unwrap().is_empty());

	let invoice = issue_at(&store, FEB10).await;
	settled(&store, &invoice, NavVerdict::Rejected, Some(("ERROR", "bad"))).await;

	let alerts = mintworks_nav::alerts(app).await.unwrap();
	assert_eq!(alerts.len(), 1);
	assert_eq!(alerts[0].code, "A-NAV-REJECTED");
	assert_eq!(alerts[0].severity, mintworks_core::alert::Severity::Error);
	assert_eq!(alerts[0].count, 1);
	assert_eq!(alerts[0].link.as_deref(), Some("/api/admin/nav-submissions?state=open"));
	// The operator must not be told to storno an invoice whose fate is unknown.
	assert!(alerts[0].message.contains("do not storno"), "{}", alerts[0].message);
}

/// The `discountRate` guard tested the *parent invoice* (`invoice.discount_kind.is_none()`)
/// while `storno::run` copies each line's `discount_kind`/`discount_value` verbatim and nulls
/// the counter-invoice's own. So the one document that must not carry a rate — the storno of
/// an invoice with both a per-line PERCENT discount and an invoice-level one — was exactly the
/// one that passed: NAV received `<discountRate>0.1000</discountRate>` beside a
/// `<discountValue>` that also holds the negated apportioned share of the invoice-level
/// discount. It is cross-validated after the invoice is ISSUED and immutable, and
/// `storno::run` refuses `E-INV-STORNO-OF-STORNO`, so a rejected cancellation has no exit.
#[tokio::test]
async fn a_storno_files_no_discount_rate_that_contradicts_its_discount_value() {
	use mintworks_invoice::draft::{Line, NewDraft, Party};
	use mintworks_invoice::money::Discount;
	use mintworks_invoice::service_api::Invoices;

	/// Both invoices of the chain, as the `InvoiceData` documents the export writes.
	async fn filed(db: &TmpDb, invoice_discount: Option<Discount>) -> String {
		let (app, _store) = setup(db).await;
		let ctx = Ctx::system("test").with_org(ORG);
		let invoices = Invoices::new(app.clone());

		let issued = invoices
			.issue_now(
				&ctx,
				&NewDraft {
					request_id: None,
					billing_party: Party::OrgDefault,
					lines: vec![Line {
						code: None,
						description: "Tanacsadas".into(),
						unit: "ora".into(),
						qty: Qty(1_000_000),
						unit_price: Some(Money(100_000)),
						vat_code: Some(VatCode::Std27),
						discount: Some(Discount::Percent(1000)),
						discount_description: None,
						note: None,
					}],
					discount: invoice_discount,
					payment_method: None,
					currency: None,
					fulfilment_date: None,
					due_date: None,
					notes: None,
					..NewDraft::default()
				},
			)
			.await
			.unwrap();
		invoices.storno(&ctx, issued.uid.as_str(), "teszt").await.unwrap();

		let mut out = Vec::new();
		Nav::new(app.clone())
			.audit_export(
				&ctx,
				SELLER,
				Selection::IssueDate { from: "2000-01-01", to: "2099-12-31" },
				&mut out,
			)
			.await
			.unwrap();
		String::from_utf8(out).unwrap()
	}

	// Both discounts: the line's stored 1000 bp no longer describes `discount_amount`, which
	// `vat::compute` folded the invoice-level share into. Neither document may carry a rate.
	let db = TmpDb::new("storno-discount-both");
	let xml = filed(&db, Some(Discount::Percent(1000))).await;
	assert_eq!(xml.matches("<InvoiceData").count(), 2, "{xml}");
	assert_eq!(xml.matches("discountRate").count(), 0, "a rate that is not the line's: {xml}");
	assert_eq!(
		xml.matches("<discountValue>").count(),
		2,
		"the resolved amount is still filed on both: {xml}"
	);

	// The mirror: with no invoice-level discount the line's own percent still reproduces its
	// own amount on both sides of the negation, so both documents keep their rate.
	let db = TmpDb::new("storno-discount-line-only");
	let xml = filed(&db, None).await;
	assert_eq!(
		xml.matches("<discountRate>0.1000</discountRate>").count(),
		2,
		"a self-consistent rate must still be filed, on the storno too: {xml}"
	);
}

/// `BY_NUMBER` compared a lexical range, justified on the grounds that a rendered number
/// is fixed-width within a series. `render_number`'s `{no:0N}` overflows past `N` rather than
/// truncating — deliberately — so past 999999 `A2026/1000000` sorts *below* `A2026/999999` and
/// the statutory audit export silently omitted rows. It compares `(length, text)` now.
#[tokio::test]
async fn a_number_range_export_spans_the_width_the_series_overflows_into() {
	let db = TmpDb::new("number-overflow");
	let (_app, store) = setup(&db).await;

	// An issued number is immutable, so the series is wound forward instead: the numbers below
	// are what `render_number` actually produces at the width boundary.
	let first = issue_at(&store, FEB10).await;
	assert_eq!(first.number.as_deref(), Some("A2026/000001"));
	sqlx::query("UPDATE doc_series SET next_no = 999999")
		.execute(store.write_pool())
		.await
		.unwrap();
	let last_six = issue_at(&store, FEB10 + 1).await;
	assert_eq!(last_six.number.as_deref(), Some("A2026/999999"));
	let seven = issue_at(&store, FEB10 + 2).await;
	assert_eq!(seven.number.as_deref(), Some("A2026/1000000"), "the width has to overflow");

	let ids = vec![first.id, last_six.id, seven.id];
	let selected = store
		.export_ids_by_number(SELLER, "A2026/000001", "A2026/1000000")
		.await
		.unwrap();
	assert_eq!(selected, ids, "the seven-digit invoice fell out of its own range");

	// Both ends are inclusive, and nothing outside them is selected.
	let selected = store
		.export_ids_by_number(SELLER, "A2026/999999", "A2026/1000000")
		.await
		.unwrap();
	assert_eq!(selected, vec![last_six.id, seven.id]);
}

/// A STORNO must not reach NAV before the invoice it cancels: NAV would answer `ABORTED`, and that
/// verdict parks a statutory filing on a person.
#[tokio::test]
async fn a_storno_waits_for_the_invoice_it_cancels_to_be_filed() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 503, "<html>maintenance</html>".to_owned()).await;

	let db = TmpDb::new("storno-order");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;

	let original = issue_at(&store, FEB10).await;
	let storno = store
		.storno(
			original.id,
			&new_invoice(InvoiceKind::Storno, Some(original.id)),
			&issue_input(0, -100_000, FEB15),
		)
		.await
		.unwrap();
	// A storno is an invoice with its own PDF, so it needs its own document row to be filable.
	put_pdf(&store, &storno).await;

	let report = || file(&app, &store, &store, storno.id);

	// The original has no submission at all yet.
	let err = report().await.expect_err("the storno must not overtake its original");
	assert_eq!(err.parts().1, "E-CORE-UNAVAILABLE", "the runner has to retry it: {err:?}");
	assert!(
		submissions(&store, storno.id).await.is_empty(),
		"and hold no submission while it waits"
	);
	assert!(server.received_requests().await.unwrap().is_empty(), "nothing may reach NAV");

	// In flight is not filed either.
	let with_nav = sent(&store, &[&original], "TX-ORIG").await;
	assert_eq!(report().await.unwrap_err().parts().1, "E-CORE-UNAVAILABLE");

	// Filed: the storno goes, and gets as far as the (mocked, unavailable) NAV.
	let row = row_ids(&store, with_nav.id).await[0];
	store.finish(row, NavVerdict::Done, None, Timestamp::now()).await.unwrap();
	report()
		.await
		.expect_err("NAV is mocked as down, but the guard is out of the way");
	assert_eq!(submissions(&store, storno.id).await.len(), 1, "the storno was actually sent");
	assert!(!server.received_requests().await.unwrap().is_empty());
	// `claim_batch` wrote a literal `'CREATE'`, so `Nav::filing` and the statutory audit export
	// reported every cancellation as a creation.
	let op: String = sqlx::query_scalar("SELECT op FROM nav_submissions WHERE invoice_id = ?")
		.bind(storno.id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(op.parse::<NavOp>().unwrap(), NavOp::Storno, "the row records what was sent");
}

/// `Nav::invoice` scopes `Public` as it scopes a user, so a `Ctx` carrying no org, which is every
/// `Ctx::public`, has no unscoped read of any org's invoice by uid.
#[tokio::test]
async fn a_public_ctx_has_no_unscoped_invoice_read() {
	let db = TmpDb::new("public-scope");
	let (app, store) = setup(&db).await;
	let invoice = issue_at(&store, JAN15).await;
	let nav = Nav::new(app.clone());

	// `E-AUTH-FORBIDDEN` is `Ctx::org`'s "no org selected", not a disclosure: it says nothing
	// about whether that uid exists.
	let err = nav
		.filing(&Ctx::public("test"), invoice.uid.as_str())
		.await
		.expect_err("a caller with no org has no unscoped read");
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");
	assert!(nav.submit(&Ctx::public("test"), invoice.uid.as_str()).await.is_err());
}

/// Reading a filing back is a service method, not a `NavStore` query a consumer's handler makes:
/// it is where the org scoping lives.
#[tokio::test]
async fn filing_reads_the_submission_and_stays_org_scoped() {
	let db = TmpDb::new("filing-read");
	let (app, store) = setup(&db).await;
	let invoice = issue_at(&store, JAN15).await;
	let nav = Nav::new(app.clone());

	assert!(
		nav.filing(&Ctx::system("test").with_org(ORG), invoice.uid.as_str())
			.await
			.unwrap()
			.is_none(),
		"nothing filed yet"
	);

	let batch = claim(&store, &[&invoice]).await;
	let id = row_ids(&store, batch.id).await[0];
	let found = nav
		.filing(&Ctx::system("test").with_org(ORG), invoice.uid.as_str())
		.await
		.unwrap()
		.expect("the owning org reads its filing");
	assert_eq!(found.row.id, id);

	let err = nav
		.filing(&Ctx::system("test").with_org(ORG + 1), invoice.uid.as_str())
		.await
		.expect_err("another org's uid is absent, not forbidden");
	assert_eq!(err.parts().1, "E-CORE-NOTFOUND");
}

/// `common:TaxpayerIdType` is `[0-9]{8}`, and only XML-escaping stood between the caller's
/// string and the element. A Hungarian tax number is *printed* `12345678-2-41` and this is
/// the lookup an invoice form runs while a user types one, so NAV answered `funcCode=ERROR`,
/// `client::taxpayer` raised `E-NAV-BUSINESS` at 502, and the user saw "service unavailable"
/// for a perfectly valid number.
#[tokio::test]
async fn a_printed_tax_number_reaches_nav_as_its_eight_digit_core() {
	let db = TmpDb::new("taxpayer-lookup");
	let (app, _store) = setup(&db).await;
	let server = MockServer::builder().start().await;
	point_at_nav(&app, &server.uri()).await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(
		&server,
		"queryTaxpayer",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <QueryTaxpayerResponse{ENVELOPE}>\
			 <common:result><common:funcCode>OK</common:funcCode></common:result>\
			 <taxpayerValidity>true</taxpayerValidity>\
			 </QueryTaxpayerResponse>"
		),
	)
	.await;

	let nav = Nav::new(app.clone());
	let ctx = Ctx::system("test").with_org(ORG);
	assert!(nav.lookup_tax_number(&ctx, "12345678-2-41").await.unwrap().valid);

	let sent = server.received_requests().await.unwrap();
	let body = sent
		.iter()
		.map(|r| String::from_utf8_lossy(&r.body).into_owned())
		.find(|b| b.contains("<taxNumber>"))
		.expect("no queryTaxpayer request was sent");
	assert!(body.contains("<taxNumber>12345678</taxNumber>"), "{body}");

	// And a number that cannot be one is refused here, without a round trip.
	let before = server.received_requests().await.unwrap().len();
	for bad in ["1234", "abc"] {
		let err = nav.lookup_tax_number(&ctx, bad).await.unwrap_err();
		assert_eq!(err.parts().1, "E-NAV-TAX-NUMBER", "{bad}");
		assert_eq!(err.parts().0.as_u16(), 400, "{bad}");
	}
	assert_eq!(server.received_requests().await.unwrap().len(), before, "NAV was called");
}

/// The `nav.software_*` settings are as capable of making every request schema-invalid as the
/// seller's address is, and `NAV_REPORT` is unbounded — so a deployment that boots on one of
/// these retries a refusal that can never change, forever.
#[tokio::test]
async fn a_software_block_or_login_nav_would_reject_refuses_to_boot() {
	let db = TmpDb::new("software-gate");
	let (app, _store) = setup(&db).await;
	app.settings
		.set("nav.base_url", "https://api-test.onlineszamla.nav.gov.hu", None)
		.await
		.unwrap();
	for (key, value) in SOFTWARE_SETTINGS {
		app.settings.set(key, value, None).await.unwrap();
	}
	app.settings.set("nav.software_dev_country", "HU", None).await.unwrap();
	app.settings.check_required("nav.").await.unwrap();

	// Refused where the operator writes it, not at the next boot: the XSD length and charset
	// are on the declaration in `mintworks_nav::SETTINGS`, so `Settings::set` is the gate.
	for (what, key, value) in [
		("a lowercase dev country", "nav.software_dev_country", "hu".to_owned()),
		("a 60-character software name", "nav.software_name", "a".repeat(60)),
		("a line break in the dev name", "nav.software_dev_name", "Teszt\u{a0}\nKft.".to_owned()),
		("a 17-character software id", "nav.software_id", "HU12345678MINTWRK".to_owned()),
	] {
		let good = app.settings.text(key).await.unwrap();
		assert_eq!(
			app.settings.set(key, &value, None).await.unwrap_err().parts().1,
			"E-CORE-SETTING",
			"{what} was accepted"
		);
		assert_eq!(app.settings.text(key).await.unwrap(), good, "{what} was stored anyway");
	}
	app.settings.check_required("nav.").await.unwrap();
}

/// The `NAV_REPORT` rows for one invoice, newest last.
async fn report_jobs(store: &SqliteStore, invoice_id: i64) -> Vec<(i64, String, Option<String>)> {
	sqlx::query_as(
		"SELECT id, status, dedup_key FROM jobs WHERE kind = 'NAV_REPORT' AND payload = ? \
		 ORDER BY id",
	)
	.bind(format!(r#"{{"invoiceId":{invoice_id}}}"#))
	.fetch_all(store.read_pool())
	.await
	.unwrap()
}

/// `retry` is the seller org's Admin's, and a test-mode invoice never reaches NAV.
#[tokio::test]
async fn a_tenant_retry_refuses_a_non_admin_and_a_test_mode_invoice() {
	let db = TmpDb::new("tenant-retry");
	let (app, store) = setup(&db).await;
	let nav = Nav::new(app.clone());
	let invoice = issue_at(&store, JAN15).await;

	// Account 1 is no member of the seller's org.
	let mut user = Ctx::system("test").with_org(ORG);
	user.actor = mintworks_core::ctx::Actor::User { account_id: 1 };
	let err = nav
		.retry(&user, invoice.uid.as_str())
		.await
		.expect_err("a non-Admin may not retry");
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");

	let test = issue_mode(&store, SELLER, JAN15, SELLER_VER, true).await;
	let ctx = Ctx::system("test").with_org(ORG);
	let err = nav.retry(&ctx, test.uid.as_str()).await.expect_err("test mode");
	assert_eq!(err.parts().1, "E-NAV-NOT-RETRYABLE");
}

/// `submit` is an operator's, and filing a statutory return is the same class of act as
/// `Invoices::issue`, which opens with `require_stepup`: a stolen access token must not file with
/// the tax authority for as long as it stays unexpired.
#[tokio::test]
async fn filing_a_statutory_return_needs_a_freshly_presented_credential() {
	let db = TmpDb::new("submit-stepup");
	let (app, store) = setup(&db).await;
	let invoice = issue_at(&store, JAN15).await;
	let nav = Nav::new(app.clone());
	let mut ctx = Ctx::system("test").with_org(ORG);
	ctx.actor = mintworks_core::ctx::Actor::User { account_id: 1 };

	ctx.auth_at = Some(Timestamp::now().0);
	let err = nav.submit(&ctx, invoice.uid.as_str()).await.expect_err("a user may not file");
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");

	// `require_operator` is Admin on the root org.
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 1, 'ADMIN', 0, 0)",
	)
	.bind(ROOT)
	.execute(store.write_pool())
	.await
	.unwrap();

	// `auth.stepup_window` is 300 s.
	ctx.auth_at = Some(Timestamp::now().0 - 600);
	let err = nav
		.submit(&ctx, invoice.uid.as_str())
		.await
		.expect_err("a stale credential must not file a return");
	assert_eq!(err.parts().1, "E-AUTH-STEPUP");
	assert!(report_jobs(&store, invoice.id).await.is_empty(), "a refused submit enqueued a job");

	// An API key or an impersonation token carries no `auth_at` and can never acquire one.
	ctx.auth_at = None;
	let err = nav.submit(&ctx, invoice.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-STEPUP-IMPOSSIBLE");

	// `Actor::System` is exempt: the store's issue enqueue and consumer Rust have no credential to
	// re-present, and gating them would break the automatic filing path outright.
	nav.submit(&Ctx::system("test").with_org(ORG), invoice.uid.as_str())
		.await
		.unwrap();
	assert_eq!(report_jobs(&store, invoice.id).await.len(), 1);

	// And a fresh one gets through: the gate is on the credential's age, not on the actor.
	let fresh = issue_at(&store, JAN15).await;
	ctx.auth_at = Some(Timestamp::now().0);
	nav.submit(&ctx, fresh.uid.as_str()).await.unwrap();
	assert_eq!(report_jobs(&store, fresh.id).await.len(), 1);
}

/// One malformation of the seller fixture, for the [`mintworks_nav::auth::check_seller`] gate. It
/// takes both halves: the gate reads `series_code`/`nav_login` off `sellers` and everything
/// statutory off the live `seller_versions` row.
type Break = fn(&mut Seller, &mut SellerVersionPatch);

#[tokio::test]
async fn a_seller_nav_would_reject_refuses_to_boot() {
	let db = TmpDb::new("seller-gate");
	let (app, store) = setup(&db).await;

	// `put_seller` matches on the uid as well as the id, so every re-seed below has to carry the
	// uid the fixture wrote.
	let uid = stored_seller(&store).await.uid;

	// The fixture is what a correct row looks like.
	assert!(mintworks_nav::auth::check_seller(&app).await.is_ok());

	// `LoginType` is `[a-zA-Z0-9]{6,15}` and `BankAccountNumberType` is HU 8-8-8, HU 8-8 or an
	// IBAN — `xml::Xml` emits `supplierBankAccountNumber` on every filing when it is set.
	let cases: [(&str, Break); 14] = [
		("a one-character postcode", |_, v| v.postcode = Some("1".into())),
		("a hyphenated login", |s, _| s.nav_login = Some("nav-user".into())),
		("a five-character login", |s, _| s.nav_login = Some("short".into())),
		("an unpunctuated account", |_, v| {
			v.bank_account = Patch::Value("1234567812345678".into());
		}),
		("a lowercase postcode", |_, v| v.postcode = Some("sw1a 1aa".into())),
		("a line break in the name", |_, v| v.name = Some("Teszt\nKft.".into())),
		("a lowercase country", |_, v| v.country = Some("hu".into())),
		("a 300-character street", |_, v| v.street = Some("a".repeat(300))),
		("a tax number of five digits", |_, v| v.tax_number = Some("12345".into())),
		// `VatCodeType` is `[1-5]{1}`: a `0` here files nothing, ever.
		("a 9th digit outside 1-5", |_, v| v.tax_number = Some("12345678042".into())),
		// `render_number` copies `series_code` verbatim into `invoiceNumber`, a
		// `SimpleText50NotBlankType`, on invoices that are immutable by the time NAV sees them.
		("a line break in the series code", |s, _| s.series_code = "A\nB".into()),
		("an over-long series code", |s, _| s.series_code = "A".repeat(39)),
		// The gate used to validate these trimmed while the wire got the column's own bytes:
		// `LoginType` is `[a-zA-Z0-9]{6,15}`, so the trailing space made every `tokenExchange`
		// schema-invalid — and the credential fault only ever retries, so nothing is filed.
		("a trailing space in the NAV login", |s, _| s.nav_login = Some("techuser ".into())),
		("a trailing space in the bank account", |_, v| {
			v.bank_account = Patch::Value("11111111-22222222-33333333 ".into());
		}),
	];
	for (what, break_it) in cases {
		let (mut seller, mut version) = (seller(), seller_version());
		seller.uid = uid.clone();
		break_it(&mut seller, &mut version);
		seed_seller(&store, &seller, &version).await;
		assert!(mintworks_nav::auth::check_seller(&app).await.is_err(), "{what} was accepted");
	}

	// And back to the fixture, so this is a gate and not a blanket refusal — with the same two
	// values untrimmed, which are exactly what goes on the wire, and every legal form of the
	// login and the account number.
	for ok in [
		(|_: &mut Seller, v: &mut SellerVersionPatch| {
			v.bank_account = Patch::Value("11111111-22222222-33333333".into());
		}) as Break,
		|s: &mut Seller, _: &mut SellerVersionPatch| s.nav_login = Some("navuser1".into()),
		|_: &mut Seller, v: &mut SellerVersionPatch| {
			v.bank_account = Patch::Value("12345678-12345678-12345678".into());
		},
		|_: &mut Seller, v: &mut SellerVersionPatch| {
			v.bank_account = Patch::Value("HU42117730161111101800000000".into());
		},
	] {
		let (mut seller, mut version) = (seller(), seller_version());
		seller.uid = uid.clone();
		ok(&mut seller, &mut version);
		seed_seller(&store, &seller, &version).await;
		assert!(mintworks_nav::auth::check_seller(&app).await.is_ok());
	}
}

// Rust has no trait delegation, so the impl below is pass-throughs.

/// Which write this double fails; everything else delegates.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
	/// The archive is a diagnostic; the reply in hand is the truth, so nothing on either job
	/// path may be decided by it.
	Archive,
	/// The write that records NAV's `transactionId`.
	SetSent,
}

struct Faulty(Arc<dyn NavStore>, Fail);

#[async_trait::async_trait]
impl NavStore for Faulty {
	async fn claim_batch(
		&self,
		seller_id: i64,
		op: NavOp,
		invoice_ids: &[i64],
		electronic: bool,
	) -> ClResult<Option<(NavBatch, Vec<i64>)>> {
		self.0.claim_batch(seller_id, op, invoice_ids, electronic).await
	}
	async fn begin_post(
		&self,
		batch_id: i64,
		request_xml: &str,
		rows: &[(i64, i64)],
		at: Timestamp,
	) -> ClResult<bool> {
		self.0.begin_post(batch_id, request_xml, rows, at).await
	}
	async fn abort_post(
		&self,
		batch_id: i64,
		fault: Option<(&str, &str)>,
		at: Timestamp,
	) -> ClResult<bool> {
		self.0.abort_post(batch_id, fault, at).await
	}
	async fn mark_in_doubt(
		&self,
		batch_id: i64,
		fault: Option<(&str, &str)>,
		at: Timestamp,
	) -> ClResult<bool> {
		self.0.mark_in_doubt(batch_id, fault, at).await
	}
	async fn set_sent(&self, batch_id: i64, transaction_id: &str) -> ClResult<bool> {
		if self.1 == Fail::SetSent {
			return Err(Error::Unavailable("the writer is down".to_owned()));
		}
		self.0.set_sent(batch_id, transaction_id).await
	}
	async fn clear_doubt(&self, batch_id: i64) -> ClResult<bool> {
		self.0.clear_doubt(batch_id).await
	}
	async fn hold_batch(
		&self,
		batch_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<bool> {
		self.0.hold_batch(batch_id, code, message, at).await
	}
	async fn spend_batch(
		&self,
		batch_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<Option<NavBatch>> {
		self.0.spend_batch(batch_id, code, message, at).await
	}
	async fn finish(
		&self,
		row_id: i64,
		verdict: NavVerdict,
		error: Option<(&str, &str)>,
		at: Timestamp,
	) -> ClResult<bool> {
		self.0.finish(row_id, verdict, error, at).await
	}
	async fn finish_held(
		&self,
		row_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<bool> {
		self.0.finish_held(row_id, code, message, at).await
	}
	async fn release_row(&self, row_id: i64) -> ClResult<bool> {
		self.0.release_row(row_id).await
	}
	async fn cancel_filing(&self, invoice_id: i64, op: NavOp, at: Timestamp) -> ClResult<bool> {
		self.0.cancel_filing(invoice_id, op, at).await
	}
	async fn reopen_cancelled(&self, row_id: i64) -> ClResult<bool> {
		self.0.reopen_cancelled(row_id).await
	}
	async fn refile(
		&self,
		row_ids: &[i64],
		code: Option<&str>,
		at: Timestamp,
	) -> ClResult<Option<NavBatch>> {
		self.0.refile(row_ids, code, at).await
	}
	async fn attach_transaction(
		&self,
		transaction_id: &str,
		seller_id: i64,
		op: NavOp,
		rows: &[(i64, i64)],
		at: Timestamp,
	) -> ClResult<Option<NavBatch>> {
		self.0.attach_transaction(transaction_id, seller_id, op, rows, at).await
	}
	async fn stamp_adopt_checked(&self, row_ids: &[i64], at: Timestamp) -> ClResult<u64> {
		self.0.stamp_adopt_checked(row_ids, at).await
	}
	async fn resolve(&self, row_id: i64, at: Timestamp) -> ClResult<bool> {
		self.0.resolve(row_id, at).await
	}
	async fn record_batch_fault(
		&self,
		batch_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<bool> {
		self.0.record_batch_fault(batch_id, code, message, at).await
	}
	async fn archive_response(&self, batch_id: i64, response_xml: &str) -> ClResult<()> {
		if self.1 == Fail::Archive {
			return Err(Error::internal("the archive write failed"));
		}
		self.0.archive_response(batch_id, response_xml).await
	}
	async fn release_seller(&self, seller_id: i64) -> ClResult<bool> {
		self.0.release_seller(seller_id).await
	}
	async fn seller_hold(&self, seller_id: i64) -> ClResult<Option<NavSellerHold>> {
		self.0.seller_hold(seller_id).await
	}
	async fn seller_holds_count(&self) -> ClResult<i64> {
		self.0.seller_holds_count().await
	}
	async fn batch_by_uid(&self, uid: &str) -> ClResult<Option<NavBatch>> {
		self.0.batch_by_uid(uid).await
	}
	async fn batch_by_transaction(&self, transaction_id: &str) -> ClResult<Option<NavBatch>> {
		self.0.batch_by_transaction(transaction_id).await
	}
	async fn batch_archive(&self, batch_id: i64) -> ClResult<Option<NavArchive>> {
		self.0.batch_archive(batch_id).await
	}
	async fn batch_rows(&self, batch_id: i64) -> ClResult<Vec<NavSubmission>> {
		self.0.batch_rows(batch_id).await
	}
	async fn filing_by_invoice(&self, invoice_id: i64) -> ClResult<Option<NavFiling>> {
		self.0.filing_by_invoice(invoice_id).await
	}
	async fn filings_by_invoice(&self, invoice_id: i64) -> ClResult<Vec<NavFiling>> {
		self.0.filings_by_invoice(invoice_id).await
	}
	async fn list_filings(
		&self,
		states: &[FilingState],
		after_id: i64,
		limit: i64,
	) -> ClResult<Vec<NavFiling>> {
		self.0.list_filings(states, after_id, limit).await
	}
	async fn supervised(&self, after_id: i64, limit: i64) -> ClResult<Vec<NavBatch>> {
		self.0.supervised(after_id, limit).await
	}
	async fn orphaned_invoices(&self, limit: i64) -> ClResult<Vec<i64>> {
		self.0.orphaned_invoices(limit).await
	}
	async fn adoptable(&self, limit: i64) -> ClResult<Vec<NavBatchId>> {
		self.0.adoptable(limit).await
	}
	async fn batch_candidates(
		&self,
		seller_id: i64,
		exclude_invoice_id: i64,
		require_document: bool,
		limit: i64,
	) -> ClResult<Vec<i64>> {
		self.0
			.batch_candidates(seller_id, exclude_invoice_id, require_document, limit)
			.await
	}
	async fn unfiled_invoices(&self, seller_id: i64, limit: i64) -> ClResult<Vec<i64>> {
		self.0.unfiled_invoices(seller_id, limit).await
	}
	async fn awaiting_operator(
		&self,
		seller_id: Option<i64>,
		fault_before: Timestamp,
	) -> ClResult<i64> {
		self.0.awaiting_operator(seller_id, fault_before).await
	}
	async fn backlog(&self, seller_id: i64) -> ClResult<i64> {
		self.0.backlog(seller_id).await
	}
	async fn oldest_open_fault(&self, seller_id: i64) -> ClResult<Option<String>> {
		self.0.oldest_open_fault(seller_id).await
	}
	async fn open_batch_keys(&self, seller_id: i64) -> ClResult<Vec<String>> {
		self.0.open_batch_keys(seller_id).await
	}
	async fn export_ids_by_date(&self, seller_id: i64, from: &str, to: &str) -> ClResult<Vec<i64>> {
		self.0.export_ids_by_date(seller_id, from, to).await
	}
	async fn export_ids_by_number(
		&self,
		seller_id: i64,
		from: &str,
		to: &str,
	) -> ClResult<Vec<i64>> {
		self.0.export_ids_by_number(seller_id, from, to).await
	}
}

/// The `transactionId` is the one thing in NAV's reply that cannot be reconstructed, and it
/// used to be written *after* the archive — so a contended or failing archive write stood
/// between an accepted filing and the id identifying it, and losing it parks the filing on a
/// person via `REQUEST_ID_NOT_UNIQUE`.
#[tokio::test]
async fn the_transaction_id_is_recorded_before_the_response_is_archived() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("report-archive-fails");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let nav: Arc<dyn NavStore> = Arc::new(Faulty(Arc::new(store.clone()), Fail::Archive));
	file(&app, &store, nav.as_ref(), invoice.id)
		.await
		.expect("a failed archive must not fail a filing NAV accepted");

	let batch = batch_of(&store, invoice.id).await;
	assert_eq!(batch.transaction_id.as_deref(), Some("TX-VERDICT"));
	let archive = store.batch_archive(batch.id).await.unwrap();
	assert!(
		archive.is_none_or(|a| a.response_xml.is_none()),
		"the archive did fail; that is the point"
	);
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "and the poll was still queued");
}

/// The same on the poll path: a failed archive write must not discard a verdict NAV already gave.
#[tokio::test]
async fn a_verdict_is_settled_even_when_the_archive_fails() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "queryTransactionStatus", 200, done_reply()).await;

	let db = TmpDb::new("poll-archive-fails");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let nav: Arc<dyn NavStore> = Arc::new(Faulty(Arc::new(store.clone()), Fail::Archive));
	let batch = sent(&store, &[&invoice], "TX-VERDICT").await;
	let id = row_ids(&store, batch.id).await[0];

	poll(&app, &store, nav.as_ref(), batch.uid.as_str())
		.await
		.expect("a failed archive must not discard a verdict NAV has given");
	assert_eq!(submissions(&store, invoice.id).await, vec![(id, Some("DONE".to_owned()))]);
}

fn done_reply() -> String {
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryTransactionStatusResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <processingResults><processingResult><index>1</index>\
		 <invoiceStatus>DONE</invoiceStatus>\
		 </processingResult></processingResults>\
		 </QueryTransactionStatusResponse>"
	)
}

// ---------------------------------------------------------------------------------------------
// Batching: one exchange token per request, up to `nav.batch_max` invoices under it, and
// §1.9.2 lost-transaction recovery.
// ---------------------------------------------------------------------------------------------

/// The newest `nav_submissions` row of an invoice with its batch: `(id, transaction_id, idx,
/// batch_uid, verdict)`. [`submissions`] answers verdicts only, and every batching assertion is
/// about the other three columns.
async fn row_of(
	store: &SqliteStore,
	invoice_id: i64,
) -> (i64, Option<String>, Option<i64>, Option<String>, Option<String>) {
	sqlx::query_as(
		"SELECT s.id, b.transaction_id, s.idx, b.uid, s.verdict
		   FROM nav_submissions s LEFT JOIN nav_batches b ON b.id = s.batch_id
		  WHERE s.invoice_id = ? ORDER BY s.id DESC LIMIT 1",
	)
	.bind(invoice_id)
	.fetch_one(store.read_pool())
	.await
	.unwrap()
}

/// The bodies the mock saw for one operation, oldest first. Counting them is how a test asserts
/// that a second POST never happened.
async fn requests(server: &MockServer, operation: &str) -> Vec<String> {
	server
		.received_requests()
		.await
		.unwrap()
		.into_iter()
		.filter(|r| r.url.path().trim_start_matches('/') == operation)
		.map(|r| String::from_utf8_lossy(&r.body).into_owned())
		.collect()
}

/// How many invoices one `ManageInvoiceRequest` carries. Counted on the `<index>` marker, not on
/// the tag: `invoiceOperation` is nested inside itself — see `job::operation_slice`.
fn operations(request: &str) -> usize {
	request.matches("<invoiceOperation><index>").count()
}

/// [`mock`] for the first matching request only; a later mount answers the rest. Mounted mocks
/// are tried in mount order, so the one-shot goes up first.
async fn mock_once(server: &MockServer, operation: &str, status: u16, body: String) {
	Mock::given(method("POST"))
		.and(path(format!("/{operation}")))
		.respond_with(ResponseTemplate::new(status).set_body_string(body))
		.up_to_n_times(1)
		.mount(server)
		.await;
}

/// The claimed `jobs` row `job::poll` reads its own next-run cadence off. `Runner::tick` mints
/// one in production; these tests drive the handler directly, so nothing else does.
fn poll_job(batch_uid: &str) -> mintworks_core::job::Job {
	mintworks_core::job::Job {
		id: 1,
		kind: mintworks_nav::job::KIND_NAV_POLL.to_owned(),
		payload: mintworks_nav::job::batch_payload(batch_uid),
		attempts: 1,
	}
}

async fn poll(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	batch_uid: &str,
) -> ClResult<Next> {
	mintworks_nav::job::poll(app, invoices, nav, &poll_job(batch_uid), batch_uid).await
}

/// The `NAV_REPORT` row issuing an invoice mints in production; these tests drive the handler
/// directly, so nothing else does.
async fn enqueue_report(app: &App, invoice_id: i64) {
	mintworks_core::job::enqueue(
		&app.store,
		mintworks_invoice::KIND_NAV_REPORT,
		&mintworks_invoice::invoice_job_payload(invoice_id),
		Some(&format!("nav:invoice:{invoice_id}")),
		Timestamp::now(),
	)
	.await
	.unwrap()
	.expect("a fresh dedup key");
}

/// `new_invoice` hardcodes `ORG`, and moving an issued invoice between orgs is not an
/// operation the store offers — this is a fixture for the cross-org leak test, nothing more.
async fn move_to_org(store: &SqliteStore, invoice_id: i64, org_id: i64) {
	sqlx::query("UPDATE invoices SET org_id = ? WHERE id = ?")
		.bind(org_id)
		.bind(invoice_id)
		.execute(store.write_pool())
		.await
		.unwrap();
}

const WARN_EXTRA: &str = "<businessValidationMessages>\
	<validationResultCode>WARN</validationResultCode>\
	</businessValidationMessages>";
const ABORT_EXTRA: &str = "<technicalValidationMessages>\
	<validationResultCode>ERROR</validationResultCode>\
	<validationErrorCode>SCHEMA_VIOLATION</validationErrorCode>\
	<message>the invoice does not validate</message>\
	</technicalValidationMessages>";

/// A `queryTransactionStatus` reply about N invoices: `(index, invoiceStatus, extra)`.
fn status_results(results: &[(i64, &str, &str)]) -> String {
	let mut body = String::new();
	for (idx, status, extra) in results {
		let _ = write!(
			body,
			"<processingResult><index>{idx}</index>\
			 <invoiceStatus>{status}</invoiceStatus>{extra}</processingResult>"
		);
	}
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryTransactionStatusResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <processingResults>{body}</processingResults>\
		 </QueryTransactionStatusResponse>"
	)
}

/// One page of `queryTransactionList`, which is half of §1.9.2: the transactions this technical
/// user submitted in the window, with no `requestId` to match them by.
fn transaction_list_reply(ids: &[&str]) -> String {
	let mut body = String::new();
	for id in ids {
		let _ = write!(body, "<transaction><transactionId>{id}</transactionId></transaction>");
	}
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryTransactionListResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <transactionListResult><currentPage>1</currentPage><availablePage>1</availablePage>\
		 {body}</transactionListResult>\
		 </QueryTransactionListResponse>"
	)
}

/// The other half: `returnOriginalRequest=true`, so each result echoes back the base64
/// `invoiceData` it was filed with. The invoice numbers inside are the only thing that ties a
/// transaction to the batch that submitted it.
fn original_request_reply(filed: &[(i64, &str)]) -> String {
	use base64::{Engine, engine::general_purpose::STANDARD as B64};

	let results: Vec<(i64, String, String)> = filed
		.iter()
		.map(|(idx, number)| {
			let data = B64.encode(format!(
				"<InvoiceData><invoiceNumber>{number}</invoiceNumber></InvoiceData>"
			));
			(*idx, "DONE".to_owned(), format!("<originalRequest>{data}</originalRequest>"))
		})
		.collect();
	let borrowed: Vec<(i64, &str, &str)> =
		results.iter().map(|(i, s, e)| (*i, s.as_str(), e.as_str())).collect();
	status_results(&borrowed)
}

/// §1.1: the exchange token is single-use and covers one request, and one request carries up to
/// 100 invoices. Three invoices therefore cost one `tokenExchange` and one `manageInvoice`, the
/// batch's uid is the `requestId`, and each row records the `<index>` its invoiceData rode under,
/// because that is what the poll matches NAV's per-result `index` against.
#[tokio::test]
async fn one_token_and_one_request_carry_the_whole_batch() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-one-token");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let first = issue_at(&store, FEB10).await;
	let second = issue_at(&store, FEB10).await;
	let third = issue_at(&store, FEB10).await;
	for id in [first.id, second.id, third.id] {
		enqueue_report(&app, id).await;
	}

	file(&app, &store, &store, first.id).await.unwrap();

	assert_eq!(requests(&server, "tokenExchange").await.len(), 1, "one token per request");
	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(sent.len(), 1, "one request for the batch");
	assert_eq!(operations(&sent[0]), 3, "{}", sent[0]);
	for idx in 1..=3 {
		assert!(sent[0].contains(&format!("<invoiceOperation><index>{idx}</index>")), "{idx}");
	}

	let batch = row_of(&store, first.id).await.3.unwrap();
	assert!(
		sent[0].contains(&format!("<common:requestId>{batch}</common:requestId>")),
		"{}",
		sent[0]
	);
	let mut seen = Vec::new();
	for invoice in [&first, &second, &third] {
		let (_, tx, idx, of, _) = row_of(&store, invoice.id).await;
		assert_eq!(tx.as_deref(), Some("TX-VERDICT"), "invoice {}", invoice.id);
		assert_eq!(of.as_deref(), Some(batch.as_str()), "invoice {}", invoice.id);
		seen.push(idx.expect("begin_post writes the index"));
	}
	seen.sort_unstable();
	assert_eq!(seen, vec![1, 2, 3], "gapless, 1-based, one per invoice");
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "one poll chain for the transaction");
}

/// The job is done only when NAV has answered about every invoice in the transaction; a row NAV has
/// answered about is settled as it goes, and nothing is settled by omission.
#[tokio::test]
async fn the_poll_gates_on_the_whole_transaction() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;
	mock_once(
		&server,
		"queryTransactionStatus",
		200,
		status_results(&[(1, "DONE", ""), (2, "PROCESSING", "")]),
	)
	.await;
	mock(
		&server,
		"queryTransactionStatus",
		200,
		status_results(&[(1, "DONE", ""), (2, "ABORTED", ABORT_EXTRA), (3, "DONE", WARN_EXTRA)]),
	)
	.await;

	let db = TmpDb::new("batch-poll-gates");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let first = issue_at(&store, FEB10).await;
	let second = issue_at(&store, FEB10).await;
	let third = issue_at(&store, FEB10).await;
	for id in [first.id, second.id, third.id] {
		enqueue_report(&app, id).await;
	}

	file(&app, &store, &store, first.id).await.unwrap();
	let uid = batch_of(&store, first.id).await.uid;

	// Index 3 is not even mentioned, and index 2 is still processing: two of the three are
	// unanswered, so the job is not finished whatever index 1 says.
	let next = poll(&app, &store, &store, uid.as_str()).await.unwrap();
	assert!(
		matches!(next, mintworks_core::job::Next::Again { .. }),
		"a verdict for one invoice does not complete the transaction: {next:?}"
	);
	let open: Vec<i64> =
		sqlx::query_scalar("SELECT idx FROM nav_submissions WHERE verdict IS NULL ORDER BY idx")
			.fetch_all(store.read_pool())
			.await
			.unwrap();
	assert_eq!(open, vec![2, 3], "nothing is settled by omission");

	assert_eq!(
		poll(&app, &store, &store, uid.as_str())
			.await
			.expect("every invoice now has a verdict"),
		mintworks_core::job::Next::Done,
	);
	let settled: Vec<(i64, Option<String>)> =
		sqlx::query_as("SELECT idx, verdict FROM nav_submissions ORDER BY idx")
			.fetch_all(store.read_pool())
			.await
			.unwrap();
	assert_eq!(
		settled,
		vec![
			(1, Some("DONE".to_owned())),
			(2, Some("REJECTED".to_owned())),
			(3, Some("WARN".to_owned())),
		],
		"each verdict lands on the row whose `idx` carried it"
	);
	assert_eq!(waiting(&store, Some(SELLER)).await, 1, "only the ABORTED row needs a person");
}

/// With `nav.electronic_invoice` on, the PDF's hash is part of the filing, so an invoice whose
/// `RENDER_PDF` has not landed is not filable yet. It is left out of the batch rather than
/// failing it — one unrendered invoice must not hold up the others' statutory deadline.
#[tokio::test]
async fn an_unrendered_pdf_is_not_a_batch_candidate() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-unrendered-candidate");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();
	app.settings.set("nav.electronic_invoice", "1", None).await.unwrap();

	let first = issue_at(&store, FEB10).await;
	let unrendered = issue_without_pdf(&store, FEB10).await;
	enqueue_report(&app, first.id).await;
	enqueue_report(&app, unrendered.id).await;

	file(&app, &store, &store, first.id).await.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(operations(&sent[0]), 1, "the unrendered invoice is left out: {}", sent[0]);
	assert!(submissions(&store, unrendered.id).await.is_empty());
	assert_eq!(
		row_of(&store, first.id).await.1.as_deref(),
		Some("TX-VERDICT"),
		"and the batch it would have joined still files"
	);
}

/// The audit export states the `invoiceAppearance` NAV holds, frozen on the filing, not what
/// `nav.electronic_invoice` says today.
#[tokio::test]
async fn the_export_keeps_the_filed_appearance_after_the_setting_flips() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("export-filed-appearance");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.electronic_invoice", "1", None).await.unwrap();
	let filed = issue_at(&store, FEB10).await;
	file(&app, &store, &store, filed.id).await.unwrap();
	assert_eq!(
		store.filing_by_invoice(filed.id).await.unwrap().unwrap().row.electronic,
		Some(true)
	);

	app.settings.set("nav.electronic_invoice", "0", None).await.unwrap();
	let mut out = Vec::new();
	let ctx = Ctx::system("test").with_org(ORG);
	Nav::new(app.clone())
		.audit_export(&ctx, SELLER, Selection::IssueDate { from: FROM, to: TO }, &mut out)
		.await
		.unwrap();
	let xml = String::from_utf8(out).unwrap();
	assert!(xml.contains("<invoiceAppearance>ELECTRONIC</invoiceAppearance>"), "{xml}");
}

/// §1.9.2. A `manageInvoice` that got no answer leaves nobody knowing whether NAV holds the
/// batch; without this, one timeout strands `nav.batch_max` invoices at `E-NAV-REQUEST-ID-REUSED`
/// with no automated way to ask. The transaction list carries no `requestId`, so the batch is
/// claimed by the invoice numbers the echoed `originalRequest` carries — and nothing is re-sent.
#[tokio::test]
async fn reconciliation_binds_the_batch_to_the_transaction_nav_kept() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	// `504` is indeterminate — NAV may have taken the filing — which is the only thing that
	// enqueues a reconciliation. A connect failure or `503` demonstrably never arrived.
	mock(&server, "manageInvoice", 504, "<html>gateway</html>".to_owned()).await;

	let db = TmpDb::new("batch-reconcile-found");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let first = issue_at(&store, FEB10).await;
	let second = issue_at(&store, FEB10).await;
	enqueue_report(&app, first.id).await;
	enqueue_report(&app, second.id).await;

	let err = file(&app, &store, &store, first.id)
		.await
		.expect_err("no answer is not a filing");
	assert_eq!(err.parts().1, "E-CORE-TIMEOUT", "{err:?}");
	assert_eq!(enqueued(&store, "NAV_RECONCILE").await, 1);

	mock(&server, "queryTransactionList", 200, transaction_list_reply(&["TX-LOST"])).await;
	mock(
		&server,
		"queryTransactionStatus",
		200,
		original_request_reply(&[
			(1, first.number.as_deref().unwrap()),
			(2, second.number.as_deref().unwrap()),
		]),
	)
	.await;

	let uid = batch_of(&store, first.id).await.uid;
	mintworks_nav::job::reconcile(&app, &store, &store, uid.as_str()).await.unwrap();

	for (invoice, idx) in [(&first, 1), (&second, 2)] {
		let (_, tx, got, _, verdict) = row_of(&store, invoice.id).await;
		assert_eq!(tx.as_deref(), Some("TX-LOST"), "invoice {}", invoice.id);
		assert_eq!(got, Some(idx), "the index it rode under");
		assert!(verdict.is_none(), "the ordinary poll decides the verdict");
	}
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "the batch is back on the poll path");
	assert_eq!(requests(&server, "manageInvoice").await.len(), 1, "nothing was re-sent");
}

/// A batch's archived envelope carries every other org's `invoiceData` as decodable base64, so
/// `Nav::filing_archives` is operator-only.
#[tokio::test]
async fn a_user_never_reads_the_batch_envelope() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-envelope-scope");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (?, 'org_u', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Masik', 1, 0)",
	)
	.bind(ORG + 1)
	.execute(store.write_pool())
	.await
	.unwrap();

	let first = issue_at(&store, FEB10).await;
	let other = issue_at(&store, FEB10).await;
	enqueue_report(&app, first.id).await;
	enqueue_report(&app, other.id).await;
	// `batch_candidates` keys on `seller_id`, not on the org, so one envelope carries both.
	move_to_org(&store, other.id, ORG + 1).await;

	file(&app, &store, &store, first.id).await.unwrap();
	assert_eq!(operations(&requests(&server, "manageInvoice").await[0]), 2);

	let handle = Nav::new(app.clone());
	let mut user = Ctx::system("test").with_org(ORG);
	user.actor = mintworks_core::ctx::Actor::User { account_id: 1 };
	let row = handle.filing(&user, first.uid.as_str()).await.unwrap().unwrap().row;
	assert_eq!(row.verdict, None, "the row's own state is still readable");
	assert!(row.done_at.is_none());

	let err = handle.filing_archives(&user, first.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN", "the envelope carries another org's invoiceData");

	let operator = Ctx::system("test").with_org(ORG);
	let archives = handle.filing_archives(&operator, first.uid.as_str()).await.unwrap();
	assert!(archives[0].request_xml.is_some(), "an operator reads the archive");
}

/// A resend under one requestId repeats what attempt one filed: `nav.electronic_invoice` flipping
/// in between must not change the appearance or drop the hash.
#[tokio::test]
async fn a_retry_files_what_the_row_recorded_after_the_setting_flips() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock_once(&server, "manageInvoice", 503, "<html>down</html>".to_owned()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("retry-keeps-electronic");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.electronic_invoice", "1", None).await.unwrap();
	let invoice = issue_at(&store, FEB10).await;
	file(&app, &store, &store, invoice.id)
		.await
		.expect_err("the first POST is lost on the wire");

	app.settings.set("nav.electronic_invoice", "0", None).await.unwrap();
	file(&app, &store, &store, invoice.id).await.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert!(sent[1].contains("electronicInvoiceHash"), "{}", sent[1]);
	let row = store.filing_by_invoice(invoice.id).await.unwrap().unwrap().row;
	assert_eq!(row.electronic, Some(true));
}

/// An invoice that cannot be built is left out of the claim, not claimed and dropped: its own
/// `NAV_REPORT` fails on its own terms and retries, and the rest of the batch files.
#[tokio::test]
async fn an_unbuildable_invoice_is_never_claimed() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-unbuildable");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let first = issue_at(&store, FEB10).await;
	let broken = issue_unfilable_at(&store, FEB10).await;
	let good = issue_at(&store, FEB10).await;
	for id in [first.id, broken.id, good.id] {
		enqueue_report(&app, id).await;
	}

	file(&app, &store, &store, first.id).await.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(operations(&sent[0]), 2, "only the two that build: {}", sent[0]);
	assert!(
		submissions(&store, broken.id).await.is_empty(),
		"nothing was claimed for it, so its own job fails on its own terms and retries"
	);
	assert_eq!(report_jobs(&store, broken.id).await[0].1, "PENDING");
}

/// A retryable fault proves nothing was filed, so the batch resends whole, under the same
/// `requestId`, with the fault on the batch and never on an invoice's row.
#[tokio::test]
async fn a_retryable_fault_resends_the_whole_batch_under_its_request_id() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	// `Disposition::Retry` in `client::FAULTS`, so `client::business` classes it
	// `Retry::Backoff`.
	mock_once(&server, "manageInvoice", 200, error_reply("OPERATION_FAILED", "try again later"))
		.await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-retryable-fault");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let first = issue_at(&store, FEB10).await;
	let second = issue_at(&store, FEB10).await;
	enqueue_report(&app, first.id).await;
	enqueue_report(&app, second.id).await;

	let err = file(&app, &store, &store, first.id).await.expect_err("a fault is not a filing");
	assert_eq!(err.retry(), mintworks_core::Retry::Backoff, "{err:?}");

	let batch = batch_of(&store, first.id).await;
	assert_eq!(batch.state, BatchState::Claimed, "proved not filed, so it is free to resend");
	assert_eq!(batch.error_code.as_deref(), Some("OPERATION_FAILED"), "the reason is on the batch");
	let kept = store.filing_by_invoice(second.id).await.unwrap().unwrap().row;
	assert_eq!(kept.batch_id, Some(batch.id), "still in the batch");
	assert!(kept.error_code.is_none(), "a batch fault is not this invoice's error");
	assert!(kept.verdict.is_none());

	file(&app, &store, &store, first.id).await.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(sent.len(), 2);
	for (n, body) in sent.iter().enumerate() {
		assert!(body.contains(batch.uid.as_str()), "attempt {n} changed the requestId");
		assert_eq!(operations(body), 2, "attempt {n} changed the rows: {body}");
	}
	assert_eq!(
		row_of(&store, second.id).await.1.as_deref(),
		Some("TX-VERDICT"),
		"and it reached NAV"
	);
}

/// `E-NAV-NO-TRANSACTION-ID` is the canonical §1.9.2 lost reply: NAV accepted and we lost the handle,
/// which is what `queryTransactionList` exists to resolve without an operator.
#[tokio::test]
async fn an_ok_with_no_transaction_id_schedules_a_reconciliation() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <ManageInvoiceResponse{ENVELOPE}>\
			 <common:result><common:funcCode>OK</common:funcCode></common:result>\
			 </ManageInvoiceResponse>"
		),
	)
	.await;

	let db = TmpDb::new("report-lost-transaction-id");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;

	let first = issue_at(&store, FEB10).await;
	enqueue_report(&app, first.id).await;

	let err = file(&app, &store, &store, first.id)
		.await
		.expect_err("OK with no transactionId is not something to act on");
	assert_eq!(err.parts().1, "E-NAV-NO-TRANSACTION-ID");

	let key: Option<String> =
		sqlx::query_scalar("SELECT dedup_key FROM jobs WHERE kind = 'NAV_RECONCILE'")
			.fetch_optional(store.read_pool())
			.await
			.unwrap();
	let uid = batch_of(&store, first.id).await.uid;
	assert_eq!(key, Some(mintworks_nav::job::reconcile_key(uid.as_str())));

	file(&app, &store, &store, first.id)
		.await
		.expect("the reconciliation decides, not a resend");
	assert_eq!(requests(&server, "manageInvoice").await.len(), 1, "§1.9.2 forbids the repeat");
}

/// `nav:reconcile:{uid}` survives `DONE`, and a reconciliation legitimately ends `DONE` having
/// settled nothing. The resend §1.9.2 then requires may lose its reply too, which must revive the
/// key to be asked about again.
#[tokio::test]
async fn a_second_lost_reply_revives_a_reconciliation_that_settled_nothing() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 504, "<html>gateway</html>".to_owned()).await;
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&[])).await;

	let db = TmpDb::new("batch-reconcile-revive");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;

	let invoice = issue_at(&store, FEB10).await;
	enqueue_report(&app, invoice.id).await;

	file(&app, &store, &store, invoice.id)
		.await
		.expect_err("no answer is not a filing");
	let batch = batch_of(&store, invoice.id).await;
	let uid = batch.uid.as_str();
	let reconcile_jobs = || async {
		sqlx::query_as::<_, (String,)>(
			"SELECT status FROM jobs WHERE kind = 'NAV_RECONCILE' AND payload = ?",
		)
		.bind(mintworks_nav::job::batch_payload(uid))
		.fetch_all(store.read_pool())
		.await
		.unwrap()
	};
	assert_eq!(reconcile_jobs().await, vec![("PENDING".to_owned(),)]);

	// It runs, finds nothing NAV kept, and completes `Ok` having settled nothing — which is the
	// runner marking the one row that holds the key `DONE`.
	mintworks_nav::job::reconcile(&app, &store, &store, uid).await.unwrap();
	sqlx::query("UPDATE jobs SET status = 'DONE' WHERE kind = 'NAV_RECONCILE'")
		.execute(store.write_pool())
		.await
		.unwrap();

	file(&app, &store, &store, invoice.id)
		.await
		.expect_err("the second reply is lost too");

	assert_eq!(requests(&server, "manageInvoice").await.len(), 2, "§1.9.2 requires the resend");
	assert_eq!(
		reconcile_jobs().await,
		vec![("PENDING".to_owned(),)],
		"one row, revived: a batch that settled nothing can be asked about again"
	);
	assert_eq!(row_of(&store, invoice.id).await.1, None, "and nothing is bound to it yet");
}

/// An `invoiceStatus` outside `InvoiceStatusType` still polls, since a status NAV has just added
/// must not park a statutory filing, but the batch carries a marker, which is what
/// `awaiting_operator` and `A-NAV-REJECTED` count.
#[tokio::test]
async fn an_unknown_invoice_status_records_a_fault_and_keeps_polling() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(
		&server,
		"queryTransactionStatus",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <QueryTransactionStatusResponse{ENVELOPE}>\
			 <common:result><common:funcCode>OK</common:funcCode></common:result>\
			 <processingResults><processingResult><index>1</index>\
			 <invoiceStatus>WEIRD</invoiceStatus>\
			 </processingResult></processingResults>\
			 </QueryTransactionStatusResponse>"
		),
	)
	.await;

	let db = TmpDb::new("poll-unknown-status");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let batch = sent(&store, &[&invoice], "TX-WEIRD").await;

	let next = poll(&app, &store, &store, batch.uid.as_str()).await.unwrap();
	assert!(
		matches!(next, mintworks_core::job::Next::Again { .. }),
		"an unknown status settles nothing, so the poll asks again: {next:?}"
	);

	assert_eq!(submissions(&store, invoice.id).await[0].1, None, "not a verdict");
	assert_eq!(
		store
			.batch_by_uid(batch.uid.as_str())
			.await
			.unwrap()
			.unwrap()
			.error_code
			.as_deref(),
		Some(mintworks_nav::job::E_NAV_UNKNOWN_STATUS)
	);
	assert_eq!(waiting(&store, Some(SELLER)).await, 1);
}

/// `Next::Again` is a success carrying its own next run: an `Err` here would make a healthy poll
/// indistinguishable from a failing one.
#[tokio::test]
async fn a_pending_poll_reschedules_itself_without_failing_the_job() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	let processing = status_results(&[(1, "PROCESSING", "")]);
	mock(&server, "queryTransactionStatus", 200, processing).await;

	let db = TmpDb::new("poll-defers");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let batch = sent(&store, &[&invoice], "TX-PENDING").await;
	let next = poll(&app, &store, &store, batch.uid.as_str()).await.unwrap();
	let mintworks_core::job::Next::Again { at } = next else {
		panic!("NAV is still processing, so the poll asks again: {next:?}");
	};
	assert!(at.0 > Timestamp::now().0, "and it asks later, not now");

	// And the filing itself is untouched: no verdict, and no marker this early.
	assert!(row_of(&store, invoice.id).await.4.is_none());
	assert!(
		store
			.batch_by_uid(batch.uid.as_str())
			.await
			.unwrap()
			.unwrap()
			.error_code
			.is_none()
	);
}

/// The tail of `reconcile` reads an empty window as "NAV never took it", but here NAV said it did:
/// a missed window would resend into the same refusal forever.
#[tokio::test]
async fn request_id_not_unique_with_no_matching_transaction_holds_and_never_resends() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;
	let db = TmpDb::new("reused-unproven");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	// NAV accepted the POST and the `transactionId` could not be recorded.
	let nav = Faulty(Arc::new(store.clone()), Fail::SetSent);
	file(&app, &store, &nav, invoice.id)
		.await
		.expect_err("the transactionId could not be recorded");
	assert_eq!(enqueued(&store, "NAV_RECONCILE").await, 1, "a failed stamp must reconcile");

	// A reconciliation that missed it resent, and NAV refused the `requestId`.
	let batch = batch_of(&store, invoice.id).await;
	assert_eq!(batch.state, BatchState::InDoubt);
	assert!(store.clear_doubt(batch.id).await.unwrap());
	server.reset().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, error_reply("REQUEST_ID_NOT_UNIQUE", "used")).await;
	let uid = batch.uid.as_str();
	let err = mintworks_nav::job::send(&app, &store, &store, uid)
		.await
		.expect_err("NAV has this requestId");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-REUSED", "{err:?}");
	assert_eq!(batch_of(&store, invoice.id).await.state, BatchState::InDoubt);
	assert_eq!(enqueued(&store, "NAV_RECONCILE").await, 1, "the refusal itself must reconcile");
	let sent = requests(&server, "manageInvoice").await.len();

	mock(&server, "queryTransactionList", 200, transaction_list_reply(&[])).await;
	mintworks_nav::job::reconcile(&app, &store, &store, uid)
		.await
		.expect("holding is a proved outcome, so the job ends");

	let found = store.filing_by_invoice(invoice.id).await.unwrap().unwrap();
	assert_eq!(found.state(), FilingState::Held);
	assert_eq!(
		found.batch.unwrap().error_code.as_deref(),
		Some(mintworks_nav::job::E_NAV_RECONCILE_UNPROVEN)
	);
	assert_eq!(requests(&server, "manageInvoice").await.len(), sent, "nothing was re-sent");
}

/// A tenant rotating the password at NAV must not kill its invoices: the refusal is retried with
/// the reason on the batch, nothing went on the wire, and reconnecting files them.
#[tokio::test]
async fn a_refused_token_exchange_keeps_the_filing_alive_until_the_tenant_reconnects() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	let db = TmpDb::new("creds-refused");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let (_, ver) = tenant(&store).await;
	let handle = Nav::new(app.clone());
	handle.set_credentials(&tenant_admin(), &tenant_creds()).await.unwrap();
	let invoice = issue_as(&store, TENANT, FEB10, ver).await;
	put_pdf(&store, &invoice).await;
	enqueue_report(&app, invoice.id).await;

	server.reset().await;
	mock(&server, "tokenExchange", 400, error_reply("INVALID_SECURITY_USER", "bad user")).await;
	let err = file(&app, &store, &store, invoice.id)
		.await
		.expect_err("NAV refused the credentials");
	assert_eq!(err.parts().1, "E-NAV-CREDENTIALS", "{err:?}");
	assert_eq!(err.retry(), mintworks_core::Retry::Backoff, "{err:?}");
	assert_eq!(batch_of(&store, invoice.id).await.state, BatchState::Claimed);

	let status = handle.credentials_status(&tenant_admin()).await.unwrap();
	assert!(status.connected && status.credentials_refused, "{status:?}");

	server.reset().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;
	let status = handle.set_credentials(&tenant_admin(), &tenant_creds()).await.unwrap();
	assert!(!status.credentials_refused, "NAV just took them");

	file(&app, &store, &store, invoice.id).await.unwrap();
	assert!(
		batch_of(&store, invoice.id).await.transaction_id.is_some(),
		"the invoice files after reconnecting"
	);
}

/// This product's sellers are tenants and its root org owns none, so the count spans every seller.
#[tokio::test]
async fn a_tenants_stuck_filing_raises_the_deployment_wide_alert() {
	let db = TmpDb::new("alert-tenant");
	let (app, store) = setup(&db).await;
	let (_, ver) = tenant(&store).await;
	let invoice = issue_as(&store, TENANT, FEB10, ver).await;
	fail_submission(&store, invoice.id).await;

	assert_eq!(waiting(&store, Some(SELLER)).await, 0);
	assert_eq!(waiting(&store, Some(TENANT)).await, 1);
	assert_eq!(waiting(&store, None).await, 1);
	let alerts = mintworks_nav::alerts(app).await.unwrap();
	assert_eq!(
		alerts.iter().map(|a| (a.code, a.count)).collect::<Vec<_>>(),
		[("A-NAV-REJECTED", 1)]
	);
}

/// Nothing feeding `invoice_data` is frozen with the claim: an invoice whose PDF hash is gone by the
/// second attempt no longer builds. Its row leaves the batch, so its own `NAV_REPORT` takes over,
/// and the rest of the batch files without it.
#[tokio::test]
async fn a_row_that_stops_building_is_released_to_its_own_report() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock_once(&server, "manageInvoice", 503, "<html>down</html>".to_owned()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-unbuildable-on-resend");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.electronic_invoice", "1", None).await.unwrap();

	let first = issue_at(&store, FEB10).await;
	let second = issue_at(&store, FEB10).await;
	for id in [first.id, second.id] {
		enqueue_report(&app, id).await;
	}

	file(&app, &store, &store, first.id)
		.await
		.expect_err("the first POST is lost on the wire");
	sqlx::query("DELETE FROM invoice_documents WHERE invoice_id = ?")
		.bind(second.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	file(&app, &store, &store, first.id).await.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(operations(&sent[1]), 1, "the batch files without it: {}", sent[1]);
	assert!(submissions(&store, second.id).await.is_empty(), "the row left, not stranded in it");
	assert_eq!(store.unfiled_invoices(SELLER, 50).await.unwrap(), vec![second.id]);
	assert_eq!(
		report_jobs(&store, second.id).await[0].1,
		"PENDING",
		"its own NAV_REPORT takes over"
	);
}

/// A window over the page ceiling is unreadable, not empty: concluding "NAV never took this batch"
/// from it would resend under a burned `requestId`, the one outcome §1.9.2 exists to avoid.
#[tokio::test]
async fn a_transaction_list_over_the_page_ceiling_settles_nothing() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 504, "<html>gateway</html>".to_owned()).await;
	mock(
		&server,
		"queryTransactionList",
		200,
		transaction_list_reply(&[]).replace("<availablePage>1<", "<availablePage>999<"),
	)
	.await;

	let db = TmpDb::new("batch-reconcile-page-ceiling");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;

	let first = issue_at(&store, FEB10).await;
	enqueue_report(&app, first.id).await;

	file(&app, &store, &store, first.id)
		.await
		.expect_err("no answer is not a filing");
	let uid = batch_of(&store, first.id).await.uid;

	let err = mintworks_nav::job::reconcile(&app, &store, &store, uid.as_str())
		.await
		.expect_err("the window is unreadable, so nothing is concluded");
	assert_eq!(err.parts().1, "E-NAV-UNAVAILABLE");
	assert_eq!(err.retry(), mintworks_core::Retry::Backoff, "{err:?}");
	assert_eq!(
		batch_of(&store, first.id).await.state,
		BatchState::InDoubt,
		"the batch must not go back to a resend under a burned requestId"
	);
}

// ---------------------------------------------------------------- the frozen supplier

/// `supplierInfo` and `smallBusinessIndicator` come from the version the invoice froze, not
/// from whatever the seller looks like when the filing is built — and `NAV_REPORT` is delayed
/// by `invoice.nav_report_delay_secs`, retried and redrivable days later, so that window is
/// wide.
#[test]
fn the_supplier_block_is_built_from_the_frozen_version() {
	let version = SellerVersion {
		seller_ver: 7,
		seller_id: SELLER,
		status: SellerVersionStatus::Archived,
		name: "Regi Kft.".into(),
		country: "HU".into(),
		tax_number: "12345678242".into(),
		group_member_tax_no: None,
		eu_vat_id: None,
		postcode: "1011".into(),
		city: "Budapest".into(),
		street: "Regi utca 1.".into(),
		bank_account: Some("11111111-22222222-33333333".into()),
		bank_name: None,
		small_business: true,
		cash_accounting: false,
		vat_scheme: "ALANYI_MENTES".into(),
		income_regime: "NONE".into(),
		expense_ratio_pct: None,
		regime_since: None,
		email: None,
		accent: None,
		logo_blob: None,
		created_at: Timestamp(0),
		valid_from: Some(Timestamp(0)),
		superseded_at: Some(Timestamp(1)),
	};
	let (invoice, lines, groups) = xml_parts();

	let xml =
		mintworks_nav::xml::invoice_data(&version, &invoice, &lines, &groups, None, true).unwrap();
	assert!(xml.contains("<supplierName>Regi Kft.</supplierName>"), "{xml}");
	assert!(
		xml.contains("<base:additionalAddressDetail>Regi utca 1.</base:additionalAddressDetail>"),
		"{xml}"
	);
	assert!(xml.contains("<supplierBankAccountNumber>11111111-22222222-33333333"), "{xml}");
	// Both flags are the version's, and both used to be read live.
	assert!(xml.contains("<individualExemption>true</individualExemption>"), "{xml}");
	assert!(xml.contains("<smallBusinessIndicator>true</smallBusinessIndicator>"), "{xml}");
}

/// Gap 2: the magnitude comes from the frozen `vat_rate_bp` column, the *classification* from
/// the `VatCode`. A statutory rate change must not re-file a historical invoice at the new
/// rate — `pdf.rs`'s `vat_label` already reads the column, and the two renderers have to agree.
#[test]
fn the_stored_vat_rate_is_filed_not_the_compiled_in_one() {
	let (invoice, mut lines, mut groups) = xml_parts();
	// 25%, the rate before 2012. `VatCode::Std27::rate_bp()` is 2700 and always will be.
	lines[0].vat_rate_bp = 2500;
	groups[0].vat_rate_bp = 2500;

	let xml = mintworks_nav::xml::invoice_data(
		&seller_version_row(),
		&invoice,
		&lines,
		&groups,
		None,
		true,
	)
	.unwrap();
	assert_eq!(xml.matches("<vatPercentage>0.2500</vatPercentage>").count(), 2, "{xml}");
	assert!(!xml.contains("0.2700"), "the compiled-in rate reached the filing:\n{xml}");
}

/// One export, two invoices issued either side of a published seller edit: each
/// `<supplierInfo>` has to be its own. The export regenerates from `invoices` on demand for
/// any historical range, so stamping one seller onto all of them is a statutory eight-year
/// record re-serialised with today's master data.
#[tokio::test]
async fn an_export_spanning_a_seller_edit_gives_each_invoice_its_own_supplier() {
	let db = TmpDb::new("export-seller-edit");
	let (app, store) = setup(&db).await;

	let old_seller = issue_at(&store, FEB01).await;
	store
		.save_seller_version_draft(
			SELLER,
			&SellerVersionPatch { name: Some("Uj Nev Kft.".into()), ..Default::default() },
		)
		.await
		.unwrap();
	let new_ver = store
		.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap()
		.unwrap();
	let new_seller = issue_at_under(&store, FEB10, new_ver).await;
	assert_ne!(old_seller.seller_ver, new_seller.seller_ver);

	let ctx = Ctx::system("test").with_org(ORG);
	let mut out = Vec::new();
	Nav::new(app)
		.audit_export(&ctx, SELLER, Selection::IssueDate { from: FROM, to: TO }, &mut out)
		.await
		.unwrap();

	let xml = String::from_utf8(out).unwrap();
	assert_eq!(xml.matches("<supplierName>Teszt Kft.</supplierName>").count(), 1, "{xml}");
	assert_eq!(xml.matches("<supplierName>Uj Nev Kft.</supplierName>").count(), 1, "{xml}");
}

/// One HUF invoice's worth of `xml::invoice_data` input, built in memory: these three tests are
/// about what the writer *reads*, so nothing here goes through a store.
fn xml_parts() -> (Invoice, Vec<mintworks_invoice::store::InvoiceLine>, Vec<InvoiceVatGroup>) {
	let mut invoice = Invoice {
		id: 1,
		uid: mintworks_core::prelude::InvoiceId::generate(),
		request_id: None,
		org_id: ORG,
		seller_id: SELLER,
		seller_ver: Some(SELLER_VER),
		billing_party_id: None,
		kind: InvoiceKind::Normal,
		status: mintworks_invoice::store::InvoiceStatus::Issued,
		series_code: Some("A".into()),
		series_year: Some(2026),
		number: Some("A2026/000001".into()),
		issued_at: Some(Timestamp(JAN15)),
		fulfilment_date: Some("2026-01-15".into()),
		due_date: Some("2026-01-23".into()),
		payment_method: PaymentMethod::Transfer,
		original_invoice_id: None,
		modification_index: None,
		currency: mintworks_core::prelude::CurrencyCode::huf(),
		rate_e6: 1_000_000,
		rate_date: None,
		rate_source: None,
		huf_rate_e6: None,
		net: Money(100_000),
		vat: Money(27_000),
		gross: Money(127_000),
		paid_amount: Money(0),
		paid_at: None,
		vat_note: None,
		notes: None,
		discount_kind: None,
		discount_value: None,
		buyer_kind: Some(PartyKind::Company),
		buyer_name: Some("Vevo Zrt.".into()),
		buyer_country: Some("HU".into()),
		buyer_tax_number: Some("87654321242".into()),
		buyer_eu_vat_id: None,
		buyer_group_tax_no: None,
		buyer_postcode: Some("1052".into()),
		buyer_city: Some("Budapest".into()),
		buyer_street: Some("Deak ter 2.".into()),
		buyer_vies_request_id: None,
		buyer_vies_checked_at: None,
		test_mode: false,
		doc_lang: None,
		created_at: Timestamp(0),
		updated_at: Timestamp(0),
		version: 1,
		period_start: None,
		period_end: None,
	};
	invoice.id = 1;
	let src = line(100_000);
	let line = mintworks_invoice::store::InvoiceLine {
		id: 1,
		invoice_id: 1,
		line_no: 1,
		service_id: src.service_id,
		description: src.description,
		unit: src.unit,
		qty: src.qty,
		unit_price: src.unit_price,
		discount_kind: src.discount_kind,
		discount_value: src.discount_value,
		discount_amount: src.discount_amount,
		discount_description: src.discount_description,
		net: src.net,
		vat_code: src.vat_code,
		vat_rate_bp: src.vat_rate_bp,
		vat: src.vat,
		gross: src.gross,
		note: src.note,
	};
	(invoice, vec![line], vec![group(1, 100_000)])
}

/// [`seller_version`] as the stored row rather than as a patch — what the writer takes.
fn seller_version_row() -> SellerVersion {
	SellerVersion {
		seller_ver: SELLER_VER,
		seller_id: SELLER,
		status: SellerVersionStatus::Current,
		name: "Teszt Kft.".into(),
		country: "HU".into(),
		tax_number: "12345678242".into(),
		group_member_tax_no: None,
		eu_vat_id: None,
		postcode: "1011".into(),
		city: "Budapest".into(),
		street: "Fo utca 1.".into(),
		bank_account: None,
		bank_name: None,
		small_business: false,
		cash_accounting: false,
		vat_scheme: "NORMAL".into(),
		income_regime: "NONE".into(),
		expense_ratio_pct: None,
		regime_since: None,
		email: None,
		accent: None,
		logo_blob: None,
		created_at: Timestamp(0),
		valid_from: Some(Timestamp(0)),
		superseded_at: None,
	}
}

// ---- tenant sellers: org-scoped NAV credentials ------------------------------------------

/// A seller minted on the SHARED org [`ORG`] rather than on root.
const TENANT: i64 = 2;

/// Seeds [`TENANT`] with no NAV connection, account 1 its org's Admin, and returns it with the
/// version it published.
async fn tenant(store: &SqliteStore) -> (Seller, i64) {
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 1, 'ADMIN', 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	let seller =
		Seller { id: TENANT, org_id: ORG, nav_login: None, series_code: "T".into(), ..seller() };
	seed_seller(store, &seller, &seller_version()).await;
	let ver = store.current_seller_version(TENANT).await.unwrap().unwrap().seller_ver;
	(store.seller_by_id(TENANT).await.unwrap().unwrap(), ver)
}

/// Account 1 acting in [`ORG`] with a freshly presented credential.
fn tenant_admin() -> Ctx {
	let mut ctx = Ctx::system("test").with_org(ORG);
	ctx.actor = mintworks_core::ctx::Actor::User { account_id: 1 };
	ctx.auth_at = Some(Timestamp::now().0);
	ctx
}

fn tenant_creds() -> mintworks_nav::NavCredentials {
	mintworks_nav::NavCredentials {
		login: " tenantuser ".into(),
		tech_password: "tenant-pw".into(),
		sign_key: "tenant-sign".into(),
		exchange_key: String::from_utf8(EXCHANGE_KEY.to_vec()).unwrap(),
	}
}

/// `(org_id, key)` of every stored NAV secret.
async fn nav_secret_rows(store: &SqliteStore) -> Vec<(i64, String)> {
	sqlx::query_as("SELECT org_id, key FROM secrets WHERE key LIKE 'nav.%' ORDER BY org_id, key")
		.fetch_all(store.read_pool())
		.await
		.unwrap()
}

#[tokio::test]
async fn set_credentials_verifies_with_nav_before_storing() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/tokenExchange"))
		.respond_with(ResponseTemplate::new(200).set_body_string(token_reply()))
		.expect(1)
		.mount(&server)
		.await;
	let db = TmpDb::new("creds-set");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	tenant(&store).await;

	let status = Nav::new(app.clone())
		.set_credentials(&tenant_admin(), &tenant_creds())
		.await
		.unwrap();
	assert!(status.connected);
	assert_eq!(status.login.as_deref(), Some("tenantuser"));

	let rows = nav_secret_rows(&store).await;
	let at_org: Vec<_> = rows.iter().filter(|(o, _)| *o == ORG).map(|(_, k)| k.as_str()).collect();
	assert_eq!(at_org, ["nav.exchange_key", "nav.sign_key", "nav.tech_password"]);
	assert_eq!(rows.iter().filter(|(o, _)| *o == ROOT).count(), 3, "the global rows are untouched");
	assert_eq!(app.secrets.get_at(ORG, "nav.sign_key").await.unwrap().unwrap(), b"tenant-sign");
	assert_eq!(app.secrets.get("nav.sign_key").await.unwrap().unwrap(), b"sign-key");
	assert_eq!(
		store.seller_by_id(TENANT).await.unwrap().unwrap().nav_login.as_deref(),
		Some("tenantuser")
	);
}

#[tokio::test]
async fn rejected_credentials_store_nothing() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 400, error_reply("INVALID_SECURITY_USER", "bad user")).await;
	let db = TmpDb::new("creds-rejected");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	tenant(&store).await;

	let err = Nav::new(app.clone())
		.set_credentials(&tenant_admin(), &tenant_creds())
		.await
		.unwrap_err();
	assert_eq!(
		err.parts(),
		(mintworks_core::error::StatusCode::BAD_REQUEST, "E-NAV-CREDENTIALS-INVALID")
	);
	assert!(err.to_string().contains("INVALID_SECURITY_USER"), "{err}");
	assert!(nav_secret_rows(&store).await.iter().all(|(o, _)| *o != ORG));
	assert!(store.seller_by_id(TENANT).await.unwrap().unwrap().nav_login.is_none());
}

#[tokio::test]
async fn unregistered_taxpayer_is_named() {
	let server = MockServer::builder().start().await;
	mock(
		&server,
		"tokenExchange",
		400,
		error_reply("NOT_REGISTERED_CUSTOMER", "Nem regisztrált felhasználó!"),
	)
	.await;
	let db = TmpDb::new("creds-unregistered");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	tenant(&store).await;

	let err = Nav::new(app.clone())
		.set_credentials(&tenant_admin(), &tenant_creds())
		.await
		.unwrap_err();
	assert_eq!(
		err.parts(),
		(mintworks_core::error::StatusCode::BAD_REQUEST, "E-NAV-TAXPAYER-UNKNOWN")
	);
	assert!(nav_secret_rows(&store).await.iter().all(|(o, _)| *o != ORG));
	assert!(store.seller_by_id(TENANT).await.unwrap().unwrap().nav_login.is_none());
}

#[tokio::test]
async fn malformed_credentials_are_refused_before_dialling() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.respond_with(ResponseTemplate::new(200).set_body_string(token_reply()))
		.expect(0)
		.mount(&server)
		.await;
	let db = TmpDb::new("creds-malformed");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	tenant(&store).await;

	let bad = mintworks_nav::NavCredentials {
		login: "ab".into(),
		tech_password: "  ".into(),
		exchange_key: "short".into(),
		..tenant_creds()
	};
	let err = Nav::new(app).set_credentials(&tenant_admin(), &bad).await.unwrap_err();
	let Error::ValidationFields(_, fields) = err else { panic!("{err:?}") };
	assert_eq!(
		fields.keys().map(String::as_str).collect::<Vec<_>>(),
		["exchangeKey", "login", "techPassword"]
	);
	assert!(nav_secret_rows(&store).await.iter().all(|(o, _)| *o != ORG));
}

#[tokio::test]
async fn set_credentials_needs_stepup() {
	let db = TmpDb::new("creds-stepup");
	let (app, store) = setup(&db).await;
	tenant(&store).await;
	let mut ctx = tenant_admin();
	ctx.auth_at = Some(Timestamp::now().0 - 600);

	let err = Nav::new(app).set_credentials(&ctx, &tenant_creds()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-STEPUP");
	assert!(nav_secret_rows(&store).await.is_empty());
}

#[tokio::test]
async fn a_tenant_seller_never_falls_back_to_the_global_credentials() {
	let db = TmpDb::new("creds-no-fallback");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, "https://nav.invalid").await;
	let (mut seller, ver) = tenant(&store).await;
	// A login alone is not a connection: the secrets must be the org's own.
	seller.nav_login = Some("tenantuser".into());
	store.put_seller(&seller).await.unwrap();
	let current = store.current_seller_version(TENANT).await.unwrap().unwrap();

	let err = mintworks_nav::auth::NavAuth::load(&app, &seller, &current).await.err().unwrap();
	assert_eq!(err.parts().1, "E-NAV-CREDENTIALS");
	issue_as(&store, TENANT, FEB10, ver).await;
	assert!(mintworks_nav::job::deferral(&app, &store, &seller).await.unwrap().is_some());

	let status = Nav::new(app).credentials_status(&tenant_admin()).await.unwrap();
	assert!(!status.connected && !status.sign_key.set, "the global rows leaked into the status");
	assert_eq!(status.unreported, 1);
}

#[tokio::test]
async fn the_deployment_sellers_credentials_are_not_settable_here() {
	let db = TmpDb::new("creds-global");
	let (app, store) = setup(&db).await;
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 1, 'ADMIN', 0, 0)",
	)
	.bind(ROOT)
	.execute(store.write_pool())
	.await
	.unwrap();

	// `ORG` owns no seller, so it resolves root's.
	let err = Nav::new(app)
		.set_credentials(&tenant_admin(), &tenant_creds())
		.await
		.unwrap_err();
	assert_eq!(
		err.parts(),
		(mintworks_core::error::StatusCode::CONFLICT, "E-NAV-CREDENTIALS-GLOBAL")
	);
	assert!(nav_secret_rows(&store).await.is_empty());
}

#[tokio::test]
async fn an_unconnected_tenant_defers_its_report_without_failing() {
	let db = TmpDb::new("creds-defer");
	let (app, store) = setup(&db).await;
	let (seller, ver) = tenant(&store).await;
	let invoice = issue_as(&store, TENANT, FEB10, ver).await;

	let at = mintworks_nav::job::deferral(&app, &store, &seller).await.unwrap().unwrap();
	assert!(at.0 >= Timestamp::now().0 + mintworks_nav::job::NOT_CONNECTED_RECHECK_SECS - 5);
	assert!(submissions(&store, invoice.id).await.is_empty(), "a deferral opens no filing");

	// The deployment's own seller never defers: its gap is an operator fault, not a wait.
	let root = stored_seller(&store).await;
	assert!(mintworks_nav::job::deferral(&app, &store, &root).await.unwrap().is_none());
}

#[tokio::test]
async fn connecting_wakes_the_deferred_backlog() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	let db = TmpDb::new("creds-wake");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let (_, ver) = tenant(&store).await;
	let invoice = issue_as(&store, TENANT, FEB10, ver).await;
	let later = Timestamp(Timestamp::now().0 + 86_400);
	mintworks_core::job::enqueue(
		&app.store,
		"NAV_REPORT",
		&mintworks_invoice::invoice_job_payload(invoice.id),
		Some(&format!("nav:invoice:{}", invoice.id)),
		later,
	)
	.await
	.unwrap()
	.unwrap();

	let status = Nav::new(app.clone())
		.set_credentials(&tenant_admin(), &tenant_creds())
		.await
		.unwrap();
	assert_eq!(status.unreported, 1);
	let run_at: i64 = sqlx::query_scalar("SELECT run_at FROM jobs WHERE kind = 'NAV_REPORT'")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert!(run_at <= Timestamp::now().0, "the deferred filing was not pulled forward");
	let seller = store.seller_by_id(TENANT).await.unwrap().unwrap();
	assert!(mintworks_nav::job::deferral(&app, &store, &seller).await.unwrap().is_none());
}

#[tokio::test]
async fn a_deployment_with_no_root_seller_boots() {
	let db = TmpDb::new("no-root-seller");
	let (app, store) = setup(&db).await;
	sqlx::query("UPDATE sellers SET org_id = ?")
		.bind(ORG)
		.execute(store.write_pool())
		.await
		.unwrap();

	mintworks_nav::job::seed(&app).await.unwrap();
	mintworks_nav::job::sweep(&app, &store).await.unwrap();
	assert!(mintworks_nav::alerts(app).await.unwrap().is_empty());
}

// ---------------------------------------------------------------- go-live

/// [`tenant`] in test mode, with account 1 promoted to its org's Owner.
async fn test_tenant(store: &SqliteStore, owner: bool) {
	tenant(store).await;
	if owner {
		sqlx::query("UPDATE memberships SET role = 'OWNER' WHERE org_id = ? AND account_id = 1")
			.bind(ORG)
			.execute(store.write_pool())
			.await
			.unwrap();
	}
	assert!(store.set_seller_test_mode(TENANT, true).await.unwrap());
}

async fn test_mode_of(store: &SqliteStore) -> bool {
	store.seller_by_id(TENANT).await.unwrap().unwrap().test_mode
}

#[tokio::test]
async fn go_live_is_refused_without_credentials() {
	let db = TmpDb::new("live-no-creds");
	let (app, store) = setup(&db).await;
	test_tenant(&store, true).await;

	let err = Nav::new(app).go_live(&tenant_admin()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-NOT-CONNECTED");
	assert!(test_mode_of(&store).await);
}

#[tokio::test]
async fn go_live_is_refused_for_an_admin_who_is_not_the_owner() {
	let db = TmpDb::new("live-not-owner");
	let (app, store) = setup(&db).await;
	test_tenant(&store, false).await;

	assert!(Nav::new(app).go_live(&tenant_admin()).await.is_err());
	assert!(test_mode_of(&store).await);
}

#[tokio::test]
async fn go_live_needs_stepup() {
	let db = TmpDb::new("live-stepup");
	let (app, store) = setup(&db).await;
	test_tenant(&store, true).await;
	let mut ctx = tenant_admin();
	ctx.auth_at = Some(Timestamp::now().0 - 600);

	let err = Nav::new(app).go_live(&ctx).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-STEPUP");
	assert!(test_mode_of(&store).await);
}

/// A retry or refile files with NAV, the same class of act as `Nav::submit`.
#[tokio::test]
async fn retry_needs_stepup() {
	let db = TmpDb::new("retry-stepup");
	let (app, store) = setup(&db).await;
	let (_, ver) = tenant(&store).await;
	let invoice = issue_mode(&store, TENANT, JAN15, ver, false).await;
	let uid = invoice.uid.as_str();
	let nav = Nav::new(app);
	let mut ctx = tenant_admin();

	ctx.auth_at = Some(Timestamp::now().0 - 600);
	assert_eq!(nav.retry(&ctx, uid).await.unwrap_err().parts().1, "E-AUTH-STEPUP");
	ctx.auth_at = None;
	assert_eq!(nav.retry(&ctx, uid).await.unwrap_err().parts().1, "E-AUTH-STEPUP-IMPOSSIBLE");

	// Past the gate nothing is filed yet, so it queues the invoice's `NAV_REPORT`.
	for ctx in [tenant_admin(), Ctx::system("test").with_org(ORG)] {
		assert!(nav.retry(&ctx, uid).await.unwrap().is_none());
	}
}

#[tokio::test]
async fn go_live_flips_test_mode_once_connected_and_is_idempotent() {
	let db = TmpDb::new("live-flip");
	let (app, store) = setup(&db).await;
	test_tenant(&store, true).await;
	// Connected as `auth::connected` reads it: a login and all three secrets at the org.
	sqlx::query("UPDATE sellers SET nav_login = 'tenantuser' WHERE id = ?")
		.bind(TENANT)
		.execute(store.write_pool())
		.await
		.unwrap();
	for key in mintworks_nav::SECRETS {
		app.secrets.set_at(ORG, key, b"x", None).await.unwrap();
	}

	let nav = Nav::new(app);
	nav.go_live(&tenant_admin()).await.unwrap();
	assert!(!test_mode_of(&store).await);
	nav.go_live(&tenant_admin()).await.unwrap();
	assert!(!test_mode_of(&store).await);
}

/// A verdict is per invoice, so one refusal refiles alone, in a new batch and so under a new
/// `requestId`, and the old row's record stands. A batch-mate with no verdict keeps its poll.
#[tokio::test]
async fn a_rejected_row_refiles_solo_and_an_unsettled_one_keeps_its_poll() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-refile");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let first = issue_at(&store, FEB10).await;
	let rejected = issue_at(&store, FEB10).await;
	let unsettled = issue_at(&store, FEB10).await;
	file(&app, &store, &store, first.id).await.unwrap();
	assert_eq!(operations(&requests(&server, "manageInvoice").await[0]), 3);
	let old = store.filing_by_invoice(rejected.id).await.unwrap().unwrap();
	store
		.finish(old.row.id, NavVerdict::Rejected, Some(("X", "no")), Timestamp::now())
		.await
		.unwrap();

	let nav = Nav::new(app.clone());
	let ctx = Ctx::system("test").with_org(ORG);
	nav.retry(&ctx, unsettled.uid.as_str()).await.unwrap();
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "its batch's poll, not a refile");

	// A new `transactionId`: `set_sent` refuses one another batch carries.
	server.reset().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply().replace("TX-VERDICT", "TX-2")).await;
	nav.retry(&ctx, rejected.uid.as_str()).await.unwrap();
	file(&app, &store, &store, rejected.id).await.unwrap();

	let rows = store.filings_by_invoice(rejected.id).await.unwrap();
	assert_eq!(rows.len(), 2);
	assert!(rows[1].row.resolved_at.is_some());
	assert_eq!(rows[1].row.verdict, Some(NavVerdict::Rejected), "the statutory record stands");
	let batch = rows[0].batch.as_ref().unwrap();
	assert_ne!(Some(batch.id), old.row.batch_id);
	assert_eq!(batch.transaction_id.as_deref(), Some("TX-2"));
	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(operations(&sent[0]), 1, "solo: {}", sent[0]);
	let rid = format!("<common:requestId>{}</common:requestId>", batch.uid.as_str());
	assert!(sent[0].contains(&rid), "{}", sent[0]);
}

/// A retry opens a new row; the exchange NAV rejected on the old one must stay reachable.
#[tokio::test]
async fn filing_archives_keeps_the_rejected_exchange_after_a_retry() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;
	mock(&server, "queryTransactionStatus", 200, aborted_reply()).await;

	let db = TmpDb::new("archives-after-retry");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;
	let uid = invoice.uid.as_str();
	file(&app, &store, &store, invoice.id).await.unwrap();
	let batch = batch_of(&store, invoice.id).await;
	poll(&app, &store, &store, batch.uid.as_str()).await.unwrap();

	server.reset().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply().replace("TX-VERDICT", "TX-2")).await;
	let nav = Nav::new(app.clone());
	let ctx = Ctx::system("test").with_org(ORG);
	nav.retry(&ctx, uid).await.unwrap();
	file(&app, &store, &store, invoice.id).await.unwrap();

	let archives = nav.filing_archives(&ctx, uid).await.unwrap();
	assert_eq!(archives.len(), 2, "one per filing");
	for archive in &archives {
		let request = archive.request_xml.as_deref().unwrap();
		assert!(request.contains("<invoiceOperation><index>1</index>"), "{request}");
	}
	let reply = |n: usize| archives[n].response_xml.as_deref().unwrap();
	assert!(reply(0).contains("TX-2"), "newest first: {}", reply(0));
	assert!(reply(1).contains("ABORTED"), "{}", reply(1));
}

// vim: ts=4
