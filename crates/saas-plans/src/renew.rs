//! `SUBSCRIPTION_RENEW`: the periodic sweep that rolls due subs into their next period with a
//! renewal invoice, and moves unpaid ones to `PAST_DUE` and `SUSPENDED`. Time is the `now`
//! parameter, so tests drive [`renew_due`] directly.

use std::sync::Arc;

use saas_billing::provider::PaymentState;
use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::job::{self, Job, Runner};
use saas_core::prelude::*;
use saas_core::refs::Refs;
use saas_core::store::CoreStore;
use saas_invoice::store::InvoiceStatus;
use saas_invoice::{Invoice, Invoices, Line, NewDraft, PaymentMethod, invoice_store};
use time::{Date, Month, OffsetDateTime};

use crate::coupon;
use crate::quote;
use crate::service::Plans;
use crate::store::{Interval, LinkKind, NewPlanInvoice, Offer, PayMethod, SubStatus, Subscription};
use crate::subscribe::{self, DAY, period_ref, update};

pub const KIND: &str = "SUBSCRIPTION_RENEW";

const EVERY_SECS: i64 = 3600;

/// Template base name: `subscription_invoice.{hu.,}{html,txt}.hbs`.
const TEMPLATE: &str = "subscription_invoice";

/// `AppBuilder::jobs(saas_plans::renew::register)`: the renewal sweep and the settlement retry.
pub fn register(runner: &mut Runner, app: App) {
	let renew_app = app.clone();
	runner.register_periodic(KIND, EVERY_SECS, move |_job: Job| {
		let app = renew_app.clone();
		async move { Plans::from_app(&app)?.renew_due(&Ctx::system("plans"), Timestamp::now()).await }
	});
	runner.register(crate::events::SETTLED_KIND, move |job: Job| {
		let app = app.clone();
		async move { crate::events::settled_job(&app, &job.payload).await }
	});
}

/// Seeds the periodic sweep. Call once at boot.
pub async fn seed(store: &Arc<dyn CoreStore>) -> ClResult<()> {
	job::seed_periodic(store, KIND).await
}

/// `start` plus the offer's `interval × interval_count`, in calendar months (a year is 12),
/// the day clamped to the target month's length: Jan 31 + 1 month = Feb 28/29.
pub(crate) fn next_period_end(offer: &Offer, start: Timestamp) -> ClResult<Timestamp> {
	let count = offer.interval_count.unwrap_or(1);
	let months = match offer.interval {
		Some(Interval::Month) => count,
		Some(Interval::Year) => count * 12,
		None => return Err(Error::internal(format!("offer '{}' has no interval", offer.code))),
	};
	add_months(start, months)
}

/// The period after one ending at `old_end`: whole intervals from `anchor`, never from the
/// clamped `old_end`, so Jan 31 → Feb 29 → Mar 31 rather than → Mar 29.
pub(crate) fn next_anchored_end(
	offer: &Offer,
	anchor: Timestamp,
	old_end: Timestamp,
) -> ClResult<Timestamp> {
	let step = next_period_end(offer, anchor)?;
	let months = months_between(anchor, step)?;
	let end = add_months(anchor, months_between(anchor, old_end)? + months)?;
	// An end off the anchor's grid (edited by hand, or an anchor after it) steps from itself.
	Ok(if end > old_end { end } else { next_period_end(offer, old_end)? })
}

/// Whole months from `from` to `to`: the largest `n` with `add_months(from, n) <= to`.
fn months_between(from: Timestamp, to: Timestamp) -> ClResult<i64> {
	let mut n = month_index(utc(to)?) - month_index(utc(from)?);
	while n > 0 && add_months(from, n)? > to {
		n -= 1;
	}
	Ok(n.max(0))
}

/// The offer's current per-seat price in `currency`.
pub(crate) fn seat_price(offer: &Offer, currency: &CurrencyCode) -> ClResult<Money> {
	offer
		.prices
		.iter()
		.find(|p| p.currency == *currency)
		.map(|p| p.amount)
		.ok_or_else(|| Error::validation(format!("offer '{}' has no {currency} price", offer.code)))
}

