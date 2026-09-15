//! The statutory *adóhatósági ellenőrzési adatszolgáltatás* — its two selection forms and the
//! file `saas_nav::Nav::audit_export` writes — plus the `NAV_REPORT` filing job.
//!
//! Both halves need a real `NavStore`, so the suite takes `store-adapter-sqlite` as a
//! dev-dependency. The selection SQL is the part 23/2014 (VI. 30.) NGM r. is strict about, so
//! it is exercised against a real database rather than a double. NAV itself is a `wiremock`
//! stand-in.
//!
//! Every invoice is issued at mid-day UTC: `export_ids_by_date` selects the Europe/Budapest
//! day (`saas_invoice::numbering::utc_span`), so a boundary invoice near midnight would be
//! flaky either way.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use saas_core::{App, AppBuilder, config::Config, ctx::Ctx, prelude::*};
use saas_invoice::{
	draft::Priced,
	service_api::SELLER_ID,
	store::{
		BuyerSnapshot, Invoice, InvoiceKind, InvoiceStore, InvoiceVatGroup, IssueInvoice,
		NewInvoice, NewInvoiceLine, PartyKind, PaymentMethod, Seller,
	},
	vat::VatCode,
};
use saas_nav::{
	Nav, NavOp, NavStore, NavVerdict,
	export::{Selection, range_error, selection},
};
use store_adapter_sqlite::SqliteStore;
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{method, path},
};

const TENANT: i64 = 1;

