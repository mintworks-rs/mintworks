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
use saas_invoice::{service_api::SELLER_ID, store::Seller};
use saas_nav::{
	NavOp,
	auth::NavAuth,
	client::{Accepted, Outcome, accepted, outcome},
};
use store_adapter_sqlite::SqliteStore;
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{method, path},
};

/// A filed invoice's `uid`, which is what `manage_invoice_request` sends as the NAV
/// `requestId`. `inv_` plus a 26-character ULID is exactly `EntityIdType`'s 30-char maximum.
const INV_UID: &str = "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV";

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
		data_dir: String::new(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	let store = SqliteStore::open(&config).await.unwrap();
	// Every `saas-core/` step, not `STEPS[..1]`: a correction appends, so the core schema is
	// no longer one leading entry — and `AppBuilder::build`'s reclaim reads `jobs.claimed_at`.
	let core: Vec<_> = store_adapter_sqlite::STEPS
		.iter()
		.filter(|s| s.name.starts_with("saas-core/"))
		.copied()
		.collect();
	store.migrate(&core).await.unwrap();
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
	match accepted(StatusCode::OK, &reply).unwrap() {
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
		let err = accepted(StatusCode::OK, &reply).unwrap_err();
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
		let err = accepted(StatusCode::OK, body).unwrap_err();
		assert_eq!(err.parts().1, "E-NAV-UNREADABLE-REPLY", "{body:?}: {err:?}");
		assert_eq!(err.retry(), saas_core::Retry::Backoff, "{err:?}");

		for status in [StatusCode::FORBIDDEN, StatusCode::NOT_FOUND, StatusCode::BAD_REQUEST] {
			match accepted(status, body).unwrap() {
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
	match accepted(StatusCode::BAD_REQUEST, &reply).unwrap() {
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
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller()).await.unwrap();

	let err = client
		.manage_invoice_request(NavOp::Create, INV_UID, "<InvoiceData/>")
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-AUTH-UNREADABLE", "{err:?}");
	assert_eq!(err.retry(), saas_core::Retry::Backoff, "{err:?}");

	// A reply that genuinely *is* NAV refusing the credentials keeps its own permanent code.
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 400, error_reply("INVALID_SECURITY_USER", "no")).await;
	let db = TmpDb::new("tokenrejected");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller()).await.unwrap();
	let err = client
		.manage_invoice_request(NavOp::Create, INV_UID, "<InvoiceData/>")
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-NAV-CREDENTIALS", "{err:?}");
}

/// The archive settles a dispute about what was sent; it must not be able to *re-send* it.
/// `passwordHash` is not a derived secret — it is what NAV authenticates on the wire.
#[tokio::test]
async fn a_redacted_request_keeps_its_shape_and_none_of_its_credentials() {
	let server = MockServer::start().await;
	mock(&server, "tokenExchange", 200, token_reply()).await;

	let db = TmpDb::new("redact");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller()).await.unwrap();
	let request = client
		.manage_invoice_request(NavOp::Create, INV_UID, "<InvoiceData/>")
		.await
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

#[tokio::test]
async fn query_status_maps_every_terminal_state() {
	let server = MockServer::start().await;
	mock(&server, "queryTransactionStatus", 200, status_reply("DONE", "")).await;

	let db = TmpDb::new("status");
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller()).await.unwrap();

	let (_, reply) = client
		.post("queryTransactionStatus", &client.query_status_request("TX1"))
		.await
		.unwrap();
	assert_eq!(outcome(&reply).unwrap(), Outcome::Done);

	assert_eq!(outcome(&status_reply("RECEIVED", "")).unwrap(), Outcome::Pending);
	assert_eq!(outcome(&status_reply("PROCESSING", "")).unwrap(), Outcome::Pending);
	assert_eq!(
		outcome(&status_reply(
			"DONE",
			"<businessValidationMessages><validationResultCode>WARN</validationResultCode>\
			 </businessValidationMessages>",
		))
		.unwrap(),
		Outcome::Warn,
	);
	assert!(matches!(
		outcome(&status_reply(
			"ABORTED",
			"<technicalValidationMessages><validationErrorCode>SCHEMA</validationErrorCode>\
			 <message>malformed</message></technicalValidationMessages>",
		))
		.unwrap(),
		Outcome::Failed { .. }
	));
	// A fault on the *query* says nothing about the invoice, which NAV may already have
	// accepted and filed. Marking the submission `ERROR` for it would record a filed invoice
	// as rejected, so it is retryable instead.
	assert!(matches!(
		outcome(&error_reply("INVALID_SECURITY_USER", "bad user")).unwrap(),
		Outcome::Unavailable { ref code, .. } if code == "INVALID_SECURITY_USER"
	));
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
	let client = NavAuth::load(&app(&db, &server.uri()).await, &seller()).await.unwrap();

	let err = client.post("queryTransactionStatus", "<x/>").await.unwrap_err();
	assert!(format!("{err:?}").contains("E-NAV-UNAVAILABLE"), "{err:?}");

	let err = client.post("manageInvoice", "<x/>").await.unwrap_err();
	assert!(matches!(err, saas_core::error::Error::Timeout(_)), "{err:?}");

	// Port 1 is not listening, so the connect fails at once: nothing reached NAV, so this is
	// `unavailable()` too and the filing is simply retried.
	let db = TmpDb::new("unreachable");
	let client = NavAuth::load(&app(&db, "http://127.0.0.1:1").await, &seller()).await.unwrap();
	let err = client.post("tokenExchange", "<x/>").await.unwrap_err();
	assert!(format!("{err:?}").contains("E-NAV-UNAVAILABLE"), "{err:?}");
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
	// next step is `queryTransactionStatus`. Sharing `E-NAV-REQUEST-ID-SPENT`'s message told
	// the operator to storno and re-issue an invoice that was on file — which is exactly what a
	// POST that succeeded and then lost its `transactionId` to a `set_sent` write failure hits.
	let reused = saas_nav::client::business("REQUEST_ID_NOT_UNIQUE", "nope");
	assert_eq!(reused.retry(), saas_core::Retry::Never, "{reused:?}");
	assert_eq!(reused.parts().1, "E-NAV-REQUEST-ID-REUSED");
	assert!(!format!("{reused}").contains("storno and re-issue"), "{reused}");
	// The majority — an outage, bad credentials, a schema complaint — still clears on its own
	// and must not drop every invoice issued while it lasts.
	for code in ["INVALID_SECURITY_USER", "OPERATION_FAILED", "SCHEMA_VIOLATION"] {
		let err = saas_nav::client::business(code, "nope");
		assert_eq!(err.retry(), saas_core::Retry::Backoff, "{code}: {err:?}");
		assert_eq!(err.parts().1, "E-NAV-BUSINESS");
	}
}

// vim: ts=4
