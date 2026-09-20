//! The example's verification: the `Bookings` handle end to end — booking -> checkout ->
//! pay -> re-checkout — plus the HTTP half, driven through `routes::api()` with
//! `tower::ServiceExt::oneshot`. Against a real file database: `sqlite::memory:` gives each
//! connection its own database, so the store needs a file.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::http::StatusCode;
use saas_billing::provider::PaymentState;
use saas_core::AppBuilder;
use saas_core::app::App;
use saas_core::config::Config;
use saas_core::ctx::{Actor, Ctx};
use saas_core::ids::SellerId;
use saas_core::prelude::*;
use saas_core::store::CoreStore;
use saas_invoice::store::{
	InvoiceStatus, InvoiceStore, PaymentMethod, SellerVersionPatch, ServiceDef,
};
use saas_invoice::{Invoices, NewDraft, Seller, VatCode};
use store_adapter_sqlite::{FRAMEWORK, SqliteStore};

use saas_example::bookings::{BookRequest, Bookings, CheckoutRequest, PayMethod};
use saas_example::routes;
use saas_example::store::{BookingStore, EXAMPLE};

/// The fixture's one seller. `put_seller` does not autoincrement, so the id is chosen here.
const SELLER: i64 = 1;

/// The platform root, moved off its natural id 1 so the customer org can have it. The
/// framework finds the root by `kind = 'ROOT'` and never by its value.
const ROOT: i64 = 0;

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("example-flow-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [7; 32],
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

/// A gateway that answers from nothing. `start` is the only method a checkout reaches; the
/// settlement half is `crates/saas-billing/tests/billing.rs`'s.
struct StubGateway;

#[async_trait::async_trait]
impl saas_billing::provider::PaymentProvider for StubGateway {
	#[allow(clippy::unnecessary_literal_bound)] // the trait ties the lifetime to `&self`
	fn id(&self) -> &str {
		"stub"
	}

	fn capabilities(&self) -> saas_billing::provider::ProviderCaps {
		saas_billing::provider::ProviderCaps::default()
	}

	async fn start(
		&self,
		req: &saas_billing::provider::StartPayment,
	) -> ClResult<saas_billing::provider::StartedPayment> {
		Ok(saas_billing::provider::StartedPayment {
			provider_ref: format!("prv-{}", req.request_id),
			redirect_url: Some(format!("https://stub.invalid/pay/{}", req.request_id)),
			state: saas_billing::provider::PaymentState::Pending,
		})
	}

	async fn fetch_state(
		&self,
		_provider_ref: &str,
	) -> ClResult<saas_billing::provider::PaymentState> {
		Ok(saas_billing::provider::PaymentState::Pending)
	}

	async fn refund(
		&self,
		_provider_ref: &str,
		amount: Money,
		_request_id: &str,
	) -> ClResult<saas_billing::provider::RefundResult> {
		Ok(saas_billing::provider::RefundResult {
			refunded: amount,
			state: saas_billing::provider::PaymentState::Refunded,
		})
	}

	async fn charge_recurring(
		&self,
		_token: &str,
		_req: &saas_billing::provider::StartPayment,
	) -> ClResult<saas_billing::provider::PaymentState> {
		Ok(saas_billing::provider::PaymentState::Pending)
	}

	fn parse_callback(
		&self,
		_headers: &axum::http::HeaderMap,
		body: &[u8],
	) -> ClResult<saas_billing::provider::CallbackRef> {
		Ok(saas_billing::provider::CallbackRef {
			provider_ref: String::from_utf8_lossy(body).into_owned(),
		})
	}
}

fn by_card() -> CheckoutRequest {
	CheckoutRequest { method: PayMethod::Card, provider: Some("stub".to_owned()) }
}

/// The chain the foreign keys demand before a draft is possible: one account, org 1, its
/// **default** billing party (what `Party::OrgDefault` resolves to) and `SELLER` under `ROOT` —
/// the layout `seed.rs` ships, where the customer's org is not the seller's.
async fn setup(db: &TmpDb, sql: &SqliteStore) -> App {
	// The same six the composition root registers: `routes::api()` reaches all of them. The
	// settings slices are the composition root's too, so this is also where a duplicate key or
	// two keys sharing one environment variable would refuse to boot.
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(sql.clone()) as Arc<dyn CoreStore>)
		.settings(saas_auth::SETTINGS)
		.settings(saas_email::SETTINGS)
		.settings(saas_invoice::SETTINGS)
		.settings(saas_nav::SETTINGS)
		.settings(saas_billing::SETTINGS)
		.settings(saas_example::SETTINGS)
		.secrets(saas_auth::SECRETS)
		.secrets(saas_email::SECRETS)
		.secrets(saas_nav::SECRETS)
		.secrets(payment_adapter_barion::SECRETS)
		.setting_default("email.from", "noreply@example.test")
		.setting_default("email.smtp.host", "127.0.0.1")
		.setting_default("nav.software_id", "SAASEXAMPLE0000001")
		.setting_default("nav.software_name", "saas-framework example")
		.setting_default("nav.software_operation", "LOCAL_SOFTWARE")
		.setting_default("nav.software_main_version", "0.1")
		.setting_default("nav.software_dev_name", "saas-framework")
		.setting_default("nav.software_dev_contact", "dev@example.com")
		.setting_default("nav.software_dev_country", "HU")
		.extension(Arc::new(sql.clone()) as Arc<dyn saas_auth::store::AuthStore>)
		.extension(Arc::new(sql.clone()) as Arc<dyn InvoiceStore>)
		.extension(Arc::new(sql.clone()) as Arc<dyn BookingStore>)
		.extension(Arc::new(sql.clone()) as Arc<dyn saas_billing::BillingStore>)
		// One stub gateway, so both halves of the checkout choice are reachable. The offline
		// demo registers none, and then only bank transfer is offered.
		.extension(Arc::new(
			saas_billing::PaymentProviders::new().with(Arc::new(StubGateway)),
		))
		.extension(Arc::new(sql.clone()) as Arc<dyn saas_nav::store::NavStore>)
		.build()
		.await
		.unwrap();

	// `ACTIVE`, and with the membership `require_auth` joins on: the HTTP tests mint a real
	// access token, and both are what `verify` reads before it hands a handler a `Ctx`.
	sqlx::query(
		"INSERT INTO accounts (id, uid, email, status, created_at)
		 VALUES (1, 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A', 't@e.st', 'ACTIVE', 0)",
	)
	.execute(sql.writer())
	.await
	.unwrap();
	sqlx::query("UPDATE orgs SET id = ? WHERE kind = 'ROOT'")
		.bind(ROOT)
		.execute(sql.writer())
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (1, 'org_01JCZ5X8K9N7QW3M6R2T4V8Y0C', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
	)
	.execute(sql.writer())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (1, 1, 'OWNER', 0, 0)",
	)
	.execute(sql.writer())
	.await
	.unwrap();
	// `consents_required` fails closed — a gating kind published in no locale gates every
	// route — so the HTTP tests need both documents and the account's acceptance of them.
	for (id, kind) in [(1, "TOS"), (2, "PRIVACY")] {
		sqlx::query(
			"INSERT INTO legal_docs
			   (id, kind, locale, version, title, body, sha256, effective_from, created_at)
			 VALUES (?, ?, 'hu', 'v1', 't', 'b', 'deadbeef', 0, 0)",
		)
		.bind(id)
		.bind(kind)
		.execute(sql.writer())
		.await
		.unwrap();
		sqlx::query(
			"INSERT INTO consents
			   (account_id, kind, legal_doc_id, doc_version, doc_sha256, granted, at)
			 VALUES (1, ?, ?, 'v1', 'deadbeef', 1, 0)",
		)
		.bind(kind)
		.bind(id)
		.execute(sql.writer())
		.await
		.unwrap();
	}
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', 1, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 1, 0, 0)",
	)
	.execute(sql.writer())
	.await
	.unwrap();
	sql.put_seller(&Seller {
		id: SELLER,
		uid: SellerId::generate(),
		org_id: ROOT,
		nav_base_url: String::new(),
		nav_login: None,
		series_code: "EX".into(),
		created_at: Timestamp::now(),
	})
	.await
	.unwrap();
	// One published version, because `issue` refuses a seller that has none.
	sql.save_seller_version_draft(
		SELLER,
		&SellerVersionPatch {
			name: Some("Példa Szolgáltató Kft.".into()),
			country: Some("HU".into()),
			tax_number: Some("12345678242".into()),
			postcode: Some("1051".into()),
			city: Some("Budapest".into()),
			street: Some("Példa utca 1.".into()),
			..Default::default()
		},
	)
	.await
	.unwrap();
	sql.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(())).await.unwrap();

	// The same two rows `seed::services` upserts, so the test bills the catalogue the
	// application ships rather than a fixture of its own.
	Invoices::new(app.clone())
		.sync_services(
			&Ctx::system("test").with_org(1),
			&[
				ServiceDef {
					code: "CONSULT".to_owned(),
					name: "Consulting".to_owned(),
					description: None,
					unit: "hour".to_owned(),
					unit_price: Money(1_500_000),
					vat_code: VatCode::Std27,
				},
				ServiceDef {
					code: "SITEVISIT".to_owned(),
					name: "Site visit".to_owned(),
					description: None,
					unit: "occasion".to_owned(),
					unit_price: Money(2_500_000),
					vat_code: VatCode::Std27,
				},
			],
		)
		.await
		.unwrap();

	app
}

