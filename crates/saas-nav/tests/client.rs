//! `NavAuth` against a `wiremock` stand-in for Online Számla: token exchange, a submission,
//! every terminal `queryTransactionStatus` state, a business rejection that must never be
//! retried, and a technical outage that must be.
//!
//! No test here reaches the real service — `nav.base_url` is repointed at the mock server, and
//! the one test that needs an unreachable host uses a closed loopback port.
//!
//! The `App` comes from [`saas_core::AppBuilder::build`], which is `run` minus the listener.
//! Only the `saas-core/init` step is migrated: `NavAuth::load` reads settings and secrets and
//! nothing else, and the `Seller` it takes is a plain struct, not a row.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aes::{
	Aes128,
	cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use saas_core::{App, AppBuilder, config::Config, error::StatusCode, prelude::*, store::CoreStore};
use saas_invoice::{
	service_api::SELLER_ID,
	store::{Seller, SellerVersion, SellerVersionStatus},
};
use saas_nav::{
	NavOp,
	auth::{Answer, NavAuth},
	client::{
		Accepted, Disposition, Outcome, accepted, original_invoice_numbers, outcomes,
		transaction_list,
	},
	reply::Reply,
};
use store_adapter_sqlite::SqliteStore;
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{method, path},
};

/// A filed invoice's `uid`, which is what `manage_invoice_request` sends as the NAV
/// `requestId`. `inv_` plus a 26-character ULID is exactly `EntityIdType`'s 30-char maximum.
const INV_UID: &str = "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// A stand-in `invoice_documents.sha256` — stored lowercase, filed uppercase.
const PDF_SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// AES-128-ECB, so exactly 16 bytes (`crypto::decrypt_exchange_token`).
const EXCHANGE_KEY: &[u8; 16] = b"0123456789abcdef";
/// Exactly one AES block, so the reply carries no padding block to strip.
const TOKEN: &str = "TOKENTOKENTOKEN1";

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, so the pool must be over a file.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("saas-nav-client-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		Self(dir)
	}

	fn path(&self) -> String {
		self.0.join("test.db").display().to_string()
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// A live `App` with `nav.base_url` pointed at `base_url` and the three NAV secrets seeded.
async fn app(db: &TmpDb, base_url: &str) -> App {
	let config = Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	let store = SqliteStore::open(&config).await.unwrap();
	// The whole framework module: this test needs only `saas-core`'s tables, but the schema is
	// one versioned unit and the rest costs a few CREATEs.
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	let app = AppBuilder::new()
		.config(config)
		.store(Arc::new(store) as Arc<dyn CoreStore>)
		.build()
		.await
		.unwrap();

	app.settings.set("nav.base_url", base_url, None).await.unwrap();
	app.secrets.set("nav.tech_password", b"tech-pw", None).await.unwrap();
	app.secrets.set("nav.sign_key", b"sign-key", None).await.unwrap();
	app.secrets.set("nav.exchange_key", EXCHANGE_KEY, None).await.unwrap();
	// The `nav.software_*` keys default to `""`, which `invoiceApi.xsd` rejects, so
	// `NavAuth::load` refuses to build an envelope out of them.
	for (key, value) in SOFTWARE_SETTINGS {
		app.settings.set(key, value, None).await.unwrap();
	}
	app
}

/// The six `nav.software_*` keys `invoiceApi.xsd` requires. `softwareId` is `[0-9A-Z\-]{18}`
/// exactly; the rest are `…NotBlankType`.
const SOFTWARE_SETTINGS: [(&str, &str); 6] = [
	("nav.software_id", "HU12345678SAASFRWK"),
	("nav.software_name", "saas-framework"),
	("nav.software_operation", "LOCAL_SOFTWARE"),
	("nav.software_main_version", "0.1"),
	("nav.software_dev_name", "Teszt Kft."),
	("nav.software_dev_contact", "dev@e.st"),
];

/// `nav_login` must be `Some` or `NavAuth::load` refuses; nothing else here is read.
fn seller() -> Seller {
	Seller {
		id: SELLER_ID,
		nav_base_url: String::new(),
		nav_login: Some("techuser".into()),
		series_code: "A".into(),
		created_at: Timestamp::now(),
	}
}

/// The live version `NavAuth::load` takes `user/taxNumber` out of — its first 8 digits are the
/// only field of it this suite reads.
fn seller_version() -> SellerVersion {
	SellerVersion {
		seller_ver: 1,
		seller_id: SELLER_ID,
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
		created_at: Timestamp::now(),
		valid_from: Some(Timestamp::now()),
		superseded_at: None,
	}
}

async fn mock(server: &MockServer, operation: &str, status: u16, body: String) {
	Mock::given(method("POST"))
		.and(path(format!("/{operation}")))
		.respond_with(ResponseTemplate::new(status).set_body_string(body))
		.mount(server)
		.await;
}

/// What NAV puts in `encodedExchangeToken`: AES-128-ECB under the exchange key, base64'd.
fn encoded_token() -> String {
	let mut block = *GenericArray::from_slice(TOKEN.as_bytes());
	Aes128::new(GenericArray::from_slice(EXCHANGE_KEY)).encrypt_block(&mut block);
	B64.encode(block)
}

const ENVELOPE: &str = concat!(
	r#" xmlns="http://schemas.nav.gov.hu/OSA/3.0/api""#,
	r#" xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common""#,
);

fn token_reply() -> String {
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <TokenExchangeResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <encodedExchangeToken>{}</encodedExchangeToken>\
		 <tokenValidityFrom>2026-09-05T10:00:00.000Z</tokenValidityFrom>\
		 <tokenValidityTo>2026-09-05T10:05:00.000Z</tokenValidityTo>\
		 </TokenExchangeResponse>",
		encoded_token(),
	)
}