/// UTC calendar months, time of day kept, the day clamped to the target month's length.
fn add_months(t: Timestamp, months: i64) -> ClResult<Timestamp> {
	let at = utc(t)?;
	let total = month_index(at) - 1 + months;
	let range = || Error::internal("period end out of range");
	let year = i32::try_from(total.div_euclid(12)).map_err(|_| range())?;
	let month = u8::try_from(total.rem_euclid(12) + 1).map_err(|_| range())?;
	let month = Month::try_from(month).map_err(|_| range())?;
	let day = at.day().min(month.length(year));
	let date = Date::from_calendar_date(year, month, day).map_err(|_| range())?;
	Ok(Timestamp(at.replace_date(date).unix_timestamp()))
}

fn utc(t: Timestamp) -> ClResult<OffsetDateTime> {
	OffsetDateTime::from_unix_timestamp(t.0).map_err(|_| Error::internal("timestamp out of range"))
}

fn month_index(at: OffsetDateTime) -> i64 {
	i64::from(at.year()) * 12 + i64::from(u8::from(at.month()))
}

/// The job body. One sub's failure is logged and the sweep goes on; the next tick retries it,
/// and every step is idempotent on the period being opened.
pub(crate) async fn renew_due(plans: &Plans, now: Timestamp) -> ClResult<()> {
	if let Err(e) = settle_catch_up(plans, now).await {
		tracing::warn!(error = %e, "settlement catch-up failed");
	}
	for sub in plans.store.subs_due(now).await? {
		if let Err(e) = renew_one(plans, &sub).await {
			tracing::warn!(subscription = %sub.uid, error = %e, "renewal failed");
		}
	}
	sweep_status(plans, now).await
}

/// Re-enqueues the settle job of every plan invoice paid in the last week. Events are not
/// durable, so a crash before `events::handle` enqueued it lost the grants; the job's dedup key
/// is permanent, so an invoice already handled is a no-op.
// ponytail: catches up the settle only; a `first_payment` reward is still event-only.
async fn settle_catch_up(plans: &Plans, now: Timestamp) -> ClResult<()> {
	for (inv, pay) in plans.store.plan_invoices_paid_since(Timestamp(now.0 - 7 * DAY)).await? {
		let payload = serde_json::json!({ "payment": pay, "invoice": inv }).to_string();
		let key = format!("plans:settled:{inv}");
		job::enqueue(&plans.app.store, crate::events::SETTLED_KIND, &payload, Some(&key), now)
			.await?;
	}
	Ok(())
}