#[tokio::test]
async fn booking_to_issued_invoice() {
	let db = TmpDb::new("flow");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[FRAMEWORK, EXAMPLE]).await.unwrap();
	let app = setup(&db, &sql).await;

	let ctx = Ctx::system("test").with_org(1);
	let bookings = Bookings::new(app.clone());

	bookings
		.book(
			&ctx,
			&BookRequest {
				service_code: "CONSULT".to_owned(),
				occurred_on: "2026-09-10".to_owned(),
				qty_e6: 2_500_000,
				note: Some("kickoff".to_owned()),
			},
		)
		.await
		.unwrap();
	bookings
		.book(
			&ctx,
			&BookRequest {
				service_code: "SITEVISIT".to_owned(),
				occurred_on: "2026-09-11".to_owned(),
				qty_e6: 1_000_000,
				note: None,
			},
		)
		.await
		.unwrap();

	let done = bookings
		.checkout(&ctx, &by_card())
		.await
		.unwrap()
		.expect("two bookings to bill");
	let draft = done.invoice;
	// Locked, not issued: the number is allocated when the money lands, and until then the
	// gateway is holding a charge against exactly these totals.
	assert!(matches!(draft.status, InvoiceStatus::Pending), "a card checkout locks its draft");
	assert!(done.redirect_url.is_some(), "the browser has somewhere to go");
	// VAT once per rate group on the summed net, never per line: (37 500 + 25 000) * 27%.
	assert_eq!(draft.net, Money(6_250_000));
	assert_eq!(draft.vat, Money(1_687_500));

	let uid = draft.uid.to_string();
	let full = Invoices::new(app.clone()).full(&ctx, &uid).await.unwrap();
	let lines = full.lines.expect("a hydrated draft carries its lines");
	assert_eq!(lines.len(), 2);
	assert_eq!(lines[0].qty, Qty(2_500_000));
	assert_eq!(lines[0].unit, "hour");
	assert_eq!(lines[0].net, Money(3_750_000));
	assert_eq!(lines[1].unit, "occasion");
	// The date and free text ride in `note`; `resolve` overwrote the description from the
	// `services` row.
	assert_eq!(lines[0].note.as_deref(), Some("2026-09-10 — kickoff"));
	assert_eq!(lines[1].note.as_deref(), Some("2026-09-11"));

	// "Pay another way": the stranded card draft is restamped and issued, with no password.
	let issued = bookings.pay_by_transfer(&ctx, &uid).await.unwrap();
	assert!(issued.number.is_some());
	assert!(matches!(issued.status, InvoiceStatus::Issued));
	assert_eq!(issued.payment_method, PaymentMethod::Transfer);

	let ledger = bookings.list(&ctx, None, Some(50)).await.unwrap().items;
	assert_eq!(ledger.len(), 2);
	assert!(ledger.iter().all(|b| b.invoice_uid.as_deref() == Some(uid.as_str())));

	// A cursor is resolved, not inlined as a subquery: an unknown uid used to make the row-value
	// comparison NULL, so a stale cursor answered an empty page the SPA read as end-of-list.
	let stale = bookings.list(&ctx, Some("bkg_nope"), Some(50)).await.unwrap_err();
	assert_eq!(stale.parts().1, "E-CORE-NOTFOUND");
	let page = bookings.list(&ctx, Some(ledger[0].uid.as_str()), Some(50)).await.unwrap();
	assert_eq!(page.items.len(), 1, "the cursor's own row is excluded");
	assert_eq!(page.items[0].uid, ledger[1].uid);
	assert!(page.next_cursor.is_none(), "a short page is the last one");

	// Nothing left unbilled: a second checkout drafts nothing rather than an empty invoice.
	assert!(bookings.checkout(&ctx, &by_card()).await.unwrap().is_none());

	// The crash between `Invoices::draft` and `settle`, driven by hand: claim, check out, then
	// rewind the settle so the bookings carry their claim again — and let a new booking land in
	// that window. Keying `request_id` on the booking set made that new booking change the key,
	// so the re-run drafted a *second* invoice over a superset of the first one's lines and both
	// were issuable. The claim is taken before the draft, so the re-run bills the claimed set.
	let mut req = BookRequest {
		service_code: "CONSULT".to_owned(),
		occurred_on: "2026-09-12".to_owned(),
		qty_e6: 1_000_000,
		note: None,
	};
	bookings.book(&ctx, &req).await.unwrap();
	let claim = sql.claim_unbilled(1).await.unwrap().expect("the third booking");
	let second = bookings
		.checkout(&ctx, &by_card())
		.await
		.unwrap()
		.expect("the open claim is resumed");
	sqlx::query("UPDATE bookings SET invoice_uid = ? WHERE invoice_uid = ?")
		.bind(&claim)
		.bind(second.invoice.uid.to_string())
		.execute(sql.writer())
		.await
		.unwrap();

	req.occurred_on = "2026-09-13".to_owned();
	bookings.book(&ctx, &req).await.unwrap();
	let retry = bookings.checkout(&ctx, &by_card()).await.unwrap().expect("the resumed claim");
	assert_eq!(
		retry.invoice.uid.to_string(),
		second.invoice.uid.to_string(),
		"the same draft, not a second one"
	);
	assert_eq!(retry.invoice.net, second.invoice.net, "and over the claimed booking only");
	// The reported bug: a resumed claim reuses the payment, whose redirect used to be dropped
	// on the floor, leaving the customer on a draft with nothing to click.
	assert_eq!(retry.redirect_url, second.redirect_url);
	assert!(retry.redirect_url.is_some());
}

