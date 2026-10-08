// SPDX-License-Identifier: MPL-2.0
//! The route bundles, driven the way the SPA drives them.
//!
//! A handler decides nothing, so what is asserted here is the wire: the camelCase shape, the
//! status codes, the `errCode`s and the cursor. The behaviour behind them is
//! `tests/billing.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use mintworks_billing::provider::{
	CallbackRef, PaymentProvider, PaymentProviders, PaymentState, ProviderCaps, RefundResult,
	StartPayment, StartedPayment,
};
use mintworks_billing::store::BillingStore;
use mintworks_core::error::StatusCode;
use mintworks_core::{App, AppBuilder, config::Config, ctx::Ctx, ids::SellerId, prelude::*};
use mintworks_invoice::{
	draft::{Line, NewDraft, Party},
	service_api::Invoices,
	store::{Invoice, InvoiceStore, Seller, SellerVersionPatch},
	vat::VatCode,
};
use mintworks_store_sqlite::SqliteStore;

const ORG: i64 = 1;

/// The platform root, moved off its natural id 1 so the fixture's own org can have it. The
/// framework finds the root by `kind = 'ROOT'` and never by its value.
const ROOT: i64 = 0;

/// The fixture's one seller. `put_seller` does not autoincrement, so the id is chosen here.
const SELLER: i64 = 1;
const ACCOUNT_UID: &str = "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A";
const ORG_UID: &str = "org_01JCZ5X8K9N7QW3M6R2T4V8Y0C";

/// One line of `NET` at 27%, so an invoice's `gross` is [`GROSS`]. Whole forints, since the VAT
/// engine rounds the group VAT to the currency's step.
const NET: i64 = 100_000;
const GROSS: i64 = NET + NET * 2700 / 10000;

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-billing-routes-{}-{name}", std::process::id()));
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

/// A gateway that never moves: every test here is about the wire, not about settlement. Two
/// are registered, because `GET /api/payment-providers` sorts and one of anything is sorted.
struct Stub(&'static str);

#[async_trait]
impl PaymentProvider for Stub {
	fn id(&self) -> &str {
		self.0
	}

	fn capabilities(&self) -> ProviderCaps {
		ProviderCaps { partial_refund: true, ..ProviderCaps::default() }
	}

	async fn start(&self, _req: &StartPayment) -> ClResult<StartedPayment> {
		Ok(StartedPayment {
			provider_ref: format!("prv-{}", ulid_ish()),
			redirect_url: Some("https://stub.invalid/pay".to_string()),
			state: PaymentState::Pending,
		})
	}

	async fn fetch_state(&self, _provider_ref: &str) -> ClResult<PaymentState> {
		Ok(PaymentState::Pending)
	}

	async fn refund(
		&self,
		_provider_ref: &str,
		amount: Money,
		_request_id: &str,
	) -> ClResult<RefundResult> {
		Ok(RefundResult { refunded: amount, state: PaymentState::Refunded })
	}

	async fn charge_recurring(
		&self,
		_token: &str,
		_req: &StartPayment,
	) -> ClResult<StartedPayment> {
		Ok(StartedPayment {
			provider_ref: "rec-1".into(),
			redirect_url: None,
			state: PaymentState::Pending,
		})
	}

	fn parse_callback(&self, _headers: &HeaderMap, body: &[u8]) -> ClResult<CallbackRef> {
		Ok(CallbackRef { provider_ref: String::from_utf8_lossy(body).into_owned() })
	}
}

/// `idx_payment_provider_ref` is unique over `(provider, provider_ref)`, so the stub has to
/// mint a distinct one per payment exactly as a real gateway does.
fn ulid_ish() -> String {
	mintworks_core::ids::PaymentId::generate().into_string()
}

async fn setup(db: &TmpDb) -> (App, Invoices, SqliteStore) {
	let config = || Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: "https://app.invalid".into(),
		jobs_workers: None,
	};
	let store = SqliteStore::open(&config()).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();

	let app = AppBuilder::new()
		.config(config())
		.store(Arc::new(store.clone()) as Arc<dyn mintworks_core::store::CoreStore>)
		.settings(mintworks_invoice::SETTINGS)
		.settings(mintworks_billing::SETTINGS)
		.extension(Arc::new(store.clone()) as Arc<dyn InvoiceStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn BillingStore>)
		.extension(Arc::new(
			PaymentProviders::new()
				.with(Arc::new(Stub("stub")))
				.with(Arc::new(Stub("another"))),
		))
		.build()
		.await
		.unwrap();

	// `ACTIVE`, the membership `require_auth` joins on, and a root `OWNER` membership:
	// `require_operator` re-resolves the role from the tree rather than trusting the token's
	// `opr`, so all three are what stands between this fixture and a 403 on every route.
	sqlx::query(
		"INSERT INTO accounts (id, uid, email, status, created_at)
		 VALUES (1, ?, 't@e.st', 'ACTIVE', 0)",
	)
	.bind(ACCOUNT_UID)
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
		 VALUES (?, ?, (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
	)
	.bind(ORG)
	.bind(ORG_UID)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 1, 'OWNER', 0, 0),
		        ((SELECT id FROM orgs WHERE kind = 'ROOT'), 1, 'OWNER', 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  email, is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', ?, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 'vevo@e.st', 1, 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();

	store
		.put_seller(&Seller {
			id: SELLER,
			uid: SellerId::generate(),
			org_id: ORG,
			nav_base_url: "https://api-test.onlineszamla.nav.gov.hu".into(),
			nav_login: None,
			series_code: "A".into(),
			closed_at: None,
			payment_days: None,
			created_at: Timestamp::now(),
		})
		.await
		.unwrap();
	store
		.save_seller_version_draft(
			SELLER,
			&SellerVersionPatch {
				name: Some("Teszt Kft.".into()),
				country: Some("HU".into()),
				tax_number: Some("12345678242".into()),
				postcode: Some("1011".into()),
				city: Some("Budapest".into()),
				street: Some("Fo utca 1.".into()),
				vat_scheme: Some("NORMAL".into()),
				..Default::default()
			},
		)
		.await
		.unwrap();
	store
		.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();

	let invoices = Invoices::new(app.clone());
	(app, invoices, store)
}

fn router(app: &App) -> axum::Router {
	let gate = mintworks_core::auth_mw::RouteGate::none();
	mintworks_billing::routes::org(&gate)
		.merge(mintworks_billing::routes::operator(&gate))
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone())
}

