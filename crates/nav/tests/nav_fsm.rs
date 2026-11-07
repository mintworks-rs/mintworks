// SPDX-License-Identifier: MPL-2.0
//! The NAV filing state machine end to end: every failure of `claude-docs/nav-filing-design.md`
//! §9, every cell of §12 and the scenarios around them, against a real store and a `wiremock` NAV.
//!
//! A crash is simulated by driving the store transitions the dead run would have made and then
//! calling the job entry point that runs next. Nothing here has a worker, so job rows stay as
//! written: a test moves `jobs.status` and `jobs.run_at` itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;
use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use mintworks_core::{
	App, AppBuilder,
	config::Config,
	ctx::Ctx,
	ids::SellerId,
	job::{Job, Next, enqueue},
	prelude::*,
};
use mintworks_invoice::{
	KIND_NAV_REPORT,
	draft::Priced,
	invoice_job_key, invoice_job_payload,
	store::{
		BuyerSnapshot, Invoice, InvoiceDocument, InvoiceKind, InvoiceStore, InvoiceVatGroup,
		IssueInvoice, NewInvoice, NewInvoiceLine, PartyKind, PaymentMethod, Seller,
		SellerVersionPatch,
	},
	vat::VatCode,
};
use mintworks_nav::{
	BatchState, E_NAV_SUBMISSION_STATE, FilingState, Nav, NavBatch, NavFiling, NavOp, NavStore,
	NavVerdict, RowState, alerts,
	client::INVOICE_NUMBER_NOT_UNIQUE,
	job::{
		self, BATCH_BYTE_BUDGET, E_NAV_OVERSIZE, E_NAV_POLL_STALE, E_NAV_RECONCILE_UNPROVEN,
		HELD_RECHECK_SECS, KIND_NAV_POLL, KIND_NAV_RECONCILE, KIND_NAV_SEND, adopt_key,
		batch_payload, poll_key, reconcile_key, send_key,
	},
	routes::filing_json,
};
use mintworks_store_sqlite::SqliteStore;
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{body_string_contains, method, path},
};

const ORG: i64 = 1;

/// The fixture's one seller. `put_seller` does not autoincrement, so the id is chosen here.
const SELLER: i64 = 1;

/// A seller minted on the SHARED org [`ORG`] rather than on root.
const TENANT: i64 = 2;

/// The root org, after `setup` renumbers it. The seller belongs to the *root* because the
/// ctx-less NAV paths resolve it through `auth::deployment_seller`, which walks up from there.
const ROOT: i64 = 0;

/// The version `setup`'s `seed_seller` publishes — the first row `seller_versions` ever gets.
const SELLER_VER: i64 = 1;

/// A `software` block `invoiceApi.xsd` accepts.
const SOFTWARE_SETTINGS: [(&str, &str); 6] = [
	("nav.software_id", "HU12345678MINTWRKS"),
	("nav.software_name", "Mintworks"),
	("nav.software_operation", "LOCAL_SOFTWARE"),
	("nav.software_main_version", "0.1"),
	("nav.software_dev_name", "Teszt Kft."),
	("nav.software_dev_contact", "dev@e.st"),
];

const ISSUED_AT: i64 = 1_770_724_800; // 2026-02-10T12:00:00Z

const HOUR: i64 = 3600;
const DAY: i64 = 86_400;

/// `nav.fault_alert_after`'s default, which is what `alerts` reads here.
const GRACE: i64 = HOUR;

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, so the two pools must be over a file.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-nav-fsm-test-{}-{name}", std::process::id()));
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
/// foreign keys demand: one account, one org, seller 1. No workers, so nothing races an assertion.
async fn setup(db: &TmpDb) -> (App, SqliteStore) {
	let store = SqliteStore::open(&db.config()).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	let nav: Arc<dyn NavStore> = Arc::new(store.clone());

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
	seed_seller(&store, &seller(), &seller_version()).await;

	(app, store)
}