/// Invoice, link, grants and mail first, all keyed on the new period; the CAS save that
/// advances the period last, so a crash in between is redone by the next tick, not lost.
async fn renew_one(plans: &Plans, sub: &Subscription) -> ClResult<()> {
	let (old_start, old_end) = (sub.period_start, sub.period_end);
	if sub.cancel_at_period_end {
		update(plans, sub.id, |s| {
			// A resume racing the sweep cleared the flag; the next tick renews it instead.
			let due = s.cancel_at_period_end
				&& s.period_end == old_end
				&& s.status != SubStatus::Canceled;
			if due {
				s.status = SubStatus::Canceled;
			}
			due
		})
		.await?;
		return Ok(());
	}

	// A scheduled tier change takes effect here, at the offer's current price.
	let offer_id = sub.next_offer_id.unwrap_or(sub.offer_id);
	let qty = sub.next_qty.unwrap_or(sub.qty);
	let offer = plans.store.offer_get(offer_id).await?.ok_or(Error::NotFound)?;
	let price = match sub.next_offer_id {
		None => sub.price,
		Some(_) => seat_price(&offer, &sub.currency)?,
	};
	let period = (old_end, next_anchored_end(&offer, sub.billing_anchor, old_end)?);
	let (seen, billed) = (tier_of(sub), (offer_id, qty, price));
	let coupon_on = sub.coupon_ref_id.is_some() && sub.coupon_periods_left != Some(0);
	let coupon = if coupon_on { coupon_of(plans, sub, &offer).await? } else { None };

	let inv = renewal_invoice(plans, sub, &offer, qty, price, coupon.as_ref(), period.0).await?;
	if plans.store.plan_invoice_get(&inv.uid).await?.is_none() {
		plans
			.store
			.plan_invoice_insert(&NewPlanInvoice {
				invoice_id: inv.id,
				subscription_id: Some(sub.id),
				offer_id,
				kind: LinkKind::Renewal,
				qty,
				period_start: Some(period.0),
				period_end: Some(period.1),
				coupon_ref_id: sub.coupon_ref_id.filter(|_| coupon.is_some()),
				prev: None,
			})
			.await?;
	}
	subscribe::provisional(plans, sub, &offer, qty, period, inv.gross == Money::ZERO).await?;
	if inv.gross != Money::ZERO && !charged(plans, sub, &inv, period.0).await {
		mail(plans, sub, &offer, &inv, period).await?;
	}

	let saved = update(plans, sub.id, |s| {
		if s.period_start != old_start || s.status == SubStatus::Canceled {
			return false;
		}
		(s.period_start, s.period_end) = period;
		(s.offer_id, s.qty, s.price, s.next_offer_id, s.next_qty) =
			renewed_tier(tier_of(s), seen, billed);
		if coupon.is_some() {
			s.coupon_periods_left = s.coupon_periods_left.map(|n| n - 1);
		}
		if s.status == SubStatus::Trialing {
			s.status = SubStatus::Active;
		}
		true
	})
	.await?;
	let sys = Ctx::system("plans").with_org(sub.org_id);
	if saved.status == SubStatus::Canceled && saved.period_start != period.0 {
		// A cancel landed between the grants and the CAS: cut the period this pass granted.
		let r = period_ref(&sub.uid, period.0);
		saas_entitle::Entitle::from_app(&plans.app)?
			.cut_to(&sys, sub.org_id, saas_entitle::Source::Subscription, &r, Timestamp::now())
			.await?;
		return Ok(());
	}
	if saved.period_start == period.0 && (saved.offer_id, saved.qty, saved.price) != billed {
		let detail = serde_json::json!({ "invoice": inv.uid, "period_start": period.0.0 });
		let uid = Some(saved.uid.as_str());
		saas_core::audit::log(
			&plans.app.store,
			&sys,
			"subscription",
			uid,
			"RENEW_TIER_STALE",
			Some(detail),
		)
		.await;
	}
	// An unpaid CARD upgrade of the closed period can no longer apply; paid late it would take
	// the money for nothing. A PENDING one refuses the delete and `events::upgrade_paid` warns.
	let invoices = Invoices::new(plans.app.clone());
	let closed = plans.store.plan_invoice_upgrades(sub.id, old_start).await?;
	for up in closed.iter().filter(|u| u.period_start < Some(period.0)) {
		let draft = invoice_store(&plans.app)?.invoice_by_uid(None, &up.invoice_uid).await?;
		if draft.is_some_and(|i| i.status == InvoiceStatus::Draft)
			&& let Err(e) = invoices.delete_draft(&sys, up.invoice_uid.as_str()).await
		{
			tracing::warn!(invoice = %up.invoice_uid, error = %e, "stale upgrade draft kept");
		}
	}
	Ok(())
}

/// `(offer_id, qty, price, next_offer_id, next_qty)`.
type Tier = (i64, i64, Money, Option<i64>, Option<i64>);

fn tier_of(s: &Subscription) -> Tier {
	(s.offer_id, s.qty, s.price, s.next_offer_id, s.next_qty)
}