fn manage_reply(transaction_id: &str) -> String {
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <ManageInvoiceResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <transactionId>{transaction_id}</transactionId>\
		 </ManageInvoiceResponse>"
	)
}

/// NAV's `GeneralErrorResponse` — a fault it will repeat next time, so never retried.
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

fn status_reply(status: &str, extra: &str) -> String {
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryTransactionStatusResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <processingResults><processingResult><index>1</index>\
		 <invoiceStatus>{status}</invoiceStatus>{extra}\
		 </processingResult></processingResults>\
		 </QueryTransactionStatusResponse>"
	)
}

/// A CDATA-wrapped `transactionId` is a perfectly readable one: it arrives as a `CData`
/// event rather than a `Text` one, which `element_text` used to skip.
#[test]
fn a_cdata_transaction_id_reads_back() {
	let reply = manage_reply("<![CDATA[4NRWX0JI8ZSJJ2SL]]>");
	match accepted(StatusCode::OK, &Reply::parse(&reply)).unwrap() {
		Accepted::Ok { transaction_id } => assert_eq!(transaction_id, "4NRWX0JI8ZSJJ2SL"),
		other @ Accepted::Fault { .. } => panic!("expected the id to read, got {other:?}"),
	}
}

/// `funcCode = OK` means NAV took the invoice. If the `transactionId` is then unreadable, the
/// filing still happened, so calling it a `Fault` would send the job back to re-file an
/// invoice NAV already accepted. It is an `Err` with `Retry::Backoff` instead: the job row
/// keeps the filing, and a resend under the same `requestId` is what NAV refuses.
///
/// The pretty-printed case is the one that mattered: NAV indents its replies, and the
/// whitespace `Text` node *after* `</transactionId>` came back as `Some("")`, which reads to
/// `accepted` as an id. The submission went `SENT` under an empty transaction id and polled
/// against it forever.
#[test]
fn an_ok_reply_with_an_unreadable_transaction_id_is_retryable_not_a_fault() {
	let pretty = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
		<ManageInvoiceResponse xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\" \
		xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\">\n\
		\x20 <common:result>\n\
		\x20   <common:funcCode>OK</common:funcCode>\n\
		\x20 </common:result>\n\
		\x20 <transactionId></transactionId>\n\
		\x20 <software>\n\
		\x20   <softwareId>ABC</softwareId>\n\
		\x20 </software>\n\
		</ManageInvoiceResponse>\n";
	for reply in [pretty.to_owned(), manage_reply(""), manage_reply("   ")] {
		let err = accepted(StatusCode::OK, &Reply::parse(&reply)).unwrap_err();
		assert_eq!(err.parts().1, "E-NAV-NO-TRANSACTION-ID", "{err:?}");
		assert_eq!(err.retry(), saas_core::Retry::Backoff, "{err:?}");
	}
}