/// A `software` block `invoiceApi.xsd` accepts: `softwareId` is `[0-9A-Z\-]{18}` exactly, and
/// the other required fields are `…NotBlankType`.
const SOFTWARE_SETTINGS: [(&str, &str); 6] = [
	("nav.software_id", "HU12345678SAASFRWK"),
	("nav.software_name", "saas-framework"),
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
			.join(format!("saas-nav-export-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [0; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: String::new(),
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
/// foreign keys demand: one account, one tenant, seller 1. `HUF` is seeded by
/// `saas_invoice::M_INIT`, and the export needs it for the currency's minor unit.
async fn setup(db: &TmpDb) -> (App, SqliteStore) {
	let store = SqliteStore::open(&db.config()).await.unwrap();
	store.migrate(store_adapter_sqlite::STEPS).await.unwrap();
	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	// No workers: every test here drives `job::report`/`job::poll` directly, and a live worker
	// claiming a row these tests assert the status of is a race with no upside — it registers
	// no NAV handler, so all it can do is terminate the row out from under the assertion.
	let app = AppBuilder::new()
		.config(Config { jobs_workers: Some(0), ..db.config() })
		.store(Arc::new(store.clone()) as Arc<dyn saas_core::store::CoreStore>)
		.extension(invoices)
		.extension(nav)
		.build()
		.await
		.unwrap();

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
	// The tenant's default billing party, so the tests that go through `Invoices` rather than
	// straight at the store can resolve `Party::TenantDefault`.
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, tenant_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', ?, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 1, 0, 0)",
	)
	.bind(TENANT)
	.execute(store.writer())
	.await
	.unwrap();
	store.put_seller(&seller()).await.unwrap();

	(app, store)
}

fn seller() -> Seller {
	Seller {
		id: SELLER_ID,
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
		nav_base_url: String::new(),
		nav_login: Some("techuser".into()),
		small_business: false,
		vat_scheme: "NORMAL".into(),
		series_code: "A".into(),
		created_at: Timestamp::now(),
	}
}

fn new_invoice(kind: InvoiceKind, original: Option<i64>) -> NewInvoice {
	NewInvoice {
		tenant_id: TENANT,
		seller_id: SELLER_ID,
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
	}
}

/// A draft with one line, issued at `issued_at`.
async fn issue_at(store: &SqliteStore, issued_at: i64) -> Invoice {
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
	// Re-read: `replace_draft_lines` above moved `updated_at`, and `issue` now checks it.
	let draft = store.invoice_by_id(draft.id).await.unwrap().unwrap();
	store
		.issue(draft.id, &issue_input(draft.id, NET, issued_at), draft.version)
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
	store.issue(draft.id, &input, draft.version).await.unwrap()
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

	let ids = store.export_ids_by_date(SELLER_ID, FROM, TO).await.unwrap();
	assert_eq!(ids, vec![lower.id, failed.id, upper.id]);
	assert!(!ids.contains(&before.id), "the day before the range must be excluded");
	assert!(!ids.contains(&after.id), "the day after the range must be excluded");

	// A draft has no number and is not an issued invoice.
	let draft = store.create_draft(&new_invoice(InvoiceKind::Normal, None)).await.unwrap();
	let ids = store.export_ids_by_date(SELLER_ID, "2026-01-01", "2026-12-31").await.unwrap();
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

	let ids = store.export_ids_by_date(SELLER_ID, FROM, TO).await.unwrap();
	assert!(ids.contains(&original.id), "the original of an in-range storno must be pulled in");
	assert!(ids.contains(&storno.id));

	// And the other direction: a January range must pull the February storno in.
	let ids = store.export_ids_by_date(SELLER_ID, "2026-01-01", "2026-01-31").await.unwrap();
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
	store.put_seller(&other).await.unwrap();
	let mut cancel = new_invoice(InvoiceKind::Storno, Some(second.id));
	cancel.seller_id = 2;
	let foreign = store
		.storno(second.id, &cancel, &issue_input(0, -100_000, FEB15))
		.await
		.unwrap();

	let ids = store.export_ids_by_date(SELLER_ID, FROM, TO).await.unwrap();
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

	let ctx = Ctx::system("test").with_tenant(TENANT);
	let mut out = Vec::new();
	let count = Nav::new(app.clone())
		.audit_export(&ctx, SELLER_ID, Selection::IssueDate { from: FROM, to: TO }, &mut out)
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
	.fetch_one(store.reader())
	.await
	.unwrap();
	assert_eq!(logged, 1);
}

/// The export spans every invoice of the seller across all tenants, and `export` took a
/// `&Ctx`, wrote an audit row, and never looked at `ctx.actor`. `saas-invoice` gates its
/// operator entry points; `saas-nav` had no equivalent.
#[tokio::test]
async fn the_export_is_operator_only() {
	let db = TmpDb::new("export-authz");
	let (app, store) = setup(&db).await;
	issue_at(&store, FEB10).await;
	let range = Selection::IssueDate { from: FROM, to: TO };
	let nav = Nav::new(app.clone());

	let mut user = Ctx::system("test").with_tenant(TENANT);
	user.actor = saas_core::ctx::Actor::User { account_id: 1 };
	let err = nav
		.audit_export(&user, SELLER_ID, range, &mut Vec::new())
		.await
		.expect_err("a tenant user must not read every tenant's invoices");
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");

	// `System` is the application's own code and is trusted, as in `service_api`.
	let system = Ctx::system("test").with_tenant(TENANT);
	assert_eq!(nav.audit_export(&system, SELLER_ID, range, &mut Vec::new()).await.unwrap(), 1);
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
	use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
	use base64::{Engine, engine::general_purpose::STANDARD as B64};

	let mut block = *GenericArray::from_slice(TOKEN.as_bytes());
	aes::Aes128::new(GenericArray::from_slice(EXCHANGE_KEY)).encrypt_block(&mut block);
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
		.fetch_all(store.reader())
		.await
		.unwrap()
}

async fn enqueued(store: &SqliteStore, kind: &str) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = ?")
		.bind(kind)
		.fetch_one(store.reader())
		.await
		.unwrap()
}

/// A `manageInvoice` that fails on the wire says nothing about the invoice, so the row stays
/// open with no verdict and the job row owns the retry. The retry reuses that one row — the
/// `requestId` is `invoices.uid`, stable across every attempt, which is what makes a resend
/// idempotent at NAV's end and what deleted the old `UNKNOWN` parking state.
///
/// `500` came from the invoice service itself and `503` from the load balancer; under retry on
/// the job they are the same case, and neither may open a second row.
#[tokio::test]
async fn a_lost_reply_stays_retryable_on_one_row() {
	for (status, name) in [(500u16, "report-500"), (503, "report-503")] {
		let server = MockServer::start().await;
		mock(&server, "tokenExchange", 200, token_reply()).await;
		mock(&server, "manageInvoice", status, "<html>down</html>".to_owned()).await;

		let db = TmpDb::new(name);
		let (app, store) = setup(&db).await;
		point_at_nav(&app, &server.uri()).await;
		let invoice = issue_at(&store, FEB10).await;

		let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
		let nav: Arc<dyn NavStore> = Arc::new(store.clone());

		let first = saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id).await;
		assert!(first.is_err(), "{status}: a lost reply must fail the job, not be swallowed");
		let rows = submissions(&store, invoice.id).await;
		assert_eq!(rows.len(), 1);
		assert_eq!(rows[0].1, None, "{status}: NAV said nothing, so there is no verdict to record");

		// The runner re-runs the handler from the top, and it must actually reach NAV again.
		let hits_before = server.received_requests().await.unwrap().len();
		saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
			.await
			.expect_err("the retry reaches NAV and fails the same way");
		assert_eq!(submissions(&store, invoice.id).await.len(), 1, "one row per (invoice, op)");
		assert!(server.received_requests().await.unwrap().len() > hits_before);

		// The sweep covers filings that were never enqueued at all. This one has a row, so the
		// job runner owns it and the sweep must keep its hands off.
		assert!(store.unfiled_invoices(SELLER_ID, 50).await.unwrap().is_empty());
	}
}