/// The renewal applies the tier it billed only over the tier it read: a reprice or a queued
/// change that landed during the invoice and charge is kept, not reverted.
fn renewed_tier(fresh: Tier, seen: Tier, (offer_id, qty, price): (i64, i64, Money)) -> Tier {
	if fresh == seen { (offer_id, qty, price, None, None) } else { fresh }
}

/// Idempotent on `request_id = sub:{uid}:{period_start}`. TRANSFER is issued at once; CARD
/// stays a draft, which the stored recurrence (or the pay route) pays and so issues. A CARD
/// draft with nothing to pay is issued here too: no payment ever would, and [`paid`] needs it.
async fn renewal_invoice(
	plans: &Plans,
	sub: &Subscription,
	offer: &Offer,
	qty: i64,
	price: Money,
	coupon: Option<&coupon::Coupon>,
	start: Timestamp,
) -> ClResult<Invoice> {
	let sys = Ctx::system("plans").with_org(sub.org_id);
	let service = quote::service_of(plans, &sys, offer).await?;
	let mut line = Line::adhoc(
		service.name.clone(),
		service.unit.clone(),
		Qty(qty * 1_000_000),
		price,
		service.vat_code,
	);
	// A coupon edited out of shape since the sale stops discounting; it does not block renewal.
	if let Some(c) = coupon {
		line.discount = Some(c.discount);
		line.discount_description = Some(c.r.code.clone());
	}
	let req = NewDraft {
		request_id: Some(period_ref(&sub.uid, start)),
		lines: vec![line],
		// CARD is stamped by billing when the charge starts (`E-INV-METHOD-RESERVED`).
		payment_method: (sub.pay_method == PayMethod::Transfer).then_some(PaymentMethod::Transfer),
		currency: Some(sub.currency.clone()),
		..NewDraft::default()
	};
	let invoices = Invoices::new(plans.app.clone());
	let inv = match sub.pay_method {
		PayMethod::Transfer => return invoices.issue_now(&sys, &req).await,
		PayMethod::Card => invoices.draft(&sys, &req).await?,
	};
	if inv.status == InvoiceStatus::Draft && inv.gross == Money::ZERO {
		return invoices.issue(&sys, inv.uid.as_str()).await;
	}
	Ok(inv)
}

/// Charges the stored card recurrence of a CARD sub; `true` when the gateway took it. A refusal
/// falls back to the pay-link mail and, unpaid, to PAST_DUE and dunning.
// ponytail: one attempt per period (the payment's request id is the period), no retry ladder.
async fn charged(plans: &Plans, sub: &Subscription, inv: &Invoice, start: Timestamp) -> bool {
	let (PayMethod::Card, Some(token), Some(provider)) =
		(sub.pay_method, &sub.recurrence_ref, &sub.provider)
	else {
		return false;
	};
	let sys = Ctx::system("plans").with_org(sub.org_id);
	let request_id = format!("rec:{}", period_ref(&sub.uid, start));
	match saas_billing::allocate::charge_recurring(
		&plans.app,
		&sys,
		&inv.uid,
		provider,
		token,
		&request_id,
	)
	.await
	{
		// A PENDING charge still gets the mail: deduped per period, a later success sends none.
		Ok(p)
			if matches!(
				p.status,
				PaymentState::Failed | PaymentState::Canceled | PaymentState::Expired
			) =>
		{
			drop_recurrence(plans, sub, token).await;
			false
		}
		Ok(p) => p.status == PaymentState::Succeeded,
		Err(e) => {
			tracing::warn!(subscription = %sub.uid, error = %e, "recurring charge failed");
			drop_recurrence(plans, sub, token).await;
			false
		}
	}
}

/// A refused recurrence is dropped, so the next pay-link payment enrols a card afresh: the
/// recurrence hooks act only while `recurrence_ref` is `None`.
async fn drop_recurrence(plans: &Plans, sub: &Subscription, token: &str) {
	let cleared = update(plans, sub.id, |s| {
		let stale = s.recurrence_ref.as_deref() == Some(token);
		if stale {
			(s.recurrence_ref, s.provider) = (None, None);
		}
		stale
	})
	.await;
	if let Err(e) = cleared {
		tracing::warn!(subscription = %sub.uid, error = %e, "refused recurrence kept");
	}
}