/// A reply with no `funcCode` at all is not NAV saying "no" — it is a reply we could not
/// read: a WAF or CDN error page served with a 200, a body an intermediary truncated, or bytes
/// `String::from_utf8_lossy` mangled on the way in. Classifying that as `Fault` means "nothing
/// was filed", which sends the job back to file an invoice NAV may already hold — the same
/// duplicate statutory filing the unreadable-`transactionId` case above guards against.
///
/// On a **4xx** the same unreadable body is the edge — a WAF, a CDN, a misrouted path —
/// rejecting the request before the invoice service ever saw it. Parking that as `UNKNOWN`
/// stranded a statutory filing permanently: `may_send` never resends an `UNKNOWN` row and
/// `unfiled_invoices` excludes it, so there was no automated way out.
#[test]
fn a_reply_with_no_func_code_is_retryable_not_a_fault() {
	for body in [
		"<html><head><title>503 Service Unavailable</title></head><body>nope</body></html>",
		"",
		// Truncated mid-envelope by a proxy, before `result` was ever written.
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?><ManageInvoiceResponse><common:head",
	] {
		let err = accepted(StatusCode::OK, &Reply::parse(body)).unwrap_err();
		assert_eq!(err.parts().1, "E-NAV-UNREADABLE-REPLY", "{body:?}: {err:?}");
		assert_eq!(err.retry(), saas_core::Retry::Backoff, "{err:?}");

		for status in [StatusCode::FORBIDDEN, StatusCode::NOT_FOUND, StatusCode::BAD_REQUEST] {
			match accepted(status, &Reply::parse(body)).unwrap() {
				Accepted::Fault { code, .. } => assert_eq!(code, "E-NAV-HTTP-STATUS"),
				other @ Accepted::Ok { .. } => panic!("expected a fault for {status}: {other:?}"),
			}
		}
	}
	// And a 4xx is transport, not a business rejection, so the job backs off rather than
	// terminating.
	let err = saas_nav::client::business("E-NAV-HTTP-STATUS", "blocked");
	assert_eq!(err.retry(), saas_core::Retry::Backoff, "{err:?}");
}

/// A 4xx that *does* carry NAV's own error XML still reads its own `funcCode` and keeps its
/// own code — the status only decides the no-`funcCode` case.
#[test]
fn a_4xx_carrying_real_nav_error_xml_keeps_navs_own_code() {
	let reply = error_reply("INVALID_SECURITY_USER", "bad user");
	match accepted(StatusCode::BAD_REQUEST, &Reply::parse(&reply)).unwrap() {
		Accepted::Fault { code, .. } => assert_eq!(code, "INVALID_SECURITY_USER"),
		other @ Accepted::Ok { .. } => panic!("expected NAV's own code, got {other:?}"),
	}
}

/// `tokenExchange` answering a 200 whose body is not a NAV reply is an outage, not a
/// credentials failure: `E-NAV-CREDENTIALS` is permanent and would terminate a filing that
/// succeeds once the outage passes.
#[tokio::test]
async fn an_unreadable_token_exchange_reply_is_transport_not_bad_credentials() {
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, "<html><body>maintenance</body></html>".to_owned()).await;

	let db = TmpDb::new("tokenunreadable");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller(), &seller_version())
		.await
		.unwrap();

	let err = client.token_exchange().await.unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-AUTH-UNREADABLE", "{err:?}");
	assert_eq!(err.retry(), saas_core::Retry::Backoff, "{err:?}");

	// A reply that genuinely *is* NAV refusing the credentials keeps its own permanent code.
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 400, error_reply("INVALID_SECURITY_USER", "no")).await;
	let db = TmpDb::new("tokenrejected");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller(), &seller_version())
		.await
		.unwrap();
	let err = client.token_exchange().await.unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-CREDENTIALS", "{err:?}");
}

