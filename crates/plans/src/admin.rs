//! Operator controls: cancel a sub (now, optionally with a prorated refund), reprice an offer
//! for its live subs, and list subs across orgs. Every entry point is `require_operator`.

use std::collections::HashMap;

use mintworks_billing::provider::PaymentState;
use mintworks_core::auth_mw::{require_operator, require_stepup};
use mintworks_core::ctx::Ctx;
use mintworks_core::event::{Event, emit};
use mintworks_core::ids::SubscriptionId;
use mintworks_core::prelude::*;
use mintworks_entitle::{Entitle, Source};
use mintworks_invoice::invoice_store;
use serde::Deserialize;

use crate::checkout::upgrade_ref;
use crate::service::Plans;
use crate::store::{AdminSubscription, PlanInvoice, SubStatus, Subscription};
use crate::subscribe::{self, period_ref};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefundMode {
	#[default]
	None,
	Prorated,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminCancelReq {
	pub immediate: bool,
	#[serde(default)]
	pub refund: RefundMode,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RepriceReq {
	pub currency: CurrencyCode,
	#[serde(deserialize_with = "decimal_money")]
	pub amount: Money,
}

/// The wire amount is a decimal string (`MoneyWire`), never minor units.
fn decimal_money<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Money, D::Error> {
	let s = String::deserialize(d)?;
	Money::parse(&s).map_err(serde::de::Error::custom)
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SubsFilter {
	pub status: Option<SubStatus>,
	pub org: Option<OrgId>,
}

/// `immediate = false` sets `cancel_at_period_end` (no refund), except on a SUSPENDED sub,
/// which no renewal reaches: that cancels now. `immediate = true` cancels now, cuts the
/// period's and its upgrades' grants at now, and with `Prorated` refunds each of those
/// invoices `paid_amount × (period_end − now) / (period_end − period_start)`.
pub(crate) async fn cancel(
	plans: &Plans,
	ctx: &Ctx,
	uid: &str,
	req: &AdminCancelReq,
) -> ClResult<Subscription> {
	require_operator(&plans.app, ctx).await?;
	let refund = req.refund == RefundMode::Prorated;
	if refund && !req.immediate {
		return Err(Error::validation("a prorated refund needs an immediate cancel"));
	}
	if req.immediate {
		// Checked before the cancel: a missing step-up must not leave it canceled unrefunded.
		require_stepup(&plans.app, ctx).await?;
	}
	let sub = cancel_sub(plans, ctx, uid, req).await?;
	let detail = serde_json::json!({ "immediate": req.immediate, "refund": refund });
	let uid = Some(sub.uid.as_str());
	mintworks_core::audit::log(
		&plans.app.store,
		ctx,
		"subscription",
		uid,
		"ADMIN_CANCEL",
		Some(detail),
	)
	.await;
	Ok(sub)
}

async fn cancel_sub(
	plans: &Plans,
	ctx: &Ctx,
	uid: &str,
	req: &AdminCancelReq,
) -> ClResult<Subscription> {
	let refund = req.refund == RefundMode::Prorated;
	let sub = plans
		.store
		.sub_by_uid(&SubscriptionId::parse(uid)?)
		.await?
		.ok_or(Error::NotFound)?;
	// A prorated cancel is retryable: a refund that failed after the cancel must be re-runnable.
	if sub.status == SubStatus::Canceled && !(req.immediate && refund) {
		return Err(Error::conflict("the subscription is canceled"));
	}
	if !req.immediate && sub.status == SubStatus::Suspended {
		require_stepup(&plans.app, ctx).await?;
		return cancel_now(plans, &sub).await;
	}
	if !req.immediate {
		return subscribe::update(plans, sub.id, |s| {
			let change = s.status != SubStatus::Canceled && !s.cancel_at_period_end;
			s.cancel_at_period_end = true;
			change
		})
		.await;
	}

	let canceled = cancel_now(plans, &sub).await?;
	if refund {
		let now = Timestamp::now();
		let period = plans
			.store
			.plan_invoice_latest(canceled.id)
			.await?
			.filter(|l| l.period_start == Some(canceled.period_start));
		let upgrades =
			plans.store.plan_invoice_upgrades(canceled.id, canceled.period_start).await?;
		for link in period.iter().chain(&upgrades) {
			refund_rest(plans, ctx, link, now).await?;
		}
	}
	Ok(canceled)
}

/// CANCELED now, with the current period's and its upgrades' grants cut at now. The family
/// is free again at once (`idx_sub_family_live`).
pub(crate) async fn cancel_now(plans: &Plans, sub: &Subscription) -> ClResult<Subscription> {
	let now = Timestamp::now();
	let canceled = subscribe::update(plans, sub.id, |s| {
		let change = s.status != SubStatus::Canceled;
		s.status = SubStatus::Canceled;
		(s.next_offer_id, s.next_qty) = (None, None);
		change
	})
	.await?;
	// `canceled`, not `sub`: a renewal between the caller's read and the CAS moved the period.
	let (org, start) = (canceled.org_id, canceled.period_start);
	let upgrades = plans.store.plan_invoice_upgrades(canceled.id, start).await?;
	let entitle = Entitle::from_app(&plans.app)?;
	let sys = Ctx::system("plans").with_org(org);
	// Both sources: the status before the CAS is unknown here, and a cut matching nothing is a no-op.
	let r = period_ref(&canceled.uid, start);
	for source in [Source::Trial, Source::Subscription] {
		entitle.cut_to(&sys, org, source, &r, now).await?;
	}
	for up in &upgrades {
		let r = upgrade_ref(&canceled.uid, &up.invoice_uid);
		entitle.cut_to(&sys, org, Source::Subscription, &r, now).await?;
	}
	Ok(canceled)
}

/// Refunds the unused share of `link`'s paid amount across its settled payments.
async fn refund_rest(plans: &Plans, ctx: &Ctx, link: &PlanInvoice, now: Timestamp) -> ClResult<()> {
	let (Some(start), Some(end)) = (link.period_start, link.period_end) else { return Ok(()) };
	let inv = invoice_store(&plans.app)?
		.invoice_by_uid(None, &link.invoice_uid)
		.await?
		.ok_or(Error::NotFound)?;
	let left = (end.0 - now.0).clamp(0, end.0 - start.0);
	let payments = mintworks_billing::store::store(&plans.app)?
		.payments_by_invoice(link.org_id, link.invoice_id)
		.await?;
	// `paid_amount` is net of refunds, so a retry targets the share of the gross paid and
	// refunds only what earlier passes have not.
	let refunded: i64 = payments.iter().map(|p| p.refunded_amount.0).sum();
	let target = round_half_up(
		i128::from((inv.paid_amount.0 + refunded).max(0)) * i128::from(left),
		i128::from((end.0 - start.0).max(1)),
	)?;
	let mut owed = target - refunded;
	for p in payments {
		let refundable = matches!(
			p.status,
			PaymentState::Succeeded | PaymentState::PartiallySucceeded | PaymentState::Refunded
		);
		let room = p.amount.0 - p.refunded_amount.0;
		if owed <= 0 || !refundable || room <= 0 {
			continue;
		}
		let amount = Money(owed.min(room));
		let reason = Some(format!("prorated cancel of {}", link.invoice_uid));
		mintworks_billing::refund(
			&plans.app,
			ctx,
			&p.uid,
			Some((amount, p.currency.clone())),
			reason,
		)
		.await?;
		owed -= amount.0;
	}
	Ok(())
}

/// Sets the offer's price in `currency` and rewrites it on every live sub of that offer and
/// currency, effective at each one's next renewal. The `OfferDef` catalogue still owns the
/// list price: the next boot's reconcile restores `offer_prices`, not the subs' rewritten
/// price.
pub(crate) async fn reprice(
	plans: &Plans,
	ctx: &Ctx,
	code: &str,
	req: &RepriceReq,
) -> ClResult<Vec<Subscription>> {
	require_operator(&plans.app, ctx).await?;
	require_stepup(&plans.app, ctx).await?;
	if req.amount.0 < 0 {
		return Err(Error::validation("the price must not be negative"));
	}
	// Renewals bill `sub.price` as an ad-hoc line, so an off-step price fails every sweep.
	let cur = mintworks_invoice::currency::get(&*invoice_store(&plans.app)?, &req.currency).await?;
	mintworks_invoice::currency::ensure_price_on_step(req.amount, cur.price_round_step)?;
	let seller = plans.app.store.root_org_id().await?;
	let offer = plans.store.offer_by_code(seller, code).await?.ok_or(Error::NotFound)?;
	let subs = plans.store.offer_reprice(offer.id, &req.currency, req.amount).await?;
	for s in &subs {
		emit(
			&plans.app,
			Event::SubscriptionRepriced {
				subscription: s.uid.clone(),
				price: s.price,
				currency: s.currency.clone(),
			},
		);
	}
	let detail = serde_json::json!({
		"currency": req.currency,
		"amount": req.amount.to_decimal_string(),
		"subs": subs.len(),
	});
	mintworks_core::audit::log(&plans.app.store, ctx, "offer", Some(code), "REPRICE", Some(detail))
		.await;
	Ok(subs)
}

/// Every org's subs, optionally filtered by status and org.
pub(crate) async fn list(
	plans: &Plans,
	ctx: &Ctx,
	filter: &SubsFilter,
) -> ClResult<Vec<AdminSubscription>> {
	require_operator(&plans.app, ctx).await?;
	let subs = if let Some(uid) = &filter.org {
		let org = plans.store.plan_org_id(uid).await?.ok_or(Error::NotFound)?;
		plans.store.subs_of_org(org).await?
	} else {
		let all = [
			SubStatus::Trialing,
			SubStatus::Active,
			SubStatus::PastDue,
			SubStatus::Suspended,
			SubStatus::Canceled,
		];
		plans
			.store
			.subs_with_status(filter.status.as_ref().map_or(&all[..], std::slice::from_ref))
			.await?
	};
	// ponytail: one lookup per distinct org; a JOIN in the listing query if the list grows
	let mut uids: HashMap<i64, OrgId> = HashMap::new();
	let mut out = Vec::with_capacity(subs.len());
	for sub in subs.into_iter().filter(|s| filter.status.is_none_or(|st| s.status == st)) {
		let org_uid = if let Some(u) = uids.get(&sub.org_id) {
			u.clone()
		} else {
			let u = plans.store.plan_org_uid(sub.org_id).await?.ok_or(Error::NotFound)?;
			uids.insert(sub.org_id, u.clone());
			u
		};
		out.push(AdminSubscription { sub, org_uid });
	}
	Ok(out)
}

// vim: ts=4