/// The operational row plus one published version: `issue` refuses a seller with no `CURRENT`
/// version, and `xml::supplier_info` reads only the version.
async fn seed_seller(store: &SqliteStore, seller: &Seller, version: &SellerVersionPatch) {
	store.put_seller(seller).await.unwrap();
	store.save_seller_version_draft(seller.id, version).await.unwrap();
	store
		.publish_seller_version(seller.id, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();
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

/// Seeds [`TENANT`] with no NAV connection and account 1 its org's Admin; returns the version it
/// published.
async fn tenant(store: &SqliteStore) -> i64 {
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
	store.current_seller_version(TENANT).await.unwrap().unwrap().seller_ver
}

/// Account 1 acting in [`ORG`] with a freshly presented credential.
fn tenant_admin() -> Ctx {
	let mut ctx = Ctx::system("test").with_org(ORG);
	ctx.actor = mintworks_core::ctx::Actor::User { account_id: 1 };
	ctx.auth_at = Some(Timestamp::now().0);
	ctx
}

/// Passes `require_operator`, step-up and `retry`'s role check, so it can run every §12 cell.
fn operator() -> Ctx {
	Ctx::system("test").with_org(ORG)
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

/// The `invoice_documents` row `job::report` requires of an electronic filing.
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

/// An invoice of `seller_id` issued with one line of `description`. It has no `NAV_REPORT` row, so
/// to the sweep it is an orphan until a test plants one.
async fn issue_lined(
	store: &SqliteStore,
	seller_id: i64,
	seller_ver: i64,
	description: String,
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
	let series_code = store.seller_by_id(seller_id).await.unwrap().unwrap().series_code;
	let invoice = store
		.issue(
			draft.id,
			&IssueInvoice {
				seller_ver,
				series_code,
				lines: vec![NewInvoiceLine { description, ..line(NET) }],
				..issue_input(draft.id, NET, ISSUED_AT)
			},
			draft.version,
		)
		.await
		.unwrap();
	put_pdf(store, &invoice).await;
	invoice
}

async fn issue(store: &SqliteStore) -> Invoice {
	issue_lined(store, SELLER, SELLER_VER, "Tanacsadas".into()).await
}

/// AES-128-ECB, so exactly 16 bytes (`crypto::decrypt_exchange_token`).
const EXCHANGE_KEY: &[u8; 16] = b"0123456789abcdef";
/// Exactly one AES block, so the reply carries no padding block to strip.
const TOKEN: &str = "TOKENTOKENTOKEN1";

const ENVELOPE: &str = concat!(
	r#" xmlns="http://schemas.nav.gov.hu/OSA/3.0/api""#,
	r#" xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common""#,
);

/// The per-invoice part of an `ABORTED` result that means NAV already holds the number.
const NUMBER_TAKEN: &str = "<technicalValidationMessages>\
	<validationResultCode>ERROR</validationResultCode>\
	<validationErrorCode>INVOICE_NUMBER_NOT_UNIQUE</validationErrorCode>\
	<message>duplicate invoice number</message>\
	</technicalValidationMessages>";

/// Points the `App` at `base_url` and seeds what `NavAuth::load` reads.
async fn point_at_nav(app: &App, base_url: &str) {
	app.settings.set("nav.base_url", base_url, None).await.unwrap();
	for (key, value) in SOFTWARE_SETTINGS {
		app.settings.set(key, value, None).await.unwrap();
	}
	app.secrets.set("nav.tech_password", b"tech-pw", None).await.unwrap();
	app.secrets.set("nav.sign_key", b"sign-key", None).await.unwrap();
	app.secrets.set("nav.exchange_key", EXCHANGE_KEY, None).await.unwrap();
}

/// A NAV that answers `tokenExchange` and, until a test mounts more, nothing else.
async fn nav_server(app: &App) -> MockServer {
	let server = MockServer::builder().start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;
	point_at_nav(app, &server.uri()).await;
	server
}

async fn mock(server: &MockServer, operation: &str, status: u16, body: String) {
	Mock::given(method("POST"))
		.and(path(format!("/{operation}")))
		.respond_with(ResponseTemplate::new(status).set_body_string(body))
		.mount(server)
		.await;
}

/// [`mock`] for the first matching request only. Mocks are tried in mount order, so the one-shot
/// goes up first.
async fn mock_once(server: &MockServer, operation: &str, status: u16, body: String) {
	Mock::given(method("POST"))
		.and(path(format!("/{operation}")))
		.respond_with(ResponseTemplate::new(status).set_body_string(body))
		.up_to_n_times(1)
		.mount(server)
		.await;
}

/// [`mock`] for requests carrying `value` as a whole element, so `TX-1` does not answer `TX-12`.
async fn mock_for(server: &MockServer, operation: &str, value: &str, body: String) {
	Mock::given(method("POST"))
		.and(path(format!("/{operation}")))
		.and(body_string_contains(format!(">{value}<")))
		.respond_with(ResponseTemplate::new(200).set_body_string(body))
		.mount(server)
		.await;
}

/// The bodies the mock saw for one operation, oldest first.
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

/// What NAV puts in `encodedExchangeToken`: AES-128-ECB under the exchange key, base64'd.
fn token_reply() -> String {
	use aes::cipher::{Block, BlockCipherEncrypt, KeyInit};

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

fn manage_ok_reply(transaction_id: &str) -> String {
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <ManageInvoiceResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <transactionId>{transaction_id}</transactionId>\
		 </ManageInvoiceResponse>"
	)
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

/// One page of `queryTransactionList`.
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

/// `returnOriginalRequest=true`: each result echoes the base64 `invoiceData` it was filed with.
fn original_request_reply(filed: &[(i64, &str)]) -> String {
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

/// `queryInvoiceData` finding a filing. `transactionId` and `index` are optional in
/// `AuditDataType`, which is what the refusal cases need.
fn invoice_data_reply(
	data: &str,
	transaction_id: Option<&str>,
	index: Option<i64>,
	compressed: bool,
) -> String {
	let mut audit = String::new();
	if let Some(id) = transaction_id {
		let _ = write!(audit, "<transactionId>{id}</transactionId>");
	}
	if let Some(index) = index {
		let _ = write!(audit, "<index>{index}</index>");
	}
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryInvoiceDataResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <invoiceDataResult><invoiceData>{data}</invoiceData><auditData>{audit}</auditData>\
		 <compressedContentIndicator>{compressed}</compressedContentIndicator>\
		 </invoiceDataResult></QueryInvoiceDataResponse>"
	)
}

/// `queryInvoiceData` finding nothing: `invoiceDataResult` is optional.
fn no_invoice_data_reply() -> String {
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryInvoiceDataResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 </QueryInvoiceDataResponse>"
	)
}

fn now() -> Timestamp {
	Timestamp::now()
}

fn ago(secs: i64) -> Timestamp {
	Timestamp(Timestamp::now().0 - secs)
}

/// What [`post_at`] archives for the row at `idx`, so an adoption test can answer with those bytes.
fn filed_data(idx: usize) -> String {
	B64.encode(format!("<InvoiceData>filed at {idx}</InvoiceData>"))
}

/// The archive of `rows` operations, in the shape `job::operation_slice` reads.
fn request_xml(rows: usize) -> String {
	let mut body = String::new();
	for idx in 1..=rows {
		let _ = write!(
			body,
			"<invoiceOperation><index>{idx}</index>\
			 <invoiceOperation>CREATE</invoiceOperation>\
			 <invoiceData>{}</invoiceData></invoiceOperation>",
			filed_data(idx)
		);
	}
	format!("<invoiceOperations>{body}</invoiceOperations>")
}

/// The `<invoiceData>` of the batch's archive at `idx`, as `begin_post` wrote it.
async fn archived_data(store: &SqliteStore, batch_id: i64, idx: usize) -> String {
	let xml = store.batch_archive(batch_id).await.unwrap().unwrap().request_xml.unwrap();
	let tail = xml.split("<invoiceData>").nth(idx).unwrap();
	tail.split("</invoiceData>").next().unwrap().to_owned()
}

async fn fresh(store: &SqliteStore, uid: &str) -> NavBatch {
	store.batch_by_uid(uid).await.unwrap().unwrap()
}

async fn filing(store: &SqliteStore, invoice: &Invoice) -> NavFiling {
	store.filing_by_invoice(invoice.id).await.unwrap().unwrap()
}

async fn state_of(store: &SqliteStore, invoice_id: i64) -> FilingState {
	let found = store.filing_by_invoice(invoice_id).await.unwrap();
	found.as_ref().map_or(FilingState::None, NavFiling::state)
}

/// The batch the invoice's newest row sits in.
async fn batch_of(store: &SqliteStore, invoice: &Invoice) -> NavBatch {
	filing(store, invoice).await.batch.unwrap()
}

async fn row_ids(store: &SqliteStore, batch_id: i64) -> Vec<i64> {
	store.batch_rows(batch_id).await.unwrap().iter().map(|r| r.id).collect()
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

/// B1, B2: the write-ahead made at `at`, rows taking `idx` 1.. in order.
async fn post_at(store: &SqliteStore, invoices: &[&Invoice], at: Timestamp) -> NavBatch {
	let batch = claim(store, invoices).await;
	let rows: Vec<(i64, i64)> = row_ids(store, batch.id).await.into_iter().zip(1..).collect();
	assert!(store.begin_post(batch.id, &request_xml(rows.len()), &rows, at).await.unwrap());
	fresh(store, batch.uid.as_str()).await
}

async fn post(store: &SqliteStore, invoices: &[&Invoice]) -> NavBatch {
	post_at(store, invoices, now()).await
}

/// B4 with no fault.
async fn doubted(store: &SqliteStore, invoices: &[&Invoice]) -> NavBatch {
	let batch = post(store, invoices).await;
	assert!(store.mark_in_doubt(batch.id, None, now()).await.unwrap());
	fresh(store, batch.uid.as_str()).await
}

/// B9: rows stay `ACTIVE` in a `HELD` batch.
async fn held(store: &SqliteStore, invoices: &[&Invoice]) -> NavBatch {
	let batch = doubted(store, invoices).await;
	assert!(
		store
			.hold_batch(batch.id, E_NAV_RECONCILE_UNPROVEN, "unproved", now())
			.await
			.unwrap()
	);
	fresh(store, batch.uid.as_str()).await
}

async fn sent_at(
	store: &SqliteStore,
	invoices: &[&Invoice],
	transaction_id: &str,
	at: Timestamp,
) -> NavBatch {
	let batch = post_at(store, invoices, at).await;
	assert!(store.set_sent(batch.id, transaction_id).await.unwrap());
	fresh(store, batch.uid.as_str()).await
}

async fn sent(store: &SqliteStore, invoices: &[&Invoice], transaction_id: &str) -> NavBatch {
	sent_at(store, invoices, transaction_id, now()).await
}

/// R3 on every row, NAV having answered at `done_at`: rows `HELD`, unchecked, the batch `CLOSED`.
async fn number_taken(
	store: &SqliteStore,
	invoices: &[&Invoice],
	transaction_id: &str,
	done_at: Timestamp,
) -> NavBatch {
	let batch = sent(store, invoices, transaction_id).await;
	for row in row_ids(store, batch.id).await {
		assert!(
			store
				.finish_held(row, INVOICE_NUMBER_NOT_UNIQUE, "duplicate", done_at)
				.await
				.unwrap()
		);
	}
	fresh(store, batch.uid.as_str()).await
}

async fn report(app: &App, store: &SqliteStore, invoice_id: i64) -> ClResult<Next> {
	job::report(app, store, store, invoice_id).await
}

async fn send(app: &App, store: &SqliteStore, uid: &str) -> ClResult<Next> {
	job::send(app, store, store, uid).await
}

async fn reconcile(app: &App, store: &SqliteStore, uid: &str) -> ClResult<()> {
	job::reconcile(app, store, store, uid).await
}

async fn poll(app: &App, store: &SqliteStore, uid: &str) -> ClResult<Next> {
	let claimed =
		Job { id: 1, kind: KIND_NAV_POLL.to_owned(), payload: batch_payload(uid), attempts: 1 };
	job::poll(app, store, store, &claimed, uid).await
}

async fn adopt(app: &App, store: &SqliteStore, uid: &str) -> ClResult<Next> {
	job::adopt(app, store, store, uid).await
}

async fn sweep(app: &App, store: &SqliteStore) {
	job::sweep(app, store).await.unwrap();
}

/// `(status, run_at)` of the job under `key`.
async fn job_of(store: &SqliteStore, key: &str) -> Option<(String, i64)> {
	sqlx::query_as("SELECT status, run_at FROM jobs WHERE dedup_key = ?")
		.bind(key)
		.fetch_optional(store.read_pool())
		.await
		.unwrap()
}

async fn assert_pending(store: &SqliteStore, key: &str) {
	let (status, _) = job_of(store, key).await.unwrap_or_else(|| panic!("no job under {key}"));
	assert_eq!(status, "PENDING", "{key}");
}

async fn enqueued(store: &SqliteStore, kind: &str) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = ?")
		.bind(kind)
		.fetch_one(store.read_pool())
		.await
		.unwrap()
}

async fn batch_count(store: &SqliteStore) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM nav_batches")
		.fetch_one(store.read_pool())
		.await
		.unwrap()
}

/// A job row as an earlier run left it, written the way `job::ensure` would have.
async fn plant(app: &App, kind: &str, payload: &str, key: &str, run_at: i64) {
	enqueue(&app.store, kind, payload, Some(key), Timestamp(run_at))
		.await
		.unwrap()
		.expect("a fresh dedup key");
}

async fn set_job_status(store: &SqliteStore, key: &str, status: &str) {
	sqlx::query("UPDATE jobs SET status = ? WHERE dedup_key = ?")
		.bind(status)
		.bind(key)
		.execute(store.write_pool())
		.await
		.unwrap();
}

async fn alert_count(app: &App, code: &str) -> i64 {
	let raised = alerts(app.clone()).await.unwrap();
	raised.iter().find(|a| a.code == code).map_or(0, |a| a.count)
}

fn number(invoice: &Invoice) -> &str {
	invoice.number.as_deref().unwrap()
}

// ---- design §9: one test per failure ---------------------------------------------------------

#[tokio::test]
async fn crash_after_claim_before_send_is_queued() {
	let db = TmpDb::new("crash-claim");
	let (app, store) = setup(&db).await;
	let invoice = issue(&store).await;
	let batch = claim(&store, &[&invoice]).await;

	sweep(&app, &store).await;

	assert_pending(&store, &send_key(batch.uid.as_str())).await;
	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Claimed);
}

/// The re-run must never POST: NAV may hold the batch, and only reconciliation may say otherwise.
#[tokio::test]
async fn crash_after_begin_post_before_the_post() {
	let db = TmpDb::new("crash-post");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&[])).await;
	let invoice = issue(&store).await;
	let batch = post(&store, &[&invoice]).await;
	let uid = batch.uid.as_str();

	assert_eq!(send(&app, &store, uid).await.unwrap(), Next::Done);

	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::InDoubt);
	let (status, run_at) = job_of(&store, &reconcile_key(uid)).await.unwrap();
	assert_eq!(status, "PENDING");
	assert_eq!(run_at, batch.posted_at.unwrap().0 + 300, "§1.9.2's five minutes run from the POST");
	assert!(requests(&server, "manageInvoice").await.is_empty());

	reconcile(&app, &store, uid).await.unwrap();

	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Claimed, "B8");
	assert_pending(&store, &send_key(uid)).await;
}

/// The supervisor's half of the row above: past the lease it does what `NAV_SEND`'s re-run does,
/// and inside the lease a worker may still hold the POST.
#[tokio::test]
async fn a_dead_post_goes_in_doubt_past_its_lease_and_a_young_one_is_left() {
	let db = TmpDb::new("crash-lease");
	let (app, store) = setup(&db).await;
	let (old, young) = (issue(&store).await, issue(&store).await);
	let dead = post_at(&store, &[&old], ago(DAY)).await;
	let live = post(&store, &[&young]).await;

	sweep(&app, &store).await;

	assert_eq!(fresh(&store, dead.uid.as_str()).await.state, BatchState::InDoubt);
	assert_pending(&store, &reconcile_key(dead.uid.as_str())).await;
	assert_eq!(fresh(&store, live.uid.as_str()).await.state, BatchState::Posting);
	assert_eq!(job_of(&store, &reconcile_key(live.uid.as_str())).await, None);
}