/// The archive settles a dispute about what was sent; it must not be able to *re-send* it.
/// `passwordHash` is not a derived secret — it is what NAV authenticates on the wire.
#[tokio::test]
async fn a_redacted_request_keeps_its_shape_and_none_of_its_credentials() {
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;

	let db = TmpDb::new("redact");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller(), &seller_version())
		.await
		.unwrap();
	let token = client.token_exchange().await.unwrap();
	let request = client
		.manage_invoice_request(
			NavOp::Create,
			INV_UID,
			&[("<InvoiceData/>".to_owned(), Some(PDF_SHA256.to_owned()))],
			&token,
		)
		.unwrap();

	// What the raw envelope carries, and what the archive must not.
	assert!(request.contains(&format!("<exchangeToken>{TOKEN}</exchangeToken>")), "{request}");
	let hash = between(&request, "<common:passwordHash", "</common:passwordHash>");
	let signature = between(&request, "<common:requestSignature", "</common:requestSignature>");
	assert!(hash.len() > 32, "expected a SHA-512 hex hash, got {hash:?}");
	assert!(signature.len() > 32, "expected a SHA3-512 hex signature, got {signature:?}");

	let archived = saas_nav::auth::redact(&request);
	assert!(!archived.contains(&hash), "the password hash survived redaction");
	assert!(!archived.contains(&signature), "the request signature survived redaction");
	assert!(!archived.contains(TOKEN), "the exchange token survived redaction");

	// Still a document with the same shape: the elements, their attributes and the request
	// id all stay, so the archive still shows what was sent.
	// The exact id, not just the element: this is what fails if `manage_invoice_request`
	// ever goes back to minting its own and the retry stops being idempotent.
	assert!(
		archived.contains(&format!("<common:requestId>{INV_UID}</common:requestId>")),
		"{archived}"
	);
	assert!(archived.contains(r#"<common:passwordHash cryptoType="SHA-512">"#), "{archived}");
	assert!(archived.contains("</common:requestSignature>"), "{archived}");
	assert!(archived.contains("<invoiceOperation>CREATE</invoiceOperation>"), "{archived}");
	assert_eq!(archived.matches("[redacted]").count(), 3, "{archived}");
}

/// The text between an open tag's `>` and its closing tag.
fn between(xml: &str, open: &str, close: &str) -> String {
	let at = xml.find(open).unwrap();
	let from = at + xml[at..].find('>').unwrap() + 1;
	let to = from + xml[from..].find(close).unwrap();
	xml[from..to].to_owned()
}

/// Every `invoiceStatus` `InvoiceStatusType` allows, and the `validationResultCode` that
/// decides a `DONE` between `Done` and `Warn`.
///
/// An `INFO`-only `businessValidationMessages` block is NAV remarking on an invoice it
/// accepted without reservation; `subtree_outcome` decided on the block's *presence*, so it
/// recorded `NavVerdict::Warn` in a statutory archive. A status outside the type used to read
/// as `Pending`, so the poll asked again every ten minutes forever and nothing said why.
#[tokio::test]
async fn query_status_maps_every_terminal_state() {
	let server = MockServer::start().await;
	mock(&server, "queryTransactionStatus", 200, status_reply("DONE", "")).await;

	let db = TmpDb::new("status");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller(), &seller_version())
		.await
		.unwrap();

	let (_, reply) = client
		.post("queryTransactionStatus", &client.query_status_request("TX1"))
		.await
		.unwrap()
		.body("queryTransactionStatus")
		.unwrap();
	// Every `status_reply` fixture carries one result, at index 1.
	let only = |reply: &str| {
		let (idx, outcome, _) = outcomes(&Reply::parse(reply)).unwrap().into_iter().next().unwrap();
		(idx, outcome)
	};
	// The wire and the hand-built fixture parse alike; the rest of the table is local.
	assert_eq!(only(&reply), (1, Outcome::Done));

	let business = |level: &str| {
		format!(
			"<businessValidationMessages><validationResultCode>{level}</validationResultCode>\
			 <validationErrorCode>B1</validationErrorCode><message>note</message>\
			 </businessValidationMessages>"
		)
	};
	let technical = "<technicalValidationMessages>\
		 <validationErrorCode>SCHEMA</validationErrorCode>\
		 <message>malformed</message></technicalValidationMessages>";
	for (status, extra, expected) in [
		("DONE", String::new(), Outcome::Done),
		("RECEIVED", String::new(), Outcome::Pending),
		("PROCESSING", String::new(), Outcome::Pending),
		("SAVED", String::new(), Outcome::Pending),
		("DONE", business("INFO"), Outcome::Done),
		("DONE", business("WARN"), Outcome::Warn),
		("DONE", business("ERROR"), Outcome::Warn),
		// The worst message decides, not the first.
		("DONE", format!("{}{}", business("INFO"), business("WARN")), Outcome::Warn),
		(
			"ABORTED",
			technical.to_owned(),
			Outcome::Failed { code: "SCHEMA".to_owned(), message: "malformed".to_owned() },
		),
		("WEIRD", String::new(), Outcome::Unknown { status: "WEIRD".to_owned() }),
	] {
		assert_eq!(only(&status_reply(status, &extra)), (1, expected), "{status}{extra}");
	}
}

