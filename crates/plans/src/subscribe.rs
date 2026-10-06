// SPDX-License-Identifier: MPL-2.0
//! Subscribing: the RECURRING checkout (with or without a trial), cancel/resume, the period
//! grants, and the CAS status update every subscription write goes through.

use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::error::StatusCode;
use mintworks_core::event::{Event, emit};
use mintworks_core::ids::SubscriptionId;
use mintworks_core::prelude::*;
use mintworks_entitle::{Entitle, EntitlementRegistry, GrantReq, Kind, Source};
use mintworks_invoice::{Invoice, Invoices};

use crate::checkout::{self, Checkout, CheckoutReq};
use crate::coupon;
use crate::quote::{Claims, Priced};
use crate::renew;
use crate::service::Plans;
use crate::store::{
	LinkKind, LinkOutcome, NewPlanInvoice, Offer, OfferEntitlement, OfferKind, PayMethod,
	SubStatus, Subscription,
};

pub(crate) const DAY: i64 = 86_400;

pub const E_FAMILY_LIVE: &str = "E-PLAN-FAMILY-LIVE";
pub const E_TRIAL_USED: &str = "E-PLAN-TRIAL-USED";

fn family_live() -> Error {
	Error::coded(StatusCode::CONFLICT, E_FAMILY_LIVE, "a subscription in this family is live")
}

/// Once per family and owner account (any org the org's owner owns), whatever became of the
/// earlier sub. An offer with no family has nothing to count trials against, so it has none.
pub(crate) async fn trial_eligible(plans: &Plans, org: i64, offer: &Offer) -> ClResult<bool> {
	if offer.kind != OfferKind::Recurring || offer.trial_days <= 0 {
		return Ok(false);
	}
	let Some(family) = &offer.family else { return Ok(false) };
	Ok(!plans.store.sub_ever_in_family(org, family).await?)
}

/// The grants' `source_ref` for one period, and the renewal invoice's `request_id`.
pub(crate) fn period_ref(uid: &SubscriptionId, period_start: Timestamp) -> String {
	format!("sub:{uid}:{}", period_start.0)
}

/// The offer's current entitlements (not the ones at sale time) × qty where per seat.
/// `minus` is an upgrade's old tier: its meters already hold the old amount for this period, so
/// a meter gets only `max(0, new − old)`; limits (max) and features (OR) need no subtraction.
/// `meters: false` skips the meters: a grant is insert-once, so a provisional meter row would
/// hand an unpaid period its whole quota. Idempotent: unique on `(org, key, source, source_ref)`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn grant_period(
	app: &App,
	org_id: i64,
	(offer, qty): (&Offer, i64),
	minus: Option<(&Offer, i64)>,
	meters: bool,
	source: Source,
	source_ref: &str,
	from: Timestamp,
	until: Option<Timestamp>,
) -> ClResult<()> {
	let ctx = Ctx::system("plans").with_org(org_id);
	let entitle = Entitle::from_app(app)?;
	let registry = app.extensions.get::<EntitlementRegistry>().cloned().unwrap_or_default();
	let seats = |e: &OfferEntitlement, qty: i64| {
		if e.per_seat { e.amount.checked_mul(qty) } else { Some(e.amount) }
			.ok_or_else(|| Error::validation("entitlement amount out of range"))
	};
	for e in &offer.entitlements {
		if !meters && registry.kind(&e.key) == Some(Kind::Meter) {
			continue;
		}
		let mut amount = seats(e, qty)?;
		if let Some((old, old_qty)) = minus
			&& registry.kind(&e.key) == Some(Kind::Meter)
		{
			let had = match old.entitlements.iter().find(|o| o.key == e.key) {
				Some(o) => seats(o, old_qty)?,
				None => 0,
			};
			amount = amount.saturating_sub(had);
			if amount <= 0 {
				continue;
			}
		}
		entitle
			.grant_to(
				&ctx,
				org_id,
				&GrantReq {
					key: e.key.clone(),
					amount,
					valid_from: Some(from),
					valid_until: until,
					source,
					source_ref: Some(source_ref.to_owned()),
				},
			)
			.await?;
	}
	Ok(())
}

