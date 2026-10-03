//! `Plans` service tests: quote → checkout → payment → grant, the token guards, and the
//! refund cut. `saas-plans` arithmetic and flow, not store conformance.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use saas_auth::InviteGate;
use saas_billing::store::BillingStore;
use saas_billing::{Allocation, ManualPayment, allocate};
use saas_core::refs::{CreateRef, RefStore, Refs};
use saas_core::{App, AppBuilder, config::Config, ctx::Ctx, ids::SellerId, prelude::*};
use saas_entitle::{Entitle, EntitleStore, EntitlementDef, EntitlementRegistry};
use saas_entitle::{GrantReq, Source};
use saas_invoice::store::{Invoice, InvoiceStatus, InvoiceStore, Seller, SellerVersionPatch};
use saas_plans::{CheckoutReq, OfferDef, PayMethod, PlanStore, Plans, QuoteReq, reconcile_with};
use saas_plans::{MeterInviteGate, Side};
use store_adapter_sqlite::SqliteStore;

const ORG: i64 = 1;
const SELLER: i64 = 1;
const ORG_UID: &str = "org_t";

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-plans-svc-test-{}-{name}", std::process::id()));
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

fn config(db: &TmpDb) -> Config {
	Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: "https://app.invalid".into(),
		jobs_workers: None,
	}
}

fn registry() -> EntitlementRegistry {
	EntitlementRegistry::new([EntitlementDef::meter("credits"), EntitlementDef::meter("invites")])
}

/// 1000 credits per unit, valid 30 days, 5000 HUF (500000 minor units) net.
fn credits() -> OfferDef {
	credits_at(500_000)
}

fn credits_at(price: i64) -> OfferDef {
	let mut d = OfferDef::one_time("credits_1000", "1000 credits", "CREDITS")
		.price(CurrencyCode::huf(), price)
		.entitle("credits", 1000, true);
	d.validity_days = Some(30);
	d
}

/// The billing suite's fixture: org `ORG` is both the seller and the buyer, root moved to 0.
async fn setup(db: &TmpDb) -> (App, SqliteStore) {
	let store = SqliteStore::open(&config(db)).await.unwrap();
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	let builder = AppBuilder::new()
		.config(config(db))
		.store(Arc::new(store.clone()) as Arc<dyn saas_core::store::CoreStore>)
		.settings(saas_invoice::SETTINGS)
		.settings(saas_billing::SETTINGS)
		.settings(saas_plans::SETTINGS)
		.secrets(saas_plans::SECRETS)
		.extension(Arc::new(store.clone()) as Arc<dyn InvoiceStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn BillingStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn EntitleStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn PlanStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn RefStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn saas_auth::store::AuthStore>);
	let builder = saas_entitle::install(
		builder,
		[EntitlementDef::meter("credits"), EntitlementDef::meter("invites")],
	);
	let app = saas_plans::install(builder, []).build().await.unwrap();

	for sql in [
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0B', 't@e.st', 0)",
		"UPDATE orgs SET id = 0 WHERE kind = 'ROOT'",
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (1, 'org_t', 0, 'SHARED', 'Teszt', 1, 0)",
		"INSERT INTO accounts (id, uid, email, created_at)
		 VALUES (2, 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0C', 'u@e.st', 0)",
		// A second buyer under the seller, which account 1 owns through org 1.
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (2, 'org_u', 1, 'SHARED', 'Masik', 2, 0)",
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (1, 1, 'OWNER', 0, 0), (2, 2, 'OWNER', 0, 0)",
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  email, is_default, created_at, updated_at)
		 VALUES (2, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0C', 2, 'C', 'Masik Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 'masik@e.st', 1, 0, 0)",
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  email, is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', 1, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 'vevo@e.st', 1, 0, 0)",
		"INSERT INTO services (uid, org_id, code, name, unit, unit_price, vat_code, created_at,
		 updated_at) VALUES ('svc_t', 1, 'CREDITS', 'Kredit csomag', 'db', 0, 'STD27', 0, 0)",
		"INSERT INTO currencies (code, price_round_step, mode, fee_bp, enabled)
		 VALUES ('EUR', 1, 'OFFICIAL', 0, 1)",
	] {
		sqlx::query(sql).execute(store.write_pool()).await.unwrap();
	}
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
	reconcile_with(&store, Some(&registry()), ORG, &[credits(), reward_offer()])
		.await
		.unwrap();
	(app, store)
}