/// A fault on the *query* says nothing about the invoice, which NAV may already have accepted
/// and filed: marking the submission `ERROR` for it would record a filed invoice as rejected,
/// so it is retryable instead.
///
/// `funcCode` and the envelope fault used to be looked up document-wide, so a
/// `returnOriginalRequest=true` reply — which echoes the whole request back — could supply
/// either of them.
#[test]
fn a_query_fault_is_read_from_the_result_block_and_is_retryable() {
	let with_decoy = format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryTransactionStatusResponse{ENVELOPE}>\
		 <common:result><common:funcCode>ERROR</common:funcCode>\
		 <common:errorCode>REAL</common:errorCode>\
		 <common:message>the envelope fault</common:message></common:result>\
		 <processingResults><processingResult><index>1</index>\
		 <invoiceStatus>DONE</invoiceStatus>\
		 <technicalValidationMessages><validationErrorCode>DECOY</validationErrorCode>\
		 <message>not this one</message></technicalValidationMessages>\
		 </processingResult></processingResults>\
		 </QueryTransactionStatusResponse>"
	);
	for (label, reply, code, message) in [
		("a decoy in the echoed request", with_decoy, "REAL", "the envelope fault"),
		(
			"a GeneralErrorResponse",
			error_reply("INVALID_SECURITY_USER", "bad user"),
			"INVALID_SECURITY_USER",
			"bad user",
		),
	] {
		let parsed = Reply::parse(&reply);
		assert!(!parsed.ok(), "{label}");
		assert_eq!(parsed.fault_pair(), (code.to_owned(), message.to_owned()), "{label}");
		assert!(
			matches!(outcomes(&parsed), Err(Outcome::Unavailable { code: ref got, .. }) if got == code),
			"{label}",
		);
	}
}

/// Each verdict belongs to the invoice whose `<index>` carries it. Scanned document-wide,
/// invoice #2's warning made invoice #1 a `Warn`, and #1's `DONE` settled #2.
#[test]
fn a_two_result_reply_keys_each_verdict_on_its_own_index() {
	let reply = format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryTransactionStatusResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <processingResults>\
		 <processingResult><index>1</index><invoiceStatus>DONE</invoiceStatus>\
		 </processingResult>\
		 <processingResult><index>2</index><invoiceStatus>DONE</invoiceStatus>\
		 <businessValidationMessages><validationResultCode>WARN</validationResultCode>\
		 </businessValidationMessages></processingResult>\
		 </processingResults>\
		 </QueryTransactionStatusResponse>"
	);
	let results = outcomes(&Reply::parse(&reply)).unwrap();
	assert_eq!(
		results.iter().map(|(i, o, _)| (*i, o)).collect::<Vec<_>>(),
		vec![(1, &Outcome::Done), (2, &Outcome::Warn)]
	);
	// The third element is the subtree `job::poll` archives on that member's row; archiving
	// the whole reply on all N rows is O(N²).
	assert!(results[1].2.contains("<index>2</index>"), "{:?}", results[1].2);
	assert!(!results[1].2.contains("<index>1</index>"), "{:?}", results[1].2);
}

/// `nav.base_url` used to default to NAV's **test** endpoint, which answers `funcCode=OK`,
/// mints transaction ids and reaches `invoiceStatus=DONE` — so a production deployment that
/// configured credentials and forgot the URL saw every `nav_submissions` row read `DONE` and
/// had reported nothing statutory. The endpoint is now an explicit choice, checked at boot.
#[tokio::test]
async fn an_unset_base_url_refuses_to_boot() {
	let db = TmpDb::new("base-url-unset");
	let app = app(&db, "https://api.onlineszamla.nav.gov.hu/invoiceService/v3").await;
	saas_nav::auth::check_software_settings(&app)
		.await
		.expect("a configured endpoint boots");

	app.settings.set("nav.base_url", "", None).await.unwrap();
	let err = saas_nav::auth::check_software_settings(&app).await.unwrap_err();
	let msg = err.to_string();
	assert!(msg.contains("nav.base_url"), "{msg}");
	// It names both endpoints, so the operator cannot guess wrong.
	assert!(msg.contains("https://api.onlineszamla.nav.gov.hu/invoiceService/v3"), "{msg}");
	assert!(msg.contains("https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3"), "{msg}");
}