/// A 4xx whose body carries no `funcCode` — a WAF page, a CDN block, a misrouted path —
/// is the edge rejecting the request before the invoice service ever saw it. It used to be
/// read as "NAV may hold the invoice" and parked `UNKNOWN`, which `may_send` never resent and
/// `unfiled_invoices` excluded: a statutory filing stranded with no automated way out.
#[tokio::test]
async fn an_unreadable_4xx_stays_retryable_rather_than_parking_the_filing() {
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 403, "<html><body>blocked by WAF</body></html>".to_owned())
		.await;

	let db = TmpDb::new("report-403");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	let err = saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("the job fails so the runner retries it");
	assert_eq!(err.parts().1, "E-NAV-BUSINESS", "{err:?}");
	// A `manageInvoice` fault means nothing was filed, so it must back off rather than
	// terminate the filing. `Nav::cancel_filing` is what ends one that can never succeed.
	assert_eq!(err.retry(), saas_core::Retry::Backoff, "{err:?}");

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
	let server = MockServer::start().await;
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
				saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), id).await.is_err()
			);
		}
	}
	assert!(submissions(&store, invoice.id).await.is_empty(), "nothing was archived");
	assert!(submissions(&store, broken.id).await.is_empty());
	// However many times it failed, neither invoice is retired from the sweep: the give-up
	// bound that did that (`nav.max_filing_attempts`, counted as rows) is gone.
	assert_eq!(store.unfiled_invoices(SELLER_ID, 50).await.unwrap(), vec![invoice.id, broken.id]);
}

/// The credential in the envelope is what NAV authenticates on the wire. An archive that kept
/// it would be a replayable credential per row.
#[tokio::test]
async fn the_archived_request_carries_no_credentials() {
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 503, "<html>down</html>".to_owned()).await;

	let db = TmpDb::new("report-redact");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let _ = saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id).await;

	let archived: String =
		sqlx::query_scalar("SELECT request_xml FROM nav_submissions WHERE invoice_id = ?")
			.bind(invoice.id)
			.fetch_one(store.reader())
			.await
			.unwrap();
	assert!(!archived.contains(TOKEN), "the exchange token was archived");
	assert!(archived.contains("<common:requestId>"), "the archive lost the request's shape");
	assert_eq!(archived.matches("[redacted]").count(), 3, "{archived}");
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

	saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();
	assert_eq!(submissions(&store, invoice.id).await.len(), 1);
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "the stranded poll is re-enqueued");

	store.finish(id, Some(NavVerdict::Done), None, Timestamp::now()).await.unwrap();
	saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
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
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;
	mock(&server, "queryTransactionStatus", 200, aborted_reply()).await;

	let db = TmpDb::new("report-rejected");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();
	let rows = submissions(&store, invoice.id).await;
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].1, None, "sent, but NAV has not answered about the invoice yet");

	saas_nav::job::poll(&app, invoices.as_ref(), nav.as_ref(), rows[0].0)
		.await
		.unwrap();
	assert_eq!(
		submissions(&store, invoice.id).await[0].1.as_deref(),
		Some("REJECTED"),
		"NAV's verdict is not a technical fault"
	);

	// `may_send` refuses it: identical data can only earn the same verdict.
	let hits = server.received_requests().await.unwrap().len();
	saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();
	assert_eq!(submissions(&store, invoice.id).await.len(), 1, "no second filing");
	assert_eq!(server.received_requests().await.unwrap().len(), hits, "NAV was contacted again");

	// It is the operator's problem now, and the hourly sweep is what keeps saying so.
	assert!(store.unfiled_invoices(SELLER_ID, 50).await.unwrap().is_empty());
	assert_eq!(store.awaiting_operator(SELLER_ID).await.unwrap(), 1);
}