/// A store error fails the renewal (the next tick retries); only an invalid coupon is `None`.
async fn coupon_of(
	plans: &Plans,
	sub: &Subscription,
	offer: &Offer,
) -> ClResult<Option<coupon::Coupon>> {
	let Some(id) = sub.coupon_ref_id else { return Ok(None) };
	let Some(r) = Refs::from_app(&plans.app)?.by_id(id).await? else { return Ok(None) };
	let Some(seller) = invoice_store(&plans.app)?.seller_for_org(sub.org_id).await? else {
		return Ok(None);
	};
	match coupon::resolve(plans, seller.org_id, &r.code, &offer.code, &sub.currency).await {
		Ok(c) => Ok(Some(c)),
		Err(Error::Coded { code, .. }) if code == quote::E_COUPON_INVALID => Ok(None),
		Err(e) => Err(e),
	}
}

/// The pay-link / transfer mail to the invoice's billing party. Deduped on the period for good,
/// as dunning's reminders are.
async fn mail(
	plans: &Plans,
	sub: &Subscription,
	offer: &Offer,
	inv: &Invoice,
	(start, end): (Timestamp, Timestamp),
) -> ClResult<()> {
	let Some(party_id) = inv.billing_party_id else { return Ok(()) };
	let party = invoice_store(&plans.app)?.party_by_id(party_id).await?;
	let Some((to, name, country)) = party.and_then(|p| Some((p.email?, p.name, p.country))) else {
		tracing::debug!(invoice = %inv.uid.as_str(), "renewal invoice, but no buyer e-mail");
		return Ok(());
	};
	let owed = inv.gross.to_wire(&inv.currency);
	let payload = serde_json::json!({
		"to": to,
		"template": TEMPLATE,
		"lang": if country == "HU" { "hu" } else { "" },
		"vars": {
			"name": name,
			"plan": offer.name,
			"period_start": saas_invoice::date_of(start)?,
			"period_end": saas_invoice::date_of(end)?,
			"invoice_number": inv.number,
			"invoice_uid": inv.uid,
			"due_date": inv.due_date,
			"amount": owed.amount,
			"currency": owed.currency,
		},
	})
	.to_string();
	let key = format!("subinv:{}:{}", sub.uid, start.0);
	job::enqueue(&plans.app.store, job::KIND_SEND_EMAIL, &payload, Some(&key), Timestamp::now())
		.await?;
	Ok(())
}