/// A 5xx is an outage, so the job returns `Err` and the runner retries it — but *which* outage
/// is recorded: a `502`/`503` is the load balancer and never reached the invoice service; a
/// `500` came from the service itself, which may have taken the invoice before failing.
#[tokio::test]
async fn server_error_is_a_retryable_outage() {
	let server = MockServer::start().await;
	mock(&server, "queryTransactionStatus", 503, String::new()).await;
	mock(&server, "manageInvoice", 500, String::new()).await;

	let db = TmpDb::new("outage");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller(), &seller_version())
		.await
		.unwrap();

	let answer = client.post("queryTransactionStatus", "<x/>").await.unwrap();
	assert!(matches!(answer, Answer::Unavailable), "{answer:?}");
	let err = answer.body("queryTransactionStatus").unwrap_err();
	assert!(format!("{err:?}").contains("E-NAV-UNAVAILABLE"), "{err:?}");

	let answer = client.post("manageInvoice", "<x/>").await.unwrap();
	assert!(matches!(answer, Answer::Indeterminate), "{answer:?}");
	let err = answer.body("manageInvoice").unwrap_err();
	assert!(matches!(err, saas_core::error::Error::Timeout(_)), "{err:?}");

	// Port 1 is not listening, so the connect fails at once: nothing reached NAV, so this is
	// `Unavailable` too and the filing is simply retried.
	let db = TmpDb::new("unreachable");
	let client = NavAuth::load(&app(&db, "http://127.0.0.1:1").await, &seller(), &seller_version())
		.await
		.unwrap();
	let answer = client.post("tokenExchange", "<x/>").await.unwrap();
	assert!(matches!(answer, Answer::Unavailable), "{answer:?}");
}

/// [`mock`] with one response header, which is the whole point of the 429 cases below.
async fn mock_with_header(
	server: &MockServer,
	operation: &str,
	status: u16,
	header: (&str, &str),
	body: String,
) {
	Mock::given(method("POST"))
		.and(path(format!("/{operation}")))
		.respond_with(
			ResponseTemplate::new(status)
				.insert_header(header.0, header.1)
				.set_body_string(body),
		)
		.mount(server)
		.await;
}

/// A 429 used to reach the operation parser, which found no `funcCode`, called it
/// `Fault{E-NAV-HTTP-STATUS}` → `E-NAV-BUSINESS` → `2^attempts` capped at 600 s — and
/// `Retry-After` was discarded by `http::post` before anyone could read it.
#[tokio::test]
async fn a_429_carries_navs_own_delay_into_the_job_row() {
	let server = MockServer::start().await;
	mock_with_header(&server, "manageInvoice", 429, ("retry-after", "30"), String::new()).await;

	let db = TmpDb::new("throttled");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller(), &seller_version())
		.await
		.unwrap();

	let answer = client.post("manageInvoice", "<x/>").await.unwrap();
	assert!(matches!(answer, Answer::Throttled { retry_after: 30 }), "{answer:?}");
	match answer.body("manageInvoice").unwrap_err() {
		saas_core::error::Error::RateLimit(secs) => assert_eq!(secs, 30),
		other => panic!("expected a rate limit, got {other:?}"),
	}
}

/// NAV need not say how long, and the answer is still a throttle rather than a business fault.
#[tokio::test]
async fn a_429_with_no_retry_after_still_throttles() {
	let server = MockServer::start().await;
	mock(&server, "manageInvoice", 429, "<html>slow down</html>".to_owned()).await;

	let db = TmpDb::new("throttled-bare");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller(), &seller_version())
		.await
		.unwrap();

	let answer = client.post("manageInvoice", "<x/>").await.unwrap();
	assert!(matches!(answer, Answer::Throttled { retry_after: 60 }), "{answer:?}");
}

