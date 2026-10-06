// SPDX-License-Identifier: MPL-2.0
//! `PlanStore` conformance: offer reconcile, one live subscription per family, the
//! invoice-to-subscription link, the dunning clock and the draft sweep's renewal guard.

use mintworks_core::ids::{SellerId, SubscriptionId};
use mintworks_core::prelude::*;
use mintworks_core::store::CoreStore;
use mintworks_entitle::{EntitlementDef, EntitlementRegistry};
use mintworks_invoice::store::{
	Invoice, InvoiceKind, InvoiceStore, NewInvoice, PaymentMethod, Seller, SellerVersionPatch,
};
use mintworks_plans::{
	Interval, LinkKind, NewPlanInvoice, OfferDef, PayMethod, PlanStore, SubStatus, Subscription,
	reconcile_with,
};
use serde_json::json;

use crate::{Harness, fresh};

/// One `PLAN` service in the root org, which sells.
async fn setup<H: Harness>(h: &H) -> i64
where
	H::Store: CoreStore,
{
	let root = h.store().root_org_id().await.unwrap();
	h.exec(
		"INSERT INTO services (uid, org_id, code, name, unit_price, vat_code, created_at,
		 updated_at) VALUES ('svc_t', ?, 'PLAN', 'Plan', 0, 'STD27', 0, 0)",
		&[json!(root)],
	)
	.await;
	root
}

fn registry() -> EntitlementRegistry {
	EntitlementRegistry::new([EntitlementDef::meter("credits"), EntitlementDef::limit("seats")])
}

fn huf() -> CurrencyCode {
	CurrencyCode::huf()
}

fn pro(name: &str, price: i64) -> OfferDef {
	let mut d = OfferDef::recurring("pro", name, "PLAN", Interval::Month, 1)
		.price(huf(), price)
		.entitle("seats", 5, false)
		.entitle("credits", 1000, true);
	d.family = Some("plan".into());
	d.rank = 1;
	d
}

fn credits() -> OfferDef {
	OfferDef::one_time("credits_1000", "1000 credits", "PLAN")
		.price(huf(), 99_000)
		.entitle("credits", 1000, false)
}

fn sub(org_id: i64, offer_id: i64, family: Option<&str>) -> Subscription {
	Subscription {
		id: 0,
		uid: SubscriptionId::generate(),
		org_id,
		offer_id,
		family: family.map(str::to_owned),
		qty: 1,
		status: SubStatus::Active,
		currency: huf(),
		price: Money(499_000),
		period_start: Timestamp(1_000),
		period_end: Timestamp(2_000),
		cancel_at_period_end: false,
		next_offer_id: None,
		next_qty: None,
		pay_method: PayMethod::Transfer,
		provider: None,
		recurrence_ref: None,
		coupon_ref_id: None,
		coupon_periods_left: None,
		created_at: Timestamp(0),
		updated_at: Timestamp(0),
		billing_anchor: Timestamp(1_000),
	}
}

async fn put_seller<S: InvoiceStore>(store: &S, root: i64) {
	store
		.put_seller(&Seller {
			id: 1,
			uid: SellerId::generate(),
			org_id: root,
			nav_base_url: "https://api-test.onlineszamla.nav.gov.hu".into(),
			nav_login: None,
			series_code: "A".into(),
			closed_at: None,
			payment_days: None,
			created_at: Timestamp::now(),
		})
		.await
		.unwrap();
}

async fn draft<S: InvoiceStore>(store: &S, root: i64, payment_method: PaymentMethod) -> Invoice {
	store
		.create_draft(&NewInvoice {
			org_id: root,
			seller_id: 1,
			billing_party_id: None,
			request_id: None,
			kind: InvoiceKind::Normal,
			original_invoice_id: None,
			currency: huf(),
			rate_e6: 1_000_000,
			payment_method,
			notes: None,
			discount_kind: None,
			discount_value: None,
		})
		.await
		.unwrap()
}

