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

use mintworks_core::{App, AppBuilder, config::Config, ctx::Ctx, ids::SellerId, prelude::*};
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
	Nav, NavOp, NavStore, NavVerdict,
	export::{Selection, range_error, selection},
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
	store
		.issue(
			draft.id,
			&IssueInvoice { seller_ver, ..issue_input(draft.id, NET, issued_at) },
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

/// A filing NAV rejected, to prove the export ignores NAV state entirely.
async fn fail_submission(store: &SqliteStore, invoice_id: i64) {
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
		.storno(second.id, &cancel, &issue_input(0, -100_000, FEB15))
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

/// A `manageInvoice` that fails on the wire says nothing about the invoice, so the row stays
/// open with no verdict and the job row owns the retry. The retry reuses that one row — the
/// `requestId` is `invoices.uid`, stable across every attempt, which is what makes a resend
/// idempotent at NAV's end and what deleted the old `UNKNOWN` parking state.
///
/// `500` came from the invoice service itself and `503` from the load balancer; neither may open
/// a second row. They part company on the resend: `503` demonstrably never reached NAV, so the
/// retry re-POSTs, while `500` is indeterminate and `NAV_RECONCILE` owns it until it settles —
/// §1.9.2 forbids the immediate repeat under the same `requestId`.
#[tokio::test]
async fn a_lost_reply_stays_retryable_on_one_row() {
	for (status, name) in [(500u16, "report-500"), (503, "report-503")] {
		let indeterminate = status == 500;
		let server = MockServer::builder().start().await;
		mock(&server, "tokenExchange", 200, token_reply()).await;
		mock(&server, "manageInvoice", status, "<html>down</html>".to_owned()).await;

		let db = TmpDb::new(name);
		let (app, store) = setup(&db).await;
		point_at_nav(&app, &server.uri()).await;
		let invoice = issue_at(&store, FEB10).await;

		let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
		let nav: Arc<dyn NavStore> = Arc::new(store.clone());

		let first =
			mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id).await;
		assert!(first.is_err(), "{status}: a lost reply must fail the job, not be swallowed");
		let rows = submissions(&store, invoice.id).await;
		assert_eq!(rows.len(), 1);
		assert_eq!(rows[0].1, None, "{status}: NAV said nothing, so there is no verdict to record");

		// The runner re-runs the handler from the top; whether that reaches NAV again depends on
		// whether NAV may already hold the filing.
		let hits_before = server.received_requests().await.unwrap().len();
		mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
			.await
			.expect_err("the retry fails, one way or the other");
		assert_eq!(submissions(&store, invoice.id).await.len(), 1, "one row per (invoice, op)");
		let hits_after = server.received_requests().await.unwrap().len();
		if indeterminate {
			assert_eq!(hits_after, hits_before, "{status}: the reconciliation asks, not a resend");
			assert_eq!(enqueued(&store, "NAV_RECONCILE").await, 1);
		} else {
			assert!(hits_after > hits_before, "{status}: the retry reaches NAV again");
		}

		// The sweep covers filings that were never enqueued at all. This one has a row, so the
		// job runner owns it and the sweep must keep its hands off.
		assert!(store.unfiled_invoices(SELLER, 50).await.unwrap().is_empty());
	}
}

/// The guard used to stop only on a `PENDING`/`RUNNING` reconciliation, so a `NAV_RECONCILE`
/// that ended `FAILED` — bad credentials, a store error, a fault NAV parks for a person — let
/// the next backoff step re-POST `manageInvoice` under a `requestId` NAV may already hold. That
/// earns `REQUEST_ID_NOT_UNIQUE` and parks the whole batch as unknown with NAV.
#[tokio::test]
async fn a_failed_reconciliation_still_blocks_the_resend() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 500, "<html>down</html>".to_owned()).await;

	let db = TmpDb::new("report-failed-reconcile");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	// A 500 is indeterminate, so the first attempt hands the batch to `NAV_RECONCILE`.
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("a lost reply fails the job");
	let key = format!("nav:reconcile:{}", invoice.uid.as_str());
	assert_eq!(app.store.job_status_by_key(&key).await.unwrap().as_deref(), Some("PENDING"));

	sqlx::query("UPDATE jobs SET status = 'FAILED', done_at = 1 WHERE dedup_key = ?")
		.bind(&key)
		.execute(store.write_pool())
		.await
		.unwrap();

	let hits_before = server.received_requests().await.unwrap().len();
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("the resend must stand down while the batch's fate is unknown");
	assert_eq!(err.parts().1, "E-CORE-UNAVAILABLE", "{err:?}");
	assert_eq!(server.received_requests().await.unwrap().len(), hits_before, "it resent anyway");
	assert_eq!(
		app.store.job_status_by_key(&key).await.unwrap().as_deref(),
		Some("PENDING"),
		"the dead reconciliation must be revived, not just waited on"
	);
}

/// A 4xx whose body carries no `funcCode` — a WAF page, a CDN block, a misrouted path —
/// is the edge rejecting the request before the invoice service ever saw it. It used to be
/// read as "NAV may hold the invoice" and parked `UNKNOWN`, which `may_send` never resent and
/// `unfiled_invoices` excluded: a statutory filing stranded with no automated way out.
#[tokio::test]
async fn an_unreadable_4xx_stays_retryable_rather_than_parking_the_filing() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 403, "<html><body>blocked by WAF</body></html>".to_owned())
		.await;

	let db = TmpDb::new("report-403");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("the job fails so the runner retries it");
	assert_eq!(err.parts().1, "E-NAV-BUSINESS", "{err:?}");
	// A `manageInvoice` fault means nothing was filed, so it must back off rather than
	// terminate the filing. `Nav::cancel_filing` is what ends one that can never succeed.
	assert_eq!(err.retry(), mintworks_core::Retry::Backoff, "{err:?}");

	let rows = submissions(&store, invoice.id).await;
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].1, None, "the edge rejected it; NAV never gave a verdict");
}

/// `tokenExchange` answering a 200 whose body is not a NAV reply is an outage, not a
/// credentials failure. It fails before `create_submission`, so the invoice keeps no filing
/// record at all — and the sweep still sees it as unfiled, which is the recovery path if the
/// `NAV_REPORT` job itself was never enqueued.
#[tokio::test]
async fn a_filing_that_fails_before_it_is_built_leaves_no_row() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, "<html><body>maintenance</body></html>".to_owned()).await;

	let db = TmpDb::new("report-preflight");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;
	// A buyer `xml::invoice_data` refuses: the other pre-flight failure, before the network.
	let broken = issue_unfilable_at(&store, FEB15).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	for _ in 0..3 {
		for id in [broken.id, invoice.id] {
			assert!(
				mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), id)
					.await
					.is_err()
			);
		}
	}
	assert!(submissions(&store, invoice.id).await.is_empty(), "nothing was archived");
	assert!(submissions(&store, broken.id).await.is_empty());
	// However many times it failed, neither invoice is retired from the sweep: the give-up
	// bound that did that (`nav.max_filing_attempts`, counted as rows) is gone.
	assert_eq!(store.unfiled_invoices(SELLER, 50).await.unwrap(), vec![invoice.id, broken.id]);
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

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let _ = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id).await;

	let archived: String = sqlx::query_scalar(
		"SELECT x.request_xml FROM nav_submission_xml x
			   JOIN nav_submissions s ON s.id = x.submission_id WHERE s.invoice_id = ?",
	)
	.bind(invoice.id)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
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

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("no PDF, no filing");
	assert_eq!(err.parts().1, "E-CORE-UNAVAILABLE", "the runner has to retry it: {err:?}");
	assert!(server.received_requests().await.unwrap().is_empty(), "nothing may reach NAV");
	assert!(
		store.submission_by_invoice(invoice.id).await.unwrap().is_none(),
		"and no filing record is burned while it waits"
	);

	// Rendered, and the same invoice files.
	put_pdf(&store, &invoice).await;
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();
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

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();

	let archived: String = sqlx::query_scalar(
		"SELECT x.request_xml FROM nav_submission_xml x
			   JOIN nav_submissions s ON s.id = x.submission_id WHERE s.invoice_id = ?",
	)
	.bind(invoice.id)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
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

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();

	let archived: String = sqlx::query_scalar(
		"SELECT x.request_xml FROM nav_submission_xml x
			   JOIN nav_submissions s ON s.id = x.submission_id WHERE s.invoice_id = ?",
	)
	.bind(invoice.id)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert!(!archived.contains("electronicInvoiceHash"), "{archived}");
	assert_eq!(submissions(&store, invoice.id).await.len(), 1, "and it still files");
}

/// An invoice already with NAV, or already settled, is never sent a second time. A row that
/// carries a `transactionId` re-enqueues its poll instead — now that the sweep re-drives only
/// never-enqueued filings, that is the only thing that recovers a stranded poll.
#[tokio::test]
async fn an_invoice_already_with_nav_is_not_reported_again() {
	let db = TmpDb::new("report-guard");
	let (app, store) = setup(&db).await;
	// Deliberately no NAV server and no secrets: reaching the network at all is the failure
	// this asserts against.
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	let id = store
		.create_submission(invoice.id, NavOp::Create, "<ManageInvoiceRequest/>")
		.await
		.unwrap()
		.unwrap();
	store.set_sent(id, "TX1", 1).await.unwrap();

	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();
	assert_eq!(submissions(&store, invoice.id).await.len(), 1);
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "the stranded poll is re-enqueued");

	store.finish(id, Some(NavVerdict::Done), None, Timestamp::now()).await.unwrap();
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();
	assert_eq!(submissions(&store, invoice.id).await.len(), 1);
}

/// `may_send` reads through the reader pool while `create_submission` writes through the
/// writer, so the check alone cannot serialize two runners. `idx_nav_submission_live` is the
/// same rule stated where it holds transactionally — and it is now a plain
/// `UNIQUE (invoice_id, op)`: exactly one filing record per operation, for the life of the
/// invoice, whatever happens in between.
#[tokio::test]
async fn one_row_per_invoice_and_op_whatever_the_outcome() {
	let db = TmpDb::new("submission-unique");
	let (_app, store) = setup(&db).await;
	let invoice = issue_at(&store, FEB10).await;

	let id = store
		.create_submission(invoice.id, NavOp::Create, "<ManageInvoiceRequest/>")
		.await
		.unwrap()
		.unwrap();
	assert!(
		store
			.create_submission(invoice.id, NavOp::Create, "<ManageInvoiceRequest/>")
			.await
			.unwrap()
			.is_none(),
		"this (invoice, op) already has its filing record; `report` reuses it"
	);
	// A different operation on the same invoice is a different filing and stays allowed.
	assert!(
		store
			.create_submission(invoice.id, NavOp::Storno, "<x/>")
			.await
			.unwrap()
			.is_some()
	);

	store
		.finish(id, Some(NavVerdict::Rejected), Some(("E", "nav said no")), Timestamp::now())
		.await
		.unwrap();
	assert!(
		store
			.create_submission(invoice.id, NavOp::Create, "<ManageInvoiceRequest/>")
			.await
			.unwrap()
			.is_none(),
		"a verdict does not release the record either — the archive is append-only"
	);
	assert_eq!(submissions(&store, invoice.id).await.len(), 2, "CREATE and STORNO, no more");

	// And the invariant every caller of `submission_by_invoice` rests on: it is
	// `ORDER BY id DESC LIMIT 1`, so with two operations on one invoice it answers the newest
	// and `may_send`, `job::report`'s storno precondition and `cancel_filing` all read that as
	// *the* record. Safe only because `report` derives `op` from `invoice.kind` and an invoice
	// is either Normal or Storno — a third operation needs those three widened first.
	assert_eq!(
		store.submission_by_invoice(invoice.id).await.unwrap().map(|s| s.op),
		Some(NavOp::Storno),
		"the newest row wins, and it is the only one anything reads"
	);
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
		 <validationErrorCode>INVOICE_NUMBER_NOT_UNIQUE</validationErrorCode>\
		 <message>duplicate invoice number</message>\
		 </technicalValidationMessages>\
		 </processingResult></processingResults>\
		 </QueryTransactionStatusResponse>"
	)
}

/// `invoiceStatus = ABORTED` is NAV's verdict on the invoice, not a transport fault. Storing
/// it as `ERROR` made every retry path read it as "nothing was filed", which left exactly two
/// behaviours, both wrong: the invoice was never re-filed while the hourly sweep warned about
/// it forever, or — once the eight fault retries released the dedup key — the sweep resubmitted
/// a permanently rejected invoice to the tax authority every hour, uncapped.
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

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();
	let rows = submissions(&store, invoice.id).await;
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].1, None, "sent, but NAV has not answered about the invoice yet");

	mintworks_nav::job::poll(
		&app,
		invoices.as_ref(),
		nav.as_ref(),
		&poll_job(rows[0].0),
		rows[0].0,
	)
	.await
	.unwrap();
	assert_eq!(
		submissions(&store, invoice.id).await[0].1.as_deref(),
		Some("REJECTED"),
		"NAV's verdict is not a technical fault"
	);

	// `may_send` refuses it: identical data can only earn the same verdict.
	let hits = server.received_requests().await.unwrap().len();
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();
	assert_eq!(submissions(&store, invoice.id).await.len(), 1, "no second filing");
	assert_eq!(server.received_requests().await.unwrap().len(), hits, "NAV was contacted again");

	// It is the operator's problem now, and the hourly sweep is what keeps saying so.
	assert!(store.unfiled_invoices(SELLER, 50).await.unwrap().is_empty());
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1);
}