#[tokio::test]
async fn crash_after_nav_took_it_before_set_sent() {
	let db = TmpDb::new("crash-took");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let invoice = issue(&store).await;
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&["TX-LOST"])).await;
	let echoed = original_request_reply(&[(1, number(&invoice))]);
	mock(&server, "queryTransactionStatus", 200, echoed).await;
	let batch = post(&store, &[&invoice]).await;
	let uid = batch.uid.as_str();

	send(&app, &store, uid).await.unwrap();
	reconcile(&app, &store, uid).await.unwrap();

	let found = fresh(&store, batch.uid.as_str()).await;
	assert_eq!(found.state, BatchState::Sent, "B7");
	assert_eq!(found.transaction_id.as_deref(), Some("TX-LOST"));
	assert_pending(&store, &poll_key(uid)).await;
	assert!(requests(&server, "manageInvoice").await.is_empty());
}

#[tokio::test]
async fn crash_after_set_sent_before_the_poll_is_queued() {
	let db = TmpDb::new("crash-sent");
	let (app, store) = setup(&db).await;
	let (first, second) = (issue(&store).await, issue(&store).await);
	let by_send = sent(&store, &[&first], "TX-1").await;
	let by_sweep = sent(&store, &[&second], "TX-2").await;

	assert_eq!(send(&app, &store, by_send.uid.as_str()).await.unwrap(), Next::Done);
	assert_pending(&store, &poll_key(by_send.uid.as_str())).await;
	assert_eq!(job_of(&store, &poll_key(by_sweep.uid.as_str())).await, None);

	sweep(&app, &store).await;
	assert_pending(&store, &poll_key(by_sweep.uid.as_str())).await;
}

#[tokio::test]
async fn crash_after_spend_before_the_new_batch_is_queued() {
	let db = TmpDb::new("crash-spend");
	let (app, store) = setup(&db).await;
	let invoice = issue(&store).await;
	let batch = post(&store, &[&invoice]).await;
	let next = store.spend_batch(batch.id, "FORBIDDEN", "no", now()).await.unwrap().unwrap();

	sweep(&app, &store).await;

	assert_pending(&store, &send_key(next.uid.as_str())).await;
	assert_eq!(job_of(&store, &send_key(batch.uid.as_str())).await, None, "closed owes nothing");
}

#[tokio::test]
async fn crash_after_clear_doubt_before_send_is_revived() {
	let db = TmpDb::new("crash-doubt");
	let (app, store) = setup(&db).await;
	let invoice = issue(&store).await;
	let batch = doubted(&store, &[&invoice]).await;
	let (uid, key) = (batch.uid.as_str(), send_key(batch.uid.as_str()));
	plant(&app, KIND_NAV_SEND, &batch_payload(uid), &key, now().0).await;
	set_job_status(&store, &key, "DONE").await;
	assert!(store.clear_doubt(batch.id).await.unwrap());

	sweep(&app, &store).await;

	assert_pending(&store, &key).await;
}

#[tokio::test]
async fn a_cancel_beating_assembly_makes_the_claim_stand_down() {
	let db = TmpDb::new("cancel-claim");
	let (app, store) = setup(&db).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();
	let (cancelled, other) = (issue(&store).await, issue(&store).await);
	let nav = Nav::new(app.clone());

	nav.cancel_filing(&operator(), cancelled.uid.as_str()).await.unwrap();

	let refused = store.claim_batch(SELLER, NavOp::Create, &[cancelled.id], true).await.unwrap();
	assert!(refused.is_none(), "a CANCELLED row refuses the create-only claim");
	assert_eq!(batch_count(&store).await, 0, "an emptied claim leaves no batch");
	assert_eq!(report(&app, &store, cancelled.id).await.unwrap(), Next::Done);
	assert_eq!(batch_count(&store).await, 0);

	report(&app, &store, other.id).await.unwrap();
	assert_eq!(store.batch_rows(batch_of(&store, &other).await.id).await.unwrap().len(), 1);
	assert_eq!(state_of(&store, cancelled.id).await, FilingState::Cancelled);
}

/// Whichever reaches the store first wins: the POST refuses the cancel, or the cancel refuses a
/// write-ahead built before it, leaving the other rows of the batch to go.
#[tokio::test]
async fn a_cancel_racing_the_first_post_has_one_loser() {
	let db = TmpDb::new("cancel-post");
	let (app, store) = setup(&db).await;
	let (posted, cancelled, kept) = (issue(&store).await, issue(&store).await, issue(&store).await);
	let nav = Nav::new(app.clone());

	post(&store, &[&posted]).await;
	let err = nav.cancel_filing(&operator(), posted.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-FILING-IN-FLIGHT");
	assert!(!store.cancel_filing(posted.id, NavOp::Create, now()).await.unwrap());

	let batch = claim(&store, &[&cancelled, &kept]).await;
	let rows: Vec<(i64, i64)> = row_ids(&store, batch.id).await.into_iter().zip(1..).collect();
	nav.cancel_filing(&operator(), cancelled.uid.as_str()).await.unwrap();

	assert!(!store.begin_post(batch.id, &request_xml(2), &rows, now()).await.unwrap());
	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Claimed);
	assert_eq!(state_of(&store, kept.id).await, FilingState::Claimed, "no other invoice moves");
	assert!(
		store
			.begin_post(batch.id, &request_xml(1), &[(rows[1].0, 1)], now())
			.await
			.unwrap()
	);
}

#[tokio::test]
async fn a_finished_job_whose_row_was_released_is_found_as_an_orphan() {
	let db = TmpDb::new("orphan-released");
	let (app, store) = setup(&db).await;
	let invoice = issue(&store).await;
	let key = invoice_job_key(invoice.id);
	plant(&app, KIND_NAV_REPORT, &invoice_job_payload(invoice.id), &key, now().0).await;
	set_job_status(&store, &key, "DONE").await;
	let batch = claim(&store, &[&invoice]).await;
	assert!(store.release_row(row_ids(&store, batch.id).await[0]).await.unwrap());

	sweep(&app, &store).await;

	assert_pending(&store, &key).await;
	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Closed);
}

/// The whole of §9's "late resend": a lost reply, reconcile finding nothing, a resend past the
/// `requestId` window, NAV aborting on the number, and adoption of the first filing.
#[tokio::test]
async fn a_late_resend_past_the_window_ends_in_adoption_not_a_second_filing() {
	let db = TmpDb::new("late-resend");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	mock_once(&server, "manageInvoice", 500, String::new()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply("TX-RESEND")).await;
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&[])).await;
	let aborted = status_results(&[(1, "ABORTED", NUMBER_TAKEN)]);
	mock(&server, "queryTransactionStatus", 200, aborted).await;
	let invoice = issue(&store).await;

	report(&app, &store, invoice.id).await.unwrap();
	let batch = batch_of(&store, &invoice).await;
	let uid = batch.uid.as_str();
	send(&app, &store, uid)
		.await
		.expect_err("a lost reply is the runner's to retry");
	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::InDoubt);
	reconcile(&app, &store, uid).await.unwrap();
	send(&app, &store, uid).await.unwrap();
	assert_eq!(poll(&app, &store, uid).await.unwrap(), Next::Done);

	assert_eq!(state_of(&store, invoice.id).await, FilingState::Held, "R3");
	assert_pending(&store, &adopt_key(uid)).await;

	let ours = archived_data(&store, batch.id, 1).await;
	let found = invoice_data_reply(&ours, Some("TX-FIRST"), Some(1), false);
	mock(&server, "queryInvoiceData", 200, found).await;
	assert_eq!(adopt(&app, &store, uid).await.unwrap(), Next::Done);

	let adopted = batch_of(&store, &invoice).await;
	assert_eq!(adopted.state, BatchState::Sent);
	assert_eq!(adopted.transaction_id.as_deref(), Some("TX-FIRST"));
	assert_eq!(requests(&server, "manageInvoice").await.len(), 2, "the lost POST and the resend");
}

#[tokio::test]
async fn a_crash_during_adoption_repeats_only_the_reads() {
	let db = TmpDb::new("crash-adopt");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let found = invoice_data_reply(&filed_data(1), Some("TX-FIRST"), Some(1), false);
	mock(&server, "queryInvoiceData", 200, found).await;
	let invoice = issue(&store).await;
	let batch = number_taken(&store, &[&invoice], "TX-RESEND", now()).await;
	let uid = batch.uid.as_str();

	sweep(&app, &store).await;
	assert_pending(&store, &adopt_key(uid)).await;

	assert_eq!(adopt(&app, &store, uid).await.unwrap(), Next::Done);
	assert_eq!(adopt(&app, &store, uid).await.unwrap(), Next::Done);
	assert_eq!(requests(&server, "queryInvoiceData").await.len(), 1, "adopted rows are not HELD");
	assert_eq!(state_of(&store, invoice.id).await, FilingState::Sent);
}

#[tokio::test]
async fn a_number_on_file_from_other_software_stays_held_for_a_person() {
	let db = TmpDb::new("other-software");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let theirs = B64.encode("<InvoiceData>another program</InvoiceData>");
	let found = invoice_data_reply(&theirs, Some("TX-THEIRS"), Some(1), false);
	mock(&server, "queryInvoiceData", 200, found).await;
	let invoice = issue(&store).await;
	let batch = number_taken(&store, &[&invoice], "TX-RESEND", now()).await;

	assert_eq!(adopt(&app, &store, batch.uid.as_str()).await.unwrap(), Next::Done);

	let row = filing(&store, &invoice).await.row;
	assert_eq!(row.state, RowState::Held);
	assert!(row.adopt_checked_at.is_some());
	assert_eq!(store.batch_by_transaction("TX-THEIRS").await.unwrap().map(|b| b.id), None);
	assert_eq!(alert_count(&app, "A-NAV-REJECTED").await, 1);
}