fn ctx() -> Ctx {
	Ctx::system("test").with_org(ORG)
}

fn quote_req(offer: &str, qty: i64) -> QuoteReq {
	QuoteReq {
		offer: offer.into(),
		qty: Some(qty),
		currency: None,
		coupon: None,
		subscription: None,
	}
}

fn code_of<T: std::fmt::Debug>(r: ClResult<T>) -> &'static str {
	match r {
		Err(Error::Coded { code, .. }) => code,
		other => panic!("expected a coded error, got {other:?}"),
	}
}

/// Polls the spawned event handler's effect.
async fn credits_become(app: &App, want: i64) {
	let entitle = Entitle::from_app(app).unwrap();
	for _ in 0..100 {
		if entitle.balance(&ctx(), "credits").await.unwrap() == want {
			return;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("credits never reached {want}");
}

/// Quote → checkout TRANSFER → manual payment; returns the payment's uid.
async fn buy_and_pay(app: &App, store: &SqliteStore) -> PaymentId {
	let plans = Plans::from_app(app).unwrap();
	let q = plans.quote(&ctx(), &quote_req("credits_1000", 2)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token.clone(), pay_method: PayMethod::Transfer };
	let out = plans.checkout(&ctx(), &req).await.unwrap();
	assert_eq!(out.next, "issued");
	// The token is the idempotency key: a replay returns the same invoice.
	assert_eq!(plans.checkout(&ctx(), &req).await.unwrap().invoice_uid, out.invoice_uid);

	let inv = store
		.invoice_by_uid(None, out.invoice_uid.as_ref().unwrap())
		.await
		.unwrap()
		.unwrap();
	assert_eq!(inv.status, InvoiceStatus::Issued);
	assert_eq!(inv.gross.to_wire(&inv.currency), q.gross, "the invoice is what was quoted");
	assert_eq!(inv.net, Money(1_000_000));
	pay(app, &inv).await
}

/// A manual TRANSFER payment of `inv` in full; returns its uid.
async fn pay(app: &App, inv: &Invoice) -> PaymentId {
	allocate::manual(
		app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "TRANSFER".into(),
			amount: inv.gross,
			currency: inv.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: None,
			note: None,
			allocations: vec![Allocation {
				invoice_uid: inv.uid.clone(),
				amount: inv.gross,
				currency: inv.currency.clone(),
			}],
		},
	)
	.await
	.unwrap()
	.uid
}

#[tokio::test]
async fn a_one_time_purchase_grants_per_seat_on_payment() {
	let db = TmpDb::new("purchase");
	let (app, store) = setup(&db).await;
	buy_and_pay(&app, &store).await;
	credits_become(&app, 2000).await;
}

#[tokio::test]
async fn a_full_refund_cuts_the_purchase() {
	let db = TmpDb::new("refund");
	let (app, store) = setup(&db).await;
	let payment = buy_and_pay(&app, &store).await;
	credits_become(&app, 2000).await;
	saas_billing::refund(&app, &ctx(), &payment, None, None).await.unwrap();
	credits_become(&app, 0).await;
}

#[tokio::test]
async fn a_price_change_makes_the_quote_stale() {
	let db = TmpDb::new("stale");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let q = plans.quote(&ctx(), &quote_req("credits_1000", 1)).await.unwrap();
	reconcile_with(&store, Some(&registry()), ORG, &[credits_at(600_000)])
		.await
		.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	assert_eq!(code_of(plans.checkout(&ctx(), &req).await), "E-PLAN-QUOTE-STALE");
}

#[tokio::test]
async fn a_quote_checked_out_by_transfer_refuses_card() {
	let db = TmpDb::new("method-switch");
	let (app, _store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let q = plans.quote(&ctx(), &quote_req("credits_1000", 1)).await.unwrap();
	let mut req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	plans.checkout(&ctx(), &req).await.unwrap();
	req.pay_method = PayMethod::Card;
	assert!(matches!(plans.checkout(&ctx(), &req).await, Err(Error::Conflict(_))));
}

#[tokio::test]
async fn an_expired_or_forged_token_is_refused() {
	let db = TmpDb::new("expired");
	let (app, _store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let q = plans.quote(&ctx(), &quote_req("credits_1000", 1)).await.unwrap();

	let (encoded, _) = q.quote_token.split_once('.').unwrap();
	let mut claims: serde_json::Value =
		serde_json::from_slice(&B64.decode(encoded).unwrap()).unwrap();
	claims["exp"] = serde_json::json!(Timestamp::now().0 - 1);
	let json = claims.to_string();
	let key = app.secrets.get_or_create("plans.quote_key", 32).await.unwrap();
	let sig = saas_core::crypto::hmac_hex(&key, &json).unwrap();
	let expired = format!("{}.{sig}", B64.encode(json.as_bytes()));
	let req = CheckoutReq { quote_token: expired, pay_method: PayMethod::Transfer };
	assert_eq!(code_of(plans.checkout(&ctx(), &req).await), "E-PLAN-QUOTE-EXPIRED");

	let forged = format!("{}.{}", B64.encode(json.as_bytes()), "0".repeat(64));
	let req = CheckoutReq { quote_token: forged, pay_method: PayMethod::Transfer };
	assert!(matches!(plans.checkout(&ctx(), &req).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn an_offer_without_a_price_in_the_currency_is_refused() {
	let db = TmpDb::new("no-price");
	let (app, store) = setup(&db).await;
	let eur_only = OfferDef::one_time("eur_only", "EUR only", "CREDITS")
		.price(CurrencyCode::parse("EUR").unwrap(), 1000);
	reconcile_with(&store, Some(&registry()), ORG, &[credits(), eur_only])
		.await
		.unwrap();
	let plans = Plans::from_app(&app).unwrap();
	assert_eq!(code_of(plans.quote(&ctx(), &quote_req("eur_only", 1)).await), "E-PLAN-NO-PRICE");
}

fn reward_offer() -> OfferDef {
	OfferDef::one_time("reward_credits", "Welcome credits", "CREDITS")
		.price(CurrencyCode::huf(), 100)
		.entitle("credits", 100, false)
}

fn user() -> Ctx {
	ctx().as_user(1)
}

/// A ref owned by the seller org `ORG`.
async fn mint(app: &App, ref_type: &str, params: serde_json::Value) -> saas_core::refs::Ref {
	mint_in(app, ORG, ref_type, params).await
}

async fn mint_in(
	app: &App,
	org: i64,
	ref_type: &str,
	params: serde_json::Value,
) -> saas_core::refs::Ref {
	let req = CreateRef { ref_type: ref_type.into(), params: Some(params), ..Default::default() };
	let ctx = Ctx::system("test").with_org(org);
	Refs::from_app(app).unwrap().mint(&ctx, &req).await.unwrap()
}

/// A buyer org under the seller, owned by account 1 through `ORG`, and by account 2 directly.
/// `ORG` is the operator org here: `root_org_id` was cached before the fixture renumbered root.
const OTHER: i64 = 2;
const ACC_UID: &str = "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0B";

#[tokio::test]
async fn a_coupon_discounts_the_line_and_is_used_once_per_org() {
	let db = TmpDb::new("coupon");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let c = mint(&app, "coupon", serde_json::json!({"discount": {"percentBp": 5000}})).await;
	let mut req = quote_req("credits_1000", 2);
	req.coupon = Some(c.code.clone());

	let q = plans.quote(&user(), &req).await.unwrap();
	let pay = CheckoutReq { quote_token: q.quote_token.clone(), pay_method: PayMethod::Transfer };
	let out = plans.checkout(&user(), &pay).await.unwrap();
	assert_eq!(plans.checkout(&user(), &pay).await.unwrap().invoice_uid, out.invoice_uid);
	let inv = store
		.invoice_by_uid(None, out.invoice_uid.as_ref().unwrap())
		.await
		.unwrap()
		.unwrap();
	assert_eq!(inv.net, Money(500_000), "50% off 2 × 5000 HUF");
	assert_eq!(inv.gross.to_wire(&inv.currency), q.gross);
	let link = store
		.plan_invoice_get(out.invoice_uid.as_ref().unwrap())
		.await
		.unwrap()
		.unwrap();
	assert_eq!(link.coupon_ref_id, Some(c.id));

	assert_eq!(code_of(plans.quote(&user(), &req).await), "E-PLAN-COUPON-INVALID");
	let other = mint(
		&app,
		"coupon",
		serde_json::json!({"offers": ["nope"], "discount": {"percentBp": 10}}),
	)
	.await;
	req.coupon = Some(other.code);
	assert_eq!(code_of(plans.quote(&user(), &req).await), "E-PLAN-COUPON-INVALID");
}

/// A CARD checkout spent its coupon at link time, so a draft abandoned unpaid burned it.
#[tokio::test]
async fn a_coupon_is_held_by_its_draft_until_paid() {
	let db = TmpDb::new("coupon-held");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let mint = CreateRef {
		ref_type: "coupon".into(),
		params: Some(serde_json::json!({"discount": {"percentBp": 5000}})),
		uses_left: Some(1),
		..Default::default()
	};
	let c = Refs::from_app(&app).unwrap().mint(&ctx(), &mint).await.unwrap();
	let req = |qty| QuoteReq { coupon: Some(c.code.clone()), ..quote_req("credits_1000", qty) };
	let checkout = async |qty, pay_method| {
		let q = plans.quote(&user(), &req(qty)).await.unwrap();
		let pay = CheckoutReq { quote_token: q.quote_token, pay_method };
		plans.checkout(&user(), &pay).await.unwrap().invoice_uid.unwrap()
	};

	let draft = checkout(2, PayMethod::Card).await;
	let invoices = saas_invoice::Invoices::new(app.clone());
	invoices.delete_draft(&ctx(), draft.as_str()).await.unwrap();

	let uid = checkout(3, PayMethod::Transfer).await;
	let inv = store.invoice_by_uid(None, &uid).await.unwrap().unwrap();
	pay(&app, &inv).await;
	credits_become(&app, 3000).await;
	let held: i64 = sqlx::query_scalar("SELECT held FROM ref_uses WHERE ref_id = ?")
		.bind(c.id)
		.fetch_one(store.write_pool())
		.await
		.unwrap();
	assert_eq!(held, 0, "settled on payment");
	assert_eq!(code_of(plans.quote(&user(), &req(4)).await), "E-PLAN-COUPON-INVALID");
}

#[tokio::test]
async fn a_ref_use_rewards_both_sides_once() {
	let db = TmpDb::new("reward");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	for side in ["inviter", "invitee"] {
		let key = format!("plans.reward.signup.{side}");
		app.settings.set(&key, "reward_credits", None).await.unwrap();
	}
	let r = mint(&app, "signup", serde_json::json!({})).await;
	store.ref_redeem(r.id, 1, OTHER, None, Timestamp::now()).await.unwrap().unwrap();
	let uid = r.uid.as_str();

	for _ in 0..2 {
		for side in [Side::Inviter, Side::Invitee] {
			assert!(plans.reward_ref_use(&ctx(), uid, ACC_UID, side).await.unwrap());
		}
	}
	credits_become(&app, 100).await;
	assert_eq!(other_credits(&app).await, 100);
	let not_operator = Ctx::system("test").with_org(OTHER).as_user(2);
	let r = plans.reward_ref_use(&not_operator, uid, ACC_UID, Side::Inviter).await;
	assert!(r.is_err(), "not operator");
}

/// Any org admin mints refs: only a root-owned one may name its reward offer.
#[tokio::test]
async fn a_user_minted_reward_override_is_ignored() {
	let db = TmpDb::new("reward-forged");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let rewards = serde_json::json!({"reward": {"invitee": "reward_credits"}});
	for (org, into, counts) in [(OTHER, ORG, false), (ORG, OTHER, true)] {
		let r = mint_in(&app, org, "affiliate", rewards.clone()).await;
		store.ref_redeem(r.id, 1, into, None, Timestamp::now()).await.unwrap().unwrap();
		let rewarded = plans.reward_ref_use(&ctx(), r.uid.as_str(), ACC_UID, Side::Invitee).await;
		assert_eq!(rewarded.unwrap(), counts, "minted in org {org}");
	}
	assert_eq!(other_credits(&app).await, 100);
}

async fn other_credits(app: &App) -> i64 {
	let other = Ctx::system("test").with_org(OTHER);
	Entitle::from_app(app).unwrap().balance(&other, "credits").await.unwrap()
}

#[tokio::test]
async fn a_self_referral_is_not_rewarded() {
	let db = TmpDb::new("reward-self");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	for key in ["plans.reward.signup.invitee", "plans.reward.coupon.invitee"] {
		app.settings.set(key, "reward_credits", None).await.unwrap();
	}
	let coupon = serde_json::json!({"discount": {"percentBp": 10}});
	for (ref_type, into, params) in
		[("signup", ORG, serde_json::json!({})), ("coupon", OTHER, coupon)]
	{
		let r = mint(&app, ref_type, params).await;
		store.ref_redeem(r.id, 1, into, None, Timestamp::now()).await.unwrap().unwrap();
		let rewarded = plans.reward_ref_use(&ctx(), r.uid.as_str(), ACC_UID, Side::Invitee).await;
		assert!(!rewarded.unwrap(), "{ref_type}");
	}
	assert_eq!(other_credits(&app).await, 0);
}

/// A coupon discounts only when the buyer's seller minted it, not the buyer itself.
#[tokio::test]
async fn a_coupon_minted_outside_the_seller_org_is_refused() {
	let db = TmpDb::new("coupon-foreign");
	let (app, _store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let c =
		mint_in(&app, OTHER, "coupon", serde_json::json!({"discount": {"percentBp": 10000}})).await;
	let mut req = quote_req("credits_1000", 1);
	req.coupon = Some(c.code);
	let buyer = Ctx::system("test").with_org(OTHER).as_user(2);
	assert_eq!(code_of(plans.quote(&buyer, &req).await), "E-PLAN-COUPON-INVALID");
}

/// The use row is per account: spending it again from another org must not pass as a replay.
#[tokio::test]
async fn a_coupon_used_in_one_org_is_refused_in_another() {
	let db = TmpDb::new("coupon-other-org");
	let (app, _store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let c = mint(&app, "coupon", serde_json::json!({"discount": {"percentBp": 5000}})).await;
	let mut req = quote_req("credits_1000", 1);
	req.coupon = Some(c.code);
	for (org, ok) in [(ORG, true), (OTHER, false)] {
		let as_user = Ctx::system("test").with_org(org).as_user(1);
		let q = plans.quote(&as_user, &req).await.unwrap();
		let pay = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
		let out = plans.checkout(&as_user, &pay).await;
		if ok {
			out.unwrap();
		} else {
			assert_eq!(code_of(out), "E-PLAN-COUPON-INVALID");
		}
	}
}

/// Two submits of one token: the pass that lost the link race skipped the coupon and issued the
/// discounted invoice anyway, with no use recorded for the org.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parallel_replay_never_issues_a_discount_without_its_use() {
	let db = TmpDb::new("coupon-race");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	for _ in 0..20 {
		let req = CreateRef {
			ref_type: "coupon".into(),
			params: Some(serde_json::json!({"discount": {"percentBp": 5000}})),
			uses_left: Some(1),
			..Default::default()
		};
		let c = Refs::from_app(&app).unwrap().mint(&ctx(), &req).await.unwrap();
		let mut q = quote_req("credits_1000", 1);
		q.coupon = Some(c.code.clone());
		let q = plans.quote(&user(), &q).await.unwrap();
		store.ref_redeem(c.id, 2, OTHER, None, Timestamp::now()).await.unwrap().unwrap();
		let pay = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
		let buyer = user();
		let both = tokio::join!(plans.checkout(&buyer, &pay), plans.checkout(&buyer, &pay));
		for out in [both.0, both.1] {
			match out {
				Err(Error::Coded { code, .. }) => assert_eq!(code, "E-PLAN-COUPON-INVALID"),
				// Both inserted the shared draft at once: refused before any link.
				Err(Error::Conflict(_)) => {}
				other => panic!("issued with the discount but no use: {other:?}"),
			}
		}
	}
}

/// A fully discounted invoice settles with nothing paid: rewarding it let burner orgs farm the
/// referral reward.
#[tokio::test]
async fn a_zero_total_settlement_pays_no_referral_reward() {
	let db = TmpDb::new("reward-zero");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	app.settings.set("plans.reward_on", "first_payment", None).await.unwrap();
	for side in ["inviter", "invitee"] {
		let key = format!("plans.reward.signup.{side}");
		app.settings.set(&key, "reward_credits", None).await.unwrap();
	}
	let r = mint_in(&app, OTHER, "signup", serde_json::json!({})).await;
	store.ref_redeem(r.id, 1, ORG, None, Timestamp::now()).await.unwrap().unwrap();
	let c = mint(&app, "coupon", serde_json::json!({"discount": {"percentBp": 10000}})).await;
	let mut req = quote_req("credits_1000", 1);
	req.coupon = Some(c.code);
	let q = plans.quote(&user(), &req).await.unwrap();
	let pay = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	let invoice = plans.checkout(&user(), &pay).await.unwrap().invoice_uid.unwrap();

	let payment = PaymentId::generate();
	let ev = saas_core::event::Event::PaymentSettled { payment, invoice };
	saas_plans::events::handle(app.clone(), ev).await.unwrap();
	assert_eq!(other_credits(&app).await, 0, "the inviter earned nothing");
}

/// A zero-gross invoice is issued and never paid, so no settlement ever granted the purchase.
#[tokio::test]
async fn a_zero_total_purchase_grants_without_a_payment() {
	let db = TmpDb::new("zero-grant");
	let (app, _store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let c = mint(&app, "coupon", serde_json::json!({"discount": {"percentBp": 10000}})).await;
	let mut req = quote_req("credits_1000", 1);
	req.coupon = Some(c.code);
	let q = plans.quote(&user(), &req).await.unwrap();
	let pay = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	assert_eq!(plans.checkout(&user(), &pay).await.unwrap().next, "issued");
	credits_become(&app, 1000).await;
}

/// The settlement retry pays a `first_payment` reward too, and only once however often it runs.
#[tokio::test]
async fn the_settled_job_pays_the_first_payment_reward_once() {
	let db = TmpDb::new("reward-job");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	app.settings.set("plans.reward_on", "first_payment", None).await.unwrap();
	app.settings
		.set("plans.reward.signup.inviter", "reward_credits", None)
		.await
		.unwrap();
	let r = mint_in(&app, OTHER, "signup", serde_json::json!({})).await;
	store.ref_redeem(r.id, 1, ORG, None, Timestamp::now()).await.unwrap().unwrap();
	// Issued, never paid: no event fires, so only the job can settle it.
	let q = plans.quote(&user(), &quote_req("credits_1000", 1)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	let invoice = plans.checkout(&user(), &req).await.unwrap().invoice_uid.unwrap();

	let payload =
		serde_json::json!({ "payment": PaymentId::generate(), "invoice": invoice }).to_string();
	let at = Timestamp::now();
	for _ in 0..2 {
		let job = saas_core::job::enqueue(&app.store, "PLANS_SETTLED", &payload, None, at);
		job.await.unwrap().unwrap();
		let mut runner =
			saas_core::job::Runner::with_settings(app.store.clone(), app.settings.clone());
		saas_plans::renew::register(&mut runner, app.clone());
		while runner.tick(at).await.unwrap() {}
	}
	credits_become(&app, 1000).await;
	assert_eq!(other_credits(&app).await, 100, "rewarded exactly once");
}

#[tokio::test]
async fn the_meter_gate_charges_one_invite_per_ref() {
	let db = TmpDb::new("gate");
	let (app, _store) = setup(&db).await;
	let gate = MeterInviteGate;
	let entitle = Entitle::from_app(&app).unwrap();
	assert_eq!(code_of(gate.may_invite(&app, &user(), "signup").await), "E-ENT-EXHAUSTED");

	let grant = GrantReq {
		key: "invites".into(),
		amount: 1,
		valid_from: None,
		valid_until: None,
		source: Source::Manual,
		source_ref: Some("t".into()),
	};
	entitle.grant_to(&ctx(), ORG, &grant).await.unwrap();
	gate.may_invite(&app, &user(), "signup").await.unwrap();
	let r = mint(&app, "signup", serde_json::json!({})).await;
	gate.invited(&app, &user(), &r.uid).await.unwrap();
	gate.invited(&app, &user(), &r.uid).await.unwrap();
	assert_eq!(entitle.balance(&ctx(), "invites").await.unwrap(), 0, "one charge per ref uid");
	assert_eq!(code_of(gate.may_invite(&app, &user(), "signup").await), "E-ENT-EXHAUSTED");
}

// vim: ts=4