/// `sellers.nav_base_url` is `NOT NULL`, sits directly beside `nav_login`, is written by
/// `upsert_seller` — and had no reader anywhere. An operator who configured the seller row for
/// production kept filing into `nav.base_url`'s default, NAV's *test* system, which answers OK
/// and reports nothing statutory.
#[tokio::test]
async fn the_sellers_own_base_url_wins_over_the_setting() {
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;

	let db = TmpDb::new("seller-base-url");
	let (app, store) = setup(&db).await;
	// The setting points somewhere else entirely; the seller row points at the mock.
	point_at_nav(&app, "https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3").await;
	store
		.put_seller(&Seller { nav_base_url: server.uri(), ..seller() })
		.await
		.unwrap();

	let seller = store.seller_by_id(SELLER_ID).await.unwrap().unwrap();
	saas_nav::auth::NavAuth::load(&app, &seller)
		.await
		.unwrap()
		.token_exchange()
		.await
		.expect("the request has to land on the seller's URL, not the setting's");
	assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// The sweep's one remaining job: the at-most-once gap at `saas_invoice::issue`'s
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
		store.unfiled_invoices(SELLER_ID, 50).await.unwrap(),
		vec![issued[2].id, issued[3].id],
		"only invoices with no filing record at all, oldest first"
	);

	saas_nav::job::sweep(&app, invoices.as_ref(), nav.as_ref()).await.unwrap();
	assert_eq!(enqueued(&store, "NAV_REPORT").await, 2);

	// `nav:invoice:{id}` is the dedup key, and it is never released, so a second tick adds
	// nothing however long the filings stay unmade.
	saas_nav::job::sweep(&app, invoices.as_ref(), nav.as_ref()).await.unwrap();
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
	saas_nav::job::sweep(&app, invoices.as_ref(), nav.as_ref()).await.unwrap();
	let status = app.store.job_status_by_key(&key).await.unwrap();
	assert_eq!(status.as_deref(), Some("PENDING"));
	assert!(!saas_nav::job::needs_operator(status.as_deref()), "a queued filing needs nobody");

	// The same invoice, same spent key, on a job that died terminally: that one does.
	sqlx::query("UPDATE jobs SET status = 'FAILED' WHERE dedup_key = ?")
		.bind(&key)
		.execute(store.writer())
		.await
		.unwrap();
	let status = app.store.job_status_by_key(&key).await.unwrap();
	assert!(saas_nav::job::needs_operator(status.as_deref()), "a spent key needs a re-drive");
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

	assert_eq!(store.awaiting_operator(SELLER_ID).await.unwrap(), rejected);
	assert!(
		store.unfiled_invoices(SELLER_ID, 50).await.unwrap().is_empty(),
		"every one of them has a filing record, so none of them is the sweep's business"
	);
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
	use saas_invoice::draft::{Line, NewDraft, Party};
	use saas_invoice::money::Discount;
	use saas_invoice::service_api::Invoices;

	/// Both invoices of the chain, as the `InvoiceData` documents the export writes.
	async fn filed(db: &TmpDb, invoice_discount: Option<Discount>) -> String {
		let (app, _store) = setup(db).await;
		let ctx = Ctx::system("test").with_tenant(TENANT);
		let invoices = Invoices::new(app.clone());

		let issued = invoices
			.issue_now(
				&ctx,
				&NewDraft {
					request_id: None,
					billing_party: Party::TenantDefault,
					lines: vec![Line {
						code: None,
						description: "Tanacsadas".into(),
						unit: "ora".into(),
						qty: Qty(1_000_000),
						unit_price: Some(Money(100_000)),
						vat_code: Some(VatCode::Std27),
						discount: Some(Discount::Percent(1000)),
						discount_description: None,
					}],
					discount: invoice_discount,
					payment_method: None,
					currency: None,
					fulfilment_date: None,
					due_date: None,
					notes: None,
				},
			)
			.await
			.unwrap();
		invoices.storno(&ctx, issued.uid.as_str(), "teszt").await.unwrap();

		let mut out = Vec::new();
		Nav::new(app.clone())
			.audit_export(
				&ctx,
				SELLER_ID,
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
		.execute(store.writer())
		.await
		.unwrap();
	let last_six = issue_at(&store, FEB10 + 1).await;
	assert_eq!(last_six.number.as_deref(), Some("A2026/999999"));
	let seven = issue_at(&store, FEB10 + 2).await;
	assert_eq!(seven.number.as_deref(), Some("A2026/1000000"), "the width has to overflow");

	let ids = vec![first.id, last_six.id, seven.id];
	let selected = store
		.export_ids_by_number(SELLER_ID, "A2026/000001", "A2026/1000000")
		.await
		.unwrap();
	assert_eq!(selected, ids, "the seven-digit invoice fell out of its own range");

	// Both ends are inclusive, and nothing outside them is selected.
	let selected = store
		.export_ids_by_number(SELLER_ID, "A2026/999999", "A2026/1000000")
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
	let server = MockServer::start().await;
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

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());
	let report = || saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), storno.id);

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
}