/// `sellers.nav_base_url` is `NOT NULL`, sits directly beside `nav_login`, is written by
/// `upsert_seller` — and had no reader anywhere. An operator who configured the seller row for
/// production kept filing into `nav.base_url`'s default, NAV's *test* system, which answers OK
/// and reports nothing statutory.
#[tokio::test]
async fn the_sellers_own_base_url_wins_over_the_setting() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;

	let db = TmpDb::new("seller-base-url");
	let (app, store) = setup(&db).await;
	// The setting points somewhere else entirely; the seller row points at the mock.
	point_at_nav(&app, "https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3").await;
	store
		.put_seller(&Seller { nav_base_url: server.uri(), ..stored_seller(&store).await })
		.await
		.unwrap();

	let seller = store.seller_by_id(SELLER).await.unwrap().unwrap();
	let current = store.current_seller_version(SELLER).await.unwrap().unwrap();
	mintworks_nav::auth::NavAuth::load(&app, &seller, &current)
		.await
		.unwrap()
		.token_exchange()
		.await
		.expect("the request has to land on the seller's URL, not the setting's");
	assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// The sweep's one remaining job: the at-most-once gap at `mintworks_invoice::issue`'s
/// `enqueue_jobs`, a crash between COMMIT and enqueue. An invoice that already has a filing
/// record belongs to its `NAV_REPORT` job row, which retries unbounded on its own backoff —
/// re-driving it from here would file it twice.
///
/// It needs no NAV credentials any more: the `tokenExchange` probe and the `nav.sweep_batch`
/// cap were containment for a stampede that keeping `dedup_key` across termination removes.
#[tokio::test]
async fn the_sweep_enqueues_only_filings_that_were_never_made() {
	let db = TmpDb::new("sweep");
	let (app, store) = setup(&db).await;
	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	let mut issued = Vec::new();
	for i in 0..4 {
		issued.push(issue_at(&store, FEB10 + i * 86_400).await);
	}
	// One is already with NAV and one was rejected: neither is the sweep's business.
	store
		.set_sent(
			store
				.create_submission(issued[0].id, NavOp::Create, "<x/>")
				.await
				.unwrap()
				.unwrap(),
			"TX1",
			1,
		)
		.await
		.unwrap();
	fail_submission(&store, issued[1].id).await;

	assert_eq!(
		store.unfiled_invoices(SELLER, 50).await.unwrap(),
		vec![issued[2].id, issued[3].id],
		"only invoices with no filing record at all, oldest first"
	);

	mintworks_nav::job::sweep(&app, invoices.as_ref(), nav.as_ref()).await.unwrap();
	assert_eq!(enqueued(&store, "NAV_REPORT").await, 2);

	// `nav:invoice:{id}` is the dedup key, and it is never released, so a second tick adds
	// nothing however long the filings stay unmade.
	mintworks_nav::job::sweep(&app, invoices.as_ref(), nav.as_ref()).await.unwrap();
	assert_eq!(enqueued(&store, "NAV_REPORT").await, 2, "the hourly sweep must not stampede");
}

/// `report` opens its `nav_submissions` row only after a full `tokenExchange`, so during any
/// NAV or token outage every issued invoice matches `UNFILED` while its live `nav:invoice:{id}`
/// key makes the re-enqueue a no-op — and the one alarm that means "a person must act" fired
/// hourly, exactly when the operator was watching.
#[tokio::test]
async fn a_filing_still_in_flight_is_not_a_re_drive_alarm() {
	let db = TmpDb::new("sweep-in-flight");
	let (app, store) = setup(&db).await;
	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let invoice = issue_at(&store, FEB10).await;
	let key = format!("nav:invoice:{}", invoice.id);

	// The sweep's own enqueue takes the key; nothing has run the handler, so it is PENDING.
	mintworks_nav::job::sweep(&app, invoices.as_ref(), nav.as_ref()).await.unwrap();
	let status = app.store.job_status_by_key(&key).await.unwrap();
	assert_eq!(status.as_deref(), Some("PENDING"));
	assert!(!mintworks_nav::job::needs_operator(status.as_deref()), "a queued filing needs nobody");

	// The same invoice, same spent key, on a job that died terminally: that one does.
	sqlx::query("UPDATE jobs SET status = 'FAILED' WHERE dedup_key = ?")
		.bind(&key)
		.execute(store.write_pool())
		.await
		.unwrap();
	let status = app.store.job_status_by_key(&key).await.unwrap();
	assert!(mintworks_nav::job::needs_operator(status.as_deref()), "a spent key needs a re-drive");
}

/// With retry unbounded, `REJECTED` and `FAILED` are the two verdicts no retry can fix, and
/// this count is what `service_api::alerts` raises `A-NAV-REJECTED` from. Nothing else may be
/// counted: a row still in flight, or one NAV accepted, is the runner's business.
#[tokio::test]
async fn awaiting_operator_counts_only_navs_rejections() {
	let db = TmpDb::new("awaiting-operator");
	let (_app, store) = setup(&db).await;

	let mut rejected = 0;
	for (n, verdict) in [
		None,
		Some(NavVerdict::Done),
		Some(NavVerdict::Warn),
		Some(NavVerdict::Rejected),
		Some(NavVerdict::Failed),
	]
	.into_iter()
	.enumerate()
	{
		let invoice = issue_at(&store, 1_770_000_000 + i64::try_from(n).unwrap() * 100).await;
		let id = store
			.create_submission(invoice.id, NavOp::Create, "<InvoiceData/>")
			.await
			.unwrap()
			.unwrap();
		match verdict {
			None => {}
			Some(v @ (NavVerdict::Rejected | NavVerdict::Failed)) => {
				rejected += 1;
				store.finish(id, Some(v), Some(("X", "x")), Timestamp::now()).await.unwrap();
			}
			Some(v) => {
				store.finish(id, Some(v), None, Timestamp::now()).await.unwrap();
			}
		}
	}

	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), rejected);
	assert!(
		store.unfiled_invoices(SELLER, 50).await.unwrap().is_empty(),
		"every one of them has a filing record, so none of them is the sweep's business"
	);
}

/// `service_api::alerts` is what turns that count into the operator-facing `A-NAV-REJECTED`,
/// and nothing exercised it: its code, severity and link were free to drift from the
/// contract. The link's `state=open` is the one filter that spans all three classes
/// `awaiting_operator` counts — a `verdict=` list cannot express the
/// verdict-NULL-with-an-error-code one.
#[tokio::test]
async fn a_rejection_awaiting_an_operator_raises_the_nav_alert() {
	let db = TmpDb::new("nav-alert");
	let (app, store) = setup(&db).await;

	// Nothing is wrong yet, so there is nothing to say.
	assert!(mintworks_nav::alerts(app.clone()).await.unwrap().is_empty());

	let invoice = issue_at(&store, FEB10).await;
	let id = store
		.create_submission(invoice.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	store
		.finish(id, Some(NavVerdict::Rejected), Some(("ERROR", "bad")), Timestamp::now())
		.await
		.unwrap();

	let alerts = mintworks_nav::alerts(app).await.unwrap();
	assert_eq!(alerts.len(), 1);
	assert_eq!(alerts[0].code, "A-NAV-REJECTED");
	assert_eq!(alerts[0].severity, mintworks_core::alert::Severity::Error);
	assert_eq!(alerts[0].count, 1);
	assert_eq!(alerts[0].link.as_deref(), Some("/api/admin/nav-submissions?state=open"));
	// The operator must not be told to storno an invoice whose fate is unknown.
	assert!(alerts[0].message.contains("do not storno"), "{}", alerts[0].message);
}

/// `awaiting_operator` counted a rejection forever and nothing could clear it, so the hourly
/// `ERROR` and `A-NAV-REJECTED` became wallpaper. Resolving is a note beside the verdict, not
/// an edit to it: the statutory archive of what NAV said must survive it intact.
#[tokio::test]
async fn resolve_drops_the_invoice_out_of_awaiting_operator() {
	let db = TmpDb::new("resolve-drops");
	let (_app, store) = setup(&db).await;
	let invoice = issue_at(&store, FEB10).await;
	let id = store
		.create_submission(invoice.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	store
		.finish(
			id,
			Some(NavVerdict::Rejected),
			Some(("INVOICE_NUMBER_NOT_UNIQUE", "already held")),
			Timestamp::now(),
		)
		.await
		.unwrap();
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1);

	assert!(store.resolve(id, Timestamp::now()).await.unwrap());
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 0);

	let row = store.submission_by_invoice(invoice.id).await.unwrap().unwrap();
	assert_eq!(row.verdict, Some(NavVerdict::Rejected));
	assert_eq!(row.error_code.as_deref(), Some("INVOICE_NUMBER_NOT_UNIQUE"));
	assert!(row.resolved_at.is_some());
}