/// `checkout` commits the claim before it drafts, so a booking the draft refuses used to be
/// re-claimed by every later checkout — the org could never bill anything again. Two guards:
/// `book` refuses the note the draft would, and a draft that fails anyway releases its claim.
#[tokio::test]
async fn a_booking_the_draft_refuses_cannot_wedge_the_checkout() {
	let db = TmpDb::new("wedge");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[FRAMEWORK, EXAMPLE]).await.unwrap();
	let app = setup(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let bookings = Bookings::new(app.clone());

	let long = BookRequest {
		service_code: "CONSULT".to_owned(),
		occurred_on: "2026-09-10".to_owned(),
		qty_e6: 1_000_000,
		note: Some("x".repeat(600)),
	};
	let err = bookings.book(&ctx, &long).await.expect_err("the note is past MAX_LINE_NOTE");
	assert_eq!(err.parts().1, "E-INV-TOO-LONG", "{err:?}");

	// The rows already in a database from before that guard: written straight past `book`.
	sqlx::query(
		"INSERT INTO bookings (uid, org_id, service_code, occurred_on, qty_e6, note, created_at)
		 VALUES ('bkg_poison', 1, 'CONSULT', '2026-09-10', 1000000, ?, 0)",
	)
	.bind("x".repeat(600))
	.execute(sql.writer())
	.await
	.unwrap();
	bookings
		.book(
			&ctx,
			&BookRequest {
				service_code: "CONSULT".to_owned(),
				occurred_on: "2026-09-11".to_owned(),
				qty_e6: 1_000_000,
				note: None,
			},
		)
		.await
		.unwrap();

	bookings
		.checkout(&ctx, &by_card())
		.await
		.expect_err("the poison row still fails the draft");
	let claimed: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM bookings WHERE substr(invoice_uid, 1, 4) = 'chk_'",
	)
	.fetch_one(sql.reader())
	.await
	.unwrap();
	assert_eq!(claimed, 0, "a failed draft leaves no claim for the next checkout to resume");

	// A card checkout naming no gateway is refused before the claim, so it leaves nothing
	// behind either — `ids().first()` used to pick one out of a `HashMap` at random.
	let err = bookings
		.checkout(&ctx, &CheckoutRequest { method: PayMethod::Card, provider: None })
		.await
		.expect_err("a card sale must name its gateway");
	assert_eq!(err.parts().1, "E-PAY-PROVIDER", "{err:?}");

	// With the bad row gone the clean booking bills, which is what the wedge made impossible.
	sqlx::query("DELETE FROM bookings WHERE uid = 'bkg_poison'")
		.execute(sql.writer())
		.await
		.unwrap();
	let invoice = bookings
		.checkout(&ctx, &by_card())
		.await
		.unwrap()
		.expect("the clean booking")
		.invoice;
	assert_eq!(invoice.net, Money(1_500_000));
}