/// `Nav::invoice` scoped only `Actor::User`; the catch-all arm lumped `Public` in with
/// `Operator` and `System`, so a `Ctx` carrying no tenant — which is every `Ctx::public` —
/// got the unscoped read and could pull any tenant's invoice by uid. Not reachable today
/// (`saas-nav` ships no route bundle), so this is defence in depth: `lookup_tax_number`
/// refuses `Public` outright and the scoped read now needs a tenant the same way.
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
	// actually exercised — `E-AUTH-FORBIDDEN` is `Ctx::tenant`'s "no tenant selected", not a
	// disclosure: it says nothing about whether that uid exists.
	let mut public = Ctx::public("test");
	public.auth_at = Some(Timestamp::now().0);
	let err = nav
		.submit(&public, invoice.uid.as_str())
		.await
		.expect_err("a caller with no tenant has no unscoped read");
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");
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
	let server = MockServer::start().await;
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
	let ctx = Ctx::system("test").with_tenant(TENANT);
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

/// The `nav.software_*` settings and `sellers.nav_login` are as capable of making every
/// request schema-invalid as the seller's address is, and `NAV_REPORT` is unbounded — so a
/// deployment that boots on one of these retries a refusal that can never change, forever.
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
	assert!(saas_nav::auth::check_software_settings(&app).await.is_ok());

	// `CountryCodeType` is `[A-Z]{2}`, and the registry's `range(2,2)` happily takes "hu".
	// `SimpleText50NotBlankType` bounds the name, which the registry does not bound at all.
	// `software_operation` is the one the registry does guard, so it cannot be written badly
	// here — `check_software_settings` still checks it, for a value that arrives some other way.
	for (what, key, value) in [
		("a lowercase dev country", "nav.software_dev_country", "hu".to_owned()),
		("a 60-character software name", "nav.software_name", "a".repeat(60)),
	] {
		let good = app.settings.text(key).await.unwrap();
		app.settings.set(key, &value, None).await.unwrap();
		assert!(
			saas_nav::auth::check_software_settings(&app).await.is_err(),
			"{what} was accepted"
		);
		app.settings.set(key, &good, None).await.unwrap();
	}
	assert!(saas_nav::auth::check_software_settings(&app).await.is_ok());
}