/// A real access token for account 1 on org 1, `opr` so the operator routes are reachable.
async fn token(app: &App) -> String {
	let key = app
		.secrets
		.get_or_create(mintworks_core::auth_mw::JWT_SECRET_KEY, 32)
		.await
		.unwrap();
	let claims = mintworks_core::auth_mw::Claims {
		sub: ACCOUNT_UID.to_owned(),
		org: Some(ORG_UID.to_owned()),
		rol: Some("OWNER".to_owned()),
		opr: true,
		ep: 0,
		auth_at: Some(Timestamp::now().0),
		ses: None,
		imp: None,
		typ: None,
		iat: 0,
		exp: i64::from(u32::MAX),
	};
	jsonwebtoken::encode(
		&jsonwebtoken::Header::default(),
		&claims,
		&jsonwebtoken::EncodingKey::from_secret(&key),
	)
	.unwrap()
}

async fn call(
	router: &axum::Router,
	method: &str,
	uri: &str,
	token: &str,
	body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
	use http_body_util::BodyExt as _;
	let req = axum::http::Request::builder()
		.method(method)
		.uri(uri)
		.header("authorization", format!("Bearer {token}"))
		.header("content-type", "application/json")
		.body(axum::body::Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
		.unwrap();
	let res = tower::ServiceExt::oneshot(router.clone(), req).await.unwrap();
	let status = res.status();
	let bytes = res.into_body().collect().await.unwrap().to_bytes();
	(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

fn ctx() -> Ctx {
	Ctx::system("test").with_org(ORG)
}

async fn issued(app: &App, invoices: &Invoices, store: &SqliteStore) -> Invoice {
	let d = invoices
		.draft(
			&ctx(),
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![Line {
					code: None,
					description: "Tanacsadas".into(),
					unit: "ora".into(),
					qty: Qty(1_000_000),
					unit_price: Some(Money(NET)),
					vat_code: Some(VatCode::Std27),
					discount: None,
					discount_description: None,
					note: None,
				}],
				..Default::default()
			},
		)
		.await
		.unwrap();
	mintworks_invoice::issue::run(app, store, d).await.unwrap()
}

fn err_code(body: &serde_json::Value) -> &str {
	body["error"]["errCode"].as_str().unwrap_or("")
}

/// serde drops unknown query parameters silently, so a filter the struct does not carry is
/// answered with everything rather than refused.
#[tokio::test]
async fn the_documented_filters_actually_filter() {
	let db = TmpDb::new("filters");
	let (app, invoices, store) = setup(&db).await;
	let inv = issued(&app, &invoices, &store).await;
	let r = router(&app);
	let t = token(&app).await;

	// One gateway payment, left PENDING, and one manual entry born SUCCEEDED.
	let (status, _) = call(
		&r,
		"POST",
		&format!("/api/invoices/{}/pay", inv.uid.as_str()),
		&t,
		Some(serde_json::json!({
			"provider": "stub",
			"returnUrl": "https://app.invalid/done",
		})),
	)
	.await;
	assert_eq!(status, StatusCode::CREATED);

	let (status, _) = call(
		&r,
		"POST",
		"/api/admin/payments",
		&t,
		Some(serde_json::json!({
			"orgUid": ORG_UID,
			"kind": "TRANSFER",
			"amount": { "amount": "1000.00", "currency": "HUF" },
			"receivedAt": Timestamp::now(),
		})),
	)
	.await;
	assert_eq!(status, StatusCode::CREATED);

	let all = call(&r, "GET", "/api/payments", &t, None).await.1;
	assert_eq!(all["items"].as_array().unwrap().len(), 2);

	// Each filter alone, and they are ANDed.
	let one = call(&r, "GET", "/api/payments?status=SUCCEEDED", &t, None).await.1;
	assert_eq!(one["items"].as_array().unwrap().len(), 1);
	assert_eq!(one["items"][0]["status"], "SUCCEEDED");

	let one = call(&r, "GET", "/api/payments?kind=TRANSFER", &t, None).await.1;
	assert_eq!(one["items"].as_array().unwrap().len(), 1);
	assert_eq!(one["items"][0]["kind"], "TRANSFER");

	let one = call(&r, "GET", "/api/payments?provider=stub", &t, None).await.1;
	assert_eq!(one["items"].as_array().unwrap().len(), 1);
	assert_eq!(one["items"][0]["provider"], "stub");

	// `invoiceUid` matches through `payment_allocations`: `payments` has no invoice column.
	let one = call(&r, "GET", &format!("/api/payments?invoiceUid={}", inv.uid.as_str()), &t, None)
		.await
		.1;
	assert_eq!(one["items"].as_array().unwrap().len(), 1);
	assert_eq!(one["items"][0]["provider"], "stub");

	// ANDed, so a pair that cannot both hold is empty rather than everything.
	let none = call(&r, "GET", "/api/payments?status=SUCCEEDED&provider=stub", &t, None)
		.await
		.1;
	assert!(none["items"].as_array().unwrap().is_empty());

	let none = call(&r, "GET", "/api/payments?receivedFrom=2099-01-01T00%3A00%3A00Z", &t, None)
		.await
		.1;
	assert!(none["items"].as_array().unwrap().is_empty());

	// A bad filter value is the client's mistake: `PaymentState::FromStr` answers
	// `Error::internal`, which made a lowercased query string a 500.
	let (status, body) = call(&r, "GET", "/api/payments?status=paid", &t, None).await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(body["error"]["errCode"], "E-CORE-VALIDATION");
}

/// `mintworks_core::ids`: only a `uid` ever appears in a response body — a sequential integer leaks
/// row volume and invites enumeration. `payments.id` is global rather than per org, so
/// `GET /api/payments?limit=1` handed any org a cross-org volume oracle.
#[tokio::test]
async fn the_cursor_is_a_uid_and_pages() {
	let db = TmpDb::new("cursor");
	let (app, invoices, store) = setup(&db).await;
	let inv = issued(&app, &invoices, &store).await;
	let r = router(&app);
	let t = token(&app).await;

	for _ in 0..3 {
		let (status, _) = call(
			&r,
			"POST",
			"/api/admin/payments",
			&t,
			Some(serde_json::json!({
				"orgUid": ORG_UID,
				"kind": "TRANSFER",
				"amount": { "amount": "10.00", "currency": "HUF" },
				"receivedAt": Timestamp::now(),
			})),
		)
		.await;
		assert_eq!(status, StatusCode::CREATED);
	}

	let page = call(&r, "GET", "/api/payments?limit=1", &t, None).await.1;
	let cursor = page["nextCursor"].as_str().expect("a full page carries a cursor");
	assert!(cursor.starts_with("pay_"), "{cursor}");
	let first = page["items"][0]["uid"].as_str().unwrap().to_owned();

	let next = call(&r, "GET", &format!("/api/payments?limit=1&cursor={cursor}"), &t, None)
		.await
		.1;
	assert_ne!(next["items"][0]["uid"].as_str().unwrap(), first, "the page moved on");

	// "another org's cursor is not a cursor": an unknown uid is an empty page, never an
	// error that would disclose which uids exist.
	let (status, body) =
		call(&r, "GET", "/api/payments?cursor=pay_01JCZ5X8K9N7QW3M6R2T4V8Y0Z", &t, None).await;
	assert_eq!(status, StatusCode::OK);
	assert!(body["items"].as_array().unwrap().is_empty());

	// And the invoice's own payments still read back, which is the SPA's cold path.
	let mine = call(&r, "GET", &format!("/api/invoices/{}/payments", inv.uid.as_str()), &t, None)
		.await
		.1;
	assert!(mine["items"].as_array().unwrap().is_empty(), "no payment names this invoice");
}

/// `amount_in` returned `(Money, CurrencyCode)` and both callers kept only `.0`, so a refund
/// posted as `{"amount":"10.00","currency":"EUR"}` against a HUF payment gave back 10.00 **HUF**
/// and answered 200. `E-PAY-CURRENCY` is in the registry for exactly this.
#[tokio::test]
async fn a_declared_currency_that_does_not_match_is_refused() {
	let db = TmpDb::new("wire-currency");
	let (app, invoices, store) = setup(&db).await;
	let inv = issued(&app, &invoices, &store).await;
	let r = router(&app);
	let t = token(&app).await;

	let (status, payment) = call(
		&r,
		"POST",
		"/api/admin/payments",
		&t,
		Some(serde_json::json!({
			"orgUid": ORG_UID,
			"kind": "TRANSFER",
			"amount": { "amount": "1270.00", "currency": "HUF" },
			"receivedAt": Timestamp::now(),
		})),
	)
	.await;
	assert_eq!(status, StatusCode::CREATED);
	let uid = payment["uid"].as_str().unwrap();

	let (status, body) = call(
		&r,
		"POST",
		&format!("/api/admin/payments/{uid}/allocations"),
		&t,
		Some(serde_json::json!({
			"invoiceUid": inv.uid.as_str(),
			"amount": { "amount": "10.00", "currency": "EUR" },
		})),
	)
	.await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(err_code(&body), "E-PAY-CURRENCY");

	let (status, body) = call(
		&r,
		"POST",
		&format!("/api/admin/payments/{uid}/refund"),
		&t,
		Some(serde_json::json!({ "amount": { "amount": "10.00", "currency": "EUR" } })),
	)
	.await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(err_code(&body), "E-PAY-CURRENCY");

	// The invoice was never touched by either.
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().paid_amount, Money::ZERO);
}

/// The documented wire shape: camelCase throughout, an amount as a string with its currency,
/// and a `pay_` uid rather than a row id.
#[tokio::test]
async fn the_payment_view_is_the_documented_shape() {
	let db = TmpDb::new("wire-shape");
	let (app, invoices, store) = setup(&db).await;
	let inv = issued(&app, &invoices, &store).await;
	let r = router(&app);
	let t = token(&app).await;

	let (status, body) = call(
		&r,
		"POST",
		&format!("/api/invoices/{}/pay", inv.uid.as_str()),
		&t,
		Some(serde_json::json!({
			"provider": "stub",
			"returnUrl": "https://app.invalid/done",
			"requestId": "req-wire",
		})),
	)
	.await;
	assert_eq!(status, StatusCode::CREATED);

	let p = &body["payment"];
	assert!(p["uid"].as_str().unwrap().starts_with("pay_"));
	assert!(p["id"].is_null(), "the row id never reaches the wire");
	assert_eq!(p["status"], "PENDING");
	assert_eq!(p["provider"], "stub");
	assert_eq!(p["requestId"], "req-wire");
	// `{"amount":"1270.00","currency":"HUF"}` — a string, because this codebase has no floats.
	assert_eq!(p["amount"]["amount"], Money(GROSS).to_decimal_string());
	assert_eq!(p["amount"]["currency"], "HUF");
	assert_eq!(p["refundedAmount"]["amount"], "0.00");
	assert_eq!(body["redirectUrl"], "https://stub.invalid/pay");
	// The zero link row is what a returning payer's page follows back to the invoice.
	assert_eq!(p["allocations"][0]["invoiceUid"], inv.uid.as_str());
	assert_eq!(p["allocations"][0]["amount"]["amount"], "0.00");
}

/// The gateway sends the payer wherever `returnUrl` points, so a checkout naming a foreign one
/// would land them on somebody else's page off a genuine success. A bare `starts_with` let
/// `https://app.invalid.evil.example/` through.
#[tokio::test]
async fn a_return_url_outside_the_base_url_is_refused() {
	let db = TmpDb::new("wire-return-url");
	let (app, invoices, store) = setup(&db).await;
	let inv = issued(&app, &invoices, &store).await;
	let r = router(&app);
	let t = token(&app).await;

	for bad in ["https://evil.example/", "https://app.invalid.evil.example/steal"] {
		let (status, body) = call(
			&r,
			"POST",
			&format!("/api/invoices/{}/pay", inv.uid.as_str()),
			&t,
			Some(serde_json::json!({ "provider": "stub", "returnUrl": bad })),
		)
		.await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
		assert_eq!(err_code(&body), "E-PAY-RETURN-URL", "{bad}");
	}

	// And the guard is not over-tightened: a path, a query and a fragment are all under it.
	for good in [
		"https://app.invalid",
		"https://app.invalid/done?x=1",
		"https://app.invalid/done#ok",
	] {
		let (status, _) = call(
			&r,
			"POST",
			&format!("/api/invoices/{}/pay", inv.uid.as_str()),
			&t,
			Some(serde_json::json!({ "provider": "stub", "returnUrl": good })),
		)
		.await;
		assert_eq!(status, StatusCode::CREATED, "{good}");
	}
}

/// `ProviderCaps` carried no `rename_all`, so the one field with two words went out as
/// `partial_refund` while `api-surface.md` and the SPA's `types.ts` both say `partialRefund` —
/// the button that offers a partial refund read `undefined`.
#[tokio::test]
async fn the_provider_list_is_sorted_and_camel_case() {
	let db = TmpDb::new("providers");
	let (app, _invoices, _store) = setup(&db).await;
	let r = router(&app);
	let t = token(&app).await;

	let (status, body) = call(&r, "GET", "/api/payment-providers", &t, None).await;
	assert_eq!(status, StatusCode::OK);
	// `PaymentProviders` is a `HashMap`, so an unsorted list made the SPA's `items[0]` a
	// different gateway between requests.
	let ids: Vec<&str> = body["items"]
		.as_array()
		.unwrap()
		.iter()
		.map(|p| p["id"].as_str().unwrap())
		.collect();
	assert_eq!(ids, vec!["another", "stub"]);

	let raw = body.to_string();
	assert!(raw.contains("\"partialRefund\""), "{raw}");
	assert!(!raw.contains("partial_refund"), "{raw}");
}

/// The status is client text and `PaymentState::FromStr` answers `Error::internal`, so mapping
/// it in `FromStr` would make a typed query string a 500.
#[tokio::test]
async fn an_unknown_status_filter_is_a_validation_error() {
	let db = TmpDb::new("bad-status");
	let (app, _invoices, _store) = setup(&db).await;
	let r = router(&app);
	let t = token(&app).await;

	let (status, body) = call(&r, "GET", "/api/payments?status=NOPE", &t, None).await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(err_code(&body), "E-CORE-VALIDATION");
}

// vim: ts=4