/// Only a row that actually counted can be resolved, and only once — otherwise `resolve`
/// would be a way to stamp `resolved_at` onto a filing NAV accepted.
#[tokio::test]
async fn resolve_is_refused_twice_and_on_a_healthy_row() {
	let db = TmpDb::new("resolve-refused");
	let (_app, store) = setup(&db).await;

	let rejected = issue_at(&store, FEB10).await;
	let id = store
		.create_submission(rejected.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	store
		.finish(id, Some(NavVerdict::Rejected), Some(("X", "x")), Timestamp::now())
		.await
		.unwrap();
	assert!(store.resolve(id, Timestamp::now()).await.unwrap());
	assert!(!store.resolve(id, Timestamp::now()).await.unwrap(), "already resolved");

	let done = issue_at(&store, FEB15).await;
	let done_id = store
		.create_submission(done.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	store
		.finish(done_id, Some(NavVerdict::Done), None, Timestamp::now())
		.await
		.unwrap();
	assert!(!store.resolve(done_id, Timestamp::now()).await.unwrap(), "NAV accepted it");

	let pristine = issue_at(&store, FEB28).await;
	let pristine_id = store
		.create_submission(pristine.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	assert!(!store.resolve(pristine_id, Timestamp::now()).await.unwrap(), "still in flight");
}

/// Resolving settles the alarm, never the filing. If it re-opened the invoice the sweep would
/// file a number NAV already holds — which is how the rejected rows arose in the first place.
#[tokio::test]
async fn a_resolved_invoice_is_still_not_unfiled() {
	let db = TmpDb::new("resolve-not-unfiled");
	let (_app, store) = setup(&db).await;
	let invoice = issue_at(&store, FEB10).await;
	let id = store
		.create_submission(invoice.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	store
		.finish(id, Some(NavVerdict::Rejected), Some(("X", "x")), Timestamp::now())
		.await
		.unwrap();
	assert!(store.resolve(id, Timestamp::now()).await.unwrap());

	assert!(
		store.unfiled_invoices(SELLER, 50).await.unwrap().is_empty(),
		"resolving must not offer the invoice to the sweep again"
	);
}

/// The open `REQUEST_ID_NOT_UNIQUE` shape — a fault recorded, no verdict — is counted by
/// `awaiting_operator`, so it must be resolvable too or that state alarms forever.
#[tokio::test]
async fn resolve_counts_an_open_faulted_row() {
	let db = TmpDb::new("resolve-faulted");
	let (_app, store) = setup(&db).await;
	let invoice = issue_at(&store, FEB10).await;
	let id = store
		.create_submission(invoice.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	store
		.record_fault(id, "REQUEST_ID_NOT_UNIQUE", "NAV may hold it")
		.await
		.unwrap();
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1);

	assert!(store.resolve(id, Timestamp::now()).await.unwrap());
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 0);
	let row = store.submission_by_invoice(invoice.id).await.unwrap().unwrap();
	assert!(row.verdict.is_none(), "resolving records a judgement, it does not invent one");
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

/// Nothing checked that the invoice a STORNO cancels had itself reached NAV. Both
/// `NAV_REPORT` jobs are enqueued with the same `run_at` and `settings['jobs.workers']` is 2,
/// so issuing and cancelling inside one runner tick could send the storno first — NAV answers
/// `ABORTED`, `poll` finishes the row `REJECTED`, and `may_send` then refuses to ever resend
/// it, parking a statutory filing on manual reconciliation.
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

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let report = || mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), storno.id);

	// The original has no submission at all yet.
	let err = report().await.expect_err("the storno must not overtake its original");
	assert_eq!(err.parts().1, "E-CORE-UNAVAILABLE", "the runner has to retry it: {err:?}");
	assert!(
		submissions(&store, storno.id).await.is_empty(),
		"and hold no submission while it waits"
	);
	assert!(server.received_requests().await.unwrap().is_empty(), "nothing may reach NAV");

	// In flight is not filed either.
	let sent = store
		.create_submission(original.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	assert_eq!(report().await.unwrap_err().parts().1, "E-CORE-UNAVAILABLE");

	// Filed: the storno goes, and gets as far as the (mocked, unavailable) NAV.
	store
		.finish(sent, Some(NavVerdict::Done), None, Timestamp::now())
		.await
		.unwrap();
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

/// `Nav::invoice` scoped only `Actor::User`; the catch-all arm lumped `Public` in with
/// `Operator` and `System`, so a `Ctx` carrying no org — which is every `Ctx::public` —
/// got the unscoped read and could pull any org's invoice by uid. Not reachable today
/// (`mintworks-nav` ships no route bundle), so this is defence in depth: `lookup_tax_number`
/// refuses `Public` outright and the scoped read now needs an org the same way.
#[tokio::test]
async fn a_public_ctx_has_no_unscoped_invoice_read() {
	let db = TmpDb::new("public-scope");
	let (app, store) = setup(&db).await;
	let invoice = issue_at(&store, JAN15).await;

	// `submit`'s step-up gate refuses a `Public` caller outright — it has no `auth_at` and no
	// way to acquire one — so in practice the read is never reached at all.
	let nav = Nav::new(app.clone());
	let err = nav.submit(&Ctx::public("test"), invoice.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-STEPUP-IMPOSSIBLE");

	// Past it — a state a real `Public` cannot reach, constructed here so the scoped read is
	// actually exercised — `E-AUTH-FORBIDDEN` is `Ctx::org`'s "no org selected", not a
	// disclosure: it says nothing about whether that uid exists.
	let mut public = Ctx::public("test");
	public.auth_at = Some(Timestamp::now().0);
	let err = nav
		.submit(&public, invoice.uid.as_str())
		.await
		.expect_err("a caller with no org has no unscoped read");
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");
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

	let id = store
		.create_submission(invoice.id, NavOp::Create, "<ManageInvoiceRequest/>")
		.await
		.unwrap()
		.unwrap();
	let found = nav
		.filing(&Ctx::system("test").with_org(ORG), invoice.uid.as_str())
		.await
		.unwrap()
		.expect("the owning org reads its filing");
	assert_eq!(found.id, id);

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

/// `submit` filed whatever uid it was handed. A `DRAFT` has no number, so the job failed
/// terminally in `xml::invoice_data` before a submission row was ever opened — having spent
/// `nav:invoice:{id}`, the key `mintworks_invoice::issue::enqueue_jobs` mints when the invoice is
/// genuinely issued. That enqueue then returned `Ok(None)` and logged nothing, and the invoice
/// was never filed until the reconciliation sweep noticed, hours later.
#[tokio::test]
async fn submitting_a_draft_is_refused_and_spends_no_dedup_key() {
	let db = TmpDb::new("submit-draft");
	let (app, store) = setup(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let draft = store.create_draft(&new_invoice(InvoiceKind::Normal, None)).await.unwrap();
	let err = Nav::new(app.clone())
		.submit(&ctx, draft.uid.as_str())
		.await
		.expect_err("a draft has no number to file");
	assert_eq!(err.parts().1, "E-NAV-NOT-ISSUED");
	assert!(report_jobs(&store, draft.id).await.is_empty(), "no job, so no key was spent");

	// And the key is still there for the issue path, which is the whole point.
	assert!(
		mintworks_core::job::enqueue(
			&app.store,
			mintworks_invoice::KIND_NAV_REPORT,
			&format!(r#"{{"invoiceId":{}}}"#, draft.id),
			Some(&format!("nav:invoice:{}", draft.id)),
			Timestamp::now(),
		)
		.await
		.unwrap()
		.is_some(),
		"the issue path must still be able to enqueue this invoice's filing"
	);
}

/// The re-drive used to enqueue a *second* `NAV_REPORT` row under a fresh
/// `nav:redrive:{id}:{ts}` key while the first row still existed, so two workers could claim
/// one each, both pass `may_send` (no `transactionId`, no verdict) and both POST
/// `manageInvoice`. One row, reset in place, makes the `jobs` claim the mutual exclusion.
#[tokio::test]
async fn an_operator_redrive_resets_the_one_job_row_rather_than_adding_a_second() {
	let db = TmpDb::new("submit-redrive");
	let (app, store) = setup(&db).await;
	let mut ctx = Ctx::system("test").with_org(ORG);
	let invoice = issue_at(&store, JAN15).await;
	let nav = Nav::new(app.clone());

	// This harness builds a real `App`, whose job workers are live and will terminate a
	// `NAV_REPORT` row the moment they claim it — no handler is registered here. So every
	// assertion below is on something a worker cannot change: the number of rows (workers
	// insert none), the `dedup_key`, and whether `submit` itself succeeded. The row's status
	// is driven from raw SQL rather than read back.
	let key = format!("nav:invoice:{}", invoice.id);
	let payload = format!(r#"{{"invoiceId":{}}}"#, invoice.id);
	let set_status = async |status: &str| {
		sqlx::query("UPDATE jobs SET status = ? WHERE kind = 'NAV_REPORT' AND payload = ?")
			.bind(status)
			.bind(&payload)
			.execute(store.write_pool())
			.await
			.unwrap();
	};

	// The at-most-once gap: an issued invoice with no job at all. The first submit is the
	// plain enqueue.
	nav.submit(&ctx, invoice.uid.as_str()).await.unwrap();
	let jobs = report_jobs(&store, invoice.id).await;
	assert_eq!(jobs.len(), 1);
	assert_eq!(jobs[0].2.as_deref(), Some(key.as_str()));

	// `FAILED` is the state a re-drive exists for — a spent `requestId`, an exhausted attempt
	// budget, or an operator's `cancel_filing`. Once set it is stable: `claim` takes only
	// `PENDING`, and `job_fail`/`job_terminate` are `RUNNING`-guarded.
	set_status("FAILED").await;

	// A re-drive is an operator's call. `auth_at` is fresh so the step-up gate is satisfied and
	// the authorization verdict is what this asserts — see
	// `filing_a_statutory_return_needs_a_freshly_presented_credential`.
	ctx.actor = mintworks_core::ctx::Actor::User { account_id: 1 };
	ctx.auth_at = Some(Timestamp::now().0);
	let err = nav
		.submit(&ctx, invoice.uid.as_str())
		.await
		.expect_err("a user may not re-drive");
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");
	assert_eq!(
		report_jobs(&store, invoice.id).await.len(),
		1,
		"a refused re-drive must leave no extra row"
	);

	// And it resets the row rather than adding one. Succeeding at all is the proof:
	// `job_redrive` answered 1, or this would be `E-NAV-NOT-REDRIVABLE`.
	ctx.actor = mintworks_core::ctx::Actor::System { source: "test" };
	nav.submit(&ctx, invoice.uid.as_str()).await.unwrap();
	let jobs = report_jobs(&store, invoice.id).await;
	assert_eq!(jobs.len(), 1, "one invoice, one NAV_REPORT row: {jobs:?}");
	assert_eq!(
		jobs[0].2.as_deref(),
		Some(key.as_str()),
		"the dedup key is still spent, so nothing can enqueue a second filing"
	);

	// A row that is not re-drivable says so rather than silently doing nothing. `DONE` is the
	// stable way to show it: unclaimable, so no worker can move it under the assertion.
	set_status("DONE").await;
	let err = nav
		.submit(&ctx, invoice.uid.as_str())
		.await
		.expect_err("there is no failed filing to re-drive");
	assert_eq!(err.parts().1, "E-NAV-NOT-REDRIVABLE");
	assert_eq!(report_jobs(&store, invoice.id).await.len(), 1, "and it added nothing");
}

/// Filing a statutory return is the same class of act as `Invoices::issue`, which opens with
/// `require_stepup` — and `Nav::submit` derived no permission beyond org ownership, so a
/// stolen access token filed with the tax authority for as long as it stayed unexpired.
#[tokio::test]
async fn filing_a_statutory_return_needs_a_freshly_presented_credential() {
	let db = TmpDb::new("submit-stepup");
	let (app, store) = setup(&db).await;
	let invoice = issue_at(&store, JAN15).await;
	let nav = Nav::new(app.clone());
	let mut ctx = Ctx::system("test").with_org(ORG);
	ctx.actor = mintworks_core::ctx::Actor::User { account_id: 1 };

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

	// `Actor::System` is exempt: `issue::enqueue_jobs` and consumer Rust have no credential to
	// re-present, and gating them would break the automatic filing path outright.
	nav.submit(&Ctx::system("test").with_org(ORG), invoice.uid.as_str())
		.await
		.unwrap();
	assert_eq!(report_jobs(&store, invoice.id).await.len(), 1);

	// And a fresh one gets through — the gate is on the credential's age, not on the actor.
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
		// schema-invalid — and `Retry::Never` on the credential fault means nothing is ever filed.
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

/// A *retryable* fault recorded nothing at all: `nav.finish` was reached only on the
/// `Retry::Never` branch, so the row kept `verdict IS NULL` **and** `error_code IS NULL`.
/// `awaiting_operator` missed it and `unfiled_invoices` skips an invoice that has any row, so
/// an invoice NAV would never accept retried forever with the archive saying nothing about why.
#[tokio::test]
async fn a_retryable_fault_is_recorded_without_settling_the_filing() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	// `Disposition::Retry` in `client::FAULTS`, so `client::business` classes it
	// `Retry::Backoff` — the majority case, and the one that recorded nothing.
	mock(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>OPERATION_FAILED</common:errorCode>\
			 <common:message>try again later</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;

	let db = TmpDb::new("report-retryable-fault");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("a fault is not a filing");
	assert_eq!(err.retry(), mintworks_core::Retry::Backoff, "{err:?}");

	let (verdict, code): (Option<String>, Option<String>) =
		sqlx::query_as("SELECT verdict, error_code FROM nav_submissions WHERE invoice_id = ?")
			.bind(invoice.id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert!(verdict.is_none(), "the filing is still open, so the retry is unchanged");
	assert_eq!(code.as_deref(), Some("OPERATION_FAILED"), "the reason has to be on the row");
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1);
}

/// A `Retry::Never` fault terminates the job row and used to leave the `nav_submissions` row
/// open — no verdict, no `transactionId`. That row is then a dead end in every direction:
/// `unfiled_invoices` skips an invoice that has *any* row, `awaiting_operator` counts only
/// settled ones, `A-JOB-FAILED` stops after `jobs.failed_alert_hours` and `job_sweep` deletes
/// the job at `jobs.retention_days`. After 90 days an issued, numbered invoice that was never
/// filed was invisible to every alert, counter and sweep in the system.
///
/// `FAILED`, not `REJECTED`: `FORBIDDEN` means NAV never examined the invoice, and `REJECTED`
/// is the archive's word for `invoiceStatus = ABORTED`.
#[tokio::test]
async fn a_spent_request_id_settles_the_submission_as_failed_not_rejected() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>FORBIDDEN</common:errorCode>\
			 <common:message>invalid user</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;

	let db = TmpDb::new("report-spent-request-id");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("a spent requestId can never be filed again under it");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-SPENT");
	assert_eq!(err.retry(), mintworks_core::Retry::Never, "{err:?}");

	let rows = submissions(&store, invoice.id).await;
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].1.as_deref(), Some("FAILED"), "the filing ended; the row has to say so");
	// `FAILED` is counted by `A-NAV-REJECTED` too, so the invoice is now visible to an operator
	// — and `nav_submissions` rows are never swept, so it stays visible.
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1);
}

/// `finish(FAILED)` was an unconditional `UPDATE … WHERE id = ?`. `report`'s pre-POST re-read
/// cannot see an *in-flight* sibling, so two attempts under the same `requestId` — the first
/// accepted, the second refused `REQUEST_ID_NOT_UNIQUE` — flipped an accepted filing to
/// `FAILED`. `poll` returns early on any verdict, so nothing ever corrected it: an invoice NAV
/// accepted was archived as a failed filing and counted in `A-NAV-REJECTED` forever.
#[tokio::test]
async fn a_burned_request_id_does_not_overwrite_a_landed_verdict() {
	let server = Arc::new(MockServer::builder().start().await);
	mock(&server, "tokenExchange", 200, token_reply()).await;
	// Held open long enough for the sibling to land its verdict while this attempt is still
	// in flight — the one window the guard above the POST is blind to.
	Mock::given(method("POST"))
		.and(path("/manageInvoice"))
		.respond_with(
			ResponseTemplate::new(200)
				.set_delay(std::time::Duration::from_secs(3))
				.set_body_string(format!(
					"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
					 <GeneralErrorResponse{ENVELOPE}>\
					 <common:result><common:funcCode>ERROR</common:funcCode>\
					 <common:errorCode>REQUEST_ID_NOT_UNIQUE</common:errorCode>\
					 <common:message>already used</common:message></common:result>\
					 </GeneralErrorResponse>"
				)),
		)
		.mount(&server)
		.await;

	let db = TmpDb::new("report-landed-verdict");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	// Pre-opened, so `report` takes the retry branch and its id is known here.
	let id = nav.create_submission(invoice.id, NavOp::Create, "<x/>").await.unwrap().unwrap();

	let sibling = {
		let nav: Arc<dyn NavStore> = Arc::new(store.clone());
		let server = Arc::clone(&server);
		tokio::spawn(async move {
			// Ordered on NAV having *received* the POST, not on a sleep: the write has to land
			// after `report`'s pre-POST re-read or the guard above catches it and the test
			// passes without exercising the fix at all.
			while !server
				.received_requests()
				.await
				.unwrap()
				.iter()
				.any(|r| r.url.path() == "/manageInvoice")
			{
				tokio::time::sleep(std::time::Duration::from_millis(5)).await;
			}
			nav.set_sent(id, "tx-1", 1).await.unwrap();
			nav.finish(id, Some(NavVerdict::Done), None, Timestamp::now()).await.unwrap();
		})
	};

	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("this attempt's requestId was refused");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-REUSED");
	sibling.await.unwrap();

	assert_eq!(submissions(&store, invoice.id).await, vec![(id, Some("DONE".to_owned()))]);
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 0);
}

/// The store-level half of the guard above: `finish` is `WHERE verdict IS NULL`, so the first
/// verdict NAV gives stands and a late fault can only be the job row's business.
#[tokio::test]
async fn a_late_fault_cannot_overwrite_an_accepted_verdict() {
	let db = TmpDb::new("finish-guard");
	let (_app, store) = setup(&db).await;
	let invoice = issue_at(&store, FEB10).await;
	let id = store
		.create_submission(invoice.id, NavOp::Create, "<x/>")
		.await
		.unwrap()
		.unwrap();

	assert!(store.finish(id, Some(NavVerdict::Done), None, Timestamp::now()).await.unwrap());
	assert!(
		!store
			.finish(id, Some(NavVerdict::Failed), Some(("X", "late")), Timestamp::now())
			.await
			.unwrap(),
		"a settled row refuses a second verdict"
	);
	assert_eq!(submissions(&store, invoice.id).await, vec![(id, Some("DONE".to_owned()))]);

	// `set_sent` is guarded the same way, and for the same reason: the loser of a double POST
	// must not replace the `transactionId` of the filing NAV actually took.
	let other = store
		.create_submission(invoice.id, NavOp::Storno, "<x/>")
		.await
		.unwrap()
		.unwrap();
	assert!(store.set_sent(other, "tx-1", 1).await.unwrap());
	assert!(!store.set_sent(other, "tx-2", 1).await.unwrap());
	assert_eq!(
		store.submission(other).await.unwrap().unwrap().transaction_id.as_deref(),
		Some("tx-1")
	);
}

/// Once `NAV_POLL` terminates `Retry::Never` nothing restarts it: `nav:poll:{tx}` survives
/// `FAILED`, the leader's `NAV_REPORT` is `DONE` so `job_redrive`'s `AND status = 'FAILED'`
/// never matches, and `Nav::submit` answered `E-NAV-NOT-REDRIVABLE`. Since batching, one dead
/// poll strands a whole `nav.batch_max` of invoices with no verdict.
#[tokio::test]
async fn a_terminated_poll_is_revived_by_an_operator_redrive() {
	// Since batching, this holds for the leader only: a member rides on the leader's poll
	// chain, and `Nav::submit` on a member answers `E-NAV-NOT-REDRIVABLE`.
	let db = TmpDb::new("poll-revive");
	let (app, store) = setup(&db).await;
	let invoice = issue_at(&store, FEB10).await;
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	// With NAV and no verdict, which is the state the filing is in once `manageInvoice` landed.
	let id = nav.create_submission(invoice.id, NavOp::Create, "<x/>").await.unwrap().unwrap();
	assert!(nav.set_sent(id, "TX1", 1).await.unwrap());
	let payload = mintworks_nav::job::poll_payload(id);
	mintworks_core::job::enqueue(
		&app.store,
		mintworks_nav::job::KIND_NAV_POLL,
		&payload,
		Some("nav:poll:TX1"),
		Timestamp::now(),
	)
	.await
	.unwrap()
	.expect("a fresh dedup key");

	let poll_row = async || -> (String, i64) {
		sqlx::query_as("SELECT status, attempts FROM jobs WHERE kind = 'NAV_POLL' AND payload = ?")
			.bind(&payload)
			.fetch_one(store.read_pool())
			.await
			.unwrap()
	};
	assert_eq!(poll_row().await, ("PENDING".to_owned(), 0));

	// Terminated — `NavAuth::load` on a rotated password, or an unparseable reply. And the
	// report job that filed it finished `DONE`, which is the state no re-drive used to reach.
	sqlx::query("UPDATE jobs SET status = 'FAILED', attempts = 4 WHERE kind = 'NAV_POLL'")
		.execute(store.write_pool())
		.await
		.unwrap();
	enqueue_report(&app, invoice.id).await;
	sqlx::query("UPDATE jobs SET status = 'DONE' WHERE kind = 'NAV_REPORT'")
		.execute(store.write_pool())
		.await
		.unwrap();

	let ctx = Ctx::system("test").with_org(ORG);
	Nav::new(app.clone()).submit(&ctx, invoice.uid.as_str()).await.unwrap();
	assert_eq!(poll_row().await, ("PENDING".to_owned(), 0), "the stranded poll is revived");
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "revived, not duplicated");
	assert_eq!(report_jobs(&store, invoice.id).await[0].1, "DONE", "the filing is not resent");

	// And the audit row says which chain moved, so it cannot claim a report re-drive.
	let detail: String = sqlx::query_scalar(
		"SELECT detail FROM audit_logs WHERE entity = 'nav_submission' AND action = 'SUBMIT'",
	)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert!(detail.contains(r#""target":"poll""#), "{detail}");

	// A `DONE` poll is the other half: `job_redrive` matches `FAILED` only.
	sqlx::query("UPDATE jobs SET status = 'DONE' WHERE kind = 'NAV_POLL'")
		.execute(store.write_pool())
		.await
		.unwrap();
	Nav::new(app.clone()).submit(&ctx, invoice.uid.as_str()).await.unwrap();
	assert_eq!(poll_row().await, ("PENDING".to_owned(), 0), "and a DONE poll too");
}

/// A `set_sent` write lost to writer saturation makes the retry resend under the same
/// `requestId`, and NAV answers `REQUEST_ID_NOT_UNIQUE` — the one fault that means *NAV may
/// already hold this filing*. Recording `FAILED` there is an archive claiming a filed invoice
/// was never filed, `may_send` then refuses to touch the row ever again, and `A-NAV-REJECTED`
/// advises the operator to correct and re-issue, which would file it twice. The row stays open
/// with the reason on it instead, and `awaiting_operator` is what surfaces it.
#[tokio::test]
async fn a_reused_request_id_leaves_the_filing_open_rather_than_failed() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>REQUEST_ID_NOT_UNIQUE</common:errorCode>\
			 <common:message>already used</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;

	let db = TmpDb::new("report-reused-request-id");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("this requestId was already processed; resending can only be refused");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-REUSED");
	assert_eq!(err.retry(), mintworks_core::Retry::Never, "{err:?}");

	let (verdict, code): (Option<String>, Option<String>) =
		sqlx::query_as("SELECT verdict, error_code FROM nav_submissions WHERE invoice_id = ?")
			.bind(invoice.id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(verdict, None, "NAV may hold this filing; it must not be archived as failed");
	assert_eq!(code.as_deref(), Some("REQUEST_ID_NOT_UNIQUE"), "the reason has to be on the row");
	// `unfiled_invoices` skips an invoice that has any row, so without this the open row would
	// be invisible to every counter and sweep in the system.
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1);
	assert!(store.unfiled_invoices(SELLER, 50).await.unwrap().is_empty());
}

// Rust has no trait delegation, so the 23 pass-throughs below are hand-written.

/// Which write this double fails; everything else delegates.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
	/// The archive is a diagnostic; the reply in hand is the truth, so nothing on either job
	/// path may be decided by it.
	Archive,
	/// The one write that stamps a whole batch with NAV's `transactionId`.
	SetSentBatch,
	/// `record_fault` for this one submission id; the rest of the batch writes normally.
	RecordFaultFor(i64),
}

struct Faulty(Arc<dyn NavStore>, Fail);

#[async_trait::async_trait]
impl NavStore for Faulty {
	async fn archive_response(&self, id: i64, response_xml: &str) -> Result<(), Error> {
		if self.1 == Fail::Archive {
			return Err(Error::internal("the archive write failed"));
		}
		self.0.archive_response(id, response_xml).await
	}
	async fn create_submission(
		&self,
		invoice_id: i64,
		op: NavOp,
		request_xml: &str,
	) -> Result<Option<i64>, Error> {
		self.0.create_submission(invoice_id, op, request_xml).await
	}
	async fn archive_request(&self, id: i64, request_xml: &str) -> Result<(), Error> {
		self.0.archive_request(id, request_xml).await
	}
	async fn request_archived(&self, id: i64) -> Result<bool, Error> {
		self.0.request_archived(id).await
	}
	async fn submission_archive(
		&self,
		id: i64,
	) -> Result<Option<mintworks_nav::submission::NavArchive>, Error> {
		self.0.submission_archive(id).await
	}
	async fn submission(&self, id: i64) -> Result<Option<mintworks_nav::NavSubmission>, Error> {
		self.0.submission(id).await
	}
	async fn batch_candidates(
		&self,
		seller_id: i64,
		exclude_invoice_id: i64,
		require_document: bool,
		limit: i64,
	) -> Result<Vec<i64>, Error> {
		self.0
			.batch_candidates(seller_id, exclude_invoice_id, require_document, limit)
			.await
	}
	async fn claim_batch(
		&self,
		leader_invoice_id: i64,
		op: NavOp,
		batch_uid: &str,
		ids: &[i64],
	) -> Result<Vec<(i64, i64)>, Error> {
		self.0.claim_batch(leader_invoice_id, op, batch_uid, ids).await
	}
	async fn submissions_by_batch(
		&self,
		batch_uid: &str,
	) -> Result<Vec<mintworks_nav::NavSubmission>, Error> {
		self.0.submissions_by_batch(batch_uid).await
	}
	async fn submissions_by_transaction(
		&self,
		transaction_id: &str,
	) -> Result<Vec<mintworks_nav::NavSubmission>, Error> {
		self.0.submissions_by_transaction(transaction_id).await
	}
	async fn release_batch(
		&self,
		batch_uid: &str,
		leader_submission_id: i64,
	) -> Result<Vec<i64>, Error> {
		self.0.release_batch(batch_uid, leader_submission_id).await
	}
	async fn release_member(&self, batch_uid: &str, invoice_id: i64) -> Result<bool, Error> {
		self.0.release_member(batch_uid, invoice_id).await
	}
	async fn submission_by_invoice(
		&self,
		invoice_id: i64,
	) -> Result<Option<mintworks_nav::NavSubmission>, Error> {
		self.0.submission_by_invoice(invoice_id).await
	}
	async fn set_sent(&self, id: i64, transaction_id: &str, idx: i64) -> Result<bool, Error> {
		assert!(self.1 != Fail::SetSentBatch, "a batch is stamped in one write, not row by row");
		self.0.set_sent(id, transaction_id, idx).await
	}
	async fn set_sent_batch(
		&self,
		rows: &[(i64, i64)],
		transaction_id: &str,
	) -> Result<Vec<i64>, Error> {
		if self.1 == Fail::SetSentBatch {
			return Err(Error::Unavailable("the writer is down".to_owned()));
		}
		self.0.set_sent_batch(rows, transaction_id).await
	}
	async fn record_fault(&self, id: i64, code: &str, message: &str) -> Result<(), Error> {
		if self.1 == Fail::RecordFaultFor(id) {
			return Err(Error::internal("the fault write failed"));
		}
		self.0.record_fault(id, code, message).await
	}
	async fn resolve(&self, id: i64, at: Timestamp) -> Result<bool, Error> {
		self.0.resolve(id, at).await
	}
	async fn finish(
		&self,
		id: i64,
		verdict: Option<NavVerdict>,
		error: Option<(&str, &str)>,
		done_at: Timestamp,
	) -> Result<bool, Error> {
		self.0.finish(id, verdict, error, done_at).await
	}
	async fn unfiled_invoices(&self, seller_id: i64, limit: i64) -> Result<Vec<i64>, Error> {
		self.0.unfiled_invoices(seller_id, limit).await
	}
	async fn awaiting_operator(&self, seller_id: i64) -> Result<i64, Error> {
		self.0.awaiting_operator(seller_id).await
	}
	async fn export_ids_by_date(
		&self,
		seller_id: i64,
		from: &str,
		to: &str,
	) -> Result<Vec<i64>, Error> {
		self.0.export_ids_by_date(seller_id, from, to).await
	}
	async fn export_ids_by_number(
		&self,
		seller_id: i64,
		from: &str,
		to: &str,
	) -> Result<Vec<i64>, Error> {
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

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(Faulty(Arc::new(store.clone()), Fail::Archive));
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect("a failed archive must not fail a filing NAV accepted");

	let row = store.submission_by_invoice(invoice.id).await.unwrap().unwrap();
	assert_eq!(row.transaction_id.as_deref(), Some("TX-VERDICT"));
	let archive = store.submission_archive(row.id).await.unwrap();
	assert!(
		archive.is_none_or(|a| a.response_xml.is_none()),
		"the archive did fail; that is the point"
	);
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "and the poll was still queued");
}

/// The same on the poll path, where `archive_response` was a `?` before the reply was even
/// read: a transient failure discarded a verdict NAV had already given and re-queried it, and
/// one mapping to `Error::internal` — `Retry::Never` — terminated the poll on a filing whose
/// `NAV_REPORT` row is already `DONE`, so nothing could ever re-enqueue it.
#[tokio::test]
async fn a_verdict_is_settled_even_when_the_archive_fails() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "queryTransactionStatus", 200, done_reply()).await;

	let db = TmpDb::new("poll-archive-fails");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(Faulty(Arc::new(store.clone()), Fail::Archive));
	let id = store
		.create_submission(invoice.id, NavOp::Create, "<x/>")
		.await
		.unwrap()
		.unwrap();
	assert!(store.set_sent(id, "TX-VERDICT", 1).await.unwrap());

	mintworks_nav::job::poll(&app, invoices.as_ref(), nav.as_ref(), &poll_job(id), id)
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

/// The newest `nav_submissions` row of an invoice: `(id, transaction_id, idx, batch_uid,
/// verdict)`. [`submissions`] answers verdicts only, and every batching assertion is about the
/// other three columns.
async fn row_of(
	store: &SqliteStore,
	invoice_id: i64,
) -> (i64, Option<String>, Option<i64>, Option<String>, Option<String>) {
	sqlx::query_as(
		"SELECT id, transaction_id, idx, batch_uid, verdict FROM nav_submissions \
		 WHERE invoice_id = ? ORDER BY id DESC LIMIT 1",
	)
	.bind(invoice_id)
	.fetch_one(store.read_pool())
	.await
	.unwrap()
}

/// The bodies the mock saw for one operation, oldest first. Counting them is how a test asserts
/// that a second POST never happened — a member must never file itself.
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
fn poll_job(submission_id: i64) -> mintworks_core::job::Job {
	mintworks_core::job::Job {
		id: submission_id,
		kind: mintworks_nav::job::KIND_NAV_POLL.to_owned(),
		payload: mintworks_nav::job::poll_payload(submission_id),
		attempts: 1,
	}
}

/// The live `PENDING` `NAV_REPORT` row every batch candidate needs: an invoice whose
/// filing an operator stopped must not be swept back in by the next leader. `issue::enqueue_jobs`
/// mints it in production; these tests drive the handler directly, so nothing else does.
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

/// Run a claimed batch member's own `NAV_REPORT` and record what the runner would: it stands
/// down inside `may_send`, returns `Ok`, and the job reaches `DONE` with `nav:invoice:{id}`
/// spent for good. Nothing else in these tests drives the runner, so the `DONE` is written here.
async fn stand_down(
	app: &App,
	store: &SqliteStore,
	invoices: &Arc<dyn InvoiceStore>,
	nav: &Arc<dyn NavStore>,
	invoice_id: i64,
) {
	mintworks_nav::job::report(app, invoices.as_ref(), nav.as_ref(), invoice_id)
		.await
		.expect("a member never files itself");
	sqlx::query("UPDATE jobs SET status = 'DONE' WHERE kind = 'NAV_REPORT' AND dedup_key = ?")
		.bind(format!("nav:invoice:{invoice_id}"))
		.execute(store.write_pool())
		.await
		.unwrap();
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
	<validationErrorCode>INVOICE_NUMBER_NOT_UNIQUE</validationErrorCode>\
	<message>duplicate invoice number</message>\
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
/// 100 invoices. Three invoices therefore cost one `tokenExchange` and one `manageInvoice`, not
/// three of each — and each row records the `<index>` its invoiceData rode under, because that
/// is what the poll matches NAV's per-result `index` against.
#[tokio::test]
async fn one_token_and_one_request_carry_the_whole_batch() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-one-token");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let second = issue_at(&store, FEB10).await;
	let third = issue_at(&store, FEB10).await;
	for id in [leader.id, second.id, third.id] {
		enqueue_report(&app, id).await;
	}

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();

	assert_eq!(requests(&server, "tokenExchange").await.len(), 1, "one token per request");
	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(sent.len(), 1, "one request for the batch");
	assert_eq!(operations(&sent[0]), 3, "{}", sent[0]);
	for idx in 1..=3 {
		assert!(sent[0].contains(&format!("<invoiceOperation><index>{idx}</index>")), "{idx}");
	}

	let mut seen = Vec::new();
	for invoice in [&leader, &second, &third] {
		let (_, tx, idx, batch, _) = row_of(&store, invoice.id).await;
		assert_eq!(tx.as_deref(), Some("TX-VERDICT"), "invoice {}", invoice.id);
		assert_eq!(batch.as_deref(), Some(leader.uid.as_str()), "invoice {}", invoice.id);
		seen.push(idx.expect("set_sent writes the index"));
	}
	seen.sort_unstable();
	assert_eq!(seen, vec![1, 2, 3], "gapless, 1-based, one per invoice");
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "one poll chain for the transaction");
}

/// The double-filing regression. Between the member's row
/// being claimed and `set_sent` landing — a whole token exchange, the POST, and every retry
/// backoff — the member's own job sees no verdict and no `transactionId`. Before `batch_uid`,
/// `may_send` said yes and the member POSTed under its *own* `requestId`: two request ids, both
/// accepted, one invoice filed twice.
#[tokio::test]
async fn a_batch_member_never_files_itself() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	// The leader's POST dies, so the rows stay claimed with no `transactionId` — exactly the
	// window the member must not file in.
	mock(&server, "manageInvoice", 500, "<html>down</html>".to_owned()).await;

	let db = TmpDb::new("batch-member-inert");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("the POST failed");
	let (_, tx, _, batch, _) = row_of(&store, member.id).await;
	assert_eq!(batch.as_deref(), Some(leader.uid.as_str()), "the member is claimed");
	assert!(tx.is_none(), "and nothing has been sent for it yet");

	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), member.id)
		.await
		.expect("a member stands down; it does not fail");

	assert_eq!(
		requests(&server, "manageInvoice").await.len(),
		1,
		"the member must not POST under its own requestId"
	);
	assert_eq!(submissions(&store, member.id).await.len(), 1, "and it opens no second row");
}