/// The `NAV_REPORT` rows for one invoice, newest last.
async fn report_jobs(store: &SqliteStore, invoice_id: i64) -> Vec<(i64, String, Option<String>)> {
	sqlx::query_as(
		"SELECT id, status, dedup_key FROM jobs WHERE kind = 'NAV_REPORT' AND payload = ? \
		 ORDER BY id",
	)
	.bind(format!(r#"{{"invoiceId":{invoice_id}}}"#))
	.fetch_all(store.reader())
	.await
	.unwrap()
}

/// `submit` filed whatever uid it was handed. A `DRAFT` has no number, so the job failed
/// terminally in `xml::invoice_data` before a submission row was ever opened — having spent
/// `nav:invoice:{id}`, the key `saas_invoice::issue::enqueue_jobs` mints when the invoice is
/// genuinely issued. That enqueue then returned `Ok(None)` and logged nothing, and the invoice
/// was never filed until the reconciliation sweep noticed, hours later.
#[tokio::test]
async fn submitting_a_draft_is_refused_and_spends_no_dedup_key() {
	let db = TmpDb::new("submit-draft");
	let (app, store) = setup(&db).await;
	let ctx = Ctx::system("test").with_tenant(TENANT);

	let draft = store.create_draft(&new_invoice(InvoiceKind::Normal, None)).await.unwrap();
	let err = Nav::new(app.clone())
		.submit(&ctx, draft.uid.as_str())
		.await
		.expect_err("a draft has no number to file");
	assert_eq!(err.parts().1, "E-NAV-NOT-ISSUED");
	assert!(report_jobs(&store, draft.id).await.is_empty(), "no job, so no key was spent");

	// And the key is still there for the issue path, which is the whole point.
	assert!(
		saas_core::job::enqueue(
			&app.store,
			saas_invoice::KIND_NAV_REPORT,
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
	let mut ctx = Ctx::system("test").with_tenant(TENANT);
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
			.execute(store.writer())
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
	ctx.actor = saas_core::ctx::Actor::User { account_id: 1 };
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
	ctx.actor = saas_core::ctx::Actor::System { source: "test" };
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
/// `require_stepup` — and `Nav::submit` derived no permission beyond tenant ownership, so a
/// stolen access token filed with the tax authority for as long as it stayed unexpired.
#[tokio::test]
async fn filing_a_statutory_return_needs_a_freshly_presented_credential() {
	let db = TmpDb::new("submit-stepup");
	let (app, store) = setup(&db).await;
	let invoice = issue_at(&store, JAN15).await;
	let nav = Nav::new(app.clone());
	let mut ctx = Ctx::system("test").with_tenant(TENANT);
	ctx.actor = saas_core::ctx::Actor::User { account_id: 1 };

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
	nav.submit(&Ctx::system("test").with_tenant(TENANT), invoice.uid.as_str())
		.await
		.unwrap();
	assert_eq!(report_jobs(&store, invoice.id).await.len(), 1);

	// And a fresh one gets through — the gate is on the credential's age, not on the actor.
	let fresh = issue_at(&store, JAN15).await;
	ctx.auth_at = Some(Timestamp::now().0);
	nav.submit(&ctx, fresh.uid.as_str()).await.unwrap();
	assert_eq!(report_jobs(&store, fresh.id).await.len(), 1);
}

/// One malformation of the `seller()` fixture, for the [`saas_nav::auth::check_seller`] gate.
type Break = fn(&mut Seller);

#[tokio::test]
async fn a_seller_nav_would_reject_refuses_to_boot() {
	let db = TmpDb::new("seller-gate");
	let (app, store) = setup(&db).await;

	// The fixture is what a correct row looks like.
	assert!(saas_nav::auth::check_seller(&app).await.is_ok());

	// `LoginType` is `[a-zA-Z0-9]{6,15}` and `BankAccountNumberType` is HU 8-8-8, HU 8-8 or an
	// IBAN — `xml::Xml` emits `supplierBankAccountNumber` on every filing when it is set.
	let cases: [(&str, Break); 14] = [
		("a one-character postcode", |s| s.postcode = "1".into()),
		("a hyphenated login", |s| s.nav_login = Some("nav-user".into())),
		("a five-character login", |s| s.nav_login = Some("short".into())),
		("an unpunctuated account", |s| s.bank_account = Some("1234567812345678".into())),
		("a lowercase postcode", |s| s.postcode = "sw1a 1aa".into()),
		("a line break in the name", |s| s.name = "Teszt\nKft.".into()),
		("a lowercase country", |s| s.country = "hu".into()),
		("a 300-character street", |s| s.street = "a".repeat(300)),
		("a tax number of five digits", |s| s.tax_number = "12345".into()),
		// `VatCodeType` is `[1-5]{1}`: a `0` here files nothing, ever.
		("a 9th digit outside 1-5", |s| s.tax_number = "12345678042".into()),
		// `render_number` copies `series_code` verbatim into `invoiceNumber`, a
		// `SimpleText50NotBlankType`, on invoices that are immutable by the time NAV sees them.
		("a line break in the series code", |s| s.series_code = "A\nB".into()),
		("an over-long series code", |s| s.series_code = "A".repeat(39)),
		// The gate used to validate these trimmed while the wire got the column's own bytes:
		// `LoginType` is `[a-zA-Z0-9]{6,15}`, so the trailing space made every `tokenExchange`
		// schema-invalid — and `Retry::Never` on the credential fault means nothing is ever filed.
		("a trailing space in the NAV login", |s| s.nav_login = Some("techuser ".into())),
		("a trailing space in the bank account", |s| {
			s.bank_account = Some("11111111-22222222-33333333 ".into());
		}),
	];
	for (what, break_it) in cases {
		let mut bad = seller();
		break_it(&mut bad);
		store.put_seller(&bad).await.unwrap();
		assert!(saas_nav::auth::check_seller(&app).await.is_err(), "{what} was accepted");
	}

	// And back to the fixture, so this is a gate and not a blanket refusal — with the same two
	// values untrimmed, which are exactly what goes on the wire, and every legal form of the
	// login and the account number.
	for ok in [
		(|s: &mut Seller| s.bank_account = Some("11111111-22222222-33333333".into())) as Break,
		|s: &mut Seller| s.nav_login = Some("navuser1".into()),
		|s: &mut Seller| s.bank_account = Some("12345678-12345678-12345678".into()),
		|s: &mut Seller| s.bank_account = Some("HU42117730161111101800000000".into()),
	] {
		let mut good = seller();
		ok(&mut good);
		store.put_seller(&good).await.unwrap();
		assert!(saas_nav::auth::check_seller(&app).await.is_ok());
	}
}

/// A *retryable* fault recorded nothing at all: `nav.finish` was reached only on the
/// `Retry::Never` branch, so the row kept `verdict IS NULL` **and** `error_code IS NULL`.
/// `awaiting_operator` missed it and `unfiled_invoices` skips an invoice that has any row, so
/// an invoice NAV would never accept retried forever with the archive saying nothing about why.
#[tokio::test]
async fn a_retryable_fault_is_recorded_without_settling_the_filing() {
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	// Not in `BURNS_REQUEST_ID`, so `client::business` classes it `Retry::Backoff` — the
	// majority case, and the one that recorded nothing.
	mock(
		&server,
		"manageInvoice",
		200,
		format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
			 <GeneralErrorResponse{ENVELOPE}>\
			 <common:result><common:funcCode>ERROR</common:funcCode>\
			 <common:errorCode>SCHEMA_VIOLATION</common:errorCode>\
			 <common:message>invoiceNumber is invalid</common:message></common:result>\
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
	let err = saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("a fault is not a filing");
	assert_eq!(err.retry(), saas_core::Retry::Backoff, "{err:?}");

	let (verdict, code): (Option<String>, Option<String>) =
		sqlx::query_as("SELECT verdict, error_code FROM nav_submissions WHERE invoice_id = ?")
			.bind(invoice.id)
			.fetch_one(store.reader())
			.await
			.unwrap();
	assert!(verdict.is_none(), "the filing is still open, so the retry is unchanged");
	assert_eq!(code.as_deref(), Some("SCHEMA_VIOLATION"), "the reason has to be on the row");
	assert_eq!(store.awaiting_operator(SELLER_ID).await.unwrap(), 1);
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
	let server = MockServer::start().await;
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
	let err = saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("a spent requestId can never be filed again under it");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-SPENT");
	assert_eq!(err.retry(), saas_core::Retry::Never, "{err:?}");

	let rows = submissions(&store, invoice.id).await;
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].1.as_deref(), Some("FAILED"), "the filing ended; the row has to say so");
	// `FAILED` is counted by `A-NAV-REJECTED` too, so the invoice is now visible to an operator
	// — and `nav_submissions` rows are never swept, so it stays visible.
	assert_eq!(store.awaiting_operator(SELLER_ID).await.unwrap(), 1);
}

/// `finish(FAILED)` was an unconditional `UPDATE … WHERE id = ?`. `report`'s pre-POST re-read
/// cannot see an *in-flight* sibling, so two attempts under the same `requestId` — the first
/// accepted, the second refused `REQUEST_ID_NOT_UNIQUE` — flipped an accepted filing to
/// `FAILED`. `poll` returns early on any verdict, so nothing ever corrected it: an invoice NAV
/// accepted was archived as a failed filing and counted in `A-NAV-REJECTED` forever.
#[tokio::test]
async fn a_burned_request_id_does_not_overwrite_a_landed_verdict() {
	let server = Arc::new(MockServer::start().await);
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

	let err = saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("this attempt's requestId was refused");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-REUSED");
	sibling.await.unwrap();

	assert_eq!(submissions(&store, invoice.id).await, vec![(id, Some("DONE".to_owned()))]);
	assert_eq!(store.awaiting_operator(SELLER_ID).await.unwrap(), 0);
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

/// `enqueue_poll` discarded `enqueue`'s `Option`, and `nav:poll:{tx}` survives `FAILED`, so a
/// terminated poll could never be restarted: `may_send` got `None`, returned `Ok(false)`, and
/// the operator re-drive that exists for exactly this reported success having done nothing.
#[tokio::test]
async fn a_terminated_poll_is_revived_by_an_operator_redrive() {
	let db = TmpDb::new("poll-revive");
	let (app, store) = setup(&db).await;
	let invoice = issue_at(&store, FEB10).await;
	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

	// With NAV and no verdict, which is the state `may_send` re-enqueues the poll from.
	let id = nav.create_submission(invoice.id, NavOp::Create, "<x/>").await.unwrap().unwrap();
	assert!(nav.set_sent(id, "TX1", 1).await.unwrap());
	saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();

	let payload = saas_nav::job::poll_payload(id);
	let poll_row = async || -> (String, i64) {
		sqlx::query_as("SELECT status, attempts FROM jobs WHERE kind = 'NAV_POLL' AND payload = ?")
			.bind(&payload)
			.fetch_one(store.reader())
			.await
			.unwrap()
	};
	assert_eq!(poll_row().await, ("PENDING".to_owned(), 0));

	// Terminated — `NavAuth::load` on a rotated password, an unparseable reply, or a cancel.
	// The key stays spent, so nothing but a re-drive can bring the row back.
	sqlx::query("UPDATE jobs SET status = 'FAILED', attempts = 4 WHERE kind = 'NAV_POLL'")
		.execute(store.writer())
		.await
		.unwrap();

	saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.unwrap();
	assert_eq!(poll_row().await, ("PENDING".to_owned(), 0), "the stranded poll is revived");
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "revived, not duplicated");
}

/// A `set_sent` write lost to writer saturation makes the retry resend under the same
/// `requestId`, and NAV answers `REQUEST_ID_NOT_UNIQUE` — the one fault that means *NAV may
/// already hold this filing*. Recording `FAILED` there is an archive claiming a filed invoice
/// was never filed, `may_send` then refuses to touch the row ever again, and `A-NAV-REJECTED`
/// advises the operator to correct and re-issue, which would file it twice. The row stays open
/// with the reason on it instead, and `awaiting_operator` is what surfaces it.
#[tokio::test]
async fn a_reused_request_id_leaves_the_filing_open_rather_than_failed() {
	let server = MockServer::start().await;
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
	let err = saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect_err("this requestId was already processed; resending can only be refused");
	assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-REUSED");
	assert_eq!(err.retry(), saas_core::Retry::Never, "{err:?}");

	let (verdict, code): (Option<String>, Option<String>) =
		sqlx::query_as("SELECT verdict, error_code FROM nav_submissions WHERE invoice_id = ?")
			.bind(invoice.id)
			.fetch_one(store.reader())
			.await
			.unwrap();
	assert_eq!(verdict, None, "NAV may hold this filing; it must not be archived as failed");
	assert_eq!(code.as_deref(), Some("REQUEST_ID_NOT_UNIQUE"), "the reason has to be on the row");
	// `unfiled_invoices` skips an invoice that has any row, so without this the open row would
	// be invisible to every counter and sweep in the system.
	assert_eq!(store.awaiting_operator(SELLER_ID).await.unwrap(), 1);
	assert!(store.unfiled_invoices(SELLER_ID, 50).await.unwrap().is_empty());
}

/// Fails every `archive_response` and delegates the rest. The archive is a diagnostic; the
/// reply in hand is the truth, so nothing on either job path may be decided by it.
struct NoArchive(Arc<dyn NavStore>);

#[async_trait::async_trait]
impl NavStore for NoArchive {
	async fn archive_response(&self, _id: i64, _response_xml: &str) -> Result<(), Error> {
		Err(Error::internal("the archive write failed"))
	}
	async fn create_submission(
		&self,
		invoice_id: i64,
		op: NavOp,
		request_xml: &str,
	) -> Result<Option<i64>, Error> {
		self.0.create_submission(invoice_id, op, request_xml).await
	}
	async fn submission(&self, id: i64) -> Result<Option<saas_nav::NavSubmission>, Error> {
		self.0.submission(id).await
	}
	async fn submission_by_invoice(
		&self,
		invoice_id: i64,
	) -> Result<Option<saas_nav::NavSubmission>, Error> {
		self.0.submission_by_invoice(invoice_id).await
	}
	async fn set_sent(&self, id: i64, transaction_id: &str, idx: i64) -> Result<bool, Error> {
		self.0.set_sent(id, transaction_id, idx).await
	}
	async fn record_fault(&self, id: i64, code: &str, message: &str) -> Result<(), Error> {
		self.0.record_fault(id, code, message).await
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
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply()).await;

	let db = TmpDb::new("report-archive-fails");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(NoArchive(Arc::new(store.clone())));
	saas_nav::job::report(&app, invoices.as_ref(), nav.as_ref(), invoice.id)
		.await
		.expect("a failed archive must not fail a filing NAV accepted");

	let row = store.submission_by_invoice(invoice.id).await.unwrap().unwrap();
	assert_eq!(row.transaction_id.as_deref(), Some("TX-VERDICT"));
	assert!(row.response_xml.is_none(), "the archive did fail; that is the point");
	assert_eq!(enqueued(&store, "NAV_POLL").await, 1, "and the poll was still queued");
}

/// The same on the poll path, where `archive_response` was a `?` before the reply was even
/// read: a transient failure discarded a verdict NAV had already given and re-queried it, and
/// one mapping to `Error::internal` — `Retry::Never` — terminated the poll on a filing whose
/// `NAV_REPORT` row is already `DONE`, so nothing could ever re-enqueue it.
#[tokio::test]
async fn a_verdict_is_settled_even_when_the_archive_fails() {
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	mock(&server, "queryTransactionStatus", 200, done_reply()).await;

	let db = TmpDb::new("poll-archive-fails");
	let (app, store) = setup(&db).await;
	point_at_nav(&app, &server.uri()).await;
	let invoice = issue_at(&store, FEB10).await;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(NoArchive(Arc::new(store.clone())));
	let id = store
		.create_submission(invoice.id, NavOp::Create, "<x/>")
		.await
		.unwrap()
		.unwrap();
	assert!(store.set_sent(id, "TX-VERDICT", 1).await.unwrap());

	saas_nav::job::poll(&app, invoices.as_ref(), nav.as_ref(), id)
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

// vim: ts=4