/// `book` and `line_for` used to spell the `"YYYY-MM-DD — "` prefix separately, so the cap
/// `book` enforced and the string the draft measured could drift apart — and the drift shows
/// only at the boundary, past a claim `checkout` has already committed.
#[tokio::test]
async fn a_note_is_measured_with_the_date_the_line_will_carry() {
	let db = TmpDb::new("note-boundary");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[FRAMEWORK, EXAMPLE]).await.unwrap();
	let app = setup(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let bookings = Bookings::new(app.clone());

	// 13 = "YYYY-MM-DD" plus " — ", so the composed note is exactly MAX_LINE_NOTE.
	let at = |n: usize, day: &str| BookRequest {
		service_code: "CONSULT".to_owned(),
		occurred_on: day.to_owned(),
		qty_e6: 1_000_000,
		note: Some("x".repeat(n)),
	};
	let fits = saas_invoice::store::MAX_LINE_NOTE - 13;

	let err = bookings.book(&ctx, &at(fits + 1, "2026-09-10")).await.expect_err("one over");
	assert_eq!(err.parts().1, "E-INV-TOO-LONG", "{err:?}");
	let claimed: i64 = sqlx::query_scalar("SELECT count(*) FROM bookings")
		.fetch_one(sql.reader())
		.await
		.unwrap();
	assert_eq!(claimed, 0, "the refusal lands before any row, let alone a claim");

	bookings.book(&ctx, &at(fits, "2026-09-10")).await.unwrap();
	bookings
		.checkout(&ctx, &by_card())
		.await
		.unwrap()
		.expect("the exact-fit note must reach the draft");
}

/// `by_checkout`, `settle` and `release` filtered on `invoice_uid` alone. The claim is a fresh
/// `chk_<ULID>` so this was not reachable, but these are the three writes that move money and
/// `example/` is what a consumer copies.
#[tokio::test]
async fn another_orgs_claim_is_invisible_not_settleable() {
	let db = TmpDb::new("claim-org");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[FRAMEWORK, EXAMPLE]).await.unwrap();
	let app = setup(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);

	Bookings::new(app.clone())
		.book(
			&ctx,
			&BookRequest {
				service_code: "CONSULT".to_owned(),
				occurred_on: "2026-09-10".to_owned(),
				qty_e6: 1_000_000,
				note: None,
			},
		)
		.await
		.unwrap();
	let claim = sql.claim_unbilled(1).await.unwrap().expect("org 1 has something to bill");

	assert!(sql.by_checkout(2, &claim).await.unwrap().is_empty(), "another org reads nothing");
	assert!(!sql.settle(2, &claim, "inv_stolen").await.unwrap(), "and settles nothing");
	sql.release(2, &claim).await.unwrap();
	assert_eq!(sql.by_checkout(1, &claim).await.unwrap().len(), 1, "the claim is still org 1's");
}