pub async fn reconcile_upserts_and_deactivates<H: Harness>()
where
	H::Store: PlanStore + CoreStore,
{
	let h = fresh!(H, "plans-reconcile");
	let root = setup(&h).await;
	let store = h.store();
	let reg = registry();

	reconcile_with(store, Some(&reg), root, &[pro("Pro", 499_000), credits()])
		.await
		.unwrap();
	let first = store.offers_active(root).await.unwrap();
	assert_eq!(first.iter().map(|o| o.code.as_str()).collect::<Vec<_>>(), ["pro", "credits_1000"]);
	let p = &first[0];
	assert_eq!(p.prices[0].amount, Money(499_000));
	assert_eq!(p.entitlements.len(), 2);
	assert!(p.entitlements.iter().any(|e| e.key == "credits" && e.per_seat));

	// Renamed, repriced, and `credits_1000` no longer declared.
	reconcile_with(store, Some(&reg), root, &[pro("Pro+", 599_000)]).await.unwrap();
	let active = store.offers_active(root).await.unwrap();
	assert_eq!(active.len(), 1);
	assert_eq!(active[0].uid, p.uid, "an update keeps the row");
	assert_eq!(active[0].name, "Pro+");
	assert_eq!(active[0].prices[0].amount, Money(599_000));
	let gone = store.offer_by_code(root, "credits_1000").await.unwrap().unwrap();
	assert!(!gone.active, "deactivated, never deleted");

	// Declared again: the same row comes back active.
	reconcile_with(store, Some(&reg), root, &[pro("Pro+", 599_000), credits()])
		.await
		.unwrap();
	let back = store.offer_by_code(root, "credits_1000").await.unwrap().unwrap();
	assert!(back.active);
	assert_eq!(back.uid, gone.uid);
}

pub async fn an_undeclared_entitlement_or_service_fails_before_any_write<H: Harness>()
where
	H::Store: PlanStore + CoreStore,
{
	let h = fresh!(H, "plans-undeclared");
	let root = setup(&h).await;
	let store = h.store();

	let bad_key = credits().entitle("nope", 1, false);
	assert!(
		reconcile_with(store, Some(&registry()), root, &[pro("Pro", 1), bad_key])
			.await
			.is_err()
	);
	assert!(reconcile_with(store, None, root, &[credits()]).await.is_err(), "no registry");
	let mut bad_service = credits();
	bad_service.service = "MISSING".into();
	assert!(
		reconcile_with(store, Some(&registry()), root, &[pro("Pro", 1), bad_service])
			.await
			.is_err()
	);
	assert!(store.offers_active(root).await.unwrap().is_empty());
}

pub async fn one_live_subscription_per_family<H: Harness>()
where
	H::Store: PlanStore + CoreStore,
{
	let h = fresh!(H, "plans-family");
	let root = setup(&h).await;
	let store = h.store();
	reconcile_with(store, Some(&registry()), root, &[pro("Pro", 1)]).await.unwrap();
	let offer = store.offer_by_code(root, "pro").await.unwrap().unwrap();

	assert!(!store.sub_ever_in_family(root, "plan").await.unwrap());
	let a = store.sub_insert(&sub(root, offer.id, Some("plan"))).await.unwrap();
	assert!(store.sub_insert(&sub(root, offer.id, Some("plan"))).await.is_err());
	// A NULL family is an add-on: any number.
	store.sub_insert(&sub(root, offer.id, None)).await.unwrap();
	store.sub_insert(&sub(root, offer.id, None)).await.unwrap();

	let mut canceled = a.clone();
	canceled.status = SubStatus::Canceled;
	let saved = store.sub_save(&canceled).await.unwrap().unwrap();
	assert!(saved.updated_at.0 > a.updated_at.0);
	assert!(store.sub_save(&canceled).await.unwrap().is_none(), "stale updated_at");

	store.sub_insert(&sub(root, offer.id, Some("plan"))).await.unwrap();
	assert!(store.sub_ever_in_family(root, "plan").await.unwrap());
	assert_eq!(store.subs_of_org(root).await.unwrap().len(), 4);
	assert_eq!(store.subs_due(Timestamp(2_000)).await.unwrap().len(), 3, "canceled is not due");
	assert!(store.subs_due(Timestamp(1_999)).await.unwrap().is_empty());
	let live = store.sub_live_in_family(root, "plan").await.unwrap().unwrap();
	assert_ne!(live.uid, a.uid);
	assert_eq!(store.subs_with_status(&[SubStatus::Canceled]).await.unwrap()[0].uid, a.uid);
}

