//! Checkout: turns a verified quote into a draft invoice for the acting org and links it in
//! `plan_invoices`. CARD leaves the draft for `POST /api/invoices/{uid}/pay`; TRANSFER issues it.
//! A RECURRING offer goes to [`crate::subscribe`]; a tier change is [`change`] here.

use saas_core::ctx::Ctx;
use saas_core::event::{Event, emit};
use saas_core::ids::SubscriptionId;
use saas_core::prelude::*;
use saas_entitle::Source;
use saas_invoice::{Invoice, Invoices, Line, NewDraft, PaymentMethod};
use serde::{Deserialize, Serialize};

use crate::coupon;
use crate::quote::{self, ChangeKind, Claims, Priced};
use crate::service::Plans;
use crate::store::{
	LinkKind, LinkOutcome, NewPlanInvoice, OfferKind, PayMethod, PlanInvoice, Subscription,
};
use crate::subscribe::{self, DAY};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutReq {
	pub quote_token: String,
	pub pay_method: PayMethod,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkout {
	/// `None` for a trial, which invoices nothing until its first renewal.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub invoice_uid: Option<InvoiceId>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub subscription_uid: Option<SubscriptionId>,
	/// `pay` (the draft awaits a card payment), `issued` (a transfer invoice was issued),
	/// `trialing` (no invoice yet) or `scheduled` (a downgrade queued for `period_end`).
	pub next: &'static str,
}

pub(crate) async fn checkout(plans: &Plans, ctx: &Ctx, req: &CheckoutReq) -> ClResult<Checkout> {
	let claims = quote::opened(plans, ctx, &req.quote_token).await?;
	if let Some(uid) = claims.subscription.clone() {
		return change(plans, ctx, req, &claims, &uid).await;
	}
	let (claims, p) = quote::verify(plans, ctx, claims).await?;
	if p.offer.kind == OfferKind::Recurring {
		return subscribe::checkout(plans, ctx, req, &claims, &p).await;
	}
	let sys = ctx.clone().as_system("plans");
	let draft_org = ctx.org()?;
	let invoices = Invoices::new(plans.app.clone());
	let draft = draft(plans, ctx, &p, req).await?;
	// Every submit of one token shares the draft: a pass that finds it linked answers as the
	// replay, where deleting the draft cascaded the winner's link away.
	if plans.store.plan_invoice_get(&draft.uid).await?.is_none() {
		let link = NewPlanInvoice {
			invoice_id: draft.id,
			subscription_id: None,
			offer_id: p.offer.id,
			kind: LinkKind::Purchase,
			qty: p.qty,
			period_start: None,
			period_end: None,
			coupon_ref_id: p.coupon.as_ref().map(|c| c.r.id),
			prev: None,
		};
		let out = link_with_coupon(plans, ctx, &link, p.coupon.as_ref(), draft_org).await?;
		if matches!(out, LinkOutcome::CouponInvalid)
			&& plans.store.plan_invoice_get(&draft.uid).await?.is_none()
		{
			if let Err(e) = invoices.delete_draft(&sys, draft.uid.as_str()).await {
				tracing::warn!(invoice = %draft.uid, error = %e, "unlinked checkout draft kept");
			}
			return Err(coupon::invalid());
		}
	}
	let next = issue_transfer(plans, ctx, &draft, req.pay_method).await?;
	// Nothing to pay settles nothing: grant here, and a replayed checkout heals a lost grant.
	if draft.gross == Money::ZERO
		&& let Some(link) = plans.store.plan_invoice_get(&draft.uid).await?
	{
		crate::events::grant_purchase(&plans.app, &link, &draft.uid).await?;
	}
	Ok(Checkout { invoice_uid: Some(draft.uid), subscription_uid: None, next })
}

/// Links the draft and redeems its coupon in one transaction, so a link implies the coupon's
/// use; an unusable coupon is [`LinkOutcome::CouponInvalid`] before the store is asked.
pub(crate) async fn link_with_coupon(
	plans: &Plans,
	ctx: &Ctx,
	link: &NewPlanInvoice,
	c: Option<&coupon::Coupon>,
	org: i64,
) -> ClResult<LinkOutcome> {
	let redeem = match c {
		None => None,
		Some(c) => match coupon::redeem_args(plans, ctx, c, org).await? {
			Some(r) => Some(r),
			None => return Ok(LinkOutcome::CouponInvalid),
		},
	};
	plans.store.plan_invoice_link(link, redeem).await
}

/// The grants' `source_ref` for an upgrade's remainder of the period.
pub(crate) fn upgrade_ref(sub: &SubscriptionId, invoice: &InvoiceId) -> String {
	format!("sub:{sub}:up:{invoice}")
}

/// A tier change (design §7.2): an upgrade is invoiced for the rest of the period, a downgrade
/// is queued for `renew_due`. A TRANSFER upgrade applies now; a CARD one when its payment
/// settles ([`crate::events`]). Money never flows back through an invoice.
async fn change(
	plans: &Plans,
	ctx: &Ctx,
	req: &CheckoutReq,
	claims: &Claims,
	uid: &str,
) -> ClResult<Checkout> {
	let sub = quote::own_sub(plans, ctx, uid).await?;
	let request_id = request_id(req, req.pay_method);
	if sub.updated_at.0 != claims.sub_updated
		&& let Some(done) = replayed(plans, ctx, req, claims, &sub, &request_id).await?
	{
		return Ok(done);
	}
	let (c, p) = quote::verify_change(plans, ctx, claims, sub).await?;
	let mut next = c.sub.clone();
	if c.kind == ChangeKind::Downgrade {
		// A seat-only downgrade keeps the grandfathered price: `renew_due` reprices only with
		// `next_offer_id` set. A second downgrade overwrites the queued one.
		next.next_offer_id = Some(p.offer.id).filter(|&id| id != next.offer_id);
		next.next_qty = Some(p.qty);
		let saved = plans.store.sub_save(&next).await?.ok_or_else(quote::stale)?;
		return Ok(Checkout {
			invoice_uid: None,
			subscription_uid: Some(saved.uid),
			next: "scheduled",
		});
	}

	let at = Timestamp(claims.at);
	let draft = draft(plans, ctx, &p, req).await?;
	if plans.store.plan_invoice_get(&draft.uid).await?.is_none() {
		plans
			.store
			.plan_invoice_insert(&NewPlanInvoice {
				invoice_id: draft.id,
				subscription_id: Some(next.id),
				offer_id: p.offer.id,
				kind: LinkKind::Upgrade,
				qty: p.qty,
				period_start: Some(at),
				period_end: Some(next.period_end),
				coupon_ref_id: None,
				prev: Some((next.offer_id, next.qty)),
			})
			.await?;
	}
	// Saved before the invoice is issued, so a lost race leaves only a draft to delete. CARD
	// saves the row unchanged: the moved `updated_at` keeps the quote single-flight.
	let transfer = req.pay_method == PayMethod::Transfer;
	if transfer {
		(next.offer_id, next.qty, next.price) = (p.offer.id, p.qty, c.seat_price);
		(next.next_offer_id, next.next_qty) = (None, None);
	}
	let Some(saved) = plans.store.sub_save(&next).await? else {
		// The link goes with the draft (ON DELETE CASCADE).
		Invoices::new(plans.app.clone())
			.delete_draft(&ctx.clone().as_system("plans"), draft.uid.as_str())
			.await?;
		return Err(quote::stale());
	};
	if transfer {
		emit(&plans.app, Event::SubscriptionTierChanged { subscription: saved.uid.clone() });
	}
	finish_upgrade(plans, ctx, req, &saved, &draft, at).await
}

/// A replayed upgrade checkout whose first pass already linked its draft answers what that
/// pass did (and finishes it); `None` when the sub moved for another reason, which is stale.
async fn replayed(
	plans: &Plans,
	ctx: &Ctx,
	req: &CheckoutReq,
	claims: &Claims,
	sub: &Subscription,
	request_id: &str,
) -> ClResult<Option<Checkout>> {
	if claims.action == "downgrade" {
		let queued = match sub.next_offer_id {
			Some(id) => plans.store.offer_get(id).await?,
			None => plans.store.offer_get(sub.offer_id).await?,
		};
		let same =
			sub.next_qty == Some(claims.qty) && queued.is_some_and(|o| o.code == claims.offer);
		return Ok(same.then(|| Checkout {
			invoice_uid: None,
			subscription_uid: Some(sub.uid.clone()),
			next: "scheduled",
		}));
	}
	let store = saas_invoice::invoice_store(&plans.app)?;
	let Some(draft) = store.invoice_by_request_id(sub.org_id, request_id).await? else {
		return Ok(None);
	};
	// The request id is this token's signature: a linked draft is this token's first pass.
	let linked = plans.store.plan_invoice_get(&draft.uid).await?;
	if linked.is_none_or(|l| l.kind != LinkKind::Upgrade) {
		return Ok(None);
	}
	let at = Timestamp(claims.at);
	finish_upgrade(plans, ctx, req, sub, &draft, at).await.map(Some)
}

/// Issues a TRANSFER upgrade with provisional grants (no meters) to `at + plans.grace_days`; a
/// CARD one's grants, and a TRANSFER one's meters, are written when it settles. Idempotent, so a replay may run it again.
async fn finish_upgrade(
	plans: &Plans,
	ctx: &Ctx,
	req: &CheckoutReq,
	sub: &Subscription,
	draft: &Invoice,
	at: Timestamp,
) -> ClResult<Checkout> {
	let next = issue_transfer(plans, ctx, draft, req.pay_method).await?;
	if req.pay_method == PayMethod::Transfer {
		let grace = plans.app.settings.int("plans.grace_days").await?;
		let until = Timestamp(at.0 + grace * DAY).min(sub.period_end);
		let link = plans.store.plan_invoice_get(&draft.uid).await?.ok_or(Error::NotFound)?;
		if until > at {
			let meters = draft.gross == Money::ZERO;
			grant_upgrade(plans, &link, &sub.uid, meters, at, until).await?;
		}
	}
	Ok(Checkout {
		invoice_uid: Some(draft.uid.clone()),
		subscription_uid: Some(sub.uid.clone()),
		next,
	})
}

/// An upgrade link's grants from `from` to `until`: the new tier, its meters (with `meters`)
/// topped up only by the difference over the tier it replaced (`link.prev`).
pub(crate) async fn grant_upgrade(
	plans: &Plans,
	link: &PlanInvoice,
	sub: &SubscriptionId,
	meters: bool,
	from: Timestamp,
	until: Timestamp,
) -> ClResult<()> {
	let offer = plans.store.offer_get(link.offer_id).await?.ok_or(Error::NotFound)?;
	let prev = match link.prev {
		Some((id, qty)) => Some((plans.store.offer_get(id).await?.ok_or(Error::NotFound)?, qty)),
		None => None,
	};
	let r = upgrade_ref(sub, &link.invoice_uid);
	subscribe::grant_period(
		&plans.app,
		link.org_id,
		(&offer, link.qty),
		prev.as_ref().map(|(o, q)| (o, *q)),
		meters,
		Source::Subscription,
		&r,
		from,
		Some(until),
	)
	.await
}

/// The idempotency key: the token's signature, so a replayed checkout gets the invoice it
/// made, and the method, so a resubmit with the other one is caught rather than replayed.
pub(crate) fn request_id(req: &CheckoutReq, method: PayMethod) -> String {
	let sig = req.quote_token.rsplit('.').next().unwrap_or_default();
	format!("plans:{sig}:{}", method.as_str())
}

/// The priced line as a draft for the acting org, idempotent on the token and method. A
/// token already drafted under the other method is a conflict.
// ponytail: two concurrent submits with different methods both pass the check; the second
// draft is a duplicate purchase, not a corrupt one.
pub(crate) async fn draft(
	plans: &Plans,
	ctx: &Ctx,
	p: &Priced,
	req: &CheckoutReq,
) -> ClResult<Invoice> {
	let pay_method = req.pay_method;
	let other = match pay_method {
		PayMethod::Card => PayMethod::Transfer,
		PayMethod::Transfer => PayMethod::Card,
	};
	let invoices = saas_invoice::invoice_store(&plans.app)?;
	if invoices
		.invoice_by_request_id(ctx.org()?, &request_id(req, other))
		.await?
		.is_some()
	{
		return Err(Error::conflict(
			"this quote was already checked out with another payment method",
		));
	}
	let mut line = Line::adhoc(
		p.line.description.clone(),
		p.line.unit.clone(),
		p.line.qty,
		p.unit_price,
		p.line.vat_code,
	);
	line.discount = p.line.discount;
	line.discount_description = p.line.discount_description.clone();
	Invoices::new(plans.app.clone())
		.draft(
			&ctx.clone().as_system("plans"),
			&NewDraft {
				request_id: Some(request_id(req, pay_method)),
				lines: vec![line],
				// CARD is reserved: `begin_card_payment` stamps it when the SPA pays.
				payment_method: (pay_method == PayMethod::Transfer)
					.then_some(PaymentMethod::Transfer),
				currency: Some(p.currency.code.clone()),
				..NewDraft::default()
			},
		)
		.await
}

/// TRANSFER, or anything with nothing to pay, issues the draft (once); CARD leaves it for the
/// pay route. The `next` answer.
pub(crate) async fn issue_transfer(
	plans: &Plans,
	ctx: &Ctx,
	draft: &Invoice,
	pay_method: PayMethod,
) -> ClResult<&'static str> {
	match pay_method {
		PayMethod::Card if draft.gross != Money::ZERO => Ok("pay"),
		_ => {
			if draft.status == saas_invoice::store::InvoiceStatus::Draft {
				Invoices::new(plans.app.clone())
					.issue(&ctx.clone().as_system("plans"), draft.uid.as_str())
					.await?;
			}
			Ok("issued")
		}
	}
}

// vim: ts=4