/// The `nav.batch_max = 2` variant of `a_lost_reply_stays_retryable_on_one_row`: at
/// `batch_max = 1` that test cannot see membership at all. A retry resends the *same* batch under
/// the same `requestId` — the leader's invoice uid — which is what makes a resend idempotent at
/// NAV's end. The leader's row here predates batching (`batch_uid IS NULL`), the case that made
/// the claim a get-or-claim: a create-only insert collided on `idx_nav_submission_live` and
/// skipped the leader out of its own batch.
#[tokio::test]
async fn a_retry_resumes_the_same_batch_under_the_same_request_id() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	// `503`, not `500`: a `500` is indeterminate, and `NAV_RECONCILE` then owns the batch until
	// it settles rather than the retry resending it. This test is about the resend.
	mock_once(&server, "manageInvoice", 503, "<html>down</html>".to_owned()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-retry-resumes");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "2", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;
	// An attempt from before the setting change: a row with no `batch_uid`.
	store
		.create_submission(leader.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("the first POST failed");
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(sent.len(), 2, "one failed attempt, one that landed");
	for (n, body) in sent.iter().enumerate() {
		assert!(body.contains(leader.uid.as_str()), "attempt {n} changed the requestId");
		assert_eq!(operations(body), 2, "attempt {n} changed the member set: {body}");
	}
	for invoice in [&leader, &member] {
		let (_, tx, _, batch, _) = row_of(&store, invoice.id).await;
		assert_eq!(batch.as_deref(), Some(leader.uid.as_str()), "invoice {}", invoice.id);
		assert_eq!(tx.as_deref(), Some("TX-VERDICT"), "invoice {}", invoice.id);
	}
	assert_eq!(submissions(&store, leader.id).await.len(), 1, "the resend reuses the one row");
}

/// `INVALID_REQUEST_SIGNATURE` burns the **leader's** `requestId` and nothing else: nothing
/// was filed, so every member's own id is pristine. Settling them `FAILED` would make them
/// unfilable three ways over — `may_send` reads a verdict as settled, `unfiled_invoices` skips an
/// invoice that has any row, and their own jobs completed `Ok`, so `job_redrive`'s
/// `AND status = 'FAILED'` never matches them either.
#[tokio::test]
async fn a_spent_request_id_releases_the_batch_members() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>INVALID_REQUEST_SIGNATURE</common:errorCode>\
			 <common:message>bad signature</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;

	let db = TmpDb::new("batch-spent-id-releases");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	// The member is claimed first and then runs its own job, which stands down inside
	// `may_send` — the common case, and the one the old release ignored.
	store
		.claim_batch(leader.id, NavOp::Create, leader.uid.as_str(), &[member.id])
		.await
		.unwrap();
	stand_down(&app, &store, &invoices, &nav, member.id).await;

	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("a spent requestId can never be filed again under it");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-SPENT");

	assert_eq!(
		submissions(&store, leader.id).await,
		vec![(row_of(&store, leader.id).await.0, Some("FAILED".to_owned()))],
		"the leader's own id is the one that is spent"
	);
	assert!(
		submissions(&store, member.id).await.is_empty(),
		"the member has no row at all, so it looks never-attempted again"
	);
	// A `DONE` job burns `nav:invoice:{id}` for good: the sweep's re-enqueue is a no-op and
	// `Nav::submit` answers `E-NAV-NOT-REDRIVABLE`, so releasing the row alone left an issued
	// invoice nothing could ever file.
	assert_eq!(
		report_jobs(&store, member.id).await[0].1,
		"PENDING",
		"the released member is put back on its own job"
	);
	assert_eq!(store.unfiled_invoices(SELLER, 50).await.unwrap(), vec![member.id]);
}