/// A `Conflict` inside a held transaction leaves it usable: PostgreSQL aborts a transaction on
/// any error, and a caller like `trial_replay` recovers from the conflict and writes on.
pub async fn a_conflict_leaves_the_outer_transaction_usable<H: Harness>()
where
	H::Store: PlanStore + CoreStore,
{
	let h = fresh!(H, "plans-conflict-tx");
	let root = setup(&h).await;
	let store = h.store();
	reconcile_with(store, Some(&registry()), root, &[pro("Pro", 1)]).await.unwrap();
	let offer = store.offer_by_code(root, "pro").await.unwrap().unwrap();

	let (tx, bound) = H::begin(store).await.unwrap();
	let a = bound.sub_insert(&sub(root, offer.id, Some("plan"))).await.unwrap();
	let dup = bound.sub_insert(&sub(root, offer.id, Some("plan"))).await;
	assert!(matches!(dup, Err(Error::Conflict(_))), "{dup:?}");
	let addon = bound.sub_insert(&sub(root, offer.id, None)).await.unwrap();
	H::commit(tx).await.unwrap();

	let mut uids: Vec<_> =
		store.subs_of_org(root).await.unwrap().into_iter().map(|s| s.uid).collect();
	uids.sort_by(|x, y| x.as_str().cmp(y.as_str()));
	let mut want = vec![a.uid, addon.uid];
	want.sort_by(|x, y| x.as_str().cmp(y.as_str()));
	assert_eq!(uids, want);
}

pub async fn a_deleted_sub_was_never_in_the_family<H: Harness>()
where
	H::Store: PlanStore + CoreStore,
{
	let h = fresh!(H, "plans-sub-delete");
	let root = setup(&h).await;
	let store = h.store();
	reconcile_with(store, Some(&registry()), root, &[pro("Pro", 1)]).await.unwrap();
	let offer = store.offer_by_code(root, "pro").await.unwrap().unwrap();
	let a = store.sub_insert(&sub(root, offer.id, Some("plan"))).await.unwrap();
	store.sub_delete(a.id).await.unwrap();
	assert!(!store.sub_ever_in_family(root, "plan").await.unwrap());
}

pub async fn sub_save_persists_the_billing_anchor<H: Harness>()
where
	H::Store: PlanStore + CoreStore,
{
	let h = fresh!(H, "plans-sub-anchor");
	let root = setup(&h).await;
	let store = h.store();
	reconcile_with(store, Some(&registry()), root, &[pro("Pro", 1)]).await.unwrap();
	let offer = store.offer_by_code(root, "pro").await.unwrap().unwrap();
	let mut s = store.sub_insert(&sub(root, offer.id, Some("plan"))).await.unwrap();
	s.billing_anchor = Timestamp(5_000);
	store.sub_save(&s).await.unwrap().unwrap();
	let again = store.sub_by_uid(&s.uid).await.unwrap().unwrap();
	assert_eq!(again.billing_anchor, Timestamp(5_000));
}