/// ACTIVE whose oldest unpaid invoice is unpaid past `plans.grace_days` → PAST_DUE; unpaid past
/// `plans.suspend_after_days` → SUSPENDED (`subs_due` skips it: renewals stop). A CARD
/// subscribe checkout still a draft past `payment.window_minutes` is canceled, freeing its family.
async fn sweep_status(plans: &Plans, now: Timestamp) -> ClResult<()> {
	let grace = plans.app.settings.int("plans.grace_days").await?;
	let suspend = plans.app.settings.int("plans.suspend_after_days").await?;
	let window = plans.app.settings.int("payment.window_minutes").await? * 60;
	let invoices = invoice_store(&plans.app)?;
	for sub in plans.store.subs_with_status(&[SubStatus::Active, SubStatus::PastDue]).await? {
		let Some(link) = plans.store.plan_invoice_latest(sub.id).await? else { continue };
		let Some(start) = link.period_start else { continue };
		let Some(inv) = invoices.invoice_by_uid(None, &link.invoice_uid).await? else { continue };
		if link.kind == LinkKind::Subscribe
			&& sub.pay_method == PayMethod::Card
			&& inv.status == InvoiceStatus::Draft
			&& now.0 >= start.0 + window
		{
			// The delete refuses while a payment is pending: that checkout may still complete.
			let sys = Ctx::system("plans").with_org(sub.org_id);
			if let Err(e) =
				Invoices::new(plans.app.clone()).delete_draft(&sys, inv.uid.as_str()).await
			{
				tracing::warn!(subscription = %sub.uid, error = %e, "abandoned checkout kept");
				continue;
			}
			subscribe::cancel_own(plans, &sub).await?;
			continue;
		}
		// The oldest unpaid link, UPGRADE included: renewals go on while PAST_DUE, so the
		// latest one's start would restart the clock every period.
		let Some(start) = plans.store.plan_invoice_oldest_unpaid(sub.id).await? else {
			continue;
		};
		let Some(start) = start.period_start else { continue };
		let to = if now.0 >= start.0 + suspend * DAY {
			SubStatus::Suspended
		} else if now.0 >= start.0 + grace * DAY {
			SubStatus::PastDue
		} else {
			continue;
		};
		if to == sub.status {
			continue;
		}
		update(plans, sub.id, |s| {
			let live = matches!(s.status, SubStatus::Active | SubStatus::PastDue);
			let change = live && s.status != to;
			if change {
				s.status = to;
			}
			change
		})
		.await?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use time::macros::datetime;

	use super::*;

	#[test]
	fn month_end_anchor_does_not_drift() {
		let offer = Offer {
			id: 0,
			uid: saas_core::ids::OfferId::generate(),
			seller_org_id: 0,
			code: "m".into(),
			name: "m".into(),
			kind: crate::store::OfferKind::Recurring,
			service_id: 0,
			family: None,
			rank: 0,
			interval: Some(Interval::Month),
			interval_count: Some(1),
			validity_days: None,
			trial_days: 0,
			active: true,
			prices: vec![],
			entitlements: vec![],
			created_at: Timestamp(0),
			updated_at: Timestamp(0),
		};
		let at = |t: OffsetDateTime| Timestamp(t.unix_timestamp());
		let anchor = at(datetime!(2024-01-31 01:00 UTC));
		let mut end = next_period_end(&offer, anchor).unwrap();
		assert_eq!(end, at(datetime!(2024-02-29 01:00 UTC)));
		for want in [
			at(datetime!(2024-03-31 01:00 UTC)),
			at(datetime!(2024-04-30 01:00 UTC)),
			at(datetime!(2024-05-31 01:00 UTC)),
		] {
			end = next_anchored_end(&offer, anchor, end).unwrap();
			assert_eq!(end, want);
		}
	}

	#[test]
	fn a_reprice_during_renewal_is_not_reverted() {
		let seen = (1, 1, Money(500_000), None, None);
		let billed = (1, 1, Money(500_000));
		let repriced = (1, 1, Money(600_000), None, None);
		assert_eq!(renewed_tier(repriced, seen, billed), repriced);
		let queued = (1, 1, Money(500_000), Some(2), Some(1));
		assert_eq!(renewed_tier(queued, seen, billed), queued);
		let down = (1, 2, Money(500_000), Some(2), Some(1));
		assert_eq!(
			renewed_tier(down, down, (2, 1, Money(300_000))),
			(2, 1, Money(300_000), None, None)
		);
	}

	#[test]
	fn months_clamp_to_the_target_month() {
		let at = |t: OffsetDateTime| Timestamp(t.unix_timestamp());
		let jan31 = at(datetime!(2024-01-31 01:00 UTC));
		assert_eq!(add_months(jan31, 1).unwrap(), at(datetime!(2024-02-29 01:00 UTC)));
		assert_eq!(add_months(jan31, 13).unwrap(), at(datetime!(2025-02-28 01:00 UTC)));
		assert_eq!(add_months(jan31, 12).unwrap(), at(datetime!(2025-01-31 01:00 UTC)));
		assert_eq!(months_between(jan31, at(datetime!(2024-03-30 23:00 UTC))).unwrap(), 1);
	}
}

// vim: ts=4