/// Returning on the leader's verdict alone completed the job, spent `nav:poll:{txid}` and
/// left every other row with a `transactionId`, no verdict and no error code — invisible to
/// `awaiting_operator` and to `unfiled_invoices` alike, which is the silently unreported invoice
/// this crate exists to prevent. The job is done only when NAV has answered about every invoice
/// in the transaction; a row NAV has answered about is settled as it goes.
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

	let leader = issue_at(&store, FEB10).await;
	let second = issue_at(&store, FEB10).await;
	let third = issue_at(&store, FEB10).await;
	for id in [leader.id, second.id, third.id] {
		enqueue_report(&app, id).await;
	}

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();
	let (leader_sub, ..) = row_of(&store, leader.id).await;

	// Index 3 is not even mentioned, and index 2 is still processing: two of the three are
	// unanswered, so the job is not finished whatever index 1 says.
	let next = mintworks_nav::job::poll(
		&app,
		invoices.as_ref(),
		nav.as_ref(),
		&poll_job(leader_sub),
		leader_sub,
	)
	.await
	.unwrap();
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
		mintworks_nav::job::poll(
			&app,
			invoices.as_ref(),
			nav.as_ref(),
			&poll_job(leader_sub),
			leader_sub
		)
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
}

/// Batching must not bypass `Nav::cancel_filing`: an invoice whose filing an operator
/// deliberately stopped has no live `PENDING` job, and without that gate the next leader picks it
/// up and files it anyway.
#[tokio::test]
async fn a_cancelled_filing_is_not_a_batch_candidate() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-cancelled-candidate");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let stopped = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, stopped.id).await;

	let ctx = Ctx::system("test").with_org(ORG);
	assert_eq!(
		Nav::new(app.clone()).cancel_filing(&ctx, stopped.uid.as_str()).await.unwrap(),
		1,
		"the operator stopped its NAV_REPORT row"
	);

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(operations(&sent[0]), 1, "the cancelled invoice is not in the batch: {}", sent[0]);
	assert!(submissions(&store, stopped.id).await.is_empty(), "and it was never claimed");
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

	let leader = issue_at(&store, FEB10).await;
	let unrendered = issue_without_pdf(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, unrendered.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(operations(&sent[0]), 1, "the unrendered invoice is left out: {}", sent[0]);
	assert!(submissions(&store, unrendered.id).await.is_empty());
	assert_eq!(
		row_of(&store, leader.id).await.1.as_deref(),
		Some("TX-VERDICT"),
		"and the batch it would have joined still files"
	);
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
	// enqueues a reconciliation. A connect failure or `502`/`503` demonstrably never arrived.
	mock(&server, "manageInvoice", 504, "<html>gateway</html>".to_owned()).await;

	let db = TmpDb::new("batch-reconcile-found");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
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
			(1, leader.number.as_deref().unwrap()),
			(2, member.number.as_deref().unwrap()),
		]),
	)
	.await;

	mintworks_nav::job::reconcile(&app, invoices.as_ref(), nav.as_ref(), leader.uid.as_str())
		.await
		.unwrap();

	for (invoice, idx) in [(&leader, 1), (&member, 2)] {
		let (_, tx, got, _, verdict) = row_of(&store, invoice.id).await;
		assert_eq!(tx.as_deref(), Some("TX-LOST"), "invoice {}", invoice.id);
		assert_eq!(got, Some(idx), "the index NAV filed it under, not the one we asked for");
		assert!(verdict.is_none(), "the ordinary poll decides the verdict");
	}
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "the batch is back on the poll path");
	assert_eq!(requests(&server, "manageInvoice").await.len(), 1, "nothing was re-sent");
}