/// A 408 used to be handed to the operation parser as a body: no `funcCode`, so
/// `Fault{E-NAV-HTTP-STATUS}` — "nothing was filed" — and the batch resent under a `requestId`
/// NAV may already have processed, with no §1.9.2 reconciliation anywhere.
#[tokio::test]
async fn a_408_is_indeterminate_so_the_batch_can_be_reconciled() {
	let server = MockServer::start().await;
	mock(&server, "manageInvoice", 408, String::new()).await;

	let db = TmpDb::new("request-timeout");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller(), &seller_version())
		.await
		.unwrap();

	let answer = client.post("manageInvoice", "<x/>").await.unwrap();
	assert!(matches!(answer, Answer::Indeterminate), "{answer:?}");
	let err = answer.body("manageInvoice").unwrap_err();
	assert!(matches!(err, saas_core::error::Error::Timeout(_)), "{err:?}");
}

/// The other 4xx stay a `Reply` for the operation parser, which is what keeps a WAF page an
/// edge refusal — "nothing was filed" — rather than a fate nobody knows.
#[tokio::test]
async fn a_non_xml_4xx_is_still_an_edge_refusal() {
	let server = MockServer::start().await;
	mock(&server, "manageInvoice", 403, "<html><body>blocked</body></html>".to_owned()).await;

	let db = TmpDb::new("waf-page");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller(), &seller_version())
		.await
		.unwrap();

	let (status, xml) = client
		.post("manageInvoice", "<x/>")
		.await
		.unwrap()
		.body("manageInvoice")
		.unwrap();
	assert_eq!(status, StatusCode::FORBIDDEN);
	match accepted(status, &Reply::parse(&xml)).unwrap() {
		Accepted::Fault { code, .. } => assert_eq!(code, "E-NAV-HTTP-STATUS"),
		other @ Accepted::Ok { .. } => panic!("expected an edge refusal, got {other:?}"),
	}
}

/// `client::business` was one blanket `coded_retry`, so a fault that *spends* the
/// `requestId` retried forever against a refusal that can never change. Online Számla 3.0,
/// §requestId: a successfully processed request and one refused with `INVALID_REQUEST_SIGNATURE`
/// or `FORBIDDEN` both burn the id, and `report` re-sends `invoice.uid`, which is immutable on
/// an `ISSUED` row — so one clock-skew signature fault burned the only id that invoice will ever
/// have, and `jobs.max_attempts.NAV_REPORT` defaults to unbounded.
#[test]
fn a_fault_that_spends_the_request_id_terminates_and_everything_else_backs_off() {
	for code in ["INVALID_REQUEST_SIGNATURE", "FORBIDDEN"] {
		let err = saas_nav::client::business(code, "nope");
		assert_eq!(err.retry(), saas_core::Retry::Never, "{code}: {err:?}");
		assert_eq!(err.parts().1, "E-NAV-REQUEST-ID-SPENT");
	}
	// Same `Retry::Never`, opposite remediation — and the `errCode` is what carries that to the
	// operator through `jobs.last_error`. `REQUEST_ID_NOT_UNIQUE` on `manageInvoice` means NAV
	// already *processed* a request under this id, so the invoice may be filed already: the
	// next step is `queryTransactionStatus`, never the storno and re-issue
	// `E-NAV-REQUEST-ID-SPENT`'s message calls for.
	let reused = saas_nav::client::business("REQUEST_ID_NOT_UNIQUE", "nope");
	assert_eq!(reused.retry(), saas_core::Retry::Never, "{reused:?}");
	assert_eq!(reused.parts().1, "E-NAV-REQUEST-ID-REUSED");
	assert!(!format!("{reused}").contains("storno and re-issue"), "{reused}");
	// The catch-all — an outage, bad credentials, and any code NAV adds after this was written
	// — still clears on its own and must not drop every invoice issued while it lasts.
	for code in ["INVALID_SECURITY_USER", "OPERATION_FAILED", "SOMETHING_NAV_ADDS_LATER"] {
		let err = saas_nav::client::business(code, "nope");
		assert_eq!(err.retry(), saas_core::Retry::Backoff, "{code}: {err:?}");
		assert_eq!(err.parts().1, "E-NAV-BUSINESS");
		assert_eq!(saas_nav::client::disposition(code), Disposition::Retry, "{code}");
	}
}