/// §9's "Poll ends `Retry::Never`".
#[tokio::test]
async fn the_supervisor_revives_a_failed_poll() {
	let db = TmpDb::new("poll-failed");
	let (app, store) = setup(&db).await;
	let invoice = issue(&store).await;
	let batch = sent(&store, &[&invoice], "TX-1").await;
	let (uid, key) = (batch.uid.as_str(), poll_key(batch.uid.as_str()));
	plant(&app, KIND_NAV_POLL, &batch_payload(uid), &key, now().0).await;
	set_job_status(&store, &key, "FAILED").await;

	sweep(&app, &store).await;

	assert_pending(&store, &key).await;
	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Sent);
}

/// The marker lands on a live poll too: one that keeps faulting is still `PENDING`.
#[tokio::test]
async fn the_supervisor_marks_a_stale_sent_and_leaves_a_young_one() {
	let db = TmpDb::new("poll-stale");
	let (app, store) = setup(&db).await;
	let (old, young) = (issue(&store).await, issue(&store).await);
	let stale = sent_at(&store, &[&old], "TX-OLD", ago(2 * DAY)).await;
	let recent = sent(&store, &[&young], "TX-YOUNG").await;
	let (uid, key) = (stale.uid.as_str(), poll_key(stale.uid.as_str()));
	plant(&app, KIND_NAV_POLL, &batch_payload(uid), &key, now().0 + HOUR).await;

	sweep(&app, &store).await;

	let marked = fresh(&store, stale.uid.as_str()).await;
	assert_eq!(marked.state, BatchState::Sent, "the poll continues");
	assert_eq!(marked.error_code.as_deref(), Some(E_NAV_POLL_STALE));
	assert_eq!(fresh(&store, recent.uid.as_str()).await.error_code, None);
	assert_eq!(alert_count(&app, "A-NAV-REJECTED").await, 1);
}

#[tokio::test]
async fn a_held_filing_keeps_the_alert_on_until_a_person_acts() {
	let db = TmpDb::new("held-forever");
	let (app, store) = setup(&db).await;
	let invoice = issue(&store).await;
	let batch = held(&store, &[&invoice]).await;

	sweep(&app, &store).await;

	assert_eq!(
		fresh(&store, batch.uid.as_str()).await.state,
		BatchState::Held,
		"nothing automatic moves it"
	);
	assert_eq!(enqueued(&store, KIND_NAV_SEND).await, 0);
	assert_eq!(enqueued(&store, KIND_NAV_RECONCILE).await, 0);
	assert_eq!(alert_count(&app, "A-NAV-REJECTED").await, 1);

	let nav = Nav::new(app.clone());
	nav.confirm_not_filed(&operator(), invoice.uid.as_str(), "checked")
		.await
		.unwrap();
	assert_eq!(alert_count(&app, "A-NAV-REJECTED").await, 0);
}

// ---- sending ---------------------------------------------------------------------------------

/// B6: a 500 `FORBIDDEN` burns the one `requestId`, so the rows move together under a new one and
/// nothing more of this seller reaches NAV.
#[tokio::test]
async fn a_500_forbidden_spends_one_id_holds_the_seller_and_refiles() {
	let db = TmpDb::new("forbidden");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	mock(&server, "manageInvoice", 500, error_reply("FORBIDDEN", "not authorised")).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();
	let (first, second) = (issue(&store).await, issue(&store).await);

	report(&app, &store, first.id).await.unwrap();
	let spent = batch_of(&store, &first).await;
	send(&app, &store, spent.uid.as_str())
		.await
		.expect_err("a spent id is an error");

	let closed = fresh(&store, spent.uid.as_str()).await;
	assert_eq!(closed.state, BatchState::Closed);
	assert_eq!(closed.error_code.as_deref(), Some("FORBIDDEN"));

	let next = batch_of(&store, &first).await;
	assert_ne!(next.id, spent.id);
	assert_eq!(next.state, BatchState::Claimed);
	assert_eq!(batch_of(&store, &second).await.id, next.id, "the rows refile into one batch");
	assert_pending(&store, &send_key(next.uid.as_str())).await;

	let history = store.filings_by_invoice(first.id).await.unwrap();
	assert_eq!(history.len(), 2);
	let old = &history[1].row;
	assert_eq!(old.state, RowState::Failed);
	assert_eq!(old.verdict, Some(NavVerdict::Failed));
	assert_eq!(old.error_code.as_deref(), Some("FORBIDDEN"));
	assert!(old.resolved_at.is_some());

	assert_eq!(store.seller_hold(SELLER).await.unwrap().unwrap().code, "FORBIDDEN");
	assert_eq!(alert_count(&app, "A-NAV-SELLER-HELD").await, 1);

	let before = now().0;
	let Next::Again { at } = send(&app, &store, next.uid.as_str()).await.unwrap() else {
		panic!("a held seller's batch waits");
	};
	assert!(at.0 >= before + HELD_RECHECK_SECS);
	assert_eq!(requests(&server, "manageInvoice").await.len(), 1, "no second id is spent");
}

/// B3. Named for what the test can stage: `tokenExchange` shares the base URL, so a closed port
/// stops before the write-ahead. `NavAuth::post` maps a connect error and a bare 503 to the same
/// `Answer::Unavailable`, which is the arm this reaches.
#[tokio::test]
async fn a_post_that_never_reached_nav_returns_to_claimed_with_no_reconcile() {
	let db = TmpDb::new("unavailable");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	mock_once(&server, "manageInvoice", 503, String::new()).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply("TX-1")).await;
	let invoice = issue(&store).await;
	report(&app, &store, invoice.id).await.unwrap();
	let batch = batch_of(&store, &invoice).await;
	let uid = batch.uid.as_str();

	send(&app, &store, uid).await.expect_err("the runner backs off");

	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Claimed);
	assert_eq!(enqueued(&store, KIND_NAV_RECONCILE).await, 0);

	assert_eq!(send(&app, &store, uid).await.unwrap(), Next::Done);
	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Sent, "a plain resend");
}

/// A 4xx with an unreadable body is a refusal, not a lost reply: NAV took nothing, so no doubt.
#[tokio::test]
async fn an_unreadable_4xx_returns_to_claimed_with_no_verdict() {
	let db = TmpDb::new("unreadable-4xx");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let waf = "<html><body>blocked by WAF</body></html>".to_owned();
	mock_once(&server, "manageInvoice", 403, waf).await;
	mock(&server, "manageInvoice", 200, manage_ok_reply("TX-1")).await;
	let invoice = issue(&store).await;
	report(&app, &store, invoice.id).await.unwrap();
	let batch = batch_of(&store, &invoice).await;
	let uid = batch.uid.as_str();

	send(&app, &store, uid).await.expect_err("the runner backs off");

	assert_eq!(fresh(&store, uid).await.state, BatchState::Claimed);
	assert_eq!(filing(&store, &invoice).await.row.verdict, None);
	assert_eq!(enqueued(&store, KIND_NAV_RECONCILE).await, 0);

	assert_eq!(send(&app, &store, uid).await.unwrap(), Next::Done);
	assert_eq!(fresh(&store, uid).await.state, BatchState::Sent, "a plain resend");
}

/// R8 deletes the live row, leaving a resolved one newest: that history must not stop the report.
#[tokio::test]
async fn a_reopened_filing_with_history_is_filed_again() {
	let db = TmpDb::new("reopened-history");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	mock(&server, "manageInvoice", 500, error_reply("FORBIDDEN", "not authorised")).await;
	let invoice = issue(&store).await;
	report(&app, &store, invoice.id).await.unwrap();
	let spent = batch_of(&store, &invoice).await;
	send(&app, &store, spent.uid.as_str())
		.await
		.expect_err("a spent id is an error");

	let (nav, op, uid) = (Nav::new(app.clone()), operator(), invoice.uid.as_str());
	let seller = store.seller_by_id(SELLER).await.unwrap().unwrap();
	nav.release_seller_hold(&op, seller.uid.as_str()).await.unwrap();
	nav.cancel_filing(&op, uid).await.unwrap();
	nav.submit(&op, uid).await.unwrap();
	assert_eq!(report(&app, &store, invoice.id).await.unwrap(), Next::Done);

	let history = store.filings_by_invoice(invoice.id).await.unwrap();
	assert_eq!(history.len(), 2);
	assert_eq!(history[0].row.state, RowState::Active);
	assert_eq!(history[0].batch.as_ref().unwrap().state, BatchState::Claimed);
	assert_eq!(history[1].row.state, RowState::Failed, "the verdict stands");
	assert_eq!(history[1].row.batch_id, Some(spent.id));
}

/// How much raw `invoiceData` each invoice weighs: two exceed [`BATCH_BYTE_BUDGET`] as base64 and
/// one does not.
const HALF_BUDGET: usize = 3_500_000;

#[tokio::test]
async fn assembly_stops_at_the_byte_budget() {
	const { assert!(2 * HALF_BUDGET / 3 * 4 > BATCH_BYTE_BUDGET) };
	let db = TmpDb::new("budget");
	let (app, store) = setup(&db).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();
	let padded = "x".repeat(HALF_BUDGET);
	let first = issue_lined(&store, SELLER, SELLER_VER, padded.clone()).await;
	let second = issue_lined(&store, SELLER, SELLER_VER, padded).await;

	report(&app, &store, first.id).await.unwrap();

	assert_eq!(store.batch_rows(batch_of(&store, &first).await.id).await.unwrap().len(), 1);
	assert_eq!(state_of(&store, second.id).await, FilingState::None, "its own job files it");
}