/// The other §1.9.2 arm: no transaction in the window that `nav_submissions` has never heard of,
/// so NAV never took the submission and the specification requires repeating it immediately. The
/// re-drive carries the same `batch_uid`, so the same `requestId` and the same members — if this
/// conclusion is ever wrong NAV refuses it rather than filing twice.
#[tokio::test]
async fn reconciliation_resends_when_nav_never_took_the_batch() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 504, "<html>gateway</html>".to_owned()).await;
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&[])).await;

	let db = TmpDb::new("batch-reconcile-resends");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;

	let leader = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("no answer is not a filing");

	// `job_redrive` matches `FAILED` only, and `jobs.max_attempts.NAV_REPORT = 0` keeps the
	// leader's row `PENDING` on its own backoff — that row resends the batch by itself. This
	// pins the other path: a row the runner has already given up on is put back at `now`.
	sqlx::query("UPDATE jobs SET status = 'FAILED' WHERE dedup_key = ?")
		.bind(format!("nav:invoice:{}", leader.id))
		.execute(store.write_pool())
		.await
		.unwrap();

	mintworks_nav::job::reconcile(&app, invoices.as_ref(), nav.as_ref(), leader.uid.as_str())
		.await
		.unwrap();

	let jobs = report_jobs(&store, leader.id).await;
	assert_eq!(jobs.len(), 1, "one NAV_REPORT row per invoice, always");
	assert_eq!(jobs[0].1, "PENDING", "the filing is re-driven, not duplicated");
	assert_eq!(row_of(&store, leader.id).await.1, None, "and still nothing is bound to it");
}

/// A batch leader's archived envelope carries every other org's `invoiceData` as decodable
/// base64, so `Nav::filing` blanks both XML columns for anyone but an operator. Not fixable by
/// archiving less: `NavStore::release_batch` depends on the leader keeping the whole envelope.
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

	let leader = issue_at(&store, FEB10).await;
	let other = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, other.id).await;
	// `batch_candidates` keys on `seller_id`, not on the org, so one envelope carries both.
	move_to_org(&store, other.id, ORG + 1).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();
	assert_eq!(operations(&requests(&server, "manageInvoice").await[0]), 2);

	let handle = Nav::new(app.clone());
	let mut user = Ctx::system("test").with_org(ORG);
	user.actor = mintworks_core::ctx::Actor::User { account_id: 1 };
	let row = handle.filing(&user, leader.uid.as_str()).await.unwrap().unwrap();
	assert_eq!(row.verdict, None, "the row's own state is still readable");
	assert!(row.done_at.is_none());

	let err = handle.filing_archive(&user, leader.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN", "the envelope carries another org's invoiceData");

	let operator = Ctx::system("test").with_org(ORG);
	let archive = handle.filing_archive(&operator, leader.uid.as_str()).await.unwrap().unwrap();
	assert!(archive.request_xml.is_some(), "an operator reads the archive");
}

/// A batch spans orgs by construction — `batch_candidates` selects on `seller_id`, and the
/// seller is the operator — so the leader's uid is another org's invoice id, time-sortable and
/// therefore dating it, and the `transactionId` is shared across the batch.
#[tokio::test]
async fn an_org_user_sees_no_batch_identifiers() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-identifier-scope");
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

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;
	move_to_org(&store, leader.id, ORG + 1).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();

	let handle = Nav::new(app.clone());
	let mut user = Ctx::system("test").with_org(ORG);
	user.actor = mintworks_core::ctx::Actor::User { account_id: 1 };
	let row = handle.filing(&user, member.uid.as_str()).await.unwrap().unwrap();
	assert!(row.batch_uid.is_none(), "the leader's uid belongs to another org's invoice");
	assert!(row.transaction_id.is_none(), "and the transactionId correlates the two");

	let operator = Ctx::system("test").with_org(ORG);
	let row = handle.filing(&operator, member.uid.as_str()).await.unwrap().unwrap();
	assert_eq!(row.batch_uid.as_deref(), Some(leader.uid.as_str()), "an operator reads both");
	assert_eq!(row.transaction_id.as_deref(), Some("TX-VERDICT"));

	// And `error_msg` with them: it is free text, and the batch fault path writes it onto every
	// member of a batch that spans orgs. `error_code` is NAV's own and stays.
	store
		.record_fault(row.id, "REQUEST_ID_NOT_UNIQUE", "already processed")
		.await
		.unwrap();
	let row = handle.filing(&user, member.uid.as_str()).await.unwrap().unwrap();
	assert!(row.error_msg.is_none());
	assert_eq!(row.error_code.as_deref(), Some("REQUEST_ID_NOT_UNIQUE"));
}

/// The leader's get-or-claim returns nothing when its own row was taken between `may_send`'s
/// read and this `BEGIN IMMEDIATE`. Claiming members anyway stamps each with a leader that will
/// never POST it: `may_send` refuses it, `UNFILED` skips any invoice that has a row, and its own
/// `NAV_REPORT` job has already completed `Ok` by standing down.
#[tokio::test]
async fn a_failed_leader_claim_claims_no_members() {
	let db = TmpDb::new("batch-leader-claim-fails");
	let (_app, store) = setup(&db).await;

	let leader = issue_at(&store, FEB10).await;
	let first = issue_at(&store, FEB10).await;
	let second = issue_at(&store, FEB10).await;

	// The leader's row is already with NAV, which is what the get-or-claim's
	// `transaction_id IS NULL` refuses.
	let taken = store
		.create_submission(leader.id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap();
	assert!(store.set_sent(taken.unwrap(), "TX-OTHER", 1).await.unwrap());

	let claimed = store
		.claim_batch(leader.id, NavOp::Create, leader.uid.as_str(), &[first.id, second.id])
		.await
		.unwrap();
	assert!(claimed.is_empty(), "no leader, no batch");
	for member in [&first, &second] {
		assert!(
			submissions(&store, member.id).await.is_empty(),
			"invoice {} was stamped with a leader that never POSTs",
			member.id
		);
	}
}

/// `cancel_filing` refuses on a member, but on a leader it used to stop only the leader's own
/// jobs — leaving every member stamped with a leader that will never POST, and their own
/// `NAV_REPORT` jobs already completed `Ok` by standing down. Cancelling one bad invoice quietly
/// un-filed up to `nav.batch_max - 1` good ones.
#[tokio::test]
async fn cancelling_a_leader_releases_its_members() {
	let db = TmpDb::new("batch-cancel-releases");
	let (app, store) = setup(&db).await;

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;
	assert_eq!(
		store
			.claim_batch(leader.id, NavOp::Create, leader.uid.as_str(), &[member.id])
			.await
			.unwrap()
			.len(),
		2
	);

	// The member's own job runs and stands down first: cancelling one leader used to strand up
	// to `nav.batch_max - 1` invoices that had nothing wrong with them.
	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	stand_down(&app, &store, &invoices, &nav, member.id).await;

	let ctx = Ctx::system("test").with_org(ORG);
	Nav::new(app.clone()).cancel_filing(&ctx, leader.uid.as_str()).await.unwrap();

	assert!(
		submissions(&store, member.id).await.is_empty(),
		"the member reverts to never-attempted, so `unfiled_invoices` offers it again"
	);
	assert_eq!(
		report_jobs(&store, member.id).await[0].1,
		"PENDING",
		"and its spent NAV_REPORT is re-driven, not left DONE"
	);
}

/// `Error::Timeout` is `Retry::Backoff` and the first backoff step is one second, so the
/// runner re-ran `report` while the reconciliation it had just scheduled was still five minutes
/// away. The resend carries the same `requestId`, which §1.9.2 forbids and NAV answers
/// `REQUEST_ID_NOT_UNIQUE` — driving the whole batch down the terminal-fault path that the
/// reconciliation exists to avoid.
#[tokio::test]
async fn a_pending_reconciliation_stops_the_leader_resending() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 504, "<html>gateway</html>".to_owned()).await;

	let db = TmpDb::new("batch-reconcile-blocks-resend");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;

	let leader = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("no answer is not a filing");
	assert_eq!(err.parts().1, "E-CORE-TIMEOUT");
	assert_eq!(enqueued(&store, "NAV_RECONCILE").await, 1);

	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("the reconciliation decides, not a resend");
	assert_eq!(err.parts().1, "E-CORE-UNAVAILABLE", "and it stays retryable");
	assert_eq!(
		requests(&server, "manageInvoice").await.len(),
		1,
		"a second POST under the same requestId is what §1.9.2 forbids"
	);
}

/// A member whose `invoiceData` cannot be built used to be claimed first and dropped after,
/// leaving a row only raw SQL could clear: the leader's next attempt filters it out on
/// `error_code`, `may_send` refuses its own job, `cancel_filing` refuses it as a member, and
/// `Nav::submit` answers `E-NAV-NOT-REDRIVABLE`. Built before the claim, it is simply never
/// claimed.
#[tokio::test]
async fn an_unbuildable_member_is_never_claimed() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-unbuildable-member");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let broken = issue_unfilable_at(&store, FEB10).await;
	let good = issue_at(&store, FEB10).await;
	for id in [leader.id, broken.id, good.id] {
		enqueue_report(&app, id).await;
	}

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(operations(&sent[0]), 2, "the leader and the good member only: {}", sent[0]);
	assert!(
		submissions(&store, broken.id).await.is_empty(),
		"nothing was claimed for it, so its own job fails on its own terms and retries"
	);
	assert_eq!(report_jobs(&store, broken.id).await[0].1, "PENDING");
}

/// `release_batch` guarded only on `transaction_id` and `verdict`, so it also deleted the
/// open-with-a-reason row `job::report` writes onto every member on `REQUEST_ID_NOT_UNIQUE` —
/// the one shape `awaiting_operator` counts as needing a person.
#[tokio::test]
async fn a_released_member_keeps_its_recorded_error() {
	let db = TmpDb::new("batch-release-keeps-error");
	let (_app, store) = setup(&db).await;

	let leader = issue_at(&store, FEB10).await;
	let pristine = issue_at(&store, FEB10).await;
	let errored = issue_at(&store, FEB10).await;
	let claimed = store
		.claim_batch(leader.id, NavOp::Create, leader.uid.as_str(), &[pristine.id, errored.id])
		.await
		.unwrap();
	let leader_sub = claimed[0].0;
	let errored_sub = row_of(&store, errored.id).await.0;
	// A member settled open: `done_at` set, still no verdict. `release_batch` keeps that row —
	// it is what `awaiting_operator` counts — and only clears its `batch_uid`.
	store
		.finish(
			errored_sub,
			None,
			Some(("REQUEST_ID_NOT_UNIQUE", "already processed")),
			Timestamp::now(),
		)
		.await
		.unwrap();

	let mut released = store.release_batch(leader.uid.as_str(), leader_sub).await.unwrap();
	released.sort_unstable();
	let mut expected = vec![pristine.id, errored.id];
	expected.sort_unstable();
	assert_eq!(released, expected, "both members are released, by invoice id");

	assert!(submissions(&store, pristine.id).await.is_empty(), "a pristine member's row goes");
	let kept = store.submission_by_invoice(errored.id).await.unwrap().unwrap();
	assert!(kept.batch_uid.is_none(), "only its ownership by a dead leader goes");
	assert_eq!(kept.error_code.as_deref(), Some("REQUEST_ID_NOT_UNIQUE"));
	assert!(kept.verdict.is_none(), "still open, which is what awaiting_operator counts");
	assert!(
		store
			.submission(leader_sub)
			.await
			.unwrap()
			.is_some_and(|s| s.batch_uid.is_some()),
		"the leader keeps the batch it owns"
	);
}

/// NAV processes only the first request under a given `requestId` and refuses every later one
/// as `REQUEST_ID_NOT_UNIQUE`, so what NAV holds — and what a dispute is settled from — is the
/// first attempt. An unconditional `UPDATE` let a resend replace it.
#[tokio::test]
async fn a_resend_keeps_the_first_archived_request() {
	let db = TmpDb::new("archive-first-attempt");
	let (_app, store) = setup(&db).await;

	let leader = issue_at(&store, FEB10).await;
	let id = store
		.claim_batch(leader.id, NavOp::Create, leader.uid.as_str(), &[])
		.await
		.unwrap()[0]
		.0;

	store.archive_request(id, "<first/>").await.unwrap();
	store.archive_request(id, "<second/>").await.unwrap();

	assert_eq!(
		store.submission_archive(id).await.unwrap().unwrap().request_xml.as_deref(),
		Some("<first/>"),
		"the archive is what NAV holds, which is the first attempt"
	);
}

