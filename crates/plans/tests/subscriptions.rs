// SPDX-License-Identifier: MPL-2.0
//! Subscription service tests: subscribe (with and without trial), renewal by transfer, the
//! grace lapse, PAST_DUE/SUSPENDED and cancel-at-period-end, driving `renew_due` with explicit
//! timestamps.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use mintworks_billing::provider::{
	CallbackRef, PaymentProvider, PaymentProviders, PaymentState, ProviderCaps, RecurrenceHook,
	RefundResult, StartPayment, StartedPayment,
};
use mintworks_billing::store::BillingStore;
use mintworks_billing::{Allocation, ManualPayment, StartRequest, allocate};
use mintworks_core::ids::SubscriptionId;
use mintworks_core::refs::RefStore;
use mintworks_core::{App, AppBuilder, config::Config, ctx::Ctx, ids::SellerId, prelude::*};
use mintworks_entitle::{EntitleStore, EntitlementDef, EntitlementRegistry};
use mintworks_invoice::store::{Invoice, InvoiceStatus, InvoiceStore, Seller, SellerVersionPatch};
use mintworks_plans::{
	AdminCancelReq, CheckoutReq, Interval, LinkKind, OfferDef, PayMethod, PlanStore, Plans,
	QuoteReq, RefundMode, RepriceReq, SubStatus, SubsFilter, Subscription, reconcile_with,
};
use mintworks_store_sqlite::SqliteStore;

const ORG: i64 = 1;
const SELLER: i64 = 1;
const ORG_UID: &str = "org_t";

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-plans-sub-test-{}-{name}", std::process::id()));
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
		// Tests drive the runner themselves (`run_jobs`).
		jobs_workers: Some(0),
	}
}

fn registry() -> EntitlementRegistry {
	EntitlementRegistry::new([EntitlementDef::meter("credits"), EntitlementDef::meter("invites")])
}

const DAY: i64 = 86_400;

/// Monthly, family `plan`, 1000 credits a period, 5000 HUF net; no trial.
fn basic() -> OfferDef {
	let mut d = OfferDef::recurring("basic", "Basic", "CREDITS", Interval::Month, 1)
		.price(CurrencyCode::huf(), 500_000)
		.entitle("credits", 1000, false);
	d.family = Some("plan".into());
	d
}

/// Same family, rank above `basic`, with a 14-day trial.
fn pro() -> OfferDef {
	let mut d = OfferDef::recurring("pro", "Pro", "CREDITS", Interval::Month, 1)
		.price(CurrencyCode::huf(), 900_000)
		.entitle("credits", 5000, false);
	d.family = Some("plan".into());
	d.rank = 1;
	d.trial_days = 14;
	d
}

/// `pro` billed yearly: same family and rank, so a trial of `pro` may be paid ahead as this.
fn pro_year() -> OfferDef {
	let mut d = OfferDef::recurring("pro_year", "Pro yearly", "CREDITS", Interval::Year, 1)
		.price(CurrencyCode::huf(), 9_000_000)
		.entitle("credits", 5000, false);
	d.family = Some("plan".into());
	d.rank = 1;
	d.trial_days = 14;
	d
}

/// Another family: a change to it from `plan` is a new subscription.
fn addon() -> OfferDef {
	let mut d = OfferDef::recurring("addon", "Addon", "CREDITS", Interval::Month, 1)
		.price(CurrencyCode::huf(), 100_000)
		.entitle("credits", 10, false);
	d.family = Some("addon".into());
	d
}

/// The billing suite's fixture: org `ORG` is both the seller and the buyer, root moved to 0.
async fn setup(db: &TmpDb) -> (App, SqliteStore) {
	setup_with(db, |b| b).await
}

async fn setup_with(
	db: &TmpDb,
	extra: impl FnOnce(AppBuilder) -> AppBuilder,
) -> (App, SqliteStore) {
	let store = SqliteStore::open(&config(db)).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let builder = AppBuilder::new()
		.config(config(db))
		.store(Arc::new(store.clone()) as Arc<dyn mintworks_core::store::CoreStore>)
		.settings(mintworks_invoice::SETTINGS)
		.settings(mintworks_billing::SETTINGS)
		.settings(mintworks_plans::SETTINGS)
		.secrets(mintworks_plans::SECRETS)
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_auth::store::AuthStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn InvoiceStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn BillingStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn EntitleStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn PlanStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn RefStore>);
	let builder = extra(builder);
	let builder = mintworks_entitle::install(
		builder,
		[EntitlementDef::meter("credits"), EntitlementDef::meter("invites")],
	);
	let app = mintworks_plans::install(builder, []).build().await.unwrap();

	for sql in [
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
		"UPDATE orgs SET id = 0 WHERE kind = 'ROOT'",
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (1, 'org_t', 0, 'SHARED', 'Teszt', 1, 0)",
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (2, 'acc_m', 'm@e.st', 0)",
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (1, 1, 'OWNER', 0, 0), (1, 2, 'MEMBER', 0, 0)",
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  email, is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', 1, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 'vevo@e.st', 1, 0, 0)",
		"INSERT INTO services (uid, org_id, code, name, unit, unit_price, vat_code, created_at,
		 updated_at) VALUES ('svc_t', 1, 'CREDITS', 'Kredit csomag', 'db', 0, 'STD27', 0, 0)",
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
			test_mode: false,
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
	reconcile_with(&store, Some(&registry()), ORG, &[basic(), pro(), pro_year(), addon()])
		.await
		.unwrap();
	(app, store)
}

fn ctx() -> Ctx {
	Ctx::system("test").with_org(ORG)
}

/// Account 1, OWNER of `ORG`.
fn owner() -> Ctx {
	ctx().as_user(1)
}