/// A period's grants at issue: provisional until `start + plans.grace_days` (design §7.1), or
/// to `end` when there is nothing to pay. Grace 0 grants nothing until the invoice is paid, and
/// meter quota is granted only once paid.
pub(crate) async fn provisional(
	plans: &Plans,
	sub: &Subscription,
	offer: &Offer,
	qty: i64,
	(start, end): (Timestamp, Timestamp),
	nothing_to_pay: bool,
) -> ClResult<()> {
	let grace = plans.app.settings.int("plans.grace_days").await?;
	let until = if nothing_to_pay { end } else { Timestamp(start.0 + grace * DAY).min(end) };
	if until <= start {
		return Ok(());
	}
	let r = period_ref(&sub.uid, start);
	let (org, src) = (sub.org_id, Source::Subscription);
	let tier = (offer, qty);
	grant_period(&plans.app, org, tier, None, nothing_to_pay, src, &r, start, Some(until)).await
}

/// Applies `f` to the current row and saves it, retrying a lost CAS; `f` returning `false`
/// means nothing to change. A status change emits `SubscriptionChanged`.
pub(crate) async fn update<F>(plans: &Plans, id: i64, f: F) -> ClResult<Subscription>
where
	F: Fn(&mut Subscription) -> bool,
{
	for _ in 0..3 {
		let cur = plans.store.sub_get(id).await?.ok_or(Error::NotFound)?;
		let mut next = cur.clone();
		if !f(&mut next) {
			return Ok(cur);
		}
		if let Some(saved) = plans.store.sub_save(&next).await? {
			if saved.status != cur.status {
				emit(
					&plans.app,
					Event::SubscriptionChanged {
						subscription: saved.uid.clone(),
						from: cur.status.as_str().to_owned(),
						to: saved.status.as_str().to_owned(),
					},
				);
			}
			return Ok(saved);
		}
	}
	Err(Error::conflict("the subscription changed concurrently; retry"))
}

/// `anchor` is the first paid period's start: `start`, or a trial's end.
fn new_sub(
	org: i64,
	p: &Priced,
	status: SubStatus,
	(start, end, anchor): (Timestamp, Timestamp, Timestamp),
	pay_method: PayMethod,
	coupon_periods_left: Option<i64>,
) -> Subscription {
	Subscription {
		id: 0,
		uid: SubscriptionId::generate(),
		org_id: org,
		offer_id: p.offer.id,
		family: p.offer.family.clone(),
		qty: p.qty,
		status,
		currency: p.currency.code.clone(),
		price: p.unit_price,
		period_start: start,
		period_end: end,
		cancel_at_period_end: false,
		next_offer_id: None,
		next_qty: None,
		pay_method,
		provider: None,
		recurrence_ref: None,
		coupon_ref_id: p.coupon.as_ref().map(|c| c.r.id),
		coupon_periods_left,
		created_at: start,
		updated_at: start,
		billing_anchor: anchor,
	}
}

async fn insert(plans: &Plans, sub: &Subscription) -> ClResult<Subscription> {
	match plans.store.sub_insert(sub).await {
		Err(Error::Conflict(_)) => Err(family_live()),
		other => other,
	}
}