/// `record_fault` used to run over every row in the batch, before the `Retry::Never`
/// branch. On the retryable path — the majority, by `client::business`'s design — nothing then
/// released the members, and the `error_code` it stamped made the leader's next attempt read
/// them as settled and drop them: unfilable by their own job (`may_send` sees another leader),
/// by the sweep (`UNFILED` skips any row), by `Nav::submit` and by `Nav::cancel_filing` alike.
///
/// `a_retry_resumes_the_same_batch_under_the_same_request_id` cannot see this: its `503` never
/// reaches `accepted`, so no fault is ever recorded.
#[tokio::test]
async fn a_retryable_fault_leaves_every_member_filable() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	// `Disposition::Retry` in `client::FAULTS`, so `client::business` classes it
	// `Retry::Backoff`.
	mock_once(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>OPERATION_FAILED</common:errorCode>\
			 <common:message>try again later</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-retryable-fault-members");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("a fault is not a filing");
	assert_eq!(err.retry(), mintworks_core::Retry::Backoff, "{err:?}");

	let kept = store.submission_by_invoice(member.id).await.unwrap().unwrap();
	assert_eq!(kept.batch_uid.as_deref(), Some(leader.uid.as_str()), "still in the batch");
	assert!(kept.error_code.is_none(), "the leader's fault must not settle the member");
	assert!(kept.verdict.is_none());

	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(sent.len(), 2);
	assert_eq!(operations(&sent[1]), 2, "the member is still in the resend: {}", sent[1]);
	assert_eq!(
		row_of(&store, member.id).await.1.as_deref(),
		Some("TX-VERDICT"),
		"and it reached NAV"
	);
}

/// `E-NAV-NO-TRANSACTION-ID` is the canonical §1.9.2 lost reply: NAV accepted and we lost
/// the handle. Only `Error::Timeout` used to schedule a reconciliation, so this resent under the
/// same `requestId`, earned `REQUEST_ID_NOT_UNIQUE` and parked the whole batch for an operator —
/// the one thing `queryTransactionList` exists to resolve without one.
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

	let leader = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("OK with no transactionId is not something to act on");
	assert_eq!(err.parts().1, "E-NAV-NO-TRANSACTION-ID");

	let key: Option<String> =
		sqlx::query_scalar("SELECT dedup_key FROM jobs WHERE kind = 'NAV_RECONCILE'")
			.fetch_optional(store.read_pool())
			.await
			.unwrap();
	assert_eq!(key.as_deref(), Some(format!("nav:reconcile:{}", leader.uid.as_str()).as_str()));

	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("the reconciliation decides, not a resend");
	assert_eq!(err.parts().1, "E-CORE-UNAVAILABLE");
	assert_eq!(requests(&server, "manageInvoice").await.len(), 1, "§1.9.2 forbids the repeat");
}

/// A resend must carry the membership the archived envelope describes: `archive_request`
/// keeps the first attempt, which is what NAV holds once one has reached it, so a candidate
/// admitted on attempt 2 would be filed under an envelope that never mentions it.
/// `a_retry_resumes_the_same_batch_under_the_same_request_id` pins `batch_max = 2`, which makes
/// `room = 0` anyway and hides the case.
#[tokio::test]
async fn a_resend_admits_no_new_member_once_the_envelope_is_archived() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	// `503`: demonstrably never reached NAV, so the retry re-POSTs rather than reconciling.
	mock_once(&server, "manageInvoice", 503, "<html>down</html>".to_owned()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-frozen-membership");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let first = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, first.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("the first POST failed");

	let latecomer = issue_at(&store, FEB10).await;
	enqueue_report(&app, latecomer.id).await;

	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(sent.len(), 2);
	assert_eq!(operations(&sent[1]), 2, "the resend grew the batch: {}", sent[1]);
	assert!(
		submissions(&store, latecomer.id).await.is_empty(),
		"the latecomer files on its own job instead"
	);
	let leader_row = store.submission_by_invoice(leader.id).await.unwrap().unwrap();
	let archived = store
		.submission_archive(leader_row.id)
		.await
		.unwrap()
		.unwrap()
		.request_xml
		.expect("the leader archives the whole envelope");
	assert_eq!(operations(&archived), 2, "the archive describes what was sent");
}

/// `REQUEST_ID_NOT_UNIQUE` leaves every row of the batch open on purpose — NAV may hold the
/// filing — but it used to do that through `NavStore::finish`, which stamps a `done_at` nothing
/// ever clears. `Nav::resolve_filing`, the remedy the branch itself documents, then had nothing
/// it could move for a member, and no other path reopens one either.
#[tokio::test]
async fn a_reused_request_id_parks_every_member_for_an_operator() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>REQUEST_ID_NOT_UNIQUE</common:errorCode>\
			 <common:message>already used</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;

	let db = TmpDb::new("batch-reused-id-parks");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("this requestId was already processed");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-REUSED");

	let row = store.submission_by_invoice(member.id).await.unwrap().unwrap();
	assert!(row.verdict.is_none(), "NAV may hold the batch; nothing is archived as failed");
	assert!(row.done_at.is_none(), "and nothing settles a row an operator still has to move");
	assert_eq!(row.error_code.as_deref(), Some("REQUEST_ID_NOT_UNIQUE"));
	// The text is NAV's, never the leader's uid — a batch spans orgs, and `Nav::filing`
	// blanks `batch_uid` for exactly the identifier this used to spell out in free text.
	let msg = row.error_msg.as_deref().unwrap_or_default();
	assert!(!msg.contains(leader.uid.as_str()), "{msg}");
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 2, "both rows need a person");

	let ctx = Ctx::system("test").with_org(ORG);
	Nav::new(app.clone())
		.resolve_filing(&ctx, member.uid.as_str(), "queried the transaction with NAV")
		.await
		.expect("the member is resolvable, which is the remedy this branch documents");
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1);
}

/// `nav:reconcile:{batch_uid}` is spent for good once a reconciliation reaches `DONE`, and
/// `job_redrive` matches `FAILED` only — but a reconciliation legitimately finishes `DONE`
/// having settled nothing ("NAV never took this batch"). A second lost reply on the same batch
/// then had no recovery path at all, while `report`'s pending-reconcile guard saw the `DONE` and
/// let the resend go under the already-burned `requestId`.
#[tokio::test]
async fn a_second_lost_reply_revives_a_reconciliation_that_settled_nothing() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 504, "<html>gateway</html>".to_owned()).await;
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&[])).await;

	let db = TmpDb::new("batch-reconcile-revive");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;

	let leader = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let reconcile_jobs = || async {
		sqlx::query_as::<_, (String,)>(
			"SELECT status FROM jobs WHERE kind = 'NAV_RECONCILE' AND payload = ?",
		)
		.bind(mintworks_nav::job::reconcile_payload(leader.uid.as_str()))
		.fetch_all(store.read_pool())
		.await
		.unwrap()
	};

	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("no answer is not a filing");
	assert_eq!(reconcile_jobs().await, vec![("PENDING".to_owned(),)]);

	// It runs, finds nothing NAV kept, and completes `Ok` having settled nothing — which is the
	// runner marking the one row that holds the key `DONE`.
	mintworks_nav::job::reconcile(&app, invoices.as_ref(), nav.as_ref(), leader.uid.as_str())
		.await
		.unwrap();
	sqlx::query("UPDATE jobs SET status = 'DONE' WHERE kind = 'NAV_RECONCILE'")
		.execute(store.write_pool())
		.await
		.unwrap();

	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("the second reply is lost too");

	assert_eq!(
		reconcile_jobs().await,
		vec![("PENDING".to_owned(),)],
		"one row, re-driven — a batch that settled nothing can be asked about again"
	);
	assert_eq!(row_of(&store, leader.id).await.1, None, "and nothing is bound to it yet");
}

/// `SCHEMA_VIOLATION` and `INVOICE_NUMBER_NOT_UNIQUE` are refusals no resend can change, and
/// they used to back off forever — `jobs.max_attempts.NAV_REPORT` is `0` — six requests an hour
/// at the tax authority until a person called `Nav::cancel_filing`. The leader parks instead:
/// nothing was filed and no `requestId` was spent, so every member goes back on its own job and
/// the leader's row stays open for an operator.
#[tokio::test]
async fn a_permanently_unfilable_fault_parks_the_leader_and_releases_its_batch() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>INVOICE_NUMBER_NOT_UNIQUE</common:errorCode>\
			 <common:message>duplicate invoice number</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;

	let db = TmpDb::new("report-unfilable");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("a fault is not a filing");
	assert_eq!(err.parts().1, "E-NAV-UNFILABLE", "{err:?}");
	assert_eq!(err.retry(), mintworks_core::Retry::Never, "{err:?}");

	// Open, not settled: no verdict NAV never gave, and the reason is on the row.
	let row = store.submission_by_invoice(leader.id).await.unwrap().unwrap();
	assert!(row.verdict.is_none(), "NAV gave no verdict on the invoice");
	assert_eq!(row.error_code.as_deref(), Some("INVOICE_NUMBER_NOT_UNIQUE"));
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1, "a person is told");

	// The member's own id is pristine — nothing was filed — so its row goes and its job comes
	// back, or the leader's refusal makes it unfilable by every path.
	assert!(store.submission_by_invoice(member.id).await.unwrap().is_none());
	let status: Option<String> = app
		.store
		.job_status_by_key(&format!("nav:invoice:{}", member.id))
		.await
		.unwrap();
	assert_eq!(status.as_deref(), Some("PENDING"), "the member files itself now");
}

/// An `invoiceStatus` outside `InvoiceStatusType` used to read as pending, so the poll asked
/// again every ten minutes forever and nothing anywhere said so. It still polls — a status NAV
/// has just added must not park a statutory filing — but the row carries a marker, which is
/// what `awaiting_operator` and `A-NAV-REJECTED` count.
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

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let id = store
		.create_submission(invoice.id, NavOp::Create, "<x/>")
		.await
		.unwrap()
		.unwrap();
	assert!(store.set_sent(id, "TX-WEIRD", 1).await.unwrap());

	let next = mintworks_nav::job::poll(&app, invoices.as_ref(), nav.as_ref(), &poll_job(id), id)
		.await
		.unwrap();
	assert!(
		matches!(next, mintworks_core::job::Next::Again { .. }),
		"an unknown status settles nothing, so the poll asks again: {next:?}"
	);

	let row = store.submission(id).await.unwrap().unwrap();
	assert!(row.verdict.is_none(), "an unknown status is not a verdict");
	assert_eq!(row.error_code.as_deref(), Some(mintworks_nav::job::E_NAV_UNKNOWN_STATUS));
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1);
}

/// HTTP 408 used to reach the operation parser as a body: no `funcCode`, so
/// `Fault{E-NAV-HTTP-STATUS}` — "nothing was filed" — and the batch resent under a `requestId`
/// NAV may already have processed, with no §1.9.2 reconciliation anywhere. It is
/// `Answer::Indeterminate` now, which is the one thing that enqueues `NAV_RECONCILE`.
#[tokio::test]
async fn a_lost_reply_on_a_408_enqueues_a_reconciliation() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 408, String::new()).await;

	let db = TmpDb::new("report-408");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("a lost reply is not a filing");
	assert!(matches!(err, mintworks_core::error::Error::Timeout(_)), "{err:?}");

	let status: Option<String> = app
		.store
		.job_status_by_key(&format!("nav:reconcile:{}", invoice.uid.as_str()))
		.await
		.unwrap();
	assert_eq!(status.as_deref(), Some("PENDING"), "§1.9.2 asks NAV what landed");
	// And the row stays open with no verdict: NAV said nothing about the invoice.
	assert_eq!(submissions(&store, invoice.id).await, vec![(1, None)]);
}

