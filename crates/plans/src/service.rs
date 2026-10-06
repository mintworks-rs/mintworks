//! `Plans`: the service handle every route and the Rune module call.

use std::sync::Arc;

use mintworks_core::app::App;
use mintworks_core::auth_mw::require_role;
use mintworks_core::ctx::Ctx;
use mintworks_core::ids::OfferId;
use mintworks_core::prelude::*;
use mintworks_core::store::Role;
use serde::Serialize;

use crate::admin::{self, AdminCancelReq, RepriceReq, SubsFilter};
use crate::checkout::{self, Checkout, CheckoutReq};
use crate::quote::{self, Quote, QuoteReq};
use crate::rewards::{self, Side};
use crate::store::{
	AdminSubscription, Interval, Offer, OfferEntitlement, OfferKind, PlanStore, Subscription,
};
use crate::{renew, subscribe};

/// An active offer on the wire: prices as `{amount, currency}` strings.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferView {
	pub uid: OfferId,
	pub code: String,
	pub name: String,
	pub kind: OfferKind,
	pub family: Option<String>,
	pub rank: i64,
	pub interval: Option<Interval>,
	pub interval_count: Option<i64>,
	pub validity_days: Option<i64>,
	pub trial_days: i64,
	pub prices: Vec<MoneyWire>,
	pub entitlements: Vec<OfferEntitlement>,
}

impl From<Offer> for OfferView {
	fn from(o: Offer) -> Self {
		Self {
			prices: o.prices.iter().map(|p| p.amount.to_wire(&p.currency)).collect(),
			uid: o.uid,
			code: o.code,
			name: o.name,
			kind: o.kind,
			family: o.family,
			rank: o.rank,
			interval: o.interval,
			interval_count: o.interval_count,
			validity_days: o.validity_days,
			trial_days: o.trial_days,
			entitlements: o.entitlements,
		}
	}
}

#[derive(Clone)]
pub struct Plans {
	pub(crate) app: App,
	pub(crate) store: Arc<dyn PlanStore>,
}

impl Plans {
	pub fn new(app: App, store: Arc<dyn PlanStore>) -> Self {
		Self { app, store }
	}

	/// Over the `Arc<dyn PlanStore>` in `app.extensions`.
	pub fn from_app(app: &App) -> ClResult<Self> {
		let store =
			app.extensions.get::<Arc<dyn PlanStore>>().cloned().ok_or_else(|| {
				Error::internal("mintworks-plans: no PlanStore extension registered")
			})?;
		Ok(Self::new(app.clone(), store))
	}

	/// The root org's active offers — the seller of every declared one. Public.
	pub async fn offers(&self, _ctx: &Ctx) -> ClResult<Vec<OfferView>> {
		let seller = self.app.store.root_org_id().await?;
		Ok(self
			.store
			.offers_active(seller)
			.await?
			.into_iter()
			.map(OfferView::from)
			.collect())
	}

	pub async fn quote(&self, ctx: &Ctx, req: &QuoteReq) -> ClResult<Quote> {
		require_role(&self.app, ctx, Role::Admin).await?;
		quote::quote(self, ctx, req).await
	}

	pub async fn checkout(&self, ctx: &Ctx, req: &CheckoutReq) -> ClResult<Checkout> {
		require_role(&self.app, ctx, Role::Admin).await?;
		checkout::checkout(self, ctx, req).await
	}

	/// The acting org's subs, canceled included, newest first.
	pub async fn subscriptions(&self, ctx: &Ctx) -> ClResult<Vec<Subscription>> {
		subscribe::list(self, ctx).await
	}

	/// Ends the acting org's sub at its `period_end`.
	pub async fn cancel(&self, ctx: &Ctx, uid: &str) -> ClResult<Subscription> {
		require_role(&self.app, ctx, Role::Admin).await?;
		subscribe::set_cancel(self, ctx, uid, true).await
	}

	/// Takes a pending cancel back.
	pub async fn resume(&self, ctx: &Ctx, uid: &str) -> ClResult<Subscription> {
		require_role(&self.app, ctx, Role::Admin).await?;
		subscribe::set_cancel(self, ctx, uid, false).await
	}

	/// Takes a queued downgrade back.
	pub async fn cancel_change(&self, ctx: &Ctx, uid: &str) -> ClResult<Subscription> {
		require_role(&self.app, ctx, Role::Admin).await?;
		let sub = quote::own_sub(self, ctx, uid).await?;
		subscribe::update(self, sub.id, |s| {
			let queued = s.next_offer_id.is_some() || s.next_qty.is_some();
			(s.next_offer_id, s.next_qty) = (None, None);
			queued
		})
		.await
	}

	/// Operator: cancel any org's sub, now or at `period_end`, optionally refunding pro rata.
	pub async fn admin_cancel(
		&self,
		ctx: &Ctx,
		uid: &str,
		req: &AdminCancelReq,
	) -> ClResult<Subscription> {
		admin::cancel(self, ctx, uid, req).await
	}

	/// Operator: reprice offer `code` for its live subs; the rewritten subs.
	pub async fn reprice(
		&self,
		ctx: &Ctx,
		code: &str,
		req: &RepriceReq,
	) -> ClResult<Vec<Subscription>> {
		admin::reprice(self, ctx, code, req).await
	}

	/// Operator: every org's subs, filtered.
	pub async fn admin_subscriptions(
		&self,
		ctx: &Ctx,
		filter: &SubsFilter,
	) -> ClResult<Vec<AdminSubscription>> {
		admin::list(self, ctx, filter).await
	}

	/// The `SUBSCRIPTION_RENEW` job body: renews every sub due at `now`, then the status sweep.
	/// Operator or system — it is not routed.
	pub async fn renew_due(&self, ctx: &Ctx, now: Timestamp) -> ClResult<()> {
		mintworks_core::auth_mw::require_operator(&self.app, ctx).await?;
		renew::renew_due(self, now).await
	}

	/// Operator or system: rewards the use of ref `ref_uid` by account `account_uid`. `false`
	/// when that side has no reward offer configured.
	pub async fn reward_ref_use(
		&self,
		ctx: &Ctx,
		ref_uid: &str,
		account_uid: &str,
		side: Side,
	) -> ClResult<bool> {
		rewards::reward_ref_use(self, ctx, ref_uid, account_uid, side).await
	}

	/// Operator or system: `offer_code` granted free to `org_uid`, idempotent on `idem`.
	pub async fn reward(
		&self,
		ctx: &Ctx,
		org_uid: &str,
		offer_code: &str,
		idem: &str,
	) -> ClResult<()> {
		rewards::reward(self, ctx, org_uid, offer_code, idem).await
	}
}

// vim: ts=4