/// A code NAV answers the same way however often it is asked used to retry six times an hour
/// forever — `jobs.max_attempts.NAV_REPORT` is `0` and `jobs.backoff_cap.NAV_REPORT` is 600 —
/// until a person called `Nav::cancel_filing`. It parks instead: no `requestId` was spent, so
/// nothing is stornoed and the row simply waits for an operator.
#[test]
fn a_fault_that_can_never_succeed_is_parked_for_a_person() {
	for code in ["INVOICE_NUMBER_NOT_UNIQUE", "SCHEMA_VIOLATION"] {
		assert_eq!(saas_nav::client::disposition(code), Disposition::NeedsPerson, "{code}");
		let err = saas_nav::client::business(code, "nope");
		assert_eq!(err.retry(), saas_core::Retry::Never, "{code}: {err:?}");
		assert_eq!(err.parts().1, "E-NAV-UNFILABLE", "{code}: {err:?}");
		// Neither remediation this is *not*: no id was burned, and nothing implies a storno.
		let msg = format!("{err}");
		assert!(!msg.contains("storno"), "{msg}");
		assert!(!msg.contains("spent"), "{msg}");
	}
}

/// A `queryTransactionList` page an intermediary cut short used to read as a complete page
/// that simply listed less, with `availablePage` falling back to 1 — so `reconcile` concluded
/// "NAV never took this batch" and re-drove a resend under a `requestId` NAV had burned.
#[test]
fn a_truncated_transaction_list_page_is_an_outage_not_an_empty_window() {
	let whole = format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryTransactionListResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <transactionListResult><currentPage>1</currentPage><availablePage>1</availablePage>\
		 <transaction><transactionId>TX1</transactionId></transaction>\
		 </transactionListResult></QueryTransactionListResponse>"
	);
	assert_eq!(transaction_list(&Reply::parse(&whole)).unwrap(), (vec!["TX1".to_owned()], 1));

	let cut = &whole[..whole.find("</transactionListResult>").unwrap() + 10];
	let err = transaction_list(&Reply::parse(cut)).unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-UNAVAILABLE", "{err:?}");
	assert_eq!(err.retry(), saas_core::Retry::Backoff, "{err:?}");
}

/// The other half of the same cut: a truncated `queryTransactionStatus` reply is a short
/// `results` list, which `reconcile` reads as "none of our invoice numbers, so NAV never took
/// the batch" — and answers with a resend under the burned `requestId`, parking the whole batch.
#[test]
fn a_truncated_status_reply_is_an_outage_not_a_short_result_list() {
	let data = B64.encode("<InvoiceData><invoiceNumber>EX-1</invoiceNumber></InvoiceData>");
	let whole = status_reply("DONE", &format!("<originalRequest>{data}</originalRequest>"));
	assert_eq!(
		original_invoice_numbers(&Reply::parse(&whole)).unwrap(),
		vec![(1, "EX-1".to_owned())]
	);

	// Cut mid-tag after the one complete result, so `truncated` is set and the list is short.
	let cut = &whole[..whole.find("</processingResults>").unwrap() + 10];
	match original_invoice_numbers(&Reply::parse(cut)) {
		Err(Outcome::Unavailable { code, .. }) => assert_eq!(code, "E-NAV-UNAVAILABLE"),
		other => panic!("a truncated reply must not read as a verdict: {other:?}"),
	}
}

/// Clamping `availablePage` silently stopped `reconcile` at page 20, which then concluded "NAV
/// never took this batch" and resent under a burned `requestId`. It is an `Err` now, so the
/// reconciliation retries on a fresher window instead of mis-concluding.
#[test]
fn a_wild_available_page_is_refused_rather_than_capped() {
	let reply = format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <QueryTransactionListResponse{ENVELOPE}>\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <transactionListResult><currentPage>1</currentPage>\
		 <availablePage>100000</availablePage>\
		 </transactionListResult></QueryTransactionListResponse>"
	);
	let err = transaction_list(&Reply::parse(&reply)).unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-UNAVAILABLE");
	assert!(matches!(err.retry(), saas_core::error::Retry::Backoff), "{err:?}");
	// The ceiling itself still reads back fine.
	let ok = reply.replace("<availablePage>100000", "<availablePage>20");
	assert_eq!(
		transaction_list(&Reply::parse(&ok)).unwrap().1,
		saas_nav::client::MAX_TRANSACTION_LIST_PAGES
	);
}

// vim: ts=4