/// A poll waiting on NAV used to return `Err(Unavailable)`, which wrote `last_error`, made the
/// row answer `job_retrying_kinds` and raised `A-JOB-STALE` — a healthy poll indistinguishable
/// from a failing one. `Next::Again` is a success carrying its own next run.
#[tokio::test]
async fn a_pending_poll_reschedules_itself_without_failing_the_job() {
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
			 <invoiceStatus>PROCESSING</invoiceStatus>\
			 </processingResult></processingResults>\
			 </QueryTransactionStatusResponse>"
		),
	)
	.await;

	let db = TmpDb::new("poll-defers");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let sub = store
		.create_submission(invoice.id, NavOp::Create, "<x/>")
		.await
		.unwrap()
		.unwrap();
	assert!(store.set_sent(sub, "TX-PENDING", 1).await.unwrap());

	// The `NAV_POLL` row the runner would claim, so the deferral has somewhere to land.
	let job_id = mintworks_core::job::enqueue(
		&app.store,
		mintworks_nav::job::KIND_NAV_POLL,
		&mintworks_nav::job::poll_payload(sub),
		Some("nav:poll:TX-PENDING"),
		Timestamp::now(),
	)
	.await
	.unwrap()
	.unwrap();
	app.store.job_claim(Timestamp::now()).await.unwrap();

	let job = mintworks_core::job::Job {
		id: job_id,
		kind: mintworks_nav::job::KIND_NAV_POLL.to_owned(),
		payload: mintworks_nav::job::poll_payload(sub),
		attempts: 1,
	};
	let next = mintworks_nav::job::poll(&app, invoices.as_ref(), nav.as_ref(), &job, sub)
		.await
		.unwrap();
	let mintworks_core::job::Next::Again { at } = next else {
		panic!("NAV is still processing, so the poll asks again: {next:?}");
	};
	assert!(at.0 > Timestamp::now().0, "and it asks later, not now");

	// Applied to the row, nothing is wrong with it, and it is not a retry.
	assert_eq!(app.store.job_defer(job_id, at).await.unwrap(), 1);
	let (status, last_error): (String, Option<String>) =
		sqlx::query_as("SELECT status, last_error FROM jobs WHERE id = ?")
			.bind(job_id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(status, "PENDING");
	assert_eq!(last_error, None, "a poll doing its job is not a failing one");
	assert!(app.store.job_retrying_kinds().await.unwrap().is_empty());

	// And the filing itself is untouched: no verdict, and no marker this early.
	let row = store.submission(sub).await.unwrap().unwrap();
	assert!(row.verdict.is_none());
	assert!(row.error_code.is_none());
}

/// `record_sent` stamped the batch one row at a time with no transaction around it, so a
/// writer failing partway left NAV holding the whole batch under a `transactionId` that only
/// some rows carried. An unstamped member is invisible to `may_send`,
/// `submissions_by_transaction`, `awaiting_operator` and `unfiled_invoices` alike — a silently
/// unreported statutory filing.
#[tokio::test]
async fn a_failed_member_write_leaves_no_member_with_a_transaction_id() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-set-sent-atomic");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(Faulty(Arc::new(store.clone()), Fail::SetSentBatch));
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("the transactionId could not be recorded");

	for invoice in [&leader, &member] {
		let row = store.submission_by_invoice(invoice.id).await.unwrap().unwrap();
		assert!(
			row.transaction_id.is_none(),
			"the batch stamp is one transaction; a partial one strands the rest"
		);
	}
}

/// The `PossiblyFiled` arm propagated a failed `record_fault`, so a store error on member 2 of
/// 100 aborted the loop, discarded `E-NAV-REQUEST-ID-REUSED` and left members 3..N with no
/// `error_code` at all — invisible to `awaiting_operator`, refused by `may_send`, skipped by
/// `unfiled_invoices`. Best-effort, like its `filing::park` and `archive_reply` siblings.
#[tokio::test]
async fn a_failed_fault_write_still_marks_the_rest_of_the_batch() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>REQUEST_ID_NOT_UNIQUE</common:errorCode>\
			 <common:message>already used</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;

	let db = TmpDb::new("batch-fault-write-fails");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;
	enqueue_report(&app, member.id).await;

	// The leader's row is the first one created, so `1` is the write that fails.
	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(Faulty(Arc::new(store.clone()), Fail::RecordFaultFor(1)));
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("this requestId was already processed");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-REUSED", "the verdict must survive the bad write");

	let led = store.submission_by_invoice(leader.id).await.unwrap().unwrap();
	assert_eq!(led.id, 1, "the injected failure is meant to land on the leader");
	let kept = store.submission_by_invoice(member.id).await.unwrap().unwrap();
	assert_eq!(
		kept.error_code.as_deref(),
		Some("REQUEST_ID_NOT_UNIQUE"),
		"one failed write must not hide the rest of the batch from an operator"
	);
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1);
}

/// `cancel_filing` on a leader calls `release_batch`, which guards only on `transaction_id` and
/// `verdict` — neither of which the lost-reply path writes. So "NAV holds this batch but the
/// reply was lost" looked identical to "the batch never left", and every released member refiled
/// under its own `requestId`, which NAV does not dedupe.
#[tokio::test]
async fn a_leader_whose_reply_was_lost_cannot_be_cancelled_until_it_is_reconciled() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 500, "<html>gateway</html>".to_owned()).await;

	let db = TmpDb::new("batch-cancel-in-flight");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;

	let leader = issue_at(&store, FEB10).await;
	let member = issue_at(&store, FEB10).await;
	for id in [leader.id, member.id] {
		enqueue_report(&app, id).await;
	}

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("no answer is not a filing");
	assert_eq!(enqueued(&store, "NAV_RECONCILE").await, 1);
	stand_down(&app, &store, &invoices, &nav, member.id).await;

	let ctx = Ctx::system("test").with_org(ORG);
	let handle = Nav::new(app.clone());
	let err = handle
		.cancel_filing(&ctx, leader.uid.as_str())
		.await
		.expect_err("NAV may hold this batch");
	assert_eq!(err.parts().1, "E-NAV-FILING-IN-FLIGHT");
	assert_eq!(err.parts().0, mintworks_core::error::StatusCode::CONFLICT);
	assert!(
		!submissions(&store, member.id).await.is_empty(),
		"releasing the member would file it again under its own requestId"
	);
	assert_eq!(report_jobs(&store, member.id).await[0].1, "DONE", "and its job stays stood down");

	// The reconciliation settles the batch's fate; only then may an operator stop the filing.
	sqlx::query("UPDATE jobs SET status = 'DONE' WHERE kind = 'NAV_RECONCILE'")
		.execute(store.write_pool())
		.await
		.unwrap();
	handle.cancel_filing(&ctx, leader.uid.as_str()).await.unwrap();
	assert!(submissions(&store, member.id).await.is_empty(), "now the member is given back");
	assert_eq!(report_jobs(&store, member.id).await[0].1, "PENDING");
}

/// `REQUEST_ID_NOT_UNIQUE` records a fault on **every** row of the batch, and the resume filter
/// dropped any member carrying an `error_code`. So the operator's re-drive resent the leader
/// alone under the batch's `requestId`, and the members were left stamped with a batch that no
/// longer carries them, which no API path can reach.
#[tokio::test]
async fn a_faulted_member_still_rides_the_resend_under_the_same_request_id() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock_once(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>REQUEST_ID_NOT_UNIQUE</common:errorCode>\
			 <common:message>already used</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-faulted-member-resends");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;

	let leader = issue_at(&store, FEB10).await;
	let first = issue_at(&store, FEB10).await;
	let second = issue_at(&store, FEB10).await;
	for id in [leader.id, first.id, second.id] {
		enqueue_report(&app, id).await;
	}

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let err = mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("NAV refused this requestId");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-REUSED");
	for id in [leader.id, first.id, second.id] {
		let row = store.submission_by_invoice(id).await.unwrap().unwrap();
		assert_eq!(row.error_code.as_deref(), Some("REQUEST_ID_NOT_UNIQUE"), "invoice {id}");
	}

	// The operator re-drives the leader once the transaction is known not to be at NAV.
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();
	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(operations(&sent[1]), 3, "the resend carries the whole batch: {}", sent[1]);
	assert!(
		sent[1].contains(leader.uid.as_str()),
		"and under the leader's uid as the requestId: {}",
		sent[1]
	);
	for id in [first.id, second.id] {
		assert_eq!(
			store
				.submission_by_invoice(id)
				.await
				.unwrap()
				.unwrap()
				.transaction_id
				.as_deref(),
			Some("TX-VERDICT"),
			"invoice {id} was left behind by the resend"
		);
	}
}

/// Nothing feeding `invoice_data` is frozen with the claim: turning `nav.electronic_invoice` on
/// between two attempts makes every member need a PDF hash, and a member claimed before the
/// switch was never screened for one. Dropped from the batch while still carrying `batch_uid`,
/// it was an issued invoice no path could ever file.
#[tokio::test]
async fn a_member_that_stops_building_is_released_rather_than_dropped() {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock_once(&server, "manageInvoice", 503, "<html>down</html>".to_owned()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("batch-member-unbuildable-on-resend");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	app.settings.set("nav.electronic_invoice", "0", None).await.unwrap();

	let leader = issue_at(&store, FEB10).await;
	// No `invoice_documents` row, which only matters once `nav.electronic_invoice` is on.
	let member = issue_without_pdf(&store, FEB10).await;
	for id in [leader.id, member.id] {
		enqueue_report(&app, id).await;
	}

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("the first POST is lost on the wire");
	stand_down(&app, &store, &invoices, &nav, member.id).await;
	assert_eq!(
		store
			.submission_by_invoice(member.id)
			.await
			.unwrap()
			.unwrap()
			.batch_uid
			.as_deref(),
		Some(leader.uid.as_str()),
		"the member is claimed into the batch"
	);

	// The operator turns electronic invoicing on between the two attempts.
	app.settings.set("nav.electronic_invoice", "1", None).await.unwrap();
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.unwrap();

	let sent = requests(&server, "manageInvoice").await;
	assert_eq!(operations(&sent[1]), 1, "the leader files without it: {}", sent[1]);
	let row = store.submission_by_invoice(member.id).await.unwrap().unwrap();
	assert!(row.batch_uid.is_none(), "the member is out of the batch, not stranded in it");
	assert_eq!(row.error_code.as_deref(), Some(mintworks_nav::job::E_NAV_UNBUILDABLE));
	assert_eq!(store.awaiting_operator(SELLER).await.unwrap(), 1, "and a person is told");
	assert_eq!(
		report_jobs(&store, member.id).await[0].1,
		"PENDING",
		"its own NAV_REPORT takes over"
	);
}

/// `availablePage` was clamped to `MAX_TRANSACTION_LIST_PAGES`, so a busy seller's transaction
/// could sit past page 20 and `reconcile` concluded "NAV never took this batch" — a resend under
/// a burned `requestId`, which is the one outcome §1.9.2 exists to avoid.
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

	let leader = issue_at(&store, FEB10).await;
	enqueue_report(&app, leader.id).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	mintworks_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), leader.id)
		.await
		.expect_err("no answer is not a filing");
	sqlx::query("UPDATE jobs SET status = 'FAILED' WHERE dedup_key = ?")
		.bind(format!("nav:invoice:{}", leader.id))
		.execute(store.write_pool())
		.await
		.unwrap();

	let err =
		mintworks_nav::job::reconcile(&app, invoices.as_ref(), nav.as_ref(), leader.uid.as_str())
			.await
			.expect_err("the window is unreadable, so nothing is concluded");
	assert_eq!(err.parts().1, "E-NAV-UNAVAILABLE");
	assert_eq!(err.retry(), mintworks_core::Retry::Backoff, "{err:?}");
	assert_eq!(
		report_jobs(&store, leader.id).await[0].1,
		"FAILED",
		"the leader must not be re-driven into a resend under a burned requestId"
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
		vat_scheme: "ALANYI_MENTES".into(),
		income_regime: "NONE".into(),
		expense_ratio_pct: None,
		regime_since: None,
		created_at: Timestamp(0),
		valid_from: Some(Timestamp(0)),
		superseded_at: Some(Timestamp(1)),
	};
	let (invoice, lines, groups) = xml_parts();

	let xml = mintworks_nav::xml::invoice_data(&version, &invoice, &lines, &groups, None).unwrap();
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

	let xml =
		mintworks_nav::xml::invoice_data(&seller_version_row(), &invoice, &lines, &groups, None)
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
		vat_scheme: "NORMAL".into(),
		income_regime: "NONE".into(),
		expense_ratio_pct: None,
		regime_since: None,
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
	mock(
		&server,
		"tokenExchange",
		400,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>INVALID_SECURITY_USER</common:errorCode>\
			 <common:message>bad user</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
	)
	.await;
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
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>NOT_REGISTERED_CUSTOMER</common:errorCode>\
			 <common:message>Nem regisztrált felhasználó!</common:message></common:result>\
			 </GeneralErrorResponse>"
		),
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
	let invoice = issue_as(&store, TENANT, FEB10, ver).await;
	assert!(mintworks_nav::job::deferral(&app, &store, invoice.id).await.unwrap().is_some());

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
	let (_, ver) = tenant(&store).await;
	let invoice = issue_as(&store, TENANT, FEB10, ver).await;

	let at = mintworks_nav::job::deferral(&app, &store, invoice.id).await.unwrap().unwrap();
	assert!(at.0 >= Timestamp::now().0 + mintworks_nav::job::NOT_CONNECTED_RECHECK_SECS - 5);
	assert!(submissions(&store, invoice.id).await.is_empty(), "a deferral opens no filing");

	// The deployment's own seller never defers: its gap is an operator fault, not a wait.
	let root_invoice = issue_at(&store, FEB10).await;
	assert!(
		mintworks_nav::job::deferral(&app, &store, root_invoice.id)
			.await
			.unwrap()
			.is_none()
	);
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
	assert!(mintworks_nav::job::deferral(&app, &store, invoice.id).await.unwrap().is_none());
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
	mintworks_nav::job::sweep(&app, &store, &store).await.unwrap();
	assert!(mintworks_nav::alerts(app).await.unwrap().is_empty());
}

// vim: ts=4