pub async fn plan_invoice_links_an_invoice_to_its_subscription<H: Harness>()
where
	H::Store: PlanStore + CoreStore + InvoiceStore,
{
	let h = fresh!(H, "plans-link");
	let root = setup(&h).await;
	let store = h.store();
	reconcile_with(store, Some(&registry()), root, &[pro("Pro", 1)]).await.unwrap();
	let offer = store.offer_by_code(root, "pro").await.unwrap().unwrap();
	let s = store.sub_insert(&sub(root, offer.id, Some("plan"))).await.unwrap();
	put_seller(store, root).await;
	let mut ids = Vec::new();
	for _ in 0..2 {
		ids.push(draft(store, root, PaymentMethod::Transfer).await);
	}
	for (inv, start, kind) in
		[(&ids[0], 1_000, LinkKind::Subscribe), (&ids[1], 2_000, LinkKind::Renewal)]
	{
		store
			.plan_invoice_insert(&NewPlanInvoice {
				invoice_id: inv.id,
				subscription_id: Some(s.id),
				offer_id: offer.id,
				kind,
				qty: 1,
				period_start: Some(Timestamp(start)),
				period_end: Some(Timestamp(start + 1_000)),
				coupon_ref_id: None,
				prev: (kind == LinkKind::Renewal).then_some((offer.id, 2)),
			})
			.await
			.unwrap();
	}

	let link = store.plan_invoice_get(&ids[0].uid).await.unwrap().unwrap();
	assert_eq!(
		(link.org_id, link.kind, link.subscription_id),
		(root, LinkKind::Subscribe, Some(s.id))
	);
	let latest = store.plan_invoice_latest(s.id).await.unwrap().unwrap();
	assert_eq!(latest.invoice_uid, ids[1].uid);
	assert_eq!((link.prev, latest.prev), (None, Some((offer.id, 2))));
	assert_eq!(store.sub_get(s.id).await.unwrap().unwrap().billing_anchor, Timestamp(1_000));

	// A PURCHASE link carries no subscription; a second link for one invoice is refused.
	let dup = NewPlanInvoice {
		invoice_id: ids[0].id,
		subscription_id: None,
		offer_id: offer.id,
		kind: LinkKind::Purchase,
		qty: 1,
		period_start: None,
		period_end: None,
		coupon_ref_id: None,
		prev: None,
	};
	assert!(store.plan_invoice_insert(&dup).await.is_err());
}

/// The dunning clock: an unpaid UPGRADE counts; an UPGRADE draft, a paid, a stornoed or a
/// zero-gross invoice does not.
pub async fn oldest_unpaid_skips_paid_and_draft_links<H: Harness>()
where
	H::Store: PlanStore + CoreStore + InvoiceStore,
{
	let h = fresh!(H, "plans-oldest-unpaid");
	let root = setup(&h).await;
	let store = h.store();
	reconcile_with(store, Some(&registry()), root, &[pro("Pro", 1)]).await.unwrap();
	let offer = store.offer_by_code(root, "pro").await.unwrap().unwrap();
	let s = store.sub_insert(&sub(root, offer.id, Some("plan"))).await.unwrap();
	put_seller(store, root).await;
	// Statuses are set by hand: a real issue needs a buyer and a priced draft. The non-draft rows
	// get the columns the `invoices` CHECKs demand instead, so no backend has to switch them off.
	let version = SellerVersionPatch {
		name: Some("Teszt Kft.".into()),
		country: Some("HU".into()),
		tax_number: Some("12345678242".into()),
		postcode: Some("1011".into()),
		city: Some("Budapest".into()),
		street: Some("Fo utca 1.".into()),
		vat_scheme: Some("NORMAL".into()),
		..Default::default()
	};
	store.save_seller_version_draft(1, &version).await.unwrap();
	let mut uids = Vec::new();
	for (start, kind, status, gross) in [
		(1_000, LinkKind::Subscribe, "PAID", 100),
		(1_100, LinkKind::Renewal, "ISSUED", 0),
		(1_200, LinkKind::Upgrade, "DRAFT", 100),
		(1_500, LinkKind::Upgrade, "ISSUED", 100),
		(2_000, LinkKind::Renewal, "STORNOED", 100),
		(3_000, LinkKind::Renewal, "DRAFT", 100),
	] {
		let inv = draft(store, root, PaymentMethod::Transfer).await;
		let sql = if status == "DRAFT" {
			"UPDATE invoices SET status = ?, gross = ? WHERE id = ?"
		} else {
			"UPDATE invoices SET status = ?, gross = ?, number = uid, issued_at = 0,
			 fulfilment_date = '2020-01-01', buyer_name = 'B',
			 seller_ver = (SELECT MAX(seller_ver) FROM seller_versions WHERE seller_id = 1)
			 WHERE id = ?"
		};
		h.exec(sql, &[json!(status), json!(gross), json!(inv.id)]).await;
		store
			.plan_invoice_insert(&NewPlanInvoice {
				invoice_id: inv.id,
				subscription_id: Some(s.id),
				offer_id: offer.id,
				kind,
				qty: 1,
				period_start: Some(Timestamp(start)),
				period_end: Some(Timestamp(4_000)),
				coupon_ref_id: None,
				prev: None,
			})
			.await
			.unwrap();
		uids.push(inv.uid);
	}
	let oldest = store.plan_invoice_oldest_unpaid(s.id).await.unwrap().unwrap();
	assert_eq!(oldest.invoice_uid, uids[3], "the issued upgrade");

	// A DRAFT turned PAID needs the issued columns too.
	let paid = "UPDATE invoices SET status = 'PAID', number = uid, issued_at = 0,
		 fulfilment_date = '2020-01-01', buyer_name = 'B',
		 seller_ver = (SELECT MAX(seller_ver) FROM seller_versions WHERE seller_id = 1)
		 WHERE uid = ?";
	h.exec(paid, &[json!(uids[3].as_str())]).await;
	let oldest = store.plan_invoice_oldest_unpaid(s.id).await.unwrap().unwrap();
	assert_eq!(oldest.invoice_uid, uids[5], "an uncharged renewal draft");

	h.exec(paid, &[json!(uids[5].as_str())]).await;
	assert!(store.plan_invoice_oldest_unpaid(s.id).await.unwrap().is_none());
}