/// The RECURRING checkout. A trial writes TRIAL grants to its end and no invoice; otherwise a
/// SUBSCRIBE draft is linked to a new ACTIVE sub, and TRANSFER issues it with provisional grants.
pub(crate) async fn checkout(
	plans: &Plans,
	ctx: &Ctx,
	req: &CheckoutReq,
	claims: &Claims,
	p: &Priced,
) -> ClResult<Checkout> {
	let org = ctx.org()?;
	let now = Timestamp::now();
	if claims.trial {
		if !trial_eligible(plans, org, &p.offer).await? {
			return trial_replay(plans, org, p).await;
		}
		let end = Timestamp(now.0 + p.offer.trial_days * DAY);
		// The trial spends none of the coupon's periods: they count paid renewals.
		let periods = p.coupon.as_ref().and_then(|c| c.periods);
		let sub = new_sub(org, p, SubStatus::Trialing, (now, end, end), req.pay_method, periods);
		let sub = match plans.store.sub_insert(&sub).await {
			Err(Error::Conflict(_)) => return trial_replay(plans, org, p).await,
			other => other?,
		};
		let r = period_ref(&sub.uid, now);
		// The redeem last: the rollback below cuts grants, but a spent use would stay spent.
		let started = async {
			let tier = (&p.offer, p.qty);
			grant_period(&plans.app, org, tier, None, true, Source::Trial, &r, now, Some(end))
				.await?;
			match &p.coupon {
				Some(c) => coupon::redeem(plans, ctx, c, org).await,
				None => Ok(()),
			}
		}
		.await;
		if let Err(e) = started {
			// Deleted, not canceled: a CANCELED row still counts as a used trial.
			let sys = Ctx::system("plans").with_org(org);
			Entitle::from_app(&plans.app)?.cut_to(&sys, org, Source::Trial, &r, now).await?;
			plans.store.sub_delete(sub.id).await?;
			return Err(e);
		}
		return Ok(Checkout {
			invoice_uid: None,
			subscription_uid: Some(sub.uid),
			next: "trialing",
		});
	}

	let draft = checkout::draft(plans, ctx, p, req).await?;
	let (sub, period) = match linked(plans, &draft, now).await? {
		Some(done) => done,
		None => link_new(plans, ctx, req, p, &draft, now).await?,
	};
	let next = checkout::issue_transfer(plans, ctx, &draft, req.pay_method).await?;
	// A paid CARD is issued inside the settlement, whose handler grants to `period_end`.
	let nothing_to_pay = draft.gross == Money::ZERO;
	if req.pay_method == PayMethod::Transfer || nothing_to_pay {
		provisional(plans, &sub, &p.offer, p.qty, period, nothing_to_pay).await?;
	}
	Ok(Checkout { invoice_uid: Some(draft.uid), subscription_uid: Some(sub.uid), next })
}

/// A trial checkout the org is no longer eligible for: the sub this token's earlier or
/// concurrent pass started answers as the replay; anything else is a used trial.
// ponytail: a second fresh trial token for the same offer looks like a replay; harmless, it
// answers the same sub.
async fn trial_replay(plans: &Plans, org: i64, p: &Priced) -> ClResult<Checkout> {
	let live = match &p.offer.family {
		Some(f) => plans.store.sub_live_in_family(org, f).await?,
		None => None,
	};
	let same = (p.offer.id, p.qty, p.coupon.as_ref().map(|c| c.r.id));
	match live {
		Some(s)
			if s.status == SubStatus::Trialing && (s.offer_id, s.qty, s.coupon_ref_id) == same =>
		{
			Ok(Checkout { invoice_uid: None, subscription_uid: Some(s.uid), next: "trialing" })
		}
		_ => Err(Error::coded(StatusCode::CONFLICT, E_TRIAL_USED, "the trial is used up")),
	}
}

/// The SUBSCRIBE link's sub and period: this token's first pass, or the pass that beat this one.
async fn linked(
	plans: &Plans,
	draft: &Invoice,
	now: Timestamp,
) -> ClResult<Option<(Subscription, (Timestamp, Timestamp))>> {
	let Some(link) = plans.store.plan_invoice_get(&draft.uid).await? else {
		return Ok(None);
	};
	let id = link.subscription_id.ok_or_else(|| Error::internal("SUBSCRIBE link, no sub"))?;
	let sub = plans.store.sub_get(id).await?.ok_or(Error::NotFound)?;
	Ok(Some((sub, link.period_start.zip(link.period_end).unwrap_or((now, now)))))
}