/// Runs every job due at `at` through a runner carrying the plans handlers.
async fn run_jobs(app: &App, at: Timestamp) {
	let mut r = mintworks_core::job::Runner::with_settings(app.store.clone(), app.settings.clone());
	mintworks_plans::renew::register(&mut r, app.clone());
	while r.tick(at).await.unwrap() {}
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

async fn sub(store: &SqliteStore, uid: &SubscriptionId) -> Subscription {
	store.sub_by_uid(uid).await.unwrap().unwrap()
}

/// `(source, valid_until)` of the org's credits grant for one period.
async fn grant(store: &SqliteStore, source_ref: &str) -> Option<(String, Option<i64>)> {
	sqlx::query_as("SELECT source, valid_until FROM grants WHERE org_id = ? AND source_ref = ?")
		.bind(ORG)
		.bind(source_ref)
		.fetch_optional(store.write_pool())
		.await
		.unwrap()
}

fn period_ref(s: &Subscription, start: Timestamp) -> String {
	format!("sub:{}:{}", s.uid, start.0)
}

/// Polls the spawned settle handler until the period's grant reaches `until`.
async fn grant_until_becomes(store: &SqliteStore, source_ref: &str, until: Timestamp) {
	for _ in 0..100 {
		if grant(store, source_ref).await.and_then(|g| g.1) == Some(until.0) {
			return;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("grant {source_ref} never reached {}", until.0);
}

async fn pay(app: &App, inv: &Invoice) {
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
	.unwrap();
}

/// Quote → checkout TRANSFER for `offer`; returns the sub and its first invoice.
async fn subscribe(app: &App, store: &SqliteStore, offer: &str) -> (Subscription, Invoice) {
	let plans = Plans::from_app(app).unwrap();
	let q = plans.quote(&ctx(), &quote_req(offer, 1)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	let out = plans.checkout(&ctx(), &req).await.unwrap();
	assert_eq!(out.next, "issued");
	let replay = plans.checkout(&ctx(), &req).await.unwrap();
	assert_eq!(replay.subscription_uid, out.subscription_uid, "a replay makes no second sub");
	let s = sub(store, out.subscription_uid.as_ref().unwrap()).await;
	// The period starts at checkout, which may fall a second after the quote under load.
	let lag = s.period_start.0 - q.period_start.unwrap().0;
	assert!((0..=1).contains(&lag), "period starts {lag}s after the quote");
	assert_eq!(s.period_end.0 - lag, q.period_end.unwrap().0);
	let inv = store
		.invoice_by_uid(None, out.invoice_uid.as_ref().unwrap())
		.await
		.unwrap()
		.unwrap();
	assert_eq!(inv.status, InvoiceStatus::Issued);
	(s, inv)
}

async fn latest_invoice(store: &SqliteStore, s: &Subscription) -> Invoice {
	let link = store.plan_invoice_latest(s.id).await.unwrap().unwrap();
	store.invoice_by_uid(None, &link.invoice_uid).await.unwrap().unwrap()
}

/// A provisional meter row is insert-once, so it handed an unpaid TRANSFER period its whole
/// quota for the grace days.
#[tokio::test]
async fn a_transfer_subscription_grants_no_meter_until_paid() {
	let db = TmpDb::new("subscribe");
	let (app, store) = setup(&db).await;
	let (s, inv) = subscribe(&app, &store, "basic").await;
	assert_eq!(s.status, SubStatus::Active);
	assert!(s.period_end.0 >= s.period_start.0 + 28 * DAY, "a calendar month");
	let r = period_ref(&s, s.period_start);
	assert_eq!(grant(&store, &r).await, None, "credits is a meter");

	pay(&app, &inv).await;
	grant_until_becomes(&store, &r, s.period_end).await;
}

#[tokio::test]
async fn a_second_live_sub_in_the_family_is_refused() {
	let db = TmpDb::new("family");
	let (app, store) = setup(&db).await;
	subscribe(&app, &store, "basic").await;
	let plans = Plans::from_app(&app).unwrap();
	// The family already had a sub, so `pro` quotes without its trial.
	let q = plans.quote(&ctx(), &quote_req("pro", 1)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	assert_eq!(code_of(plans.checkout(&ctx(), &req).await), "E-PLAN-FAMILY-LIVE");
}

#[tokio::test]
async fn a_trial_invoices_nothing_then_renews_into_a_paid_period() {
	let db = TmpDb::new("trial");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let q = plans.quote(&ctx(), &quote_req("pro", 1)).await.unwrap();
	assert_eq!(q.gross.amount, Money::ZERO.to_wire(&CurrencyCode::huf()).amount);
	assert_eq!(q.period_end.unwrap().0 - q.period_start.unwrap().0, 14 * DAY);
	let out = plans
		.checkout(
			&ctx(),
			&CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer },
		)
		.await
		.unwrap();
	assert_eq!((out.next, out.invoice_uid.is_none()), ("trialing", true));
	let s = sub(&store, out.subscription_uid.as_ref().unwrap()).await;
	assert_eq!(s.status, SubStatus::Trialing);
	let (source, until) = grant(&store, &period_ref(&s, s.period_start)).await.unwrap();
	assert_eq!((source.as_str(), until), ("TRIAL", Some(s.period_end.0)));

	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let renewed = sub(&store, &s.uid).await;
	assert_eq!(renewed.status, SubStatus::Active);
	assert_eq!(renewed.period_start, s.period_end);
	let inv = latest_invoice(&store, &renewed).await;
	assert_eq!(inv.status, InvoiceStatus::Issued);
	assert_eq!(inv.net, Money(900_000));
	let link = store.plan_invoice_get(&inv.uid).await.unwrap().unwrap();
	assert_eq!(link.kind, LinkKind::Renewal);

	// Idempotent: the same tick again renews nothing further.
	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	assert_eq!(sub(&store, &s.uid).await.period_start, renewed.period_start);
	assert_eq!(latest_invoice(&store, &renewed).await.uid, inv.uid);
}

/// A trial whose checkout fails leaves no row behind: a CANCELED one would count as used.
#[tokio::test]
async fn a_failed_trial_checkout_keeps_the_trial() {
	let db = TmpDb::new("trial-failed");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let req = mintworks_core::refs::CreateRef {
		ref_type: "coupon".into(),
		params: Some(serde_json::json!({"discount": {"percentBp": 1000}})),
		uses_left: Some(1),
		..Default::default()
	};
	let c = mintworks_core::refs::Refs::from_app(&app)
		.unwrap()
		.mint(&ctx(), &req)
		.await
		.unwrap();
	let mut with_coupon = quote_req("pro", 1);
	with_coupon.coupon = Some(c.code);
	let q = plans.quote(&ctx(), &with_coupon).await.unwrap();
	store.ref_redeem(c.id, 2, 1, None, Timestamp::now()).await.unwrap().unwrap();
	let pay = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	assert_eq!(code_of(plans.checkout(&ctx(), &pay).await), "E-PLAN-COUPON-INVALID");

	let q = plans.quote(&ctx(), &quote_req("pro", 1)).await.unwrap();
	assert_eq!(q.period_end.unwrap().0 - q.period_start.unwrap().0, 14 * DAY, "still a trial");
}

/// The coupon was redeemed before the trial's grants, so a grant failure rolled the sub back
/// but left the use spent.
#[tokio::test]
async fn a_failed_trial_grant_leaves_the_coupon_unspent() {
	let db = TmpDb::new("trial-grant-failed");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let c = mint(&app, "coupon", serde_json::json!({"discount": {"percentBp": 1000}})).await;
	let q = plans
		.quote(&owner(), &QuoteReq { coupon: Some(c.code), ..quote_req("pro", 2) })
		.await
		.unwrap();
	// Per seat × 2 overflows, so the trial grant fails after the sub is inserted.
	sqlx::query(
		"UPDATE offer_entitlements SET amount = ?, per_seat = 1
		 WHERE offer_id = (SELECT id FROM offers WHERE code = 'pro')",
	)
	.bind(i64::MAX)
	.execute(store.write_pool())
	.await
	.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	assert!(plans.checkout(&owner(), &req).await.is_err());
	assert_eq!(store.ref_use_of(c.id, 1).await.unwrap(), None, "the coupon stays unspent");
	assert!(store.subs_of_org(ORG).await.unwrap().is_empty(), "the sub is rolled back");
}

/// A replay of a trial checkout found the trial used by its own first pass and answered 409.
#[tokio::test]
async fn a_replayed_trial_checkout_returns_the_same_sub() {
	let db = TmpDb::new("trial-replay");
	let (app, _store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let q = plans.quote(&ctx(), &quote_req("pro", 1)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	let out = plans.checkout(&ctx(), &req).await.unwrap();
	let replay = plans.checkout(&ctx(), &req).await.unwrap();
	assert_eq!((replay.next, replay.subscription_uid), ("trialing", out.subscription_uid));
}

/// A trial was once per org, so its owner took a fresh one in every org they created.
#[tokio::test]
async fn a_second_org_of_the_same_owner_gets_no_trial() {
	let db = TmpDb::new("trial-owner");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (3, 'org_3', 1, 'SHARED', 'Masik', 1, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	let span = |q: &mintworks_plans::Quote| q.period_end.unwrap().0 - q.period_start.unwrap().0;
	let other = Ctx::system("test").with_org(3);
	assert_eq!(span(&plans.quote(&other, &quote_req("pro", 1)).await.unwrap()), 14 * DAY);
	let q = plans.quote(&ctx(), &quote_req("pro", 1)).await.unwrap();
	assert_eq!(span(&q), 14 * DAY);
	checkout_transfer(&plans, q.quote_token).await.unwrap();
	assert_ne!(span(&plans.quote(&other, &quote_req("pro", 1)).await.unwrap()), 14 * DAY);
}

/// `PaymentSettled` is not durable: a crash before its handler enqueued the settle job lost
/// the period's grants for good.
#[tokio::test]
async fn a_lost_settlement_is_caught_up_by_the_renewal_sweep() {
	let db = TmpDb::new("settle-catch-up");
	let (app, store) = setup(&db).await;
	let (s, inv) = subscribe(&app, &store, "basic").await;
	let r = period_ref(&s, s.period_start);
	pay(&app, &inv).await;
	grant_until_becomes(&store, &r, s.period_end).await;
	sqlx::query("DELETE FROM jobs WHERE kind = 'PLANS_SETTLED'")
		.execute(store.write_pool())
		.await
		.unwrap();
	sqlx::query("DELETE FROM grants WHERE source_ref = ?")
		.bind(&r)
		.execute(store.write_pool())
		.await
		.unwrap();

	let now = Timestamp::now();
	Plans::from_app(&app)
		.unwrap()
		.renew_due(&Ctx::system("test"), now)
		.await
		.unwrap();
	run_jobs(&app, now).await;
	assert_eq!(grant(&store, &r).await.and_then(|g| g.1), Some(s.period_end.0));
}

#[tokio::test]
async fn an_unpaid_renewal_goes_past_due_then_suspended() {
	let db = TmpDb::new("dunning");
	let (app, store) = setup(&db).await;
	let (s, first) = subscribe(&app, &store, "basic").await;
	pay(&app, &first).await;
	let plans = Plans::from_app(&app).unwrap();

	let start = s.period_end;
	plans.renew_due(&Ctx::system("test"), start).await.unwrap();
	let renewed = sub(&store, &s.uid).await;
	let r = period_ref(&renewed, start);
	assert_eq!(grant(&store, &r).await, None, "no meter until paid");
	assert_eq!(latest_invoice(&store, &renewed).await.status, InvoiceStatus::Issued);

	plans
		.renew_due(&Ctx::system("test"), Timestamp(start.0 + 2 * DAY))
		.await
		.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Active, "inside grace");
	plans
		.renew_due(&Ctx::system("test"), Timestamp(start.0 + 3 * DAY))
		.await
		.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::PastDue);
	plans
		.renew_due(&Ctx::system("test"), Timestamp(start.0 + 14 * DAY))
		.await
		.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Suspended);

	// Renewals stop: a SUSPENDED sub is never due.
	let end = sub(&store, &s.uid).await.period_end;
	plans.renew_due(&Ctx::system("test"), end).await.unwrap();
	assert_eq!(sub(&store, &s.uid).await.period_end, end);
}

/// PAST_DUE subs still renew, so a clock started at the latest period never reached a
/// `suspend_after_days` longer than one period.
#[tokio::test]
async fn a_past_due_sub_is_suspended_even_when_suspend_exceeds_a_period() {
	let db = TmpDb::new("dunning-long");
	let (app, store) = setup(&db).await;
	app.settings.set("plans.suspend_after_days", "45", None).await.unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let plans = Plans::from_app(&app).unwrap();
	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let renewed = sub(&store, &s.uid).await;
	assert_eq!(renewed.status, SubStatus::PastDue);
	plans.renew_due(&Ctx::system("test"), renewed.period_end).await.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Suspended);
}

#[tokio::test]
async fn an_unpaid_transfer_upgrade_goes_past_due() {
	let db = TmpDb::new("dunning-upgrade");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, first) = subscribe(&app, &store, "basic").await;
	pay(&app, &first).await;
	grant_until_becomes(&store, &period_ref(&s, s.period_start), s.period_end).await;
	let s = mid_period(&store, &s).await;
	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	assert_eq!(checkout_transfer(&plans, q.quote_token).await.unwrap().next, "issued");

	plans
		.renew_due(&Ctx::system("test"), Timestamp(Timestamp::now().0 + 4 * DAY))
		.await
		.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::PastDue);
}

#[tokio::test]
async fn cancel_at_period_end_ends_the_sub_without_a_renewal() {
	let db = TmpDb::new("cancel");
	let (app, store) = setup(&db).await;
	let (s, first) = subscribe(&app, &store, "basic").await;
	let plans = Plans::from_app(&app).unwrap();

	assert!(plans.cancel(&ctx(), s.uid.as_str()).await.unwrap().cancel_at_period_end);
	assert!(!plans.resume(&ctx(), s.uid.as_str()).await.unwrap().cancel_at_period_end);
	plans.cancel(&ctx(), s.uid.as_str()).await.unwrap();

	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let done = sub(&store, &s.uid).await;
	assert_eq!(done.status, SubStatus::Canceled);
	assert_eq!(done.period_end, s.period_end);
	assert_eq!(latest_invoice(&store, &done).await.uid, first.uid, "no renewal invoice");
	assert!(plans.resume(&ctx(), s.uid.as_str()).await.is_err(), "CANCELED is final");
}

fn change_req(offer: &str, qty: i64, s: &Subscription) -> QuoteReq {
	QuoteReq { subscription: Some(s.uid.to_string()), ..quote_req(offer, qty) }
}

/// Puts the sub 15 days into a 30-day period, so the remainder is exactly half.
async fn mid_period(store: &SqliteStore, s: &Subscription) -> Subscription {
	let now = Timestamp::now().0;
	sqlx::query("UPDATE subscriptions SET period_start = ?, period_end = ? WHERE id = ?")
		.bind(now - 15 * DAY)
		.bind(now + 15 * DAY)
		.bind(s.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	sub(store, &s.uid).await
}

async fn checkout_transfer(plans: &Plans, token: String) -> ClResult<mintworks_plans::Checkout> {
	let req = CheckoutReq { quote_token: token, pay_method: PayMethod::Transfer };
	plans.checkout(&ctx(), &req).await
}

#[tokio::test]
async fn an_upgrade_invoices_the_prorated_difference_and_applies_now() {
	let db = TmpDb::new("upgrade");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let s = mid_period(&store, &s).await;

	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	assert_eq!(q.effective, "now");
	// (9000 − 5000) × 15/30 days.
	assert_eq!(q.net.amount, "2000.00");
	assert_eq!(q.period_end, Some(s.period_end));
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	let out = plans.checkout(&ctx(), &req).await.unwrap();
	assert_eq!(out.next, "issued");
	let replay = plans.checkout(&ctx(), &req).await.unwrap();
	assert_eq!(replay.invoice_uid, out.invoice_uid, "a replay makes no second invoice");

	let inv = out.invoice_uid.unwrap();
	let link = store.plan_invoice_get(&inv).await.unwrap().unwrap();
	assert_eq!(link.kind, LinkKind::Upgrade);
	let after = sub(&store, &s.uid).await;
	assert_eq!((after.qty, after.price), (1, Money(900_000)));
	assert_eq!((after.period_start, after.period_end), (s.period_start, s.period_end));
	assert_eq!(grant(&store, &format!("sub:{}:up:{inv}", s.uid)).await, None, "unpaid meter");
	// The upgrade link is not a period: `plan_invoice_latest` (the status sweep) skips it.
	assert_ne!(store.plan_invoice_latest(after.id).await.unwrap().unwrap().invoice_uid, inv);
}

#[tokio::test]
async fn a_change_to_the_sub_since_the_quote_is_stale() {
	let db = TmpDb::new("change-stale");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	plans.cancel(&ctx(), s.uid.as_str()).await.unwrap();
	assert_eq!(code_of(checkout_transfer(&plans, q.quote_token).await), "E-PLAN-QUOTE-STALE");
	assert_eq!(sub(&store, &s.uid).await.price, Money(500_000), "nothing applied");
}

#[tokio::test]
async fn a_downgrade_is_free_and_applied_by_renew_due_at_the_boundary() {
	let db = TmpDb::new("downgrade");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	checkout_transfer(&plans, q.quote_token).await.unwrap();
	let s = sub(&store, &s.uid).await;

	let q = plans.quote(&ctx(), &change_req("basic", 1, &s)).await.unwrap();
	assert_eq!((q.effective, q.gross.amount.as_str()), ("period_end", "0.00"));
	assert_eq!(q.period_start, Some(s.period_end));
	let out = checkout_transfer(&plans, q.quote_token).await.unwrap();
	assert_eq!((out.next, out.invoice_uid), ("scheduled", None));
	let queued = sub(&store, &s.uid).await;
	assert!(queued.next_offer_id.is_some());
	assert_eq!(queued.price, Money(900_000), "nothing changes before the boundary");

	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let after = sub(&store, &s.uid).await;
	assert_eq!((after.price, after.next_offer_id, after.next_qty), (Money(500_000), None, None));
	assert_eq!(after.period_start, s.period_end);
	assert_eq!(latest_invoice(&store, &after).await.net, Money(500_000));
}

#[tokio::test]
async fn seats_go_up_now_and_down_at_the_boundary() {
	let db = TmpDb::new("seats");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let s = mid_period(&store, &s).await;

	let q = plans.quote(&ctx(), &change_req("basic", 3, &s)).await.unwrap();
	assert_eq!((q.effective, q.net.amount.as_str()), ("now", "5000.00"));
	checkout_transfer(&plans, q.quote_token).await.unwrap();
	let s = sub(&store, &s.uid).await;
	assert_eq!(s.qty, 3);

	let q = plans.quote(&ctx(), &change_req("basic", 2, &s)).await.unwrap();
	assert_eq!(q.effective, "period_end");
	checkout_transfer(&plans, q.quote_token).await.unwrap();
	let s = sub(&store, &s.uid).await;
	assert_eq!((s.qty, s.next_offer_id, s.next_qty), (3, None, Some(2)));
	plans.cancel_change(&ctx(), s.uid.as_str()).await.unwrap();
	assert_eq!(sub(&store, &s.uid).await.next_qty, None);

	let same = plans.quote(&ctx(), &change_req("basic", 3, &s)).await;
	assert_eq!(code_of(same), "E-PLAN-CHANGE-INVALID");
}

#[tokio::test]
async fn a_change_across_families_is_refused() {
	let db = TmpDb::new("cross-family");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let r = plans.quote(&ctx(), &change_req("addon", 1, &s)).await;
	assert_eq!(code_of(r), "E-PLAN-FAMILY-MISMATCH");
}

#[tokio::test]
async fn an_immediate_operator_cancel_cuts_grants_and_refunds_the_rest() {
	let db = TmpDb::new("admin-cancel");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, inv) = subscribe(&app, &store, "basic").await;
	pay(&app, &inv).await;
	grant_until_becomes(&store, &period_ref(&s, s.period_start), s.period_end).await;

	let later = AdminCancelReq { immediate: false, refund: RefundMode::Prorated };
	assert!(matches!(
		plans.admin_cancel(&ctx(), s.uid.as_str(), &later).await,
		Err(Error::Validation(_))
	));
	let now = AdminCancelReq { immediate: true, refund: RefundMode::Prorated };
	let out = plans.admin_cancel(&ctx(), s.uid.as_str(), &now).await.unwrap();
	assert_eq!(out.status, SubStatus::Canceled);
	let g = grant(&store, &period_ref(&s, s.period_start)).await.unwrap();
	assert!(g.1.unwrap() <= Timestamp::now().0, "cut at now");
	// Canceled seconds after the period began: the whole payment comes back, rounded once.
	let paid = store.payments_by_invoice(ORG, inv.id).await.unwrap();
	assert_eq!(paid[0].refunded_amount, inv.gross);
	// A prorated cancel is retryable, so a refund that failed after the cancel can be re-run;
	// one that already went through refunds nothing more.
	plans.admin_cancel(&ctx(), s.uid.as_str(), &now).await.unwrap();
	let again = store.payments_by_invoice(ORG, inv.id).await.unwrap();
	let sum = |ps: &[mintworks_billing::store::Payment]| {
		ps.iter().map(|p| p.refunded_amount.0).sum::<i64>()
	};
	assert_eq!(sum(&again), sum(&paid), "a retry refunded again");
	let later = AdminCancelReq { immediate: false, refund: RefundMode::None };
	assert!(plans.admin_cancel(&ctx(), s.uid.as_str(), &later).await.is_err(), "canceled is final");
}

#[tokio::test]
async fn an_unrefunded_immediate_cancel_may_be_rerun_prorated() {
	let db = TmpDb::new("admin-cancel-rerun-refund");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, inv) = subscribe(&app, &store, "basic").await;
	pay(&app, &inv).await;
	grant_until_becomes(&store, &period_ref(&s, s.period_start), s.period_end).await;
	let inv_id = inv.id;
	let refunded = |store: SqliteStore| async move {
		let ps = store.payments_by_invoice(ORG, inv_id).await.unwrap();
		ps.iter().map(|p| p.refunded_amount.0).sum::<i64>()
	};

	let none = AdminCancelReq { immediate: true, refund: RefundMode::None };
	plans.admin_cancel(&ctx(), s.uid.as_str(), &none).await.unwrap();
	assert_eq!(refunded(store.clone()).await, 0);
	let prorated = AdminCancelReq { immediate: true, refund: RefundMode::Prorated };
	plans.admin_cancel(&ctx(), s.uid.as_str(), &prorated).await.unwrap();
	assert_eq!(refunded(store.clone()).await, inv.gross.0, "the unused share, once");
	plans.admin_cancel(&ctx(), s.uid.as_str(), &prorated).await.unwrap();
	assert_eq!(refunded(store.clone()).await, inv.gross.0, "nothing more");
}