/// A RENEWAL draft is what a suspended subscriber still owes: the sweep keeps it until the
/// subscription is `CANCELED`, while an unlinked draft of the same age goes.
pub async fn sweep_drafts_keeps_a_live_subscriptions_renewal<H: Harness>()
where
	H::Store: PlanStore + CoreStore + InvoiceStore,
{
	let h = fresh!(H, "plans-sweep-renewal");
	let root = setup(&h).await;
	let store = h.store();
	reconcile_with(store, Some(&registry()), root, &[pro("Pro", 1)]).await.unwrap();
	let offer = store.offer_by_code(root, "pro").await.unwrap().unwrap();
	let s = store.sub_insert(&sub(root, offer.id, Some("plan"))).await.unwrap();
	put_seller(store, root).await;
	let mut ids = Vec::new();
	for _ in 0..2 {
		ids.push(draft(store, root, PaymentMethod::Card).await.id);
	}
	store
		.plan_invoice_insert(&NewPlanInvoice {
			invoice_id: ids[0],
			subscription_id: Some(s.id),
			offer_id: offer.id,
			kind: LinkKind::Renewal,
			qty: 1,
			period_start: Some(Timestamp(2_000)),
			period_end: Some(Timestamp(3_000)),
			coupon_ref_id: None,
			prev: None,
		})
		.await
		.unwrap();
	let exists = async |id: i64| {
		h.scalar_i64("SELECT COUNT(*) FROM invoices WHERE id = ?", &[json!(id)]).await
	};
	let cutoff = Timestamp(Timestamp::now().0 + 10);

	assert_eq!(store.sweep_drafts(cutoff).await.unwrap(), 1);
	assert_eq!((exists(ids[0]).await, exists(ids[1]).await), (1, 0));

	h.exec("UPDATE subscriptions SET status = 'CANCELED' WHERE id = ?", &[json!(s.id)])
		.await;
	assert_eq!(store.sweep_drafts(cutoff).await.unwrap(), 1);
	assert_eq!(exists(ids[0]).await, 0);
}

// vim: ts=4