/// One invoice over the budget needs the gzip this crate never sends: it stops short of NAV,
/// faulted, for a person to cancel.
#[tokio::test]
async fn an_invoice_over_the_byte_budget_stops_before_the_token() {
	let db = TmpDb::new("oversize");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let invoice = issue_lined(&store, SELLER, SELLER_VER, "x".repeat(2 * HALF_BUDGET)).await;

	report(&app, &store, invoice.id).await.unwrap();
	let batch = batch_of(&store, &invoice).await;
	send(&app, &store, batch.uid.as_str())
		.await
		.expect_err("the runner keeps it alive");

	let parked = fresh(&store, batch.uid.as_str()).await;
	assert_eq!(parked.state, BatchState::Claimed);
	assert_eq!(parked.error_code.as_deref(), Some(E_NAV_OVERSIZE));
	assert!(requests(&server, "tokenExchange").await.is_empty());
	Nav::new(app.clone())
		.cancel_filing(&operator(), invoice.uid.as_str())
		.await
		.unwrap();
	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Closed);
}

#[tokio::test]
async fn two_jobs_assembling_at_once_make_one_batch() {
	let db = TmpDb::new("two-jobs");
	let (app, store) = setup(&db).await;
	app.settings.set("nav.batch_max", "10", None).await.unwrap();
	let (first, second) = (issue(&store).await, issue(&store).await);

	let (a, b) = tokio::join!(report(&app, &store, first.id), report(&app, &store, second.id));

	assert_eq!((a.unwrap(), b.unwrap()), (Next::Done, Next::Done), "the loser stands down");
	assert_eq!(batch_count(&store).await, 1);
	assert_eq!(batch_of(&store, &first).await.id, batch_of(&store, &second).await.id);
	assert_eq!(enqueued(&store, KIND_NAV_SEND).await, 1);
}

// ---- reconciliation --------------------------------------------------------------------------

/// NAV refuses a `queryTransactionList` over 35 days, so an old batch's window is walked in slices.
#[tokio::test]
async fn a_window_over_35_days_makes_several_list_calls() {
	let db = TmpDb::new("window");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&[])).await;
	let invoice = issue(&store).await;
	let batch = doubted(&store, &[&invoice]).await;
	sqlx::query("UPDATE nav_batches SET created_at = ? WHERE id = ?")
		.bind(ago(40 * DAY).0)
		.bind(batch.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	reconcile(&app, &store, batch.uid.as_str()).await.unwrap();

	assert_eq!(requests(&server, "queryTransactionList").await.len(), 2);
}

/// NAV's per-IP limit counts `queryTransactionList` too, and a run's first request has no memory
/// of the last one, so both pages wait out `QUERY_PACE`.
#[tokio::test]
async fn every_transaction_list_page_is_paced_the_first_included() {
	let db = TmpDb::new("list-pace");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let two_pages = transaction_list_reply(&[]).replace("<availablePage>1", "<availablePage>2");
	mock(&server, "queryTransactionList", 200, two_pages).await;
	let invoice = issue(&store).await;
	let batch = doubted(&store, &[&invoice]).await;

	let started = std::time::Instant::now();
	reconcile(&app, &store, batch.uid.as_str()).await.unwrap();

	assert_eq!(requests(&server, "queryTransactionList").await.len(), 2);
	assert!(started.elapsed() >= std::time::Duration::from_millis(2 * 1100));
}

/// §1.9.2 requires the repeat immediately, so a send deferred by its backoff is pulled forward.
#[tokio::test]
async fn nothing_found_revives_send_at_once() {
	let db = TmpDb::new("nothing-found");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&[])).await;
	let invoice = issue(&store).await;
	let batch = doubted(&store, &[&invoice]).await;
	let (uid, key) = (batch.uid.as_str(), send_key(batch.uid.as_str()));
	plant(&app, KIND_NAV_SEND, &batch_payload(uid), &key, now().0 + HOUR).await;

	reconcile(&app, &store, uid).await.unwrap();

	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Claimed);
	let (status, run_at) = job_of(&store, &key).await.unwrap();
	assert_eq!(status, "PENDING");
	assert!(run_at <= now().0, "still deferred");
}

/// B9: guessing which half landed is how an invoice gets filed twice.
#[tokio::test]
async fn a_transaction_carrying_part_of_the_batch_holds_it() {
	let db = TmpDb::new("partial-match");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let (first, second) = (issue(&store).await, issue(&store).await);
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&["TX-PART"])).await;
	let echoed = original_request_reply(&[(1, number(&first))]);
	mock(&server, "queryTransactionStatus", 200, echoed).await;
	let batch = doubted(&store, &[&first, &second]).await;

	reconcile(&app, &store, batch.uid.as_str()).await.unwrap();

	let found = fresh(&store, batch.uid.as_str()).await;
	assert_eq!(found.state, BatchState::Held);
	assert_eq!(found.error_code.as_deref(), Some(E_NAV_RECONCILE_UNPROVEN));
	assert_eq!(enqueued(&store, KIND_NAV_SEND).await, 0);
}

/// A lost `[A,B]`, A cancelled and `[B]` reposted: claiming the old transaction hands B A's verdict.
#[tokio::test]
async fn a_transaction_with_an_extra_invoice_holds_the_batch() {
	let db = TmpDb::new("extra-match");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let invoice = issue(&store).await;
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&["TX-MORE"])).await;
	let echoed = original_request_reply(&[(1, "ANOTHER-1"), (2, number(&invoice))]);
	mock(&server, "queryTransactionStatus", 200, echoed).await;
	let batch = doubted(&store, &[&invoice]).await;

	reconcile(&app, &store, batch.uid.as_str()).await.unwrap();

	let found = fresh(&store, batch.uid.as_str()).await;
	assert_eq!(found.state, BatchState::Held);
	assert_eq!(found.error_code.as_deref(), Some(E_NAV_RECONCILE_UNPROVEN));
	assert_eq!(found.transaction_id, None);
}

/// `poll` matches results by index, so swapped indexes swap the invoices' verdicts.
#[tokio::test]
async fn a_transaction_with_the_same_numbers_at_other_indexes_holds_the_batch() {
	let db = TmpDb::new("index-match");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let (first, second) = (issue(&store).await, issue(&store).await);
	mock(&server, "queryTransactionList", 200, transaction_list_reply(&["TX-SWAP"])).await;
	let echoed = original_request_reply(&[(2, number(&first)), (1, number(&second))]);
	mock(&server, "queryTransactionStatus", 200, echoed).await;
	let batch = doubted(&store, &[&first, &second]).await;

	reconcile(&app, &store, batch.uid.as_str()).await.unwrap();

	let found = fresh(&store, batch.uid.as_str()).await;
	assert_eq!(found.state, BatchState::Held);
	assert_eq!(found.transaction_id, None);
}

// ---- polling and the supervisor --------------------------------------------------------------

#[tokio::test]
async fn number_not_unique_holds_one_row_while_its_batch_closes() {
	let db = TmpDb::new("number-taken");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let reply = status_results(&[(1, "DONE", ""), (2, "ABORTED", NUMBER_TAKEN)]);
	mock(&server, "queryTransactionStatus", 200, reply).await;
	let (done, taken) = (issue(&store).await, issue(&store).await);
	let batch = sent(&store, &[&done, &taken], "TX-1").await;
	let uid = batch.uid.as_str();

	assert_eq!(poll(&app, &store, uid).await.unwrap(), Next::Done);

	assert_eq!(state_of(&store, done.id).await, FilingState::Accepted);
	assert_eq!(state_of(&store, taken.id).await, FilingState::Held);
	let row = filing(&store, &taken).await.row;
	assert_eq!(row.verdict, Some(NavVerdict::Rejected), "what NAV said stands");
	assert_eq!(row.error_code.as_deref(), Some(INVOICE_NUMBER_NOT_UNIQUE));
	assert_eq!(fresh(&store, batch.uid.as_str()).await.state, BatchState::Closed);
	assert_pending(&store, &adopt_key(uid)).await;
}

#[tokio::test]
async fn the_supervisor_finds_a_tenants_orphan() {
	let db = TmpDb::new("orphan-tenant");
	let (app, store) = setup(&db).await;
	let ver = tenant(&store).await;
	let invoice = issue_lined(&store, TENANT, ver, "Tanacsadas".into()).await;

	sweep(&app, &store).await;

	assert_pending(&store, &invoice_job_key(invoice.id)).await;
}

/// `wake = false`: the supervisor must not defeat a backoff or a handler's own deferral.
#[tokio::test]
async fn the_supervisor_leaves_deferred_jobs_alone() {
	let db = TmpDb::new("deferred");
	let (app, store) = setup(&db).await;
	let (claimed, posted, waiting) =
		(issue(&store).await, issue(&store).await, issue(&store).await);
	let batch = claim(&store, &[&claimed]).await;
	let polled = sent(&store, &[&posted], "TX-1").await;
	let later = now().0 + 2 * HOUR;
	let jobs = [
		(KIND_NAV_SEND, batch_payload(batch.uid.as_str()), send_key(batch.uid.as_str())),
		(KIND_NAV_POLL, batch_payload(polled.uid.as_str()), poll_key(polled.uid.as_str())),
		(KIND_NAV_REPORT, invoice_job_payload(waiting.id), invoice_job_key(waiting.id)),
	];
	for (kind, payload, key) in &jobs {
		plant(&app, kind, payload, key, later).await;
	}

	sweep(&app, &store).await;

	for (_, _, key) in &jobs {
		assert_eq!(job_of(&store, key).await, Some(("PENDING".to_owned(), later)), "{key}");
	}
}

// ---- design §12 ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Start {
	None,
	Claimed,
	Posting,
	InDoubt,
	/// `ACTIVE` rows in a `HELD` batch: B9.
	HeldBatch,
	/// A `HELD` row: R3.
	HeldRow,
	Sent,
	Accepted,
	Rejected,
	Failed,
	ResolvedRejected,
	ResolvedFailed,
	Cancelled,
}

#[derive(Clone, Copy, Debug)]
enum Action {
	Retry,
	Submit,
	Cancel,
	Resolve,
	Attach,
	Confirm,
}