/// An unbounded claim handed `Invoices::draft` every booking an org ever made — past
/// `MAX_LINES` the draft is refused outright, and the whole set is on one transaction on the
/// single writer connection. The leftovers are the next checkout's set.
#[tokio::test]
async fn a_claim_is_capped_at_the_frameworks_line_limit() {
	let db = TmpDb::new("claim-cap");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[FRAMEWORK, EXAMPLE]).await.unwrap();
	setup(&db, &sql).await;

	let over = i64::try_from(saas_invoice::draft::MAX_LINES).unwrap() + 1;
	sqlx::query(
		"INSERT INTO bookings (uid, org_id, service_code, occurred_on, qty_e6, note, created_at)
		 WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < ?)
		 SELECT 'bkg_' || printf('%04d', n), 1, 'CONSULT', '2026-09-10', 1000000, NULL, 0 FROM s",
	)
	.bind(over)
	.execute(sql.writer())
	.await
	.unwrap();

	let claim = sql.claim_unbilled(1).await.unwrap().expect("something to bill");
	assert_eq!(
		sql.by_checkout(1, &claim).await.unwrap().len(),
		saas_invoice::draft::MAX_LINES,
		"the claim stops at the cap"
	);

	// Only once the first claim is off the table — an open one is resumed, not extended.
	assert!(sql.settle(1, &claim, "inv_first").await.unwrap());
	let rest = sql.claim_unbilled(1).await.unwrap().expect("the leftover");
	assert_eq!(sql.by_checkout(1, &rest).await.unwrap().len(), 1);
}

/// Between `claim_unbilled` and `settle` the column holds a `chk_` claim, and the SPA links
/// `invoiceUid` straight to `/invoices/{uid}` — so the claim rendered as a dead link.
#[tokio::test]
async fn a_checkout_claim_never_reaches_the_wire() {
	let db = TmpDb::new("claim-wire");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[FRAMEWORK, EXAMPLE]).await.unwrap();
	let app = setup(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let bookings = Bookings::new(app.clone());

	bookings
		.book(
			&ctx,
			&BookRequest {
				service_code: "CONSULT".to_owned(),
				occurred_on: "2026-09-10".to_owned(),
				qty_e6: 1_000_000,
				note: None,
			},
		)
		.await
		.unwrap();

	let claim = sql.claim_unbilled(1).await.unwrap().expect("something to bill");
	let wire = serde_json::to_value(&sql.by_checkout(1, &claim).await.unwrap()[0]).unwrap();
	assert_eq!(wire["invoiceUid"], serde_json::Value::Null, "{wire}");

	assert!(sql.settle(1, &claim, "inv_01JCZ5X8K9N7QW3M6R2T4V8Y0B").await.unwrap());
	let billed = bookings.list(&ctx, None, Some(50)).await.unwrap().items;
	let wire = serde_json::to_value(&billed[0]).unwrap();
	assert_eq!(wire["invoiceUid"], "inv_01JCZ5X8K9N7QW3M6R2T4V8Y0B", "{wire}");
}

/// The router the browser drives, with the `App` extension `AppBuilder::run` normally layers.
/// No `ClientIp`: the rate limiters read it as absent, which is what a unit-socket deployment
/// looks like too.
fn router(app: &App) -> axum::Router {
	routes::api().layer(axum::Extension(app.clone())).with_state(app.clone())
}

/// A real access token for account 1 on org 1, minted against the app's own signing key.
/// Registering and activating through `saas-auth` would be a second feature's worth of
/// fixture for what `verify` reduces to three columns.
async fn token(app: &App) -> String {
	token_at(app, Timestamp::now().0).await
}

