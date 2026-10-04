//! `PlanStore`: the offer catalogue, subscriptions and the `plan_invoices` links.

use async_trait::async_trait;
use saas_core::error::ClResult;
use saas_core::ids::{InvoiceId, OfferId, OrgId, PaymentId, SubscriptionId};
use saas_core::money::{CurrencyCode, Money};
use saas_core::prelude::Timestamp;
use serde::{Deserialize, Serialize};

/// `Money` has no `Serialize`: an amount goes out as the wire's decimal string, beside its
/// sibling `currency` field.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `serialize_with` signature
fn decimal<S: serde::Serializer>(m: &Money, s: S) -> Result<S::Ok, S::Error> {
	s.serialize_str(&m.to_decimal_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OfferKind {
	OneTime,
	Recurring,
}

saas_core::str_enum!(OfferKind { OneTime => "ONE_TIME", Recurring => "RECURRING" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Interval {
	Month,
	Year,
}

saas_core::str_enum!(Interval { Month => "MONTH", Year => "YEAR" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubStatus {
	Trialing,
	Active,
	PastDue,
	Suspended,
	Canceled,
}

saas_core::str_enum!(SubStatus {
	Trialing => "TRIALING",
	Active => "ACTIVE",
	PastDue => "PAST_DUE",
	Suspended => "SUSPENDED",
	Canceled => "CANCELED",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PayMethod {
	Card,
	Transfer,
}

saas_core::str_enum!(PayMethod { Card => "CARD", Transfer => "TRANSFER" });

/// Why an invoice was issued, on its `plan_invoices` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LinkKind {
	Purchase,
	Subscribe,
	Renewal,
	Upgrade,
}

saas_core::str_enum!(LinkKind {
	Purchase => "PURCHASE",
	Subscribe => "SUBSCRIBE",
	Renewal => "RENEWAL",
	Upgrade => "UPGRADE",
});

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferPrice {
	pub currency: CurrencyCode,
	#[serde(serialize_with = "decimal")]
	pub amount: Money,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferEntitlement {
	pub key: String,
	pub amount: i64,
	/// Granted `amount × qty` rather than `amount`.
	pub per_seat: bool,
}

/// One `offers` row with its prices and entitlements.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
	#[serde(skip)]
	pub id: i64,
	pub uid: OfferId,
	#[serde(skip)]
	pub seller_org_id: i64,
	pub code: String,
	pub name: String,
	pub kind: OfferKind,
	#[serde(skip)]
	pub service_id: i64,
	pub family: Option<String>,
	pub rank: i64,
	pub interval: Option<Interval>,
	pub interval_count: Option<i64>,
	/// ONE_TIME: how long its grants last; `None` = forever.
	pub validity_days: Option<i64>,
	pub trial_days: i64,
	pub active: bool,
	/// By currency code.
	pub prices: Vec<OfferPrice>,
	/// By key.
	pub entitlements: Vec<OfferEntitlement>,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
}

/// What a catalogue declaration writes: [`PlanStore::offer_upsert`] keys it on
/// `(seller_org_id, code)`, and `uid` is used only when the row is new.
#[derive(Clone, Debug)]
pub struct NewOffer {
	pub uid: OfferId,
	pub seller_org_id: i64,
	pub code: String,
	pub name: String,
	pub kind: OfferKind,
	pub service_id: i64,
	pub family: Option<String>,
	pub rank: i64,
	pub interval: Option<Interval>,
	pub interval_count: Option<i64>,
	pub validity_days: Option<i64>,
	pub trial_days: i64,
	pub prices: Vec<OfferPrice>,
	pub entitlements: Vec<OfferEntitlement>,
}

/// One `subscriptions` row. [`PlanStore::sub_insert`] ignores `id`, `created_at` and
/// `updated_at`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
	#[serde(skip)]
	pub id: i64,
	pub uid: SubscriptionId,
	#[serde(skip)]
	pub org_id: i64,
	#[serde(skip)]
	pub offer_id: i64,
	pub family: Option<String>,
	pub qty: i64,
	pub status: SubStatus,
	pub currency: CurrencyCode,
	/// Per seat per period, grandfathered: a later offer price change reaches it only through
	/// an operator's [`PlanStore::offer_reprice`].
	#[serde(serialize_with = "decimal")]
	pub price: Money,
	pub period_start: Timestamp,
	pub period_end: Timestamp,
	pub cancel_at_period_end: bool,
	#[serde(skip)]
	pub next_offer_id: Option<i64>,
	pub next_qty: Option<i64>,
	pub pay_method: PayMethod,
	#[serde(skip)]
	pub provider: Option<String>,
	/// A gateway reference, not a credential.
	#[serde(skip)]
	pub recurrence_ref: Option<String>,
	#[serde(skip)]
	pub coupon_ref_id: Option<i64>,
	pub coupon_periods_left: Option<i64>,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
	/// The first paid period's start, fixed at insert: every renewal steps whole intervals from
	/// it, so a month-end clamp (Jan 31 → Feb 28) never compounds into the months after.
	#[serde(skip)]
	pub billing_anchor: Timestamp,
}

/// A [`Subscription`] in the operator's cross-org list, which needs the org it belongs to.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSubscription {
	#[serde(flatten)]
	pub sub: Subscription,
	pub org_uid: OrgId,
}

#[derive(Clone, Debug)]
pub struct NewPlanInvoice {
	pub invoice_id: i64,
	/// `None` exactly when `kind` is PURCHASE.
	pub subscription_id: Option<i64>,
	pub offer_id: i64,
	pub kind: LinkKind,
	pub qty: i64,
	pub period_start: Option<Timestamp>,
	pub period_end: Option<Timestamp>,
	pub coupon_ref_id: Option<i64>,
	/// UPGRADE only: the `(offer_id, qty)` it replaced, whose meters the upgrade tops up.
	pub prev: Option<(i64, i64)>,
}

/// One `plan_invoices` row plus the invoice's uid and buyer org.
#[derive(Clone, Debug)]
pub struct PlanInvoice {
	pub invoice_id: i64,
	pub invoice_uid: InvoiceId,
	pub org_id: i64,
	pub subscription_id: Option<i64>,
	pub offer_id: i64,
	pub kind: LinkKind,
	pub qty: i64,
	pub period_start: Option<Timestamp>,
	pub period_end: Option<Timestamp>,
	pub coupon_ref_id: Option<i64>,
	pub prev: Option<(i64, i64)>,
}

/// What [`PlanStore::plan_invoice_link`] did.
#[derive(Clone, Debug)]
pub enum LinkOutcome {
	Linked(PlanInvoice),
	/// Another pass linked the invoice first; nothing was redeemed.
	Exists(PlanInvoice),
	/// The coupon could not be redeemed; nothing was linked.
	CouponInvalid,
}

#[async_trait]
pub trait PlanStore: Send + Sync {
	/// `services.id` for the seller org's service `code`.
	async fn plan_service_id(&self, seller_org_id: i64, code: &str) -> ClResult<Option<i64>>;

	/// Inserts or updates the offer `(seller_org_id, code)`, sets it active, and replaces its
	/// prices and entitlements with the declared ones, in one transaction. `updated_at` moves
	/// only when an `offers` column changed.
	async fn offer_upsert(&self, new: &NewOffer) -> ClResult<Offer>;

	/// `active = 0` on the seller's active offers whose code is not in `keep`. Rows touched.
	async fn offers_deactivate_except(&self, seller_org_id: i64, keep: &[String]) -> ClResult<u64>;

	async fn offer_get(&self, id: i64) -> ClResult<Option<Offer>>;

	async fn offer_by_code(&self, seller_org_id: i64, code: &str) -> ClResult<Option<Offer>>;

	/// The seller's active offers, ordered by `family`, `rank`, `code`.
	async fn offers_active(&self, seller_org_id: i64) -> ClResult<Vec<Offer>>;

	/// A second live sub in one non-NULL family is `Error::Conflict`: the partial unique
	/// index on `(org_id, family)` is integrity, not a lookup aid.
	async fn sub_insert(&self, new: &Subscription) -> ClResult<Subscription>;

	/// Undoes a [`Self::sub_insert`] whose checkout failed before anything referenced the row.
	async fn sub_delete(&self, id: i64) -> ClResult<()>;

	async fn sub_get(&self, id: i64) -> ClResult<Option<Subscription>>;

	async fn sub_by_uid(&self, uid: &SubscriptionId) -> ClResult<Option<Subscription>>;

	/// Every sub of the org, canceled included, newest first.
	async fn subs_of_org(&self, org_id: i64) -> ClResult<Vec<Subscription>>;

	/// The org's sub in `family` whose status is not CANCELED.
	async fn sub_live_in_family(&self, org_id: i64, family: &str)
	-> ClResult<Option<Subscription>>;

	/// Whether the org, or any org with the same non-NULL `owner_account_id`, ever had a sub in
	/// `family`, in any status: a trial is once per family and owner, so a fresh org of the same
	/// owner gets none. An org with no owner (the root) counts only itself.
	async fn sub_ever_in_family(&self, org_id: i64, family: &str) -> ClResult<bool>;

	/// Subs with `period_end <= now` in TRIALING, ACTIVE or PAST_DUE, oldest `period_end` first.
	async fn subs_due(&self, now: Timestamp) -> ClResult<Vec<Subscription>>;

	/// Subs in any of `statuses`, by `id`.
	async fn subs_with_status(&self, statuses: &[SubStatus]) -> ClResult<Vec<Subscription>>;

	/// Writes every mutable column of `sub` if the row's `updated_at` still equals
	/// `sub.updated_at`; `None` when it moved (a concurrent write won). The stored
	/// `updated_at` becomes `max(now, old + 1)`, so two saves in one second still differ.
	async fn sub_save(&self, sub: &Subscription) -> ClResult<Option<Subscription>>;

	async fn plan_invoice_insert(&self, new: &NewPlanInvoice) -> ClResult<()>;

	/// One transaction: inserts the link and, when `redeem` is `(ref_id, account_id, org_id)`,
	/// redeems that ref as [`saas_core::refs::RefStore::ref_redeem`] does. A link exists ⇒ its
	/// coupon was redeemed: an existing link is [`LinkOutcome::Exists`] with no redeem, and a
	/// redeem that refused or found an earlier use is [`LinkOutcome::CouponInvalid`] with no link.
	async fn plan_invoice_link(
		&self,
		new: &NewPlanInvoice,
		redeem: Option<(i64, i64, i64)>,
	) -> ClResult<LinkOutcome>;

	/// `(invoice uid, payment uid)` of every plan invoice that is PAID with `paid_at >= since`,
	/// one row per SUCCEEDED payment allocated to it: the settle catch-up replays these.
	async fn plan_invoices_paid_since(
		&self,
		since: Timestamp,
	) -> ClResult<Vec<(InvoiceId, PaymentId)>>;

	async fn plan_invoice_get(&self, invoice: &InvoiceId) -> ClResult<Option<PlanInvoice>>;

	/// The sub's period link (UPGRADE excluded: it is not a period) with the latest
	/// `period_start`, then the highest `invoice_id`.
	async fn plan_invoice_latest(&self, subscription_id: i64) -> ClResult<Option<PlanInvoice>>;

	/// The sub's oldest unpaid link by `period_start`, then `invoice_id`: the dunning clock.
	/// Unpaid is `paid_amount < gross` and `ISSUED` (a partly paid invoice stays `ISSUED`), or
	/// a period link still `DRAFT`/`PENDING` (an uncharged CARD renewal); an UPGRADE draft, a
	/// `PAID`, `STORNOED` or zero-gross invoice is not.
	async fn plan_invoice_oldest_unpaid(
		&self,
		subscription_id: i64,
	) -> ClResult<Option<PlanInvoice>>;

	/// The sub's UPGRADE links with `period_start >= from`, by `invoice_id`.
	async fn plan_invoice_upgrades(
		&self,
		subscription_id: i64,
		from: Timestamp,
	) -> ClResult<Vec<PlanInvoice>>;

	/// Moves the link's period window: a late-paid SUSPENDED sub restarts its period, and the
	/// invoice then pays for the restarted window.
	async fn plan_invoice_set_period(
		&self,
		invoice_id: i64,
		start: Timestamp,
		end: Timestamp,
	) -> ClResult<()>;

	/// In one transaction: sets the offer's `currency` price to `amount`, and rewrites `price`
	/// on that offer's subs in `currency` that are not CANCELED and not already at `amount`,
	/// moving their `updated_at` as [`Self::sub_save`] does. Returns the rewritten subs.
	async fn offer_reprice(
		&self,
		offer_id: i64,
		currency: &CurrencyCode,
		amount: Money,
	) -> ClResult<Vec<Subscription>>;

	/// `orgs.id` for a public org uid.
	async fn plan_org_id(&self, uid: &OrgId) -> ClResult<Option<i64>>;

	/// The public uid for `orgs.id`: the mirror of [`Self::plan_org_id`].
	async fn plan_org_uid(&self, id: i64) -> ClResult<Option<OrgId>>;
}

// vim: ts=4