#[tokio::test]
async fn a_reprice_rewrites_live_subs_for_their_next_renewal() {
	let db = TmpDb::new("reprice");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let req = RepriceReq { currency: s.currency.clone(), amount: Money(600_000) };
	let subs = plans.reprice(&ctx(), "basic", &req).await.unwrap();
	assert_eq!(subs.len(), 1);
	assert_eq!(sub(&store, &s.uid).await.price, Money(600_000));
	assert!(plans.reprice(&ctx(), "basic", &req).await.unwrap().is_empty(), "already at it");
}

#[tokio::test]
async fn a_reprice_needs_step_up() {
	let db = TmpDb::new("reprice-stepup");
	let (app, store) = setup(&db).await;
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (0, 1, 'ADMIN', 0, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	let plans = Plans::from_app(&app).unwrap();
	let req = RepriceReq { currency: CurrencyCode::huf(), amount: Money(600_000) };
	let operator = ctx().as_user(1);
	assert_eq!(code_of(plans.reprice(&operator, "basic", &req).await), "E-AUTH-STEPUP-IMPOSSIBLE");
	let stale = Ctx { auth_at: Some(Timestamp::now().0 - 86_400), ..operator };
	assert_eq!(code_of(plans.reprice(&stale, "basic", &req).await), "E-AUTH-STEPUP");
}

#[tokio::test]
async fn a_reprice_off_the_round_step_is_refused() {
	let db = TmpDb::new("reprice-step");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let req = RepriceReq { currency: s.currency.clone(), amount: Money(600_050) };
	assert_eq!(code_of(plans.reprice(&ctx(), "basic", &req).await), "E-INV-LINE");
	assert_eq!(sub(&store, &s.uid).await.price, s.price);
}

#[tokio::test]
async fn a_member_cannot_checkout_or_cancel() {
	let db = TmpDb::new("member");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let member = ctx().as_user(2);
	assert_eq!(code_of(plans.quote(&member, &quote_req("basic", 1)).await), "E-AUTH-FORBIDDEN");
	let q = plans.quote(&owner(), &quote_req("basic", 1)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	assert_eq!(code_of(plans.checkout(&member, &req).await), "E-AUTH-FORBIDDEN");

	let (s, _) = subscribe(&app, &store, "basic").await;
	let uid = s.uid.as_str();
	assert_eq!(code_of(plans.cancel(&member, uid).await), "E-AUTH-FORBIDDEN");
	assert_eq!(code_of(plans.resume(&member, uid).await), "E-AUTH-FORBIDDEN");
	assert_eq!(code_of(plans.cancel_change(&member, uid).await), "E-AUTH-FORBIDDEN");
	assert_eq!(plans.subscriptions(&member).await.unwrap().len(), 1, "reading stays open");
}

/// `subscriptions_in` reads another org's subs whatever org is acting, for its OWNER only.
#[tokio::test]
async fn subscriptions_in_is_for_the_orgs_owner_only() {
	let db = TmpDb::new("subs-in");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let org = "org_01JCZ5X8K9N7QW3M6R2T4V8Y0C";
	sqlx::query("UPDATE orgs SET uid = ? WHERE id = 1")
		.bind(org)
		.execute(store.write_pool())
		.await
		.unwrap();
	subscribe(&app, &store, "basic").await;
	let elsewhere = || Ctx::system("test").with_org(0);
	assert_eq!(plans.subscriptions_in(&elsewhere().as_user(1), org).await.unwrap().len(), 1);
	let member = plans.subscriptions_in(&elsewhere().as_user(2), org).await;
	assert!(matches!(member, Err(Error::NotFound)), "{member:?}");
	// A key minted for another org is bound to that org, whatever its holder owns.
	let mut key = elsewhere();
	key.actor = mintworks_core::ctx::Actor::Key { account_id: 1, key_id: 1 };
	let key = plans.subscriptions_in(&key, org).await;
	assert!(matches!(key, Err(Error::NotFound)), "{key:?}");
	let stranger = plans.subscriptions_in(&elsewhere().as_user(999), org).await;
	assert!(matches!(stranger, Err(Error::NotFound)), "{stranger:?}");
	let unknown = plans
		.subscriptions_in(&elsewhere().as_user(1), "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D")
		.await;
	assert!(matches!(unknown, Err(Error::NotFound)), "{unknown:?}");
	sqlx::query("UPDATE orgs SET status = 'SUSPENDED' WHERE id = 1")
		.execute(store.write_pool())
		.await
		.unwrap();
	// `org_role` reads a suspended org as no membership: its owner is a stranger too.
	let owner = plans.subscriptions_in(&elsewhere().as_user(1), org).await;
	assert!(matches!(owner, Err(Error::NotFound)), "{owner:?}");
	let member = plans.subscriptions_in(&elsewhere().as_user(2), org).await;
	assert!(matches!(member, Err(Error::NotFound)), "{member:?}");
}

#[tokio::test]
async fn a_settlement_is_applied_by_the_job_and_replay_is_harmless() {
	let db = TmpDb::new("settle-job");
	let (app, store) = setup(&db).await;
	let (s, inv) = subscribe(&app, &store, "basic").await;
	let r = period_ref(&s, s.period_start);
	pay(&app, &inv).await;
	grant_until_becomes(&store, &r, s.period_end).await;

	// The retry row is there, deduped on the invoice; a forced second run replays it.
	let payload: String =
		sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'PLANS_SETTLED'")
			.fetch_one(store.write_pool())
			.await
			.unwrap();
	let key = format!("plans:settled:{}", inv.uid);
	let at = Timestamp::now();
	let again = mintworks_core::job::enqueue(&app.store, "PLANS_SETTLED", &payload, Some(&key), at);
	assert_eq!(again.await.unwrap(), None);
	let forced = mintworks_core::job::enqueue(&app.store, "PLANS_SETTLED", &payload, None, at);
	assert!(forced.await.unwrap().is_some());
	run_jobs(&app, Timestamp(at.0 + 120)).await;

	let rows: Vec<(i64, Option<i64>)> =
		sqlx::query_as("SELECT amount, valid_until FROM grants WHERE source_ref = ?")
			.bind(&r)
			.fetch_all(store.write_pool())
			.await
			.unwrap();
	assert_eq!(rows, vec![(1000, Some(s.period_end.0))], "one grant, extended once");
	let failed: i64 = sqlx::query_scalar(
		"SELECT COUNT(*) FROM jobs WHERE kind = 'PLANS_SETTLED' AND status <> 'DONE'",
	)
	.fetch_one(store.write_pool())
	.await
	.unwrap();
	assert_eq!(failed, 0);
}

/// Suspends a paid-once `basic` sub on its unpaid first renewal; returns it and that invoice.
async fn suspended(app: &App, store: &SqliteStore) -> (Subscription, Invoice) {
	let (s, first) = subscribe(app, store, "basic").await;
	pay(app, &first).await;
	let plans = Plans::from_app(app).unwrap();
	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	plans
		.renew_due(&Ctx::system("test"), Timestamp(s.period_end.0 + 14 * DAY))
		.await
		.unwrap();
	let s = sub(store, &s.uid).await;
	assert_eq!(s.status, SubStatus::Suspended);
	(s.clone(), latest_invoice(store, &s).await)
}

#[tokio::test]
async fn paying_a_suspended_sub_restores_active() {
	let db = TmpDb::new("suspended-paid");
	let (app, store) = setup(&db).await;
	let (s, overdue) = suspended(&app, &store).await;
	pay(&app, &overdue).await;
	for _ in 0..100 {
		if sub(&store, &s.uid).await.status == SubStatus::Active {
			return;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("the paid sub stayed {:?}", sub(&store, &s.uid).await.status);
}

/// The latest-only gate brought the sub back while an older period was still unpaid, and paying
/// that older one last never did.
#[tokio::test]
async fn paying_only_the_latest_of_two_unpaid_periods_stays_past_due() {
	let db = TmpDb::new("two-unpaid");
	let (app, store) = setup(&db).await;
	let (s, first) = subscribe(&app, &store, "basic").await;
	pay(&app, &first).await;
	let plans = Plans::from_app(&app).unwrap();
	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let second = sub(&store, &s.uid).await;
	let older = latest_invoice(&store, &second).await;
	plans
		.renew_due(&Ctx::system("test"), Timestamp(s.period_end.0 + 3 * DAY))
		.await
		.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::PastDue);
	plans.renew_due(&Ctx::system("test"), second.period_end).await.unwrap();
	let third = sub(&store, &s.uid).await;
	let latest = latest_invoice(&store, &third).await;
	assert_ne!(latest.uid, older.uid);
	assert_ne!(third.status, SubStatus::Active);

	// The job reruns the settlement to completion, so the status is final once it returns.
	pay(&app, &latest).await;
	grant_until_becomes(&store, &period_ref(&s, third.period_start), third.period_end).await;
	run_jobs(&app, Timestamp(Timestamp::now().0 + 120)).await;
	assert_eq!(sub(&store, &s.uid).await.status, third.status, "the older period is unpaid");

	pay(&app, &older).await;
	grant_until_becomes(&store, &period_ref(&s, second.period_start), second.period_end).await;
	run_jobs(&app, Timestamp(Timestamp::now().0 + 120)).await;
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Active);
}

#[tokio::test]
async fn a_late_payment_restarts_the_period_instead_of_billing_the_gap() {
	let db = TmpDb::new("suspended-late");
	let (app, store) = setup(&db).await;
	let (s, overdue) = suspended(&app, &store).await;
	// The settlement reads the wall clock: move the suspended period wholly into the past.
	let now = Timestamp::now();
	sqlx::query("UPDATE subscriptions SET period_start = ?, period_end = ? WHERE id = ?")
		.bind(now.0 - 90 * DAY)
		.bind(now.0 - 60 * DAY)
		.bind(s.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	pay(&app, &overdue).await;
	for _ in 0..100 {
		if sub(&store, &s.uid).await.status == SubStatus::Active {
			break;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	let resumed = sub(&store, &s.uid).await;
	assert_eq!(resumed.status, SubStatus::Active);
	assert!(resumed.period_end.0 > now.0 + 27 * DAY, "the period restarts at payment");
	Plans::from_app(&app)
		.unwrap()
		.renew_due(&Ctx::system("test"), Timestamp::now())
		.await
		.unwrap();
	assert_eq!(latest_invoice(&store, &resumed).await.uid, overdue.uid, "no gap renewal");
}

/// The restart kept the grants and the link on the old window, so an immediate cancel cut
/// nothing and refunded nothing.
#[tokio::test]
async fn an_immediate_cancel_after_a_late_restart_cuts_its_grants_and_refunds() {
	let db = TmpDb::new("suspended-late-cancel");
	let (app, store) = setup(&db).await;
	let (s, overdue) = suspended(&app, &store).await;
	let now = Timestamp::now();
	sqlx::query("UPDATE subscriptions SET period_start = ?, period_end = ? WHERE id = ?")
		.bind(now.0 - 90 * DAY)
		.bind(now.0 - 60 * DAY)
		.bind(s.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	pay(&app, &overdue).await;
	for _ in 0..100 {
		if sub(&store, &s.uid).await.status == SubStatus::Active {
			break;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	let resumed = sub(&store, &s.uid).await;
	let r = period_ref(&s, resumed.period_start);
	grant_until_becomes(&store, &r, resumed.period_end).await;
	let link = store.plan_invoice_get(&overdue.uid).await.unwrap().unwrap();
	assert_eq!(link.period_start, Some(resumed.period_start), "the link moved with the restart");

	let plans = Plans::from_app(&app).unwrap();
	let listed = plans.admin_subscriptions(&ctx(), &SubsFilter::default()).await.unwrap();
	assert_eq!(listed[0].org_uid.as_str(), ORG_UID);
	let now = AdminCancelReq { immediate: true, refund: RefundMode::Prorated };
	plans.admin_cancel(&ctx(), s.uid.as_str(), &now).await.unwrap();
	let g = grant(&store, &r).await.unwrap();
	assert!(g.1.unwrap() <= Timestamp::now().0, "the restarted period is cut");
	let paid = store.payments_by_invoice(ORG, overdue.id).await.unwrap();
	assert_eq!(paid[0].refunded_amount, overdue.gross, "canceled seconds into the restart");
}

/// A stub gateway: payer-present payments end in the given state at once (`Succeeded` unless a
/// test wants one left open), recurring charges are refused.
struct RefusingCard(std::sync::atomic::AtomicUsize, PaymentState);

#[async_trait::async_trait]
impl PaymentProvider for RefusingCard {
	#[allow(clippy::unnecessary_literal_bound)]
	fn id(&self) -> &str {
		"stub"
	}

	fn capabilities(&self) -> ProviderCaps {
		ProviderCaps { recurring: true, ..ProviderCaps::default() }
	}

	async fn start(&self, _req: &StartPayment) -> ClResult<StartedPayment> {
		let n = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		Ok(StartedPayment {
			provider_ref: format!("pay-{n}"),
			redirect_url: Some("https://stub.invalid/pay".into()),
			state: self.1,
		})
	}

	async fn fetch_state(&self, _provider_ref: &str) -> ClResult<PaymentState> {
		Ok(self.1)
	}

	async fn refund(&self, _: &str, amount: Money, _: &str) -> ClResult<RefundResult> {
		Ok(RefundResult { refunded: amount, state: PaymentState::Refunded })
	}

	async fn charge_recurring(&self, _: &str, _: &StartPayment) -> ClResult<StartedPayment> {
		Ok(StartedPayment {
			provider_ref: "rec".into(),
			redirect_url: None,
			state: PaymentState::Failed,
		})
	}

	fn parse_callback(&self, _: &axum::http::HeaderMap, _: &[u8]) -> ClResult<CallbackRef> {
		Err(Error::NotFound)
	}
}

async fn recurrence_becomes(store: &SqliteStore, uid: &SubscriptionId, want: Option<String>) {
	for _ in 0..100 {
		if sub(store, uid).await.recurrence_ref == want {
			return;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("recurrence_ref never became {want:?}");
}

/// A refused card stayed the sub's recurrence for good: the hooks enrol one only while none is
/// stored, so no pay-link payment could replace it.
#[tokio::test]
async fn a_refused_recurring_charge_drops_the_card_and_a_pay_link_enrols_a_new_one() {
	let db = TmpDb::new("refused-card");
	let (app, store) = setup_with(&db, |b| {
		let card = Arc::new(RefusingCard(
			std::sync::atomic::AtomicUsize::default(),
			PaymentState::Succeeded,
		));
		b.extension(Arc::new(PaymentProviders::new().with(card)))
			.extension(Arc::new(mintworks_plans::events::Recurrence) as Arc<dyn RecurrenceHook>)
	})
	.await;
	let plans = Plans::from_app(&app).unwrap();
	let start = async |inv: &InvoiceId| {
		let req = StartRequest {
			provider: "stub".into(),
			request_id: None,
			return_url: "https://app.invalid/back".into(),
			locale: None,
		};
		allocate::start(&app, &owner(), inv, req).await.unwrap();
	};
	let q = plans.quote(&owner(), &quote_req("basic", 1)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
	let out = plans.checkout(&owner(), &req).await.unwrap();
	let (uid, first) = (out.subscription_uid.unwrap(), out.invoice_uid.unwrap());
	start(&first).await;
	recurrence_becomes(&store, &uid, Some(format!("{uid}:{first}"))).await;

	let s = sub(&store, &uid).await;
	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let renewed = sub(&store, &uid).await;
	assert_eq!(
		(renewed.recurrence_ref.as_deref(), renewed.provider.as_deref()),
		(None, None),
		"the refused card is dropped"
	);
	let due = mails(&store, "subscription_payment_due").await;
	assert_eq!((due.len(), &due[0]["vars"]["trial"]), (1, &serde_json::json!(false)));
	assert!(mails(&store, "subscription_invoice").await.is_empty(), "no invoice before the pay");
	let renewal = latest_invoice(&store, &renewed).await.uid;
	start(&renewal).await;
	recurrence_becomes(&store, &uid, Some(format!("{uid}:{renewal}"))).await;
}

/// An invalidated coupon stops discounting, so it must not spend a discounted period either.
#[tokio::test]
async fn an_invalid_coupon_renews_at_full_price_and_keeps_its_periods() {
	let db = TmpDb::new("coupon-invalid-renew");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let mint = mintworks_core::refs::CreateRef {
		ref_type: "coupon".into(),
		params: Some(serde_json::json!({"discount": {"percentBp": 5000}, "periods": 3})),
		..Default::default()
	};
	let c = mintworks_core::refs::Refs::from_app(&app)
		.unwrap()
		.mint(&ctx(), &mint)
		.await
		.unwrap();
	let q = plans
		.quote(&owner(), &QuoteReq { coupon: Some(c.code.clone()), ..quote_req("basic", 1) })
		.await
		.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Transfer };
	let out = plans.checkout(&owner(), &req).await.unwrap();
	let s = sub(&store, out.subscription_uid.as_ref().unwrap()).await;
	let left = s.coupon_periods_left;
	assert!(left.is_some_and(|n| n > 0));
	sqlx::query("UPDATE refs SET params = '{\"discount\": {\"percentBp\": 0}}' WHERE code = ?")
		.bind(&c.code)
		.execute(store.write_pool())
		.await
		.unwrap();

	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let renewed = sub(&store, &s.uid).await;
	assert_eq!(renewed.period_start, s.period_end, "renewed");
	assert_eq!(latest_invoice(&store, &renewed).await.gross, Money(635_000), "full price");
	assert_eq!(renewed.coupon_periods_left, left);
}

#[tokio::test]
async fn an_abandoned_card_checkout_frees_the_family() {
	let db = TmpDb::new("abandoned-card");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let req = mintworks_core::refs::CreateRef {
		ref_type: "coupon".into(),
		params: Some(serde_json::json!({"discount": {"percentBp": 1000}})),
		uses_left: Some(1),
		..Default::default()
	};
	let c = mintworks_core::refs::Refs::from_app(&app)
		.unwrap()
		.mint(&ctx(), &req)
		.await
		.unwrap();
	let with_coupon = |qty| QuoteReq { coupon: Some(c.code.clone()), ..quote_req("basic", qty) };
	let q = plans.quote(&owner(), &with_coupon(1)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
	let out = plans.checkout(&owner(), &req).await.unwrap();
	let s = sub(&store, out.subscription_uid.as_ref().unwrap()).await;
	let draft = out.invoice_uid.unwrap();
	assert_eq!(s.status, SubStatus::Active);

	// `payment.window_minutes` defaults to 10.
	plans
		.renew_due(&Ctx::system("test"), Timestamp(s.period_start.0 + 10 * 60 + 3600))
		.await
		.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Canceled);
	assert!(store.invoice_by_uid(None, &draft).await.unwrap().is_none(), "the draft is deleted");
	// The deleted draft held the coupon's only use: the retry gets it back.
	let q = plans.quote(&owner(), &with_coupon(2)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
	plans.checkout(&owner(), &req).await.unwrap();
}

#[tokio::test]
async fn cancelling_a_suspended_sub_frees_the_family() {
	let db = TmpDb::new("suspended-cancel");
	let (app, store) = setup(&db).await;
	let (s, _) = suspended(&app, &store).await;
	let plans = Plans::from_app(&app).unwrap();
	assert!(plans.resume(&owner(), s.uid.as_str()).await.is_err(), "nothing to resume");
	let out = plans.cancel(&owner(), s.uid.as_str()).await.unwrap();
	assert_eq!(out.status, SubStatus::Canceled, "at once: no renewal ever reaches it");
	// qty 2: the same quote within the same second is the same token, hence the old invoice.
	let q = plans.quote(&owner(), &quote_req("basic", 2)).await.unwrap();
	let again = checkout_transfer(&plans, q.quote_token).await.unwrap();
	assert_ne!(again.subscription_uid.as_ref(), Some(&s.uid));
}

#[tokio::test]
async fn a_free_card_renewal_is_issued_and_stays_active() {
	let db = TmpDb::new("free-card");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let req = mintworks_core::refs::CreateRef {
		ref_type: "coupon".into(),
		params: Some(serde_json::json!({"discount": {"percentBp": 10000}})),
		..Default::default()
	};
	let c = mintworks_core::refs::Refs::from_app(&app)
		.unwrap()
		.mint(&ctx(), &req)
		.await
		.unwrap();
	let q = plans
		.quote(&owner(), &QuoteReq { coupon: Some(c.code), ..quote_req("basic", 1) })
		.await
		.unwrap();
	let pay = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
	let out = plans.checkout(&owner(), &pay).await.unwrap();
	assert_eq!(out.next, "issued", "nothing to pay, so no card payment");
	let s = sub(&store, out.subscription_uid.as_ref().unwrap()).await;
	assert_eq!(
		grant(&store, &period_ref(&s, s.period_start)).await.unwrap().1,
		Some(s.period_end.0)
	);

	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let renewed = sub(&store, &s.uid).await;
	let inv = latest_invoice(&store, &renewed).await;
	assert_eq!((inv.status, inv.gross), (InvoiceStatus::Issued, Money::ZERO));
	plans
		.renew_due(&Ctx::system("test"), Timestamp(s.period_end.0 + 14 * DAY))
		.await
		.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Active);
}

/// The credits grant of an upgrade link.
async fn upgrade_credits(store: &SqliteStore, s: &Subscription, inv: &InvoiceId) -> Option<i64> {
	sqlx::query_scalar("SELECT amount FROM grants WHERE source_ref = ?")
		.bind(format!("sub:{}:up:{inv}", s.uid))
		.fetch_optional(store.write_pool())
		.await
		.unwrap()
}

#[tokio::test]
async fn an_upgrade_adds_only_the_meter_difference() {
	let db = TmpDb::new("upgrade-delta");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let s = mid_period(&store, &s).await;
	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	let out = checkout_transfer(&plans, q.quote_token).await.unwrap();
	let inv_uid = out.invoice_uid.unwrap();
	assert_eq!(upgrade_credits(&store, &s, &inv_uid).await, None, "no meter until paid");
	let inv = store.invoice_by_uid(None, &inv_uid).await.unwrap().unwrap();
	pay(&app, &inv).await;
	// pro's 5000 less basic's 1000: only the difference, whether or not basic's is paid yet.
	for _ in 0..100 {
		if upgrade_credits(&store, &s, &inv_uid).await == Some(4000) {
			return;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("the paid upgrade never granted its meter difference");
}

#[tokio::test]
async fn an_unpaid_card_upgrade_leaves_the_tier_unchanged() {
	let db = TmpDb::new("upgrade-card");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let s = mid_period(&store, &s).await;
	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
	let out = plans.checkout(&ctx(), &req).await.unwrap();
	assert_eq!(out.next, "pay");
	let replay = plans.checkout(&ctx(), &req).await.unwrap();
	assert_eq!(replay.invoice_uid, out.invoice_uid, "a replay makes no second invoice");
	let unpaid = sub(&store, &s.uid).await;
	assert_eq!((unpaid.offer_id, unpaid.price), (s.offer_id, Money(500_000)), "not applied");
	let inv_uid = out.invoice_uid.unwrap();
	assert_eq!(upgrade_credits(&store, &s, &inv_uid).await, None, "nothing granted unpaid");

	// Settled (here by transfer, so no gateway is needed): the tier applies then.
	let inv = mintworks_invoice::Invoices::new(app.clone())
		.issue(&ctx(), inv_uid.as_str())
		.await
		.unwrap();
	pay(&app, &inv).await;
	for _ in 0..100 {
		if sub(&store, &s.uid).await.price == Money(900_000) {
			assert_eq!(upgrade_credits(&store, &s, &inv_uid).await, Some(4000));
			return;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("the paid upgrade never applied");
}

#[tokio::test]
async fn a_second_upgrade_is_refused_while_one_is_unpaid() {
	let db = TmpDb::new("upgrade-twice");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let s = mid_period(&store, &s).await;
	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
	assert_eq!(plans.checkout(&ctx(), &req).await.unwrap().next, "pay");

	let s = sub(&store, &s.uid).await;
	let again = plans.quote(&ctx(), &change_req("pro", 2, &s)).await;
	assert_eq!(code_of(again), mintworks_plans::quote::E_CHANGE_INVALID);
}

#[tokio::test]
async fn a_downgrade_is_allowed_while_a_card_upgrade_is_unpaid() {
	let db = TmpDb::new("downgrade-card-pending");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let s = mid_period(&store, &s).await;
	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	checkout_transfer(&plans, q.quote_token).await.unwrap();
	let s = sub(&store, &s.uid).await;
	let q = plans.quote(&ctx(), &change_req("pro", 2, &s)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
	assert_eq!(plans.checkout(&ctx(), &req).await.unwrap().next, "pay");

	let s = sub(&store, &s.uid).await;
	let down = plans.quote(&ctx(), &change_req("basic", 1, &s)).await.unwrap();
	assert_eq!(down.effective, "period_end");
	assert_eq!(down.period_start, Some(s.period_end));
}

#[tokio::test]
async fn a_chained_transfer_upgrade_still_grants_the_first_payment() {
	let db = TmpDb::new("upgrade-chain");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let s = mid_period(&store, &s).await;
	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	let first = checkout_transfer(&plans, q.quote_token).await.unwrap().invoice_uid.unwrap();
	let s = sub(&store, &s.uid).await;
	let q = plans.quote(&ctx(), &change_req("pro", 2, &s)).await.unwrap();
	checkout_transfer(&plans, q.quote_token).await.unwrap();

	let inv = store.invoice_by_uid(None, &first).await.unwrap().unwrap();
	pay(&app, &inv).await;
	// pro's 5000 less basic's 1000, though the sub has since moved on to two pro seats.
	for _ in 0..100 {
		if upgrade_credits(&store, &s, &first).await == Some(4000) {
			return;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("the first of two chained upgrades never granted its meter difference");
}

#[tokio::test]
async fn cancelling_a_suspended_sub_needs_step_up() {
	let db = TmpDb::new("cancel-suspended-stepup");
	let (app, store) = setup(&db).await;
	let (s, _) = suspended(&app, &store).await;
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (0, 1, 'ADMIN', 0, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	let plans = Plans::from_app(&app).unwrap();
	let later = AdminCancelReq { immediate: false, refund: RefundMode::None };
	let out = plans.admin_cancel(&ctx().as_user(1), s.uid.as_str(), &later).await;
	assert_eq!(code_of(out), "E-AUTH-STEPUP-IMPOSSIBLE");
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Suspended);
}

/// A CARD upgrade paid after the renewal took the money and changed nothing; the renewal voids
/// the draft so it cannot be paid late.
#[tokio::test]
async fn a_card_upgrade_draft_is_voided_at_renewal() {
	let db = TmpDb::new("upgrade-card-renewal");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, _) = subscribe(&app, &store, "basic").await;
	let s = mid_period(&store, &s).await;
	let q = plans.quote(&ctx(), &change_req("pro", 1, &s)).await.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
	let out = plans.checkout(&ctx(), &req).await.unwrap();
	let inv_uid = out.invoice_uid.unwrap();

	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	assert!(store.invoice_by_uid(None, &inv_uid).await.unwrap().is_none(), "draft kept");
	let renewed = sub(&store, &s.uid).await;
	assert_eq!(renewed.period_start, s.period_end, "renewed");
	assert_eq!((renewed.offer_id, renewed.price), (s.offer_id, s.price), "tier unchanged");
}

async fn mint(app: &App, ref_type: &str, params: serde_json::Value) -> mintworks_core::refs::Ref {
	let req = mintworks_core::refs::CreateRef {
		ref_type: ref_type.into(),
		params: Some(params),
		..Default::default()
	};
	let ctx = Ctx::system("test").with_org(ORG);
	mintworks_core::refs::Refs::from_app(app)
		.unwrap()
		.mint(&ctx, &req)
		.await
		.unwrap()
}

/// Both submits of one token share the draft, and the loser's `delete_draft` cascaded away the
/// link the winner had just made, leaving an ACTIVE sub with no invoice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_concurrent_double_checkout_leaves_one_linked_sub() {
	let db = TmpDb::new("double-checkout");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	for _ in 0..20 {
		let c = mint(&app, "coupon", serde_json::json!({"discount": {"percentBp": 5000}})).await;
		let mut q = quote_req("basic", 1);
		q.coupon = Some(c.code.clone());
		let q = plans.quote(&owner(), &q).await.unwrap();
		let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
		let (a, b) = (owner(), owner());
		let _ = tokio::join!(plans.checkout(&a, &req), plans.checkout(&b, &req));

		let subs = store.subs_of_org(ORG).await.unwrap();
		let live: Vec<_> = subs.iter().filter(|s| s.status != SubStatus::Canceled).collect();
		assert!(live.len() <= 1, "two live subs in one family: {live:?}");
		for s in live {
			let link = store.plan_invoice_latest(s.id).await.unwrap();
			assert_eq!(link.map(|l| l.kind), Some(LinkKind::Subscribe), "{s:?} has no invoice");
		}
		sqlx::query("UPDATE subscriptions SET status = 'CANCELED'")
			.execute(store.write_pool())
			.await
			.unwrap();
	}
}

/// A CARD `pro` trial on the stub gateway (payer-present payments succeed at once).
async fn card_trial(db: &TmpDb) -> (App, SqliteStore, Subscription) {
	card_trial_with(db, None).await
}

/// [`card_trial`], bought with a coupon of `coupon_bp` basis points off.
async fn card_trial_with(db: &TmpDb, coupon_bp: Option<i64>) -> (App, SqliteStore, Subscription) {
	card_trial_on(db, coupon_bp, PaymentState::Succeeded).await
}

/// [`card_trial_with`] on a gateway whose payer-present payments end in `paid`.
async fn card_trial_on(
	db: &TmpDb,
	coupon_bp: Option<i64>,
	paid: PaymentState,
) -> (App, SqliteStore, Subscription) {
	let (app, store) = setup_with(db, |b| {
		let card = Arc::new(RefusingCard(std::sync::atomic::AtomicUsize::default(), paid));
		b.extension(Arc::new(PaymentProviders::new().with(card)))
			.extension(Arc::new(mintworks_plans::events::Recurrence) as Arc<dyn RecurrenceHook>)
	})
	.await;
	let plans = Plans::from_app(&app).unwrap();
	let coupon = match coupon_bp {
		Some(bp) => Some(
			mint(&app, "coupon", serde_json::json!({"discount": {"percentBp": bp}}))
				.await
				.code,
		),
		None => None,
	};
	let q = plans
		.quote(&owner(), &QuoteReq { coupon, ..quote_req("pro", 1) })
		.await
		.unwrap();
	let req = CheckoutReq { quote_token: q.quote_token, pay_method: PayMethod::Card };
	let out = plans.checkout(&owner(), &req).await.unwrap();
	assert_eq!(out.next, "trialing");
	let s = sub(&store, out.subscription_uid.as_ref().unwrap()).await;
	(app, store, s)
}

async fn pay_by_card(app: &App, inv: &InvoiceId) {
	let req = StartRequest {
		provider: "stub".into(),
		request_id: None,
		return_url: "https://app.invalid/back".into(),
		locale: None,
	};
	allocate::start(app, &owner(), inv, req).await.unwrap();
}

async fn status_becomes(
	store: &SqliteStore,
	uid: &SubscriptionId,
	want: SubStatus,
) -> Subscription {
	for _ in 0..100 {
		let s = sub(store, uid).await;
		if s.status == want {
			return s;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("the sub stayed {:?}", sub(store, uid).await.status);
}

/// The `SEND_EMAIL` payloads naming `template`.
async fn mails(store: &SqliteStore, template: &str) -> Vec<serde_json::Value> {
	let rows: Vec<String> =
		sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'SEND_EMAIL'")
			.fetch_all(store.write_pool())
			.await
			.unwrap();
	rows.iter()
		.map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
		.filter(|p| p["template"] == template)
		.collect()
}

#[tokio::test]
async fn a_trial_paid_ahead_starts_its_paid_period_at_the_trial_end() {
	let db = TmpDb::new("activate");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let a = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	let again = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	assert_ne!(again.invoice_uid, a.invoice_uid, "redrafted at current terms");
	assert!(store.invoice_by_uid(None, &a.invoice_uid).await.unwrap().is_none(), "one draft");
	let a = again;
	let draft = store.invoice_by_uid(None, &a.invoice_uid).await.unwrap().unwrap();
	assert_eq!((draft.status, draft.number.is_none()), (InvoiceStatus::Draft, true));
	let link = store.plan_invoice_get(&a.invoice_uid).await.unwrap().unwrap();
	assert_eq!((link.kind, link.period_start), (LinkKind::Renewal, Some(s.period_end)));

	pay_by_card(&app, &a.invoice_uid).await;
	let paid = status_becomes(&store, &s.uid, SubStatus::Active).await;
	assert_eq!(paid.period_start, s.period_end, "the trial days are kept");
	assert_eq!(Some(paid.period_end), link.period_end);
	grant_until_becomes(&store, &period_ref(&s, s.period_end), paid.period_end).await;
	let (source, until) = grant(&store, &period_ref(&s, s.period_start)).await.unwrap();
	assert_eq!((source.as_str(), until), ("TRIAL", Some(s.period_end.0)), "the trial runs on");

	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	assert_eq!(sub(&store, &s.uid).await.period_start, paid.period_start, "nothing renewed");
	assert_eq!(latest_invoice(&store, &paid).await.uid, a.invoice_uid, "no second draft");
	assert!(mails(&store, "subscription_payment_due").await.is_empty());
	assert!(mails(&store, "subscription_invoice").await.is_empty());
	let view = plans.subscriptions(&owner()).await.unwrap();
	assert_eq!((view[0].pending_invoice_uid.as_ref(), view[0].grace_until), (None, None));
}

#[tokio::test]
async fn a_trial_paid_ahead_may_switch_its_interval_only() {
	let db = TmpDb::new("activate-interval");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let uid = s.uid.as_str();
	let year = plans.activate(&owner(), uid, Some("pro_year")).await.unwrap();
	let link = store.plan_invoice_get(&year.invoice_uid).await.unwrap().unwrap();
	assert!(link.period_end.unwrap().0 >= s.period_end.0 + 365 * DAY, "a year from the trial end");
	let switched = sub(&store, &s.uid).await;
	assert_eq!(
		(switched.offer_id, switched.price, switched.next_offer_id),
		(s.offer_id, s.price, Some(link.offer_id)),
		"queued, not applied"
	);
	let same = plans.activate(&owner(), uid, None).await.unwrap();
	let same_link = store.plan_invoice_get(&same.invoice_uid).await.unwrap().unwrap();
	assert_eq!(same_link.offer_id, link.offer_id, "None keeps the switched offer");
	let view = &plans.subscriptions(&owner()).await.unwrap()[0];
	assert_eq!(
		(view.offer_code.as_str(), view.next_offer_code.as_deref()),
		("pro", Some("pro_year"))
	);

	let month = plans.activate(&owner(), uid, Some("pro")).await.unwrap();
	assert_ne!(month.invoice_uid, same.invoice_uid);
	assert!(store.invoice_by_uid(None, &same.invoice_uid).await.unwrap().is_none(), "replaced");
	assert_eq!(
		code_of(plans.activate(&owner(), uid, Some("basic")).await),
		"E-PLAN-CHANGE-INVALID"
	);
	assert_eq!(
		code_of(plans.activate(&owner(), uid, Some("addon")).await),
		"E-PLAN-CHANGE-INVALID"
	);
}

#[tokio::test]
async fn a_repeated_pay_ahead_redrafts_at_current_terms() {
	let db = TmpDb::new("activate-reprice");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let first = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	let old = store.invoice_by_uid(None, &first.invoice_uid).await.unwrap().unwrap();
	sqlx::query("UPDATE subscriptions SET price = price * 2 WHERE id = ?")
		.bind(s.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let second = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	assert_ne!(second.invoice_uid, first.invoice_uid);
	assert!(store.invoice_by_uid(None, &first.invoice_uid).await.unwrap().is_none(), "dropped");
	let new = store.invoice_by_uid(None, &second.invoice_uid).await.unwrap().unwrap();
	assert_eq!(new.gross.0, old.gross.0 * 2, "the new price");
}

#[tokio::test]
async fn a_pay_ahead_for_a_moved_trial_end_grants_nothing() {
	let db = TmpDb::new("activate-stale");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let a = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	sqlx::query("UPDATE subscriptions SET period_end = period_end + 86400 WHERE id = ?")
		.bind(s.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	pay_by_card(&app, &a.invoice_uid).await;
	let mut flagged = false;
	for _ in 0..100 {
		flagged = sqlx::query_scalar::<_, i64>(
			"SELECT COUNT(*) FROM audit_logs WHERE action = 'PAY_AHEAD_STALE' AND entity_id = ?",
		)
		.bind(a.invoice_uid.as_str())
		.fetch_one(store.write_pool())
		.await
		.unwrap() > 0;
		if flagged {
			break;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	assert!(flagged, "surfaced for a refund");
	assert!(grant(&store, &period_ref(&s, s.period_end)).await.is_none());
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Trialing);
}

#[tokio::test]
async fn only_a_running_card_trial_can_be_paid_ahead() {
	let db = TmpDb::new("activate-refused");
	let (app, store) = setup(&db).await;
	let (s, _) = subscribe(&app, &store, "basic").await;
	let plans = Plans::from_app(&app).unwrap();
	let r = plans.activate(&owner(), s.uid.as_str(), None).await;
	assert_eq!(code_of(r), "E-PLAN-CHANGE-INVALID");
	let member = ctx().as_user(2);
	assert_eq!(code_of(plans.activate(&member, s.uid.as_str(), None).await), "E-AUTH-FORBIDDEN");
}

#[tokio::test]
async fn an_unpaid_trial_end_asks_for_payment_without_an_invoice() {
	let db = TmpDb::new("trial-end-unpaid");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let renewed = sub(&store, &s.uid).await;
	assert_eq!((renewed.status, renewed.period_start), (SubStatus::Active, s.period_end));
	let draft = latest_invoice(&store, &renewed).await;
	assert_eq!((draft.status, draft.number.is_none()), (InvoiceStatus::Draft, true));
	let sent = mails(&store, "subscription_payment_due").await;
	assert_eq!(sent.len(), 1);
	let vars = &sent[0]["vars"];
	assert_eq!(vars["trial"], true);
	assert_eq!(vars["pay_url"], "https://app.invalid/settings/subscription");
	assert!(vars.get("invoice_number").is_none() && vars.get("invoice_uid").is_none());
	assert!(mails(&store, "subscription_invoice").await.is_empty());

	let view = plans.subscriptions(&owner()).await.unwrap();
	assert_eq!(view[0].pending_invoice_uid.as_ref(), Some(&draft.uid));
	assert_eq!(view[0].grace_until, Some(Timestamp(s.period_end.0 + 3 * DAY)));
	let wire = serde_json::to_value(&view[0]).unwrap();
	assert_eq!(wire["pendingInvoiceUid"], draft.uid.as_str());
	assert!(wire["graceUntil"].is_string() && wire["periodEnd"].is_string());
	assert_eq!(wire["offerCode"], "pro");

	// Paid inside the grace: the draft is the period's, and the sub stays ACTIVE.
	pay_by_card(&app, &draft.uid).await;
	grant_until_becomes(&store, &period_ref(&s, s.period_end), renewed.period_end).await;
	plans
		.renew_due(&Ctx::system("test"), Timestamp(s.period_end.0 + 4 * DAY))
		.await
		.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Active);
}

#[tokio::test]
async fn an_abandoned_pay_ahead_leaves_the_trial_running() {
	let db = TmpDb::new("activate-abandoned");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let a = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	plans
		.renew_due(&Ctx::system("test"), Timestamp(Timestamp::now().0 + 2 * 3600))
		.await
		.unwrap();
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Trialing);
	assert!(store.invoice_by_uid(None, &a.invoice_uid).await.unwrap().is_some(), "draft kept");
}

#[tokio::test]
async fn the_trial_reminder_is_mailed_once_near_the_end() {
	let db = TmpDb::new("trial-reminder");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let at = |days: i64| Timestamp(s.period_end.0 - days * DAY);
	plans.renew_due(&Ctx::system("test"), at(4)).await.unwrap();
	assert!(mails(&store, "subscription_trial_ending").await.is_empty(), "too early");
	plans.renew_due(&Ctx::system("test"), at(3)).await.unwrap();
	plans.renew_due(&Ctx::system("test"), at(2)).await.unwrap();
	let sent = mails(&store, "subscription_trial_ending").await;
	assert_eq!(sent.len(), 1, "deduped on the trial end");
	assert_eq!(sent[0]["to"], "vevo@e.st");
	assert_eq!(sent[0]["vars"]["pay_url"], "https://app.invalid/settings/subscription");
	assert_eq!(sent[0]["vars"]["amount"], "11430.00", "9000 HUF + 27% VAT");
}

/// The trial reminder's amount for `s`, mailed two days before its end.
async fn reminder_amount(app: &App, store: &SqliteStore, s: &Subscription) -> serde_json::Value {
	let plans = Plans::from_app(app).unwrap();
	let at = Timestamp(s.period_end.0 - 2 * DAY);
	plans.renew_due(&Ctx::system("test"), at).await.unwrap();
	mails(store, "subscription_trial_ending").await[0]["vars"]["amount"].clone()
}

#[tokio::test]
async fn the_trial_reminder_quotes_the_coupon() {
	let db = TmpDb::new("trial-reminder-coupon");
	let (app, store, s) = card_trial_with(&db, Some(5000)).await;
	assert_eq!(reminder_amount(&app, &store, &s).await, "5715.00", "half of 11430");
}

#[tokio::test]
async fn the_trial_reminder_quotes_the_grandfathered_price() {
	let db = TmpDb::new("trial-reminder-reprice");
	let (app, store, s) = card_trial(&db).await;
	sqlx::query("UPDATE subscriptions SET price = 1000000 WHERE id = ?")
		.bind(s.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let s = sub(&store, &s.uid).await;
	assert_eq!(reminder_amount(&app, &store, &s).await, "12700.00", "10000 HUF + 27% VAT");
}

#[tokio::test]
async fn a_trial_paid_ahead_gets_no_reminder() {
	let db = TmpDb::new("trial-reminder-paid");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let a = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	pay_by_card(&app, &a.invoice_uid).await;
	status_becomes(&store, &s.uid, SubStatus::Active).await;
	plans
		.renew_due(&Ctx::system("test"), Timestamp(s.period_end.0 - 2 * DAY))
		.await
		.unwrap();
	assert!(mails(&store, "subscription_trial_ending").await.is_empty());
}

#[tokio::test]
async fn switching_the_interval_back_keeps_the_grandfathered_price() {
	let db = TmpDb::new("activate-switch-back");
	let (app, store, s) = card_trial(&db).await;
	sqlx::query("UPDATE subscriptions SET price = 1000000 WHERE id = ?")
		.bind(s.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let plans = Plans::from_app(&app).unwrap();
	plans.activate(&owner(), s.uid.as_str(), Some("pro_year")).await.unwrap();
	let back = plans.activate(&owner(), s.uid.as_str(), Some("pro")).await.unwrap();
	let after = sub(&store, &s.uid).await;
	assert_eq!((after.price, after.next_offer_id), (Money(1_000_000), None));
	let draft = store.invoice_by_uid(None, &back.invoice_uid).await.unwrap().unwrap();
	assert_eq!(draft.net, Money(1_000_000), "10 000 HUF");
}

#[tokio::test]
async fn a_paid_ahead_switch_bills_the_new_interval() {
	let db = TmpDb::new("activate-switch-paid");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let a = plans.activate(&owner(), s.uid.as_str(), Some("pro_year")).await.unwrap();
	let link = store.plan_invoice_get(&a.invoice_uid).await.unwrap().unwrap();
	pay_by_card(&app, &a.invoice_uid).await;
	let paid = status_becomes(&store, &s.uid, SubStatus::Active).await;
	assert_eq!(
		(paid.offer_id, paid.price, paid.next_offer_id, Some(paid.period_end)),
		(link.offer_id, Money(9_000_000), None, link.period_end)
	);
	assert_eq!(plans.subscriptions(&owner()).await.unwrap()[0].offer_code, "pro_year");
}

/// The pay-ahead draft and the renewal share a `request_id`, so they must share the period too.
#[tokio::test]
async fn a_pay_ahead_bills_the_period_the_renewal_would() {
	let db = TmpDb::new("activate-period");
	let (app, store, s) = card_trial(&db).await;
	let a = Plans::from_app(&app)
		.unwrap()
		.activate(&owner(), s.uid.as_str(), None)
		.await
		.unwrap();
	let ahead = store.plan_invoice_get(&a.invoice_uid).await.unwrap().unwrap();

	let db2 = TmpDb::new("activate-period-renewed");
	let (app2, store2, s2) = card_trial(&db2).await;
	Plans::from_app(&app2)
		.unwrap()
		.renew_due(&Ctx::system("test"), s2.period_end)
		.await
		.unwrap();
	let renewed = store2.plan_invoice_latest(s2.id).await.unwrap().unwrap();
	assert_eq!(
		ahead.period_end.unwrap().0 - s.period_end.0,
		renewed.period_end.unwrap().0 - s2.period_end.0
	);
}

#[tokio::test]
async fn an_immediate_cancel_after_a_pay_ahead_ends_the_trial_grants() {
	let db = TmpDb::new("activate-cancel-now");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let a = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	pay_by_card(&app, &a.invoice_uid).await;
	status_becomes(&store, &s.uid, SubStatus::Active).await;
	let now = AdminCancelReq { immediate: true, refund: RefundMode::None };
	plans.admin_cancel(&ctx(), s.uid.as_str(), &now).await.unwrap();
	let (source, until) = grant(&store, &period_ref(&s, s.period_start)).await.unwrap();
	assert_eq!(source, "TRIAL");
	assert!(until.unwrap() <= Timestamp::now().0, "the trial grant is cut");
}

#[tokio::test]
async fn cancelling_a_trial_drops_its_pay_ahead_draft() {
	let db = TmpDb::new("activate-cancel");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let a = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	plans.cancel(&owner(), s.uid.as_str()).await.unwrap();
	assert!(store.invoice_by_uid(None, &a.invoice_uid).await.unwrap().is_none());
	plans.resume(&owner(), s.uid.as_str()).await.unwrap();
	plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
}

/// A trial with its pay-ahead payment still open, on a gateway that leaves it PENDING.
async fn pending_pay_ahead(db: &TmpDb) -> (App, SqliteStore, Subscription, Invoice) {
	let (app, store, s) = card_trial_on(db, None, PaymentState::Pending).await;
	let a = Plans::from_app(&app)
		.unwrap()
		.activate(&owner(), s.uid.as_str(), None)
		.await
		.unwrap();
	pay_by_card(&app, &a.invoice_uid).await;
	let inv = store.invoice_by_uid(None, &a.invoice_uid).await.unwrap().unwrap();
	assert_eq!(inv.status, InvoiceStatus::Pending);
	(app, store, s, inv)
}

/// Settles `inv`'s open payment, then expects it refunded in full and the invoice stornoed; a
/// replayed settlement refunds and stornoes nothing more.
async fn settles_into_a_refund(app: &App, store: &SqliteStore, inv: &Invoice) {
	let p = store.payments_by_invoice(ORG, inv.id).await.unwrap().remove(0);
	allocate::apply_state(app, &p, PaymentState::Succeeded).await.unwrap();
	let mut back = None;
	for _ in 0..100 {
		let now = store.payments_by_invoice(ORG, inv.id).await.unwrap().remove(0);
		let status = store.invoice_by_uid(None, &inv.uid).await.unwrap().unwrap().status;
		if now.refunded_amount == now.amount && status == InvoiceStatus::Stornoed {
			back = Some(now);
			break;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	let back = back.expect("the pay-ahead was never refunded and stornoed");
	assert_eq!(back.status, PaymentState::Refunded);
	let invoice = inv.uid.clone();
	let org = mintworks_core::ids::OrgId::from_trusted(ORG_UID.to_string());
	let ev = mintworks_core::event::Event::PaymentSettled { payment: p.uid, invoice, org };
	mintworks_plans::events::handle(app.clone(), ev).await.unwrap();
	let again = store.payments_by_invoice(ORG, inv.id).await.unwrap().remove(0);
	assert_eq!(again.refunded_amount, back.refunded_amount, "refunded once");
}

#[tokio::test]
async fn a_pay_ahead_settling_after_the_trial_end_on_a_canceled_sub_is_refunded() {
	let db = TmpDb::new("activate-pending-late");
	let (app, store, s, inv) = pending_pay_ahead(&db).await;
	let now = AdminCancelReq { immediate: true, refund: RefundMode::None };
	Plans::from_app(&app)
		.unwrap()
		.admin_cancel(&ctx(), s.uid.as_str(), &now)
		.await
		.unwrap();
	// The trial ended a day ago: the sub's end and the pay-ahead's start move back together.
	let shift = s.period_end.0 - (Timestamp::now().0 - DAY);
	sqlx::query("UPDATE subscriptions SET period_end = period_end - ? WHERE id = ?")
		.bind(shift)
		.bind(s.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	sqlx::query("UPDATE plan_invoices SET period_start = period_start - ? WHERE invoice_id = ?")
		.bind(shift)
		.bind(inv.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	settles_into_a_refund(&app, &store, &inv).await;
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Canceled);
}

#[tokio::test]
async fn cancel_change_refuses_while_a_switched_pay_ahead_payment_is_open() {
	let db = TmpDb::new("activate-switch-pending");
	let (app, store, s) = card_trial_on(&db, None, PaymentState::Pending).await;
	let plans = Plans::from_app(&app).unwrap();
	let uid = s.uid.as_str();
	let a = plans.activate(&owner(), uid, Some("pro_year")).await.unwrap();
	pay_by_card(&app, &a.invoice_uid).await;
	let r = plans.cancel_change(&owner(), uid).await;
	assert!(
		matches!(r, Err(Error::Coded { status, code: "E-PLAN-PAYMENT-OPEN", .. }) if status == 409)
	);
	assert!(sub(&store, &s.uid).await.next_offer_id.is_some(), "the switch stays queued");
}

#[tokio::test]
async fn an_open_pay_ahead_payment_blocks_a_switch_but_not_a_cancel() {
	let db = TmpDb::new("activate-pending");
	let (app, store, s, inv) = pending_pay_ahead(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let uid = s.uid.as_str();
	assert_eq!(code_of(plans.activate(&owner(), uid, None).await), "E-PLAN-PAYMENT-OPEN");
	plans.cancel(&owner(), uid).await.unwrap();
	settles_into_a_refund(&app, &store, &inv).await;
	let after = sub(&store, &s.uid).await;
	assert_eq!((after.status, after.cancel_at_period_end), (SubStatus::Trialing, true));
}

#[tokio::test]
async fn a_refunded_pay_ahead_leaves_the_period_owed() {
	let db = TmpDb::new("activate-refunded");
	let (app, store, s, inv) = pending_pay_ahead(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let uid = s.uid.as_str();
	plans.cancel(&owner(), uid).await.unwrap();
	settles_into_a_refund(&app, &store, &inv).await;
	plans.resume(&owner(), uid).await.unwrap();
	let again = plans.activate(&owner(), uid, None).await.unwrap();
	assert_ne!(again.invoice_uid, inv.uid);
	plans
		.renew_due(&Ctx::system("test"), Timestamp(s.period_end.0 + 1))
		.await
		.unwrap();
	let latest = latest_invoice(&store, &sub(&store, &s.uid).await).await;
	assert_eq!((latest.uid, latest.status), (again.invoice_uid, InvoiceStatus::Draft));
	assert!(store.plan_invoice_oldest_unpaid(s.id).await.unwrap().is_some());
}

/// The pay-ahead's payment is still PENDING, so its refund fails: the reward must not ride on the
/// refund having landed.
#[tokio::test]
async fn a_called_off_pay_ahead_pays_no_reward_even_if_its_refund_failed() {
	let db = TmpDb::new("activate-reward-refund-failed");
	let (app, store, s, inv) = pending_pay_ahead(&db).await;
	app.settings.set("plans.reward_on", "first_payment", None).await.unwrap();
	app.settings.set("plans.reward.signup.invitee", "addon", None).await.unwrap();
	let r = mintworks_core::refs::Refs::from_app(&app)
		.unwrap()
		.mint(
			&Ctx::system("test").with_org(0),
			&mintworks_core::refs::CreateRef { ref_type: "signup".into(), ..Default::default() },
		)
		.await
		.unwrap();
	store.ref_redeem(r.id, 1, ORG, None, Timestamp::now()).await.unwrap().unwrap();
	let plans = Plans::from_app(&app).unwrap();
	plans.cancel(&owner(), s.uid.as_str()).await.unwrap();

	let p = store.payments_by_invoice(ORG, inv.id).await.unwrap().remove(0);
	let org = mintworks_core::ids::OrgId::from_trusted(ORG_UID.to_string());
	let ev = mintworks_core::event::Event::PaymentSettled {
		payment: p.uid,
		invoice: inv.uid.clone(),
		org,
	};
	let rewards = async || {
		sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM grants WHERE source = 'REWARD'")
			.fetch_one(store.write_pool())
			.await
			.unwrap()
	};
	assert!(mintworks_plans::events::handle(app.clone(), ev.clone()).await.is_err(), "refunded");
	assert_eq!(rewards().await, 0);
	// The control: the same settlement on a resumed trial is a payment and earns the reward.
	plans.resume(&owner(), s.uid.as_str()).await.unwrap();
	let _ = mintworks_plans::events::handle(app.clone(), ev).await;
	assert!(rewards().await > 0);
}

/// The open payment may still fail and drop the invoice back to a draft: the mail is the one
/// notice before PAST_DUE, and it is sent once per period.
#[tokio::test]
async fn a_pending_pay_ahead_still_gets_the_payment_due_mail() {
	let db = TmpDb::new("activate-pending-end");
	let (app, store, s, _) = pending_pay_ahead(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	plans
		.renew_due(&Ctx::system("test"), Timestamp(s.period_end.0 + 1))
		.await
		.unwrap();
	assert_eq!(mails(&store, "subscription_payment_due").await.len(), 1);
}

#[tokio::test]
async fn a_deactivated_service_still_renews() {
	let db = TmpDb::new("renew-inactive-service");
	let (app, store) = setup(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let (s, inv) = subscribe(&app, &store, "basic").await;
	pay(&app, &inv).await;
	sqlx::query(
		"UPDATE services SET active = 0 WHERE id = (SELECT service_id FROM offers WHERE id = ?)",
	)
	.bind(s.offer_id)
	.execute(store.write_pool())
	.await
	.unwrap();
	plans.renew_due(&Ctx::system("test"), s.period_end).await.unwrap();
	let link = store.plan_invoice_latest(s.id).await.unwrap().unwrap();
	assert_ne!(link.invoice_uid, inv.uid);
	assert_eq!(link.period_start, Some(s.period_end));
}

#[tokio::test]
async fn an_open_pay_ahead_settling_after_an_immediate_cancel_is_refunded() {
	let db = TmpDb::new("activate-pending-now");
	let (app, store, s, inv) = pending_pay_ahead(&db).await;
	let now = AdminCancelReq { immediate: true, refund: RefundMode::None };
	Plans::from_app(&app)
		.unwrap()
		.admin_cancel(&ctx(), s.uid.as_str(), &now)
		.await
		.unwrap();
	settles_into_a_refund(&app, &store, &inv).await;
	assert_eq!(sub(&store, &s.uid).await.status, SubStatus::Canceled);
}

#[tokio::test]
async fn a_paid_ahead_trial_cannot_upgrade_before_its_period_starts() {
	let db = TmpDb::new("activate-upgrade");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let a = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	pay_by_card(&app, &a.invoice_uid).await;
	let paid = status_becomes(&store, &s.uid, SubStatus::Active).await;
	let up = plans.quote(&owner(), &change_req("pro", 2, &paid)).await;
	assert_eq!(code_of(up), "E-PLAN-CHANGE-INVALID");
}

#[tokio::test]
async fn cancel_change_keeps_the_pay_ahead_unless_a_change_was_queued() {
	let db = TmpDb::new("activate-cancel-change");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let uid = s.uid.as_str();
	let a = plans.activate(&owner(), uid, None).await.unwrap();
	plans.cancel_change(&owner(), uid).await.unwrap();
	assert!(store.invoice_by_uid(None, &a.invoice_uid).await.unwrap().is_some(), "kept");

	let year = plans.activate(&owner(), uid, Some("pro_year")).await.unwrap();
	plans.cancel_change(&owner(), uid).await.unwrap();
	assert!(store.invoice_by_uid(None, &year.invoice_uid).await.unwrap().is_none(), "dropped");
}

#[tokio::test]
async fn the_trial_reminder_quotes_a_queued_change() {
	let db = TmpDb::new("trial-reminder-queued");
	let (app, store, s) = card_trial(&db).await;
	sqlx::query(
		"UPDATE subscriptions SET next_qty = 2,
		 next_offer_id = (SELECT id FROM offers WHERE code = 'pro_year') WHERE id = ?",
	)
	.bind(s.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	assert_eq!(reminder_amount(&app, &store, &s).await, "228600.00", "2 × 90 000 HUF + 27% VAT");
}

#[tokio::test]
async fn an_immediate_cancel_reruns_the_cuts_on_a_canceled_sub() {
	let db = TmpDb::new("cancel-rerun");
	let (app, store, s) = card_trial(&db).await;
	let plans = Plans::from_app(&app).unwrap();
	let a = plans.activate(&owner(), s.uid.as_str(), None).await.unwrap();
	pay_by_card(&app, &a.invoice_uid).await;
	status_becomes(&store, &s.uid, SubStatus::Active).await;
	// A pass whose CAS landed but whose cuts failed.
	sqlx::query("UPDATE subscriptions SET status = 'CANCELED' WHERE id = ?")
		.bind(s.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let now = AdminCancelReq { immediate: true, refund: RefundMode::None };
	plans.admin_cancel(&ctx(), s.uid.as_str(), &now).await.unwrap();
	let g = grant(&store, &period_ref(&s, s.period_start)).await.unwrap();
	assert!(g.1.unwrap() <= Timestamp::now().0, "the cuts re-ran");
}

// vim: ts=4