/// The first pass: a new ACTIVE sub, then the link and the coupon's use in one transaction.
/// Every submit of one token shares the draft, so a pass that lost the race answers as the
/// replay; deleting the draft cascaded the winner's link away.
async fn link_new(
	plans: &Plans,
	ctx: &Ctx,
	req: &CheckoutReq,
	p: &Priced,
	draft: &Invoice,
	now: Timestamp,
) -> ClResult<(Subscription, (Timestamp, Timestamp))> {
	let org = ctx.org()?;
	let live = match &p.offer.family {
		Some(f) => plans.store.sub_live_in_family(org, f).await?.is_some(),
		None => false,
	};
	if live {
		return lost(plans, ctx, p, draft, None, family_live(), now).await;
	}
	let period = (now, renew::next_period_end(&p.offer, now)?);
	// This first period spends one of the coupon's.
	let left = p.coupon.as_ref().and_then(|c| c.periods).map(|n| n - 1);
	let (start, end) = period;
	let sub = new_sub(org, p, SubStatus::Active, (start, end, start), req.pay_method, left);
	let sub = match insert(plans, &sub).await {
		Ok(s) => s,
		Err(e) => return lost(plans, ctx, p, draft, None, e, now).await,
	};
	let link = NewPlanInvoice {
		invoice_id: draft.id,
		subscription_id: Some(sub.id),
		offer_id: p.offer.id,
		kind: LinkKind::Subscribe,
		qty: p.qty,
		period_start: Some(period.0),
		period_end: Some(period.1),
		coupon_ref_id: sub.coupon_ref_id,
		prev: None,
	};
	let e = match checkout::link_with_coupon(plans, ctx, &link, p.coupon.as_ref(), org).await {
		Ok(LinkOutcome::Linked(_)) => return Ok((sub, period)),
		Ok(LinkOutcome::Exists(_)) => Error::conflict("the invoice is already linked"),
		Ok(LinkOutcome::CouponInvalid) => coupon::invalid(),
		Err(e) => e,
	};
	lost(plans, ctx, p, draft, Some(&sub), e, now).await
}

/// A first pass that failed before linking: cancels the sub it made, then answers as the replay
/// if another pass linked the draft, and only otherwise deletes it.
#[allow(clippy::too_many_arguments)]
async fn lost(
	plans: &Plans,
	ctx: &Ctx,
	p: &Priced,
	draft: &Invoice,
	mine: Option<&Subscription>,
	e: Error,
	now: Timestamp,
) -> ClResult<(Subscription, (Timestamp, Timestamp))> {
	if let Some(sub) = mine {
		cancel_own(plans, sub).await?;
	}
	if let Some(done) = linked(plans, draft, now).await? {
		return Ok(done);
	}
	// A live sub no older than the draft may be the other pass, about to link it: a re-read
	// then delete still cascaded that link away. Kept, the draft is that pass's to answer.
	if let Some(f) = &p.offer.family
		&& let Some(live) = plans.store.sub_live_in_family(ctx.org()?, f).await?
		&& live.created_at >= draft.created_at
	{
		return Err(e);
	}
	let sys = ctx.clone().as_system("plans");
	if let Err(d) = Invoices::new(plans.app.clone()).delete_draft(&sys, draft.uid.as_str()).await {
		tracing::warn!(invoice = %draft.uid, error = %d, "unlinked checkout draft kept");
	}
	Err(e)
}

/// A sub this checkout pass inserted and then could not keep; it holds no grants yet.
pub(crate) async fn cancel_own(plans: &Plans, sub: &Subscription) -> ClResult<()> {
	update(plans, sub.id, |s| {
		let change = s.status != SubStatus::Canceled;
		s.status = SubStatus::Canceled;
		change
	})
	.await?;
	Ok(())
}

pub(crate) async fn list(plans: &Plans, ctx: &Ctx) -> ClResult<Vec<Subscription>> {
	plans.store.subs_of_org(ctx.org()?).await
}

/// `cancel = true` ends the sub at `period_end`; `false` takes that back. A CANCELED sub is
/// final. A SUSPENDED sub never reaches its `period_end` renewal, so its cancel is immediate.
pub(crate) async fn set_cancel(
	plans: &Plans,
	ctx: &Ctx,
	uid: &str,
	cancel: bool,
) -> ClResult<Subscription> {
	let sub = crate::quote::own_sub(plans, ctx, uid).await?;
	if sub.status == SubStatus::Canceled {
		return Err(Error::conflict("the subscription is canceled"));
	}
	if sub.status == SubStatus::Suspended {
		if !cancel {
			return Err(Error::conflict("a suspended subscription has no pending cancel"));
		}
		return crate::admin::cancel_now(plans, &sub).await;
	}
	update(plans, sub.id, |s| {
		let change = s.status != SubStatus::Canceled && s.cancel_at_period_end != cancel;
		s.cancel_at_period_end = cancel;
		change
	})
	.await
}

// vim: ts=4