/// The same token with `auth_at` chosen by the caller. Nothing the SPA reaches is step-up
/// gated any more, so a stale credential is what proves the customer is never asked for one.
async fn token_at(app: &App, auth_at: i64) -> String {
	let key = app.secrets.get_or_create(saas_core::auth_mw::JWT_SECRET_KEY, 32).await.unwrap();
	let claims = saas_core::auth_mw::Claims {
		sub: "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A".to_owned(),
		org: Some("org_01JCZ5X8K9N7QW3M6R2T4V8Y0C".to_owned()),
		rol: Some("OWNER".to_owned()),
		opr: false,
		ep: 0,
		auth_at: Some(auth_at),
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

/// One request, returning the status and the body parsed as JSON (`Null` when there is none,
/// which is what a 204 answers with).
async fn call(
	router: &axum::Router,
	method: &str,
	uri: &str,
	token: Option<&str>,
	body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
	use http_body_util::BodyExt as _;
	let mut req = axum::http::Request::builder().method(method).uri(uri);
	if let Some(t) = token {
		req = req.header("authorization", format!("Bearer {t}"));
	}
	let req = req
		.header("content-type", "application/json")
		.body(axum::body::Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
		.unwrap();
	let res = tower::ServiceExt::oneshot(router.clone(), req).await.unwrap();
	let status = res.status();
	let bytes = res.into_body().collect().await.unwrap().to_bytes();
	(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// `routes.rs` was compiled by the binary alone, so none of this — auth, the error envelope,
/// 204-on-nothing-to-bill, org scoping — was covered by anything but a browser.
#[tokio::test]
async fn the_api_answers_over_http_the_way_the_spa_expects() {
	let db = TmpDb::new("http");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[FRAMEWORK, EXAMPLE]).await.unwrap();
	let app = setup(&db, &sql).await;
	let router = router(&app);
	let token = token(&app).await;

	// The customer's org is not the seller's (that is the root's), so a draft under the
	// customer's own `Ctx` is refused — `Bookings::checkout` drafts as the system instead.
	let customer = Ctx {
		actor: Actor::User { account_id: 1 },
		auth_at: Some(Timestamp::now().0),
		..Ctx::system("test").with_org(1)
	};
	assert_eq!(
		Invoices::new(app.clone())
			.draft(&customer, &NewDraft::default())
			.await
			.unwrap_err()
			.parts()
			.1,
		"E-AUTH-FORBIDDEN"
	);

	// An unmatched /api path is the error envelope, never index.html with a 200.
	let (status, body) = call(&router, "GET", "/api/nope", Some(&token), None).await;
	assert_eq!(status, StatusCode::NOT_FOUND);
	assert_eq!(body["error"]["errCode"], "E-CORE-NOTFOUND", "{body}");

	let (status, body) = call(&router, "GET", "/api/bookings", None, None).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

	// Storno left the SPA with `demo_buyer_invoice_routes`: the customer never touches a
	// numbered document.
	let (status, _) = call(
		&router,
		"POST",
		"/api/invoices/inv_01JCZ5X8K9N7QW3M6R2T4V8Y0B/cancel",
		Some(&token),
		Some(serde_json::json!({ "reason": "kesobb" })),
	)
	.await;
	assert_eq!(status, StatusCode::NOT_FOUND);

	let card = serde_json::json!({ "method": "CARD", "provider": "stub" });
	let transfer = serde_json::json!({ "method": "TRANSFER" });
	let (status, body) =
		call(&router, "POST", "/api/bookings/checkout", Some(&token), Some(transfer.clone())).await;
	assert_eq!(status, StatusCode::NO_CONTENT, "nothing to bill is 204, not an empty draft");
	assert_eq!(body, serde_json::Value::Null);

	let (status, body) = call(
		&router,
		"POST",
		"/api/bookings",
		Some(&token),
		Some(serde_json::json!({
			"serviceCode": "CONSULT", "occurredOn": "2026-09-10", "qtyE6": 1_000_000
		})),
	)
	.await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
	// A bank transfer is issued at checkout and never asks for a password: the token's
	// `auth_at` is a day stale, which is what `require_stepup` refuses for anyone but `System`.
	let stale = token_at(&app, Timestamp::now().0 - 86_400).await;
	let (status, body) =
		call(&router, "POST", "/api/bookings/checkout", Some(&stale), Some(transfer.clone())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["status"], "ISSUED", "{body}");
	assert_eq!(body["paymentMethod"], "TRANSFER", "{body}");
	assert!(body["number"].is_string(), "a transfer needs a number to quote: {body}");
	assert!(body["dueDate"].is_string(), "{body}");
	let uid = body["uid"].as_str().expect("an issued invoice").to_owned();

	// The buyer's own payment route is gone — paying is `saas-billing`'s. It falls through to
	// the `/api` catch-all.
	let (status, body) =
		call(&router, "POST", &format!("/api/invoices/{uid}/payment"), Some(&token), None).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

	// A card checkout comes back PENDING — unnumbered, and frozen while the gateway holds a
	// charge against its total — with a redirect. Going back and checking out again resumes the
	// *same* claim, and so must answer with the *same* URL — the reported bug was that it
	// answered with none, stranding the customer on a draft.
	let (status, body) = call(
		&router,
		"POST",
		"/api/bookings",
		Some(&token),
		Some(serde_json::json!({
			"serviceCode": "SITEVISIT", "occurredOn": "2026-09-11", "qtyE6": 1_000_000
		})),
	)
	.await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
	let (status, body) =
		call(&router, "POST", "/api/bookings/checkout", Some(&token), Some(card.clone())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["status"], "PENDING", "{body}");
	assert!(body["number"].is_null(), "a locked draft is still unnumbered: {body}");
	assert_eq!(body["paymentMethod"], "CARD", "{body}");
	let second = body["uid"].as_str().expect("a second draft").to_owned();
	let redirect = body["redirectUrl"].as_str().expect("a gateway URL").to_owned();

	// Going back is *not* a second checkout — the bookings are billed, so there is nothing
	// left to bill. The draft is reached from the invoice page instead, and
	// `GET /api/invoices/{uid}/payments` is what puts the "Continue payment" URL back on it.
	// The reported bug was that this answer had no URL anywhere in it.
	let (status, _) =
		call(&router, "POST", "/api/bookings/checkout", Some(&token), Some(card.clone())).await;
	assert_eq!(status, StatusCode::NO_CONTENT);
	let (status, body) =
		call(&router, "GET", &format!("/api/invoices/{second}/payments"), Some(&token), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["items"][0]["redirectUrl"], redirect, "{body}");
	assert_eq!(body["items"][0]["status"], "PENDING", "{body}");
	let pay_uid = body["items"][0]["uid"].as_str().expect("a payment uid").to_owned();

	// Not while that payment is live: `delete_draft` drops the zero link row with the invoice, so
	// a payment that then succeeded would be charged, unallocated, and its released bookings
	// billed a second time by the next checkout.
	let (status, body) =
		call(&router, "DELETE", &format!("/api/invoices/{second}"), Some(&token), None).await;
	assert_eq!(status, StatusCode::CONFLICT, "{body}");
	assert_eq!(body["error"]["errCode"], "E-BOOK-PAYMENT-OPEN", "{body}");
	let (status, body) =
		call(&router, "GET", &format!("/api/invoices/{second}"), Some(&token), None).await;
	assert_eq!(status, StatusCode::OK, "the draft is still there: {body}");

	// The payer walked away and the gateway gave up — what `PAYMENT_SWEEP` drives, applied here
	// directly because `StubGateway::fetch_state` only ever answers PENDING. Expiry unwinds the
	// lock, so the invoice is an editable DRAFT again and the discard puts the bookings back in
	// the unbilled set.
	let payment = saas_billing::store::store(&app)
		.unwrap()
		.payment_by_uid(Some(1), &PaymentId::parse(&pay_uid).unwrap())
		.await
		.unwrap()
		.expect("the payment row");
	saas_billing::allocate::apply_state(&app, &payment, PaymentState::Expired)
		.await
		.unwrap();
	let (status, body) =
		call(&router, "GET", &format!("/api/invoices/{second}"), Some(&token), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["status"], "DRAFT", "a dead payment hands the cart back: {body}");

	let (status, _) =
		call(&router, "DELETE", &format!("/api/invoices/{second}"), Some(&token), None).await;
	assert_eq!(status, StatusCode::NO_CONTENT);
	let (status, body) =
		call(&router, "POST", "/api/bookings/checkout", Some(&token), Some(transfer.clone())).await;
	assert_eq!(status, StatusCode::OK, "the released booking bills again: {body}");
	assert_eq!(body["status"], "ISSUED", "{body}");

	// A numbered invoice is not discardable.
	let (status, body) =
		call(&router, "DELETE", &format!("/api/invoices/{uid}"), Some(&token), None).await;
	assert_eq!(status, StatusCode::CONFLICT, "{body}");
	assert_eq!(body["error"]["errCode"], "E-INV-NOT-DRAFT", "{body}");

	// Paging is the service's decision, cursor and all: two bookings, one per page.
	let (status, first) = call(&router, "GET", "/api/bookings?limit=1", Some(&token), None).await;
	assert_eq!(status, StatusCode::OK, "{first}");
	assert_eq!(first["items"].as_array().unwrap().len(), 1, "{first}");
	let cursor = first["nextCursor"].as_str().expect("a full page carries a cursor").to_owned();
	let (_, next) =
		call(&router, "GET", &format!("/api/bookings?limit=1&cursor={cursor}"), Some(&token), None)
			.await;
	assert_eq!(next["items"].as_array().unwrap().len(), 1, "{next}");
	assert_ne!(next["items"][0]["uid"], first["items"][0]["uid"], "the cursor's own row is out");
	// A full page always carries a cursor, so the run ends on the empty page after it — not on
	// a short one. The SPA stops when `items` is empty or the cursor is null, whichever first.
	let cursor = next["nextCursor"].as_str().expect("page two is full too").to_owned();
	let (_, last) =
		call(&router, "GET", &format!("/api/bookings?limit=1&cursor={cursor}"), Some(&token), None)
			.await;
	assert!(last["items"].as_array().unwrap().is_empty(), "{last}");
	assert!(last["nextCursor"].is_null(), "an empty page carries no cursor: {last}");

	// "Pay another way" with a live gateway payment still open: the payer walked off the gateway
	// page, which the gateway keeps reporting as live until it expires, so the button has to
	// abandon that payment locally and unlock the invoice rather than refuse `E-INV-LOCKED`.
	let (status, body) = call(
		&router,
		"POST",
		"/api/bookings",
		Some(&token),
		Some(serde_json::json!({
			"serviceCode": "CONSULT", "occurredOn": "2026-09-12", "qtyE6": 1_000_000
		})),
	)
	.await;
	assert_eq!(status, StatusCode::CREATED, "{body}");
	let (status, body) =
		call(&router, "POST", "/api/bookings/checkout", Some(&token), Some(card.clone())).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["status"], "PENDING", "{body}");
	let stranded = body["uid"].as_str().expect("a locked draft").to_owned();
	let (status, body) = call(
		&router,
		"POST",
		&format!("/api/invoices/{stranded}/pay-by-transfer"),
		Some(&token),
		None,
	)
	.await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["status"], "ISSUED", "{body}");
	assert_eq!(body["paymentMethod"], "TRANSFER", "{body}");

	// The quantity cap: a checkout mints a numbered document under the operator's tax number.
	let (status, body) = call(
		&router,
		"POST",
		"/api/bookings",
		Some(&token),
		Some(serde_json::json!({
			"serviceCode": "CONSULT", "occurredOn": "2026-09-12", "qtyE6": 25_000_000
		})),
	)
	.await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

	// `GET /{uid}/nav` and its three-arm message mapper: nothing is filed here — the harness
	// seeds no NAV credentials — so the route's 204 arm is the state the demo stays in.
	let (status, body) =
		call(&router, "GET", &format!("/api/invoices/{uid}/nav"), Some(&token), None).await;
	assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

	sqlx::query(
		"INSERT INTO nav_submissions (invoice_id, op, created_at, done_at, error_code, error_msg)
		 SELECT id, 'CREATE', 0, 1770000000, 'REQUEST_ID_NOT_UNIQUE', 'already processed'
		   FROM invoices WHERE uid = ?",
	)
	.bind(&uid)
	.execute(sql.writer())
	.await
	.unwrap();
	let (status, body) =
		call(&router, "GET", &format!("/api/invoices/{uid}/nav"), Some(&token), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["op"], "CREATE", "{body}");
	assert!(body["verdict"].is_null(), "open, not settled: {body}");
	assert!(body["submittedAt"].is_string(), "{body}");
	// `error_msg` is free text a batch path writes and can name another org's invoice, so
	// `Nav::filing` blanks it for anyone but an operator — the message is NAV's code alone.
	assert_eq!(body["message"], "REQUEST_ID_NOT_UNIQUE", "{body}");

	// Another org's invoice is absent, never forbidden: a 403 confirms the uid exists.
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (2, 'org_u', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Masik', 1, 0)",
	)
	.execute(sql.writer())
	.await
	.unwrap();
	sqlx::query("UPDATE invoices SET org_id = 2 WHERE uid = ?")
		.bind(&uid)
		.execute(sql.writer())
		.await
		.unwrap();
	for (method, route) in [
		("POST", format!("/api/invoices/{uid}/pay-by-transfer")),
		("GET", format!("/api/invoices/{uid}/payments")),
		("GET", format!("/api/invoices/{uid}/nav")),
	] {
		let (status, body) = call(&router, method, &route, Some(&token), None).await;
		assert_eq!(status, StatusCode::NOT_FOUND, "{route}: {body}");
		assert_eq!(body["error"]["errCode"], "E-CORE-NOTFOUND", "{route}: {body}");
	}
}

/// `settle` returning `false` means the bookings are unbilled again while their invoice
/// stays issuable, so the next checkout bills them a second time. A `tracing::error!` at the
/// moment it happened was the only trace; `A-BOOKING-ORPHANED` is the standing one.
#[tokio::test]
async fn a_lost_settle_is_visible_as_an_alert() {
	let db = TmpDb::new("orphaned-claim");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[FRAMEWORK, EXAMPLE]).await.unwrap();
	let app = setup(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let bookings = Bookings::new(app.clone());

	bookings
		.book(
			&ctx,
			&BookRequest {
				service_code: "CONSULT".to_owned(),
				occurred_on: "2026-09-10".to_owned(),
				qty_e6: 1_000_000,
				note: None,
			},
		)
		.await
		.unwrap();
	let claim = sql.claim_unbilled(1).await.unwrap().expect("something to bill");
	let invoice = bookings
		.checkout(&ctx, &by_card())
		.await
		.unwrap()
		.expect("the open claim is resumed")
		.invoice;
	assert_eq!(sql.orphaned_claims().await.unwrap(), 0, "a settled checkout is not an orphan");
	assert!(saas_example::bookings::alerts(app.clone()).await.unwrap().is_empty());

	// The lost settle, by hand: the invoice is committed under this claim and the bookings
	// still carry it, which is exactly the state `settle` returning `false` leaves behind.
	sqlx::query("UPDATE bookings SET invoice_uid = ? WHERE invoice_uid = ?")
		.bind(&claim)
		.bind(invoice.uid.to_string())
		.execute(sql.writer())
		.await
		.unwrap();

	assert_eq!(sql.orphaned_claims().await.unwrap(), 1);
	let alerts = saas_example::bookings::alerts(app.clone()).await.unwrap();
	assert_eq!(alerts.len(), 1);
	assert_eq!(alerts[0].code, "A-BOOKING-ORPHANED");
	assert!(matches!(alerts[0].severity, saas_core::alert::Severity::Error), "this is money");
	assert_eq!(alerts[0].count, 1);

	// And the customer is not blocked meanwhile: the resumed claim yields the same invoice.
	let again = bookings
		.checkout(&ctx, &by_card())
		.await
		.unwrap()
		.expect("the resumed claim")
		.invoice;
	assert_eq!(again.uid.to_string(), invoice.uid.to_string());
	assert_eq!(sql.orphaned_claims().await.unwrap(), 0, "and the settle clears the alert");
	assert!(saas_example::bookings::alerts(app).await.unwrap().is_empty());
}

/// `discard` deletes the draft and releases the bookings in two calls a crash can land between.
/// The bookings then carry an `inv_…` naming no invoice: `claim_unbilled` only picks up
/// `invoice_uid IS NULL`, so they are never billable again, and the `chk_` join could not see
/// this shape at all — the customer's bookings vanished from both lists, permanently.
#[tokio::test]
async fn a_booking_left_on_a_deleted_invoice_is_visible_as_an_alert() {
	let db = TmpDb::new("orphaned-invoice");
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[FRAMEWORK, EXAMPLE]).await.unwrap();
	let app = setup(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let bookings = Bookings::new(app.clone());

	bookings
		.book(
			&ctx,
			&BookRequest {
				service_code: "CONSULT".to_owned(),
				occurred_on: "2026-09-10".to_owned(),
				qty_e6: 1_000_000,
				note: None,
			},
		)
		.await
		.unwrap();
	let invoice = bookings
		.checkout(&ctx, &by_card())
		.await
		.unwrap()
		.expect("the open claim is resumed")
		.invoice;
	assert_eq!(sql.orphaned_claims().await.unwrap(), 0);

	// The crash window, by hand. `delete_draft` drops the payment's zero link row with the
	// invoice, so the payment goes first here too — the FK is what makes that the only order.
	sqlx::query("DELETE FROM payments").execute(sql.writer()).await.unwrap();
	sqlx::query("DELETE FROM invoices WHERE id = ?")
		.bind(invoice.id)
		.execute(sql.writer())
		.await
		.unwrap();

	assert_eq!(sql.orphaned_claims().await.unwrap(), 1);
	let alerts = saas_example::bookings::alerts(app).await.unwrap();
	assert_eq!(alerts.len(), 1);
	assert_eq!(alerts[0].code, "A-BOOKING-ORPHANED");
	assert_eq!(alerts[0].count, 1);
}

// vim: ts=4