const ACTIONS: [Action; 6] = [
	Action::Retry,
	Action::Submit,
	Action::Cancel,
	Action::Resolve,
	Action::Attach,
	Action::Confirm,
];

#[derive(Clone, Copy)]
enum Cell {
	/// The error code, nothing moving.
	Refused(&'static str),
	/// Allowed: the filing state after it, and the job kind it owes.
	Moves(FilingState, Option<&'static str>),
}

use Cell::{Moves, Refused};

const NO_RETRY: Cell = Refused("E-NAV-NOT-RETRYABLE");
const NO_SUBMIT: Cell = Refused("E-NAV-NOT-REDRIVABLE");
const IN_FLIGHT: Cell = Refused("E-NAV-FILING-IN-FLIGHT");
const STATE: Cell = Refused(E_NAV_SUBMISSION_STATE);
const TO_REPORT: Cell = Moves(FilingState::None, Some(KIND_NAV_REPORT));
const TO_CLAIMED: Cell = Moves(FilingState::Claimed, Some(KIND_NAV_SEND));
const TO_SENT: Cell = Moves(FilingState::Sent, Some(KIND_NAV_POLL));
const TO_CANCELLED: Cell = Moves(FilingState::Cancelled, None);

/// Design §12, columns in [`ACTIONS`] order. `HELD`, `REJECTED` and `FAILED` each take two rows:
/// the two ways into `HELD` and the two sides of a resolve.
const MATRIX: &[(Start, [Cell; 6])] = &[
	(Start::None, [TO_REPORT, TO_REPORT, TO_CANCELLED, Refused("E-CORE-NOTFOUND"), STATE, STATE]),
	(Start::Claimed, [TO_CLAIMED, TO_CLAIMED, TO_CANCELLED, STATE, STATE, STATE]),
	(Start::Posting, [NO_RETRY, NO_SUBMIT, IN_FLIGHT, STATE, STATE, STATE]),
	(
		Start::InDoubt,
		[
			NO_RETRY,
			Moves(FilingState::InDoubt, Some(KIND_NAV_RECONCILE)),
			IN_FLIGHT,
			STATE,
			STATE,
			STATE,
		],
	),
	(Start::HeldBatch, [NO_RETRY, NO_SUBMIT, IN_FLIGHT, STATE, TO_SENT, TO_CLAIMED]),
	(Start::HeldRow, [NO_RETRY, NO_SUBMIT, IN_FLIGHT, STATE, TO_SENT, TO_CLAIMED]),
	(Start::Sent, [TO_SENT, TO_SENT, IN_FLIGHT, STATE, STATE, STATE]),
	(Start::Accepted, [NO_RETRY, NO_SUBMIT, STATE, STATE, STATE, STATE]),
	(
		Start::Rejected,
		[TO_CLAIMED, NO_SUBMIT, STATE, Moves(FilingState::Rejected, None), STATE, STATE],
	),
	(Start::Failed, [TO_CLAIMED, NO_SUBMIT, STATE, Moves(FilingState::Failed, None), STATE, STATE]),
	(Start::ResolvedRejected, [TO_CLAIMED, TO_CLAIMED, STATE, STATE, STATE, STATE]),
	(Start::ResolvedFailed, [TO_CLAIMED, TO_CLAIMED, STATE, STATE, STATE, STATE]),
	(Start::Cancelled, [NO_RETRY, TO_REPORT, STATE, STATE, STATE, STATE]),
];

/// A `SENT` filing given `verdict`; returns its row.
async fn settled(store: &SqliteStore, invoice: &Invoice, verdict: NavVerdict) -> i64 {
	let batch = sent(store, &[invoice], &format!("TX-{}", invoice.id)).await;
	let row = row_ids(store, batch.id).await[0];
	let error = (verdict != NavVerdict::Done).then_some(("SOME_CODE", "NAV's text"));
	assert!(store.finish(row, verdict, error, now()).await.unwrap());
	row
}

/// A fresh invoice driven to `start`.
async fn reach(store: &SqliteStore, start: Start) -> Invoice {
	let invoice = issue(store).await;
	let one = [&invoice];
	let tx = format!("TX-{}", invoice.id);
	match start {
		Start::None => {}
		Start::Claimed => {
			claim(store, &one).await;
		}
		Start::Posting => {
			post(store, &one).await;
		}
		Start::InDoubt => {
			doubted(store, &one).await;
		}
		Start::HeldBatch => {
			held(store, &one).await;
		}
		Start::HeldRow => {
			number_taken(store, &one, &tx, now()).await;
		}
		Start::Sent => {
			sent(store, &one, &tx).await;
		}
		Start::Accepted => {
			settled(store, &invoice, NavVerdict::Done).await;
		}
		Start::Rejected => {
			settled(store, &invoice, NavVerdict::Rejected).await;
		}
		Start::Failed => {
			settled(store, &invoice, NavVerdict::Failed).await;
		}
		Start::ResolvedRejected => {
			let row = settled(store, &invoice, NavVerdict::Rejected).await;
			assert!(store.resolve(row, now()).await.unwrap());
		}
		Start::ResolvedFailed => {
			let row = settled(store, &invoice, NavVerdict::Failed).await;
			assert!(store.resolve(row, now()).await.unwrap());
		}
		Start::Cancelled => {
			assert!(store.cancel_filing(invoice.id, NavOp::Create, now()).await.unwrap());
		}
	}
	invoice
}

/// The dedup key of the job of `kind` this invoice's filing owes.
async fn owed_key(store: &SqliteStore, invoice: &Invoice, kind: &str) -> String {
	if kind == KIND_NAV_REPORT {
		return invoice_job_key(invoice.id);
	}
	let batch = batch_of(store, invoice).await;
	match kind {
		KIND_NAV_SEND => send_key(batch.uid.as_str()),
		KIND_NAV_POLL => poll_key(batch.uid.as_str()),
		KIND_NAV_RECONCILE => reconcile_key(batch.uid.as_str()),
		other => panic!("no such owed kind: {other}"),
	}
}

async fn act(nav: &Nav, action: Action, uid: &str, transaction_id: &str) -> ClResult<()> {
	let ctx = operator();
	match action {
		Action::Retry => nav.retry(&ctx, uid).await.map(|_| ()),
		Action::Submit => nav.submit(&ctx, uid).await,
		Action::Cancel => nav.cancel_filing(&ctx, uid).await,
		Action::Resolve => nav.resolve_filing(&ctx, uid, "handled").await,
		Action::Attach => nav.attach_transaction(&ctx, uid, transaction_id).await,
		Action::Confirm => nav.confirm_not_filed(&ctx, uid, "checked").await,
	}
}

#[tokio::test]
async fn every_cell_of_the_operator_matrix() {
	let db = TmpDb::new("matrix");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let nav = Nav::new(app.clone());

	for (start, cells) in MATRIX {
		for (action, cell) in ACTIONS.iter().zip(cells) {
			let invoice = reach(&store, *start).await;
			let uid = invoice.uid.as_str();
			let before = state_of(&store, invoice.id).await;
			let tx = format!("TX-ATTACH-{}", invoice.id);
			if matches!(action, Action::Attach) {
				let echoed = original_request_reply(&[(1, number(&invoice))]);
				mock_for(&server, "queryTransactionStatus", &tx, echoed).await;
			}

			let result = act(&nav, *action, uid, &tx).await;

			let after = state_of(&store, invoice.id).await;
			match cell {
				Refused(code) => {
					let err = result.expect_err(&format!("{start:?} {action:?} must be refused"));
					assert_eq!(err.parts().1, *code, "{start:?} {action:?}");
					assert_eq!(after, before, "{start:?} {action:?} moved a refused filing");
				}
				Moves(state, owed) => {
					result.unwrap_or_else(|e| panic!("{start:?} {action:?}: {e:?}"));
					assert_eq!(after, *state, "{start:?} {action:?}");
					if let Some(kind) = owed {
						assert_pending(&store, &owed_key(&store, &invoice, kind).await).await;
					}
					if matches!(action, Action::Resolve) {
						assert!(filing(&store, &invoice).await.row.resolved_at.is_some());
					}
				}
			}
		}
	}
}

/// Beyond §12: a `CANCELLED` row on a draft would keep it from ever being filed once issued.
#[tokio::test]
async fn a_draft_can_be_neither_cancelled_nor_submitted() {
	let db = TmpDb::new("draft");
	let (app, store) = setup(&db).await;
	let draft = store.create_draft(&new_invoice(InvoiceKind::Normal, None)).await.unwrap();
	let (nav, uid) = (Nav::new(app.clone()), draft.uid.as_str());

	let err = nav.cancel_filing(&operator(), uid).await.unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-NOT-ISSUED");
	let err = nav.submit(&operator(), uid).await.unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-NOT-ISSUED");
	assert_eq!(state_of(&store, draft.id).await, FilingState::None);
	assert_eq!(enqueued(&store, KIND_NAV_REPORT).await, 0, "no job, so no dedup key was spent");
}

// ---- attach ----------------------------------------------------------------------------------

#[tokio::test]
async fn attach_binds_a_matching_transaction() {
	let db = TmpDb::new("attach-match");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let invoice = issue(&store).await;
	let echoed = original_request_reply(&[(7, number(&invoice))]);
	mock(&server, "queryTransactionStatus", 200, echoed).await;
	let old = held(&store, &[&invoice]).await;

	let nav = Nav::new(app.clone());
	nav.attach_transaction(&operator(), invoice.uid.as_str(), "TX-1").await.unwrap();

	let found = filing(&store, &invoice).await;
	let batch = found.batch.as_ref().unwrap();
	assert_eq!(found.state(), FilingState::Sent);
	assert_eq!(batch.transaction_id.as_deref(), Some("TX-1"));
	assert_eq!(found.row.idx, Some(7), "NAV's index, not ours");
	assert_eq!(fresh(&store, old.uid.as_str()).await.state, BatchState::Closed);
	assert_pending(&store, &poll_key(batch.uid.as_str())).await;
}

/// The replaced row keeps its batch, or the XML sent to NAV is unreachable from the invoice.
#[tokio::test]
async fn attach_keeps_the_old_request_in_the_archives() {
	let db = TmpDb::new("attach-archive");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let invoice = issue(&store).await;
	let echoed = original_request_reply(&[(7, number(&invoice))]);
	mock(&server, "queryTransactionStatus", 200, echoed).await;
	held(&store, &[&invoice]).await;

	let (nav, uid) = (Nav::new(app.clone()), invoice.uid.as_str());
	nav.attach_transaction(&operator(), uid, "TX-1").await.unwrap();

	let archives = nav.filing_archives(&operator(), uid).await.unwrap();
	assert_eq!(archives.len(), 1);
	assert!(archives[0].request_xml.as_deref().unwrap().contains(&filed_data(1)));
}

#[tokio::test]
async fn attach_refuses_a_transaction_without_this_number() {
	let db = TmpDb::new("attach-mismatch");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let echoed = original_request_reply(&[(1, "SOMEONE/ELSE")]);
	mock(&server, "queryTransactionStatus", 200, echoed).await;
	let invoice = issue(&store).await;
	held(&store, &[&invoice]).await;

	let nav = Nav::new(app.clone());
	let err = nav
		.attach_transaction(&operator(), invoice.uid.as_str(), "TX-1")
		.await
		.unwrap_err();

	assert_eq!(err.parts().1, "E-NAV-ATTACH-MISMATCH");
	assert_eq!(state_of(&store, invoice.id).await, FilingState::Held);
}

#[tokio::test]
async fn a_partial_attach_leaves_the_rest_held() {
	let db = TmpDb::new("attach-partial");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let (found, missing) = (issue(&store).await, issue(&store).await);
	let echoed = original_request_reply(&[(1, number(&found))]);
	mock(&server, "queryTransactionStatus", 200, echoed).await;
	let old = held(&store, &[&found, &missing]).await;

	let nav = Nav::new(app.clone());
	nav.attach_transaction(&operator(), found.uid.as_str(), "TX-1").await.unwrap();

	assert_eq!(state_of(&store, found.id).await, FilingState::Sent);
	assert_eq!(state_of(&store, missing.id).await, FilingState::Held);
	assert_eq!(
		fresh(&store, old.uid.as_str()).await.state,
		BatchState::Held,
		"a row is still in it"
	);
}

#[tokio::test]
async fn attach_refuses_a_transaction_a_batch_already_carries() {
	let db = TmpDb::new("attach-bound");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let (carried, invoice) = (issue(&store).await, issue(&store).await);
	sent(&store, &[&carried], "TX-1").await;
	held(&store, &[&invoice]).await;

	let nav = Nav::new(app.clone());
	let err = nav
		.attach_transaction(&operator(), invoice.uid.as_str(), "TX-1")
		.await
		.unwrap_err();

	assert_eq!(err.parts().1, E_NAV_SUBMISSION_STATE);
	assert!(requests(&server, "queryTransactionStatus").await.is_empty());
	assert_eq!(state_of(&store, invoice.id).await, FilingState::Held);
}

/// `verdict` and `error_code` are the statutory record: R4 adds a row, it rewrites nothing.
#[tokio::test]
async fn attach_after_r3_keeps_the_rejected_row() {
	let db = TmpDb::new("attach-r3");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let invoice = issue(&store).await;
	let echoed = original_request_reply(&[(2, number(&invoice))]);
	mock(&server, "queryTransactionStatus", 200, echoed).await;
	number_taken(&store, &[&invoice], "TX-RESEND", now()).await;

	let nav = Nav::new(app.clone());
	nav.attach_transaction(&operator(), invoice.uid.as_str(), "TX-FIRST")
		.await
		.unwrap();

	let history = store.filings_by_invoice(invoice.id).await.unwrap();
	assert_eq!(history.len(), 2);
	assert_eq!(history[0].state(), FilingState::Sent);
	assert_eq!(history[0].row.idx, Some(2));
	let old = &history[1].row;
	assert_eq!(old.state, RowState::Rejected);
	assert_eq!(old.verdict, Some(NavVerdict::Rejected));
	assert_eq!(old.error_code.as_deref(), Some(INVOICE_NUMBER_NOT_UNIQUE));
	assert!(old.resolved_at.is_some());
}

// ---- adoption --------------------------------------------------------------------------------

/// `WARN` is no `invoiceStatus`: NAV says `DONE`, and this message makes it the `WARN` verdict.
const WARN_MESSAGE: &str = "<businessValidationMessages>\
	<validationResultCode>WARN</validationResultCode>\
	<validationErrorCode>X</validationErrorCode>\
	<message>m</message></businessValidationMessages>";

/// The new row has no verdict of its own: the poll fetches NAV's true one, here a `WARN`.
#[tokio::test]
async fn adoption_is_proved_and_polled_to_warn() {
	let db = TmpDb::new("adopt-warn");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let found = invoice_data_reply(&filed_data(1), Some("TX-FIRST"), Some(4), false);
	mock(&server, "queryInvoiceData", 200, found).await;
	mock(&server, "queryTransactionStatus", 200, status_results(&[(4, "DONE", WARN_MESSAGE)]))
		.await;
	let invoice = issue(&store).await;
	let lost = number_taken(&store, &[&invoice], "TX-RESEND", now()).await;

	assert_eq!(adopt(&app, &store, lost.uid.as_str()).await.unwrap(), Next::Done);

	let adopted = batch_of(&store, &invoice).await;
	assert_eq!(adopted.transaction_id.as_deref(), Some("TX-FIRST"));
	assert_eq!(filing(&store, &invoice).await.row.idx, Some(4));
	assert_pending(&store, &poll_key(adopted.uid.as_str())).await;

	assert_eq!(poll(&app, &store, adopted.uid.as_str()).await.unwrap(), Next::Done);

	let settled = store.filings_by_invoice(invoice.id).await.unwrap();
	assert_eq!(settled[0].state(), FilingState::Accepted);
	assert_eq!(settled[0].row.verdict, Some(NavVerdict::Warn));
	assert_eq!(settled[1].row.verdict, Some(NavVerdict::Rejected), "the R3 record stands");
}

/// `QUERY_PACE` makes this test sleep about three seconds.
#[tokio::test]
async fn three_rows_of_one_lost_request_share_one_new_batch() {
	let db = TmpDb::new("adopt-three");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let invoices = [issue(&store).await, issue(&store).await, issue(&store).await];
	for (i, invoice) in invoices.iter().enumerate() {
		let idx = i + 1;
		let found = invoice_data_reply(
			&filed_data(idx),
			Some("TX-FIRST"),
			Some(i64::try_from(idx).unwrap()),
			false,
		);
		mock_for(&server, "queryInvoiceData", number(invoice), found).await;
	}
	let refs: Vec<&Invoice> = invoices.iter().collect();
	let lost = number_taken(&store, &refs, "TX-RESEND", now()).await;

	assert_eq!(adopt(&app, &store, lost.uid.as_str()).await.unwrap(), Next::Done);

	let adopted = store.batch_by_transaction("TX-FIRST").await.unwrap().unwrap();
	assert_eq!(adopted.state, BatchState::Sent);
	let rows = store.batch_rows(adopted.id).await.unwrap();
	let idxs: Vec<Option<i64>> = rows.iter().map(|r| r.idx).collect();
	assert_eq!(idxs, [Some(1), Some(2), Some(3)]);
	assert_eq!(batch_count(&store).await, 2, "the lost batch and the one new one");
}

#[tokio::test]
async fn adoption_is_refused_on_anything_short_of_proof() {
	let db = TmpDb::new("adopt-refused");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let ours = filed_data(1);
	let cases = [
		("differing content", invoice_data_reply(&filed_data(2), Some("TX-P"), Some(1), false)),
		("no transactionId", invoice_data_reply(&ours, None, Some(1), false)),
		("no index", invoice_data_reply(&ours, Some("TX-P"), None, false)),
		("compressed content", invoice_data_reply(&ours, Some("TX-P"), Some(1), true)),
	];

	for (case, reply) in cases {
		let invoice = issue(&store).await;
		mock_for(&server, "queryInvoiceData", number(&invoice), reply).await;
		let tx = format!("TX-RESEND-{}", invoice.id);
		let lost = number_taken(&store, &[&invoice], &tx, now()).await;

		assert_eq!(adopt(&app, &store, lost.uid.as_str()).await.unwrap(), Next::Done, "{case}");

		let row = filing(&store, &invoice).await.row;
		assert_eq!(row.state, RowState::Held, "{case}");
		assert!(row.adopt_checked_at.is_some(), "{case}");
		assert!(store.batch_by_transaction("TX-P").await.unwrap().is_none(), "{case}");
	}
}

#[tokio::test]
async fn adoption_is_refused_on_a_transaction_already_carried() {
	let db = TmpDb::new("adopt-carried");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	let found = invoice_data_reply(&filed_data(1), Some("TX-TAKEN"), Some(1), false);
	mock(&server, "queryInvoiceData", 200, found).await;
	let (carried, invoice) = (issue(&store).await, issue(&store).await);
	let holder = sent(&store, &[&carried], "TX-TAKEN").await;
	let lost = number_taken(&store, &[&invoice], "TX-RESEND", now()).await;

	assert_eq!(adopt(&app, &store, lost.uid.as_str()).await.unwrap(), Next::Done);

	let row = filing(&store, &invoice).await.row;
	assert_eq!(row.state, RowState::Held);
	assert!(row.adopt_checked_at.is_some());
	assert_eq!(row_ids(&store, holder.id).await.len(), 1, "the carrying batch gains no row");
}

/// Inside the hour the first filing may still be processing, so no answer is not an answer yet.
#[tokio::test]
async fn no_result_retries_inside_the_hour_and_stamps_after_it() {
	let db = TmpDb::new("adopt-wait");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	mock(&server, "queryInvoiceData", 200, no_invoice_data_reply()).await;
	let (recent, old) = (issue(&store).await, issue(&store).await);
	let waiting = number_taken(&store, &[&recent], "TX-1", now()).await;
	let expired = number_taken(&store, &[&old], "TX-2", ago(2 * HOUR)).await;

	let before = now().0;
	let Next::Again { at } = adopt(&app, &store, waiting.uid.as_str()).await.unwrap() else {
		panic!("inside the hour the job asks again");
	};
	assert!(at.0 >= before + 600);
	assert_eq!(filing(&store, &recent).await.row.adopt_checked_at, None);

	assert_eq!(adopt(&app, &store, expired.uid.as_str()).await.unwrap(), Next::Done);
	let row = filing(&store, &old).await.row;
	assert_eq!(row.state, RowState::Held);
	assert!(row.adopt_checked_at.is_some());
}

#[tokio::test]
async fn a_nav_fault_during_adoption_stamps_nothing() {
	let db = TmpDb::new("adopt-fault");
	let (app, store) = setup(&db).await;
	let server = nav_server(&app).await;
	mock(&server, "queryInvoiceData", 500, error_reply("OPERATION_FAILED", "down")).await;
	let invoice = issue(&store).await;
	let lost = number_taken(&store, &[&invoice], "TX-1", ago(2 * HOUR)).await;

	adopt(&app, &store, lost.uid.as_str())
		.await
		.expect_err("NAV could not be asked");

	assert_eq!(filing(&store, &invoice).await.row.adopt_checked_at, None);
	assert_eq!(store.adoptable(10).await.unwrap().len(), 1, "the sweep still owes it a NAV_ADOPT");
}

#[tokio::test]
async fn list_filings_reads_the_raw_query_behind_the_operator_check() {
	let db = TmpDb::new("list-filings");
	let (app, store) = setup(&db).await;
	let invoice = issue(&store).await;
	let batch = doubted(&store, &[&invoice]).await;
	let row = row_ids(&store, batch.id).await[0];
	let nav = Nav::new(app);

	let err = nav.list_filings(&operator(), Some("held"), None, None).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-VALIDATION");
	let err = nav.list_filings(&tenant_admin(), Some("held"), None, None).await.unwrap_err();
	assert_ne!(err.parts().1, "E-CORE-VALIDATION", "the auth error comes first");

	let full = nav.list_filings(&operator(), Some("IN_DOUBT"), None, None).await.unwrap();
	assert_eq!(full.items.len(), 1);
	assert_eq!(full.items[0].0, invoice.uid);
	assert_eq!(full.next_cursor, Some(row.to_string()));

	let empty = nav.list_filings(&operator(), None, Some(row), None).await.unwrap();
	assert!(empty.items.is_empty());
	assert_eq!(empty.next_cursor, None);
}

// ---- alerts and the operator's exits ---------------------------------------------------------

/// `NAV_ADOPT` still owns an unchecked row, so it needs no person yet.
#[tokio::test]
async fn an_unchecked_held_row_raises_no_alert() {
	let db = TmpDb::new("unchecked");
	let (app, store) = setup(&db).await;
	let invoice = issue(&store).await;
	let batch = number_taken(&store, &[&invoice], "TX-1", now()).await;

	assert_eq!(alert_count(&app, "A-NAV-REJECTED").await, 0);

	let rows = row_ids(&store, batch.id).await;
	assert_eq!(store.stamp_adopt_checked(&rows, now()).await.unwrap(), 1);
	assert_eq!(alert_count(&app, "A-NAV-REJECTED").await, 1);
}

#[tokio::test]
async fn confirm_refiles_three_rows_into_one_batch() {
	let db = TmpDb::new("confirm-three");
	let (app, store) = setup(&db).await;
	let invoices = [issue(&store).await, issue(&store).await, issue(&store).await];
	let refs: Vec<&Invoice> = invoices.iter().collect();
	let old = held(&store, &refs).await;

	let nav = Nav::new(app.clone());
	nav.confirm_not_filed(&operator(), invoices[1].uid.as_str(), "checked")
		.await
		.unwrap();

	assert_eq!(fresh(&store, old.uid.as_str()).await.state, BatchState::Closed);
	for row in store.batch_rows(old.id).await.unwrap() {
		assert_eq!(row.state, RowState::Failed);
		assert_eq!(row.verdict, Some(NavVerdict::Failed));
		assert_eq!(row.error_code.as_deref(), Some("E-NAV-CONFIRMED-NOT-FILED"));
		assert_eq!(row.error_msg, None);
		assert!(row.resolved_at.is_some());
	}
	let new = batch_of(&store, &invoices[0]).await;
	assert_ne!(new.id, old.id);
	assert_eq!(new.state, BatchState::Claimed);
	let rows = store.batch_rows(new.id).await.unwrap();
	assert_eq!(rows.len(), 3);
	assert!(rows.iter().all(|r| r.state == RowState::Active));
	assert_pending(&store, &send_key(new.uid.as_str())).await;
}

/// `job_wake` pulls `PENDING` rows forward, so each job is planted at the hold's own deferral.
#[tokio::test]
async fn releasing_a_hold_wakes_the_sellers_jobs() {
	let db = TmpDb::new("release");
	let (app, store) = setup(&db).await;
	let (claimed, unfiled) = (issue(&store).await, issue(&store).await);
	let batch = claim(&store, &[&claimed]).await;
	let later = now().0 + HELD_RECHECK_SECS;
	let send = send_key(batch.uid.as_str());
	let filed = invoice_job_key(unfiled.id);
	plant(&app, KIND_NAV_SEND, &batch_payload(batch.uid.as_str()), &send, later).await;
	plant(&app, KIND_NAV_REPORT, &invoice_job_payload(unfiled.id), &filed, later).await;
	let spent = post(&store, &[&issue(&store).await]).await;
	store.spend_batch(spent.id, "FORBIDDEN", "no", now()).await.unwrap().unwrap();
	let seller = store.seller_by_id(SELLER).await.unwrap().unwrap();

	let nav = Nav::new(app.clone());
	nav.release_seller_hold(&operator(), seller.uid.as_str()).await.unwrap();

	assert!(store.seller_hold(SELLER).await.unwrap().is_none());
	for key in [send, filed] {
		let (status, run_at) = job_of(&store, &key).await.unwrap();
		assert_eq!(status, "PENDING");
		assert!(run_at <= now().0, "{key} was not woken");
	}
}

#[tokio::test]
async fn a_transient_fault_raises_no_alert_until_the_grace_passes() {
	let db = TmpDb::new("grace");
	let (app, store) = setup(&db).await;
	let (fresh_fault, old_fault) = (issue(&store).await, issue(&store).await);
	let young = claim(&store, &[&fresh_fault]).await;
	assert!(
		store
			.record_batch_fault(young.id, "OPERATION_FAILED", "down", now())
			.await
			.unwrap()
	);

	assert_eq!(alert_count(&app, "A-NAV-REJECTED").await, 0);

	let aged = claim(&store, &[&old_fault]).await;
	let past = ago(GRACE + 60);
	assert!(
		store
			.record_batch_fault(aged.id, "OPERATION_FAILED", "down", past)
			.await
			.unwrap()
	);

	assert_eq!(alert_count(&app, "A-NAV-REJECTED").await, 1, "only the fault past its grace");
}

/// A batch spans orgs, so its uid, transaction and fault are shared with other orgs' invoices.
/// `batch.error_code` stays on the Rust value for `state()`; the wire is what a tenant reads.
#[tokio::test]
async fn a_tenant_reads_no_batch_uid_transaction_id_or_batch_fault() {
	let db = TmpDb::new("tenant-view");
	let (app, store) = setup(&db).await;
	let ver = tenant(&store).await;
	let invoice = issue_lined(&store, TENANT, ver, "Tanacsadas".into()).await;
	let batch = sent(&store, &[&invoice], "TX-SECRET").await;
	let (code, msg) = (E_NAV_POLL_STALE, "about the whole batch");
	assert!(store.record_batch_fault(batch.id, code, msg, now()).await.unwrap());
	let (nav, uid) = (Nav::new(app.clone()), invoice.uid.as_str());

	let seen = nav.filing(&tenant_admin(), uid).await.unwrap().unwrap();

	assert_eq!(seen.state(), FilingState::Sent, "blanked, not dropped");
	let blanked = seen.batch.as_ref().unwrap();
	assert!(blanked.uid.as_str().is_empty());
	assert_eq!(blanked.transaction_id, None);
	assert_eq!(blanked.error_msg, None);
	let wire = filing_json(&seen);
	assert!(wire["batchUid"].is_null());
	assert!(wire["transactionId"].is_null());
	assert!(wire["errorCode"].is_null());
	assert!(wire["errorMsg"].is_null());
	let text = wire.to_string();
	for secret in [batch.uid.as_str(), "TX-SECRET", code, msg] {
		assert!(!text.contains(secret), "{secret} reached a tenant");
	}

	let full = nav.filing(&operator(), uid).await.unwrap().unwrap();
	assert_eq!(filing_json(&full)["transactionId"], "TX-SECRET", "an operator sees it all");
}

/// A batch held by reconciliation has no row error, so its fault is all an operator can act on.
#[tokio::test]
async fn a_held_batchs_fault_reaches_the_operator_and_not_the_tenant() {
	let db = TmpDb::new("held-fault");
	let (app, store) = setup(&db).await;
	let ver = tenant(&store).await;
	let invoice = issue_lined(&store, TENANT, ver, "Tanacsadas".into()).await;
	held(&store, &[&invoice]).await;
	let (nav, uid) = (Nav::new(app.clone()), invoice.uid.as_str());

	let wire = filing_json(&nav.filing(&operator(), uid).await.unwrap().unwrap());
	assert_eq!(wire["errorCode"], E_NAV_RECONCILE_UNPROVEN);
	assert_eq!(wire["errorMsg"], "unproved");

	let wire = filing_json(&nav.filing(&tenant_admin(), uid).await.unwrap().unwrap());
	assert!(wire["errorCode"].is_null());
	assert!(wire["errorMsg"].is_null());
}

// vim: ts=4
