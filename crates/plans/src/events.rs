// SPDX-License-Identifier: MPL-2.0
//! The `on_event` handler: grants a purchase's entitlements when its invoice is paid, and cuts
//! them when the payment is refunded in full. A partial refund cuts nothing. A paid subscription
//! period's grants are written (or extended) to `period_end`, and a paid CARD upgrade applies
//! its tier. It also fires the `plans.reward_on` rewards.
//!
//! Events are not durable, so a settlement is also enqueued as [`SETTLED_KIND`], a minute out:
//! the job redoes it if the inline pass failed or the process died. Every step is replayable.

use std::sync::Arc;

use async_trait::async_trait;
use mintworks_billing::provider::{RecurrenceHook, providers};
use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::event::{Event, emit};
use mintworks_core::job;
use mintworks_core::prelude::*;
use mintworks_entitle::{Entitle, GrantReq, Source};
use mintworks_invoice::invoice_store;

use crate::service::Plans;
use crate::store::{LinkKind, PayMethod, PlanInvoice, SubStatus};
use crate::subscribe::{self, period_ref};

const DAY: i64 = 86_400;

/// The settlement retry, registered by [`crate::renew::register`]. Payload
/// `{"payment": uid, "invoice": uid}`, deduped on the invoice for good.
pub const SETTLED_KIND: &str = "PLANS_SETTLED";

/// How long the job waits for the inline pass before redoing it.
const SETTLED_RETRY_SECS: i64 = 60;

/// Registered by [`crate::install`] as `.on_event(events::handle)`.
pub async fn handle(app: App, ev: Event) -> ClResult<()> {
	match &ev {
		Event::PaymentSettled { payment, invoice } => {
			let payload = serde_json::json!({ "payment": payment, "invoice": invoice }).to_string();
			let key = format!("plans:settled:{invoice}");
			let at = Timestamp(Timestamp::now().0 + SETTLED_RETRY_SECS);
			let _ = job::enqueue(&app.store, SETTLED_KIND, &payload, Some(&key), at).await?;
			let done = settled(&app, payment, invoice).await;
			// A failed settlement must not starve the rewards; the job retries both.
			let rewarded = crate::rewards::on_event(&app, &ev).await;
			return done.and(rewarded);
		}
		// ponytail: no retry for a lost refund cut; add a job when one is observed.
		Event::PaymentRefunded { invoice: Some(invoice), .. } => refunded(&app, invoice).await?,
		_ => {}
	}
	crate::rewards::on_event(&app, &ev).await
}

fn purchase_ref(invoice: &InvoiceId) -> String {
	format!("inv:{invoice}")
}

/// The [`SETTLED_KIND`] job body; `renew::settle_catch_up` re-enqueues it after a crash.
pub(crate) async fn settled_job(app: &App, payload: &str) -> ClResult<()> {
	#[derive(serde::Deserialize)]
	struct Settled {
		payment: PaymentId,
		invoice: InvoiceId,
	}
	let p: Settled = serde_json::from_str(payload)
		.map_err(|e| Error::internal(format!("{SETTLED_KIND} payload: {e}")))?;
	settled(app, &p.payment, &p.invoice).await?;
	let ev = Event::PaymentSettled { payment: p.payment, invoice: p.invoice };
	crate::rewards::on_event(app, &ev).await
}

/// Idempotent: a grant is unique on `(org, key, source, source_ref)`, extensions only move
/// later, and each status write is a guarded CAS.
async fn settled(app: &App, payment: &PaymentId, invoice: &InvoiceId) -> ClResult<()> {
	let plans = Plans::from_app(app)?;
	let Some(link) = plans.store.plan_invoice_get(invoice).await? else {
		return Ok(());
	};
	// A coupon's use is held by the draft until here, so an abandoned checkout releases it.
	mintworks_core::refs::Refs::from_app(app)?.settle(link.invoice_id).await?;
	match link.kind {
		LinkKind::Purchase => {}
		LinkKind::Subscribe => {
			// Recurrence first: a caller that waits on the extended grants may renew straight away.
			store_recurrence(&plans, &link, payment).await?;
			return period_paid(&plans, &link).await;
		}
		LinkKind::Renewal => {
			// A CARD trial's first payment is its first renewal: it starts the recurrence.
			store_recurrence(&plans, &link, payment).await?;
			return period_paid(&plans, &link).await;
		}
		LinkKind::Upgrade => return upgrade_paid(&plans, &link).await,
	}
	grant_purchase(app, &link, invoice).await
}

/// A purchase's grants, from settlement or (nothing to pay) from checkout itself.
pub(crate) async fn grant_purchase(
	app: &App,
	link: &PlanInvoice,
	invoice: &InvoiceId,
) -> ClResult<()> {
	let plans = Plans::from_app(app)?;
	let offer = plans.store.offer_get(link.offer_id).await?.ok_or(Error::NotFound)?;
	let ctx = Ctx::system("plans").with_org(link.org_id);
	let now = Timestamp::now();
	let valid_until = offer.validity_days.map(|d| Timestamp(now.0 + d * DAY));
	let entitle = Entitle::from_app(app)?;
	// The offer's current entitlements, not the ones at sale time.
	for e in &offer.entitlements {
		let amount = if e.per_seat { e.amount.checked_mul(link.qty) } else { Some(e.amount) }
			.ok_or_else(|| Error::validation("entitlement amount out of range"))?;
		entitle
			.grant_to(
				&ctx,
				link.org_id,
				&GrantReq {
					key: e.key.clone(),
					amount,
					valid_from: Some(now),
					valid_until,
					source: Source::Purchase,
					source_ref: Some(purchase_ref(invoice)),
				},
			)
			.await?;
	}
	Ok(())
}

/// Grants written inline at issue are provisional; this writes them when missing (a CARD
/// SUBSCRIBE is issued inside its own settlement) and extends them all to `period_end`. A
/// CANCELED sub's grants were cut on purpose: a late or replayed settlement leaves them be.
async fn period_paid(plans: &Plans, link: &PlanInvoice) -> ClResult<()> {
	let (Some(sub_id), Some(start), Some(end)) =
		(link.subscription_id, link.period_start, link.period_end)
	else {
		return Ok(());
	};
	let sub = plans.store.sub_get(sub_id).await?.ok_or(Error::NotFound)?;
	if sub.status == SubStatus::Canceled {
		return Ok(());
	}
	let offer = plans.store.offer_get(link.offer_id).await?.ok_or(Error::NotFound)?;
	let r = period_ref(&sub.uid, start);
	let (org, src) = (link.org_id, Source::Subscription);
	let tier = (&offer, link.qty);
	subscribe::grant_period(&plans.app, org, tier, None, true, src, &r, start, Some(end)).await?;
	let ctx = Ctx::system("plans").with_org(link.org_id);
	Entitle::from_app(&plans.app)?
		.extend_to(&ctx, link.org_id, Source::Subscription, &r, Some(end))
		.await?;
	// Only a payment that leaves nothing unpaid brings a PAST_DUE or SUSPENDED sub back: the
	// dunning sweep reads the same oldest-unpaid link and would demote it again.
	if plans.store.plan_invoice_oldest_unpaid(sub_id).await?.is_none() {
		let now = Timestamp::now();
		let restart = (now, crate::renew::next_period_end(&offer, now)?);
		let saved = subscribe::update(plans, sub_id, |s| {
			let back = matches!(s.status, SubStatus::PastDue | SubStatus::Suspended);
			// A resume after the period ended restarts it now: the gap is never billed.
			if s.status == SubStatus::Suspended && s.period_end <= now {
				(s.period_start, s.period_end) = restart;
				s.billing_anchor = now;
			}
			if back {
				s.status = SubStatus::Active;
			}
			back
		})
		.await?;
		// Re-keyed so the sub, its grants and this link name one window: `cancel_now` and the
		// refund find the restarted period by the sub's `period_start`.
		if (saved.period_start, saved.period_end) == restart && start != restart.0 {
			let new_ref = period_ref(&sub.uid, restart.0);
			let until = Some(restart.1);
			subscribe::grant_period(
				&plans.app, org, tier, None, true, src, &new_ref, restart.0, until,
			)
			.await?;
			plans
				.store
				.plan_invoice_set_period(link.invoice_id, restart.0, restart.1)
				.await?;
			Entitle::from_app(&plans.app)?.cut_to(&ctx, org, src, &r, restart.0).await?;
		}
	}
	Ok(())
}

/// A paid upgrade's grants (written here for CARD) run to the link's `period_end`. A CARD
/// upgrade's tier applies here, and only while the sub still holds the tier it replaced: a
/// replay finds it applied, and a sub that moved on (renewed, canceled) is left alone.
async fn upgrade_paid(plans: &Plans, link: &PlanInvoice) -> ClResult<()> {
	let (Some(sub_id), Some(start), Some(end)) =
		(link.subscription_id, link.period_start, link.period_end)
	else {
		return Ok(());
	};
	let sub = plans.store.sub_get(sub_id).await?.ok_or(Error::NotFound)?;
	if sub.status == SubStatus::Canceled {
		return Ok(());
	}
	let inv = invoice_store(&plans.app)?
		.invoice_by_uid(None, &link.invoice_uid)
		.await?
		.ok_or(Error::NotFound)?;
	// TRANSFER applied the tier at checkout. The request id, not `payment_method`: an unpaid
	// CARD draft carries the TRANSFER default, and the sub's tier may have moved on since.
	let suffix = format!(":{}", PayMethod::Transfer.as_str());
	let applied = inv.request_id.is_some_and(|r| r.ends_with(&suffix));
	// Stale before any grant: a sub that renewed or moved off `prev` must not get this tier.
	if sub.period_end != end || (link.prev != Some((sub.offer_id, sub.qty)) && !applied) {
		// The money is taken with no tier change. Surfaced, not refunded:
		// `mintworks_billing::refund` needs an operator with step-up.
		tracing::warn!(subscription = %sub.uid, invoice = %link.invoice_uid,
			"an upgrade was paid after its period renewed or its tier moved on; refund it");
		let ctx = Ctx::system("plans").with_org(link.org_id);
		let detail = serde_json::json!({ "subscription": sub.uid });
		mintworks_core::audit::log(
			&plans.app.store,
			&ctx,
			"invoice",
			Some(link.invoice_uid.as_str()),
			"UPGRADE_PAID_STALE",
			Some(detail),
		)
		.await;
		return Ok(());
	}
	// Grants before the tier: a crash between them replays with the sub still on `prev`.
	crate::checkout::grant_upgrade(plans, link, &sub.uid, true, start, end).await?;
	let r = crate::checkout::upgrade_ref(&sub.uid, &link.invoice_uid);
	let ctx = Ctx::system("plans").with_org(link.org_id);
	Entitle::from_app(&plans.app)?
		.extend_to(&ctx, link.org_id, Source::Subscription, &r, Some(end))
		.await?;
	if applied {
		return Ok(());
	}
	let offer = plans.store.offer_get(link.offer_id).await?.ok_or(Error::NotFound)?;
	let price = crate::renew::seat_price(&offer, &sub.currency)?;
	let (prev, tier) = (link.prev, (link.offer_id, link.qty));
	let saved = subscribe::update(plans, sub_id, |s| {
		let apply = s.status != SubStatus::Canceled && prev == Some((s.offer_id, s.qty));
		if apply {
			(s.offer_id, s.qty, s.price) = (tier.0, tier.1, price);
			(s.next_offer_id, s.next_qty) = (None, None);
		}
		apply
	})
	.await?;
	if (saved.offer_id, saved.qty) == tier {
		emit(&plans.app, Event::SubscriptionTierChanged { subscription: saved.uid });
	}
	Ok(())
}

/// Answers the sub uid for a SUBSCRIBE or RENEWAL invoice of a CARD sub with no recurrence yet,
/// so its first paid invoice initiates the card recurrence renewals charge. The app registers
/// it as `Arc<dyn RecurrenceHook>`.
pub struct Recurrence;

#[async_trait]
impl RecurrenceHook for Recurrence {
	async fn recurrence_for(&self, app: &App, invoice: &InvoiceId) -> ClResult<Option<String>> {
		let plans = Plans::from_app(app)?;
		let Some(link) = plans.store.plan_invoice_get(invoice).await? else { return Ok(None) };
		let Some(sub_id) = link
			.subscription_id
			.filter(|_| matches!(link.kind, LinkKind::Subscribe | LinkKind::Renewal))
		else {
			return Ok(None);
		};
		let sub = plans.store.sub_get(sub_id).await?.ok_or(Error::NotFound)?;
		let wants = sub.pay_method == PayMethod::Card && sub.recurrence_ref.is_none();
		// Fresh per enrolment: a gateway may refuse a reused id. `store_recurrence` stores the same.
		Ok(wants.then(|| format!("{}:{}", sub.uid, invoice)))
	}
}

/// After a CARD SUBSCRIBE settles through a recurring-capable gateway, the recurrence
/// [`Recurrence`] asked for exists there: record it, so renewals charge it.
async fn store_recurrence(plans: &Plans, link: &PlanInvoice, payment: &PaymentId) -> ClResult<()> {
	let app = &plans.app;
	if app.extensions.get::<Arc<dyn RecurrenceHook>>().is_none() {
		return Ok(());
	}
	let Some(sub_id) = link.subscription_id else { return Ok(()) };
	let pay = mintworks_billing::store::store(app)?.payment_by_uid(None, payment).await?;
	let Some(provider) = pay.and_then(|p| p.provider) else { return Ok(()) };
	if !providers(app)?.get(&provider).is_some_and(|p| p.capabilities().recurring) {
		return Ok(());
	}
	subscribe::update(plans, sub_id, |s| {
		let set = s.pay_method == PayMethod::Card && s.recurrence_ref.is_none();
		if set {
			let rec = format!("{}:{}", s.uid, link.invoice_uid);
			(s.recurrence_ref, s.provider) = (Some(rec), Some(provider.clone()));
		}
		set
	})
	.await?;
	Ok(())
}

async fn refunded(app: &App, invoice: &InvoiceId) -> ClResult<()> {
	let plans = Plans::from_app(app)?;
	let Some(link) = plans.store.plan_invoice_get(invoice).await? else {
		return Ok(());
	};
	if link.kind != LinkKind::Purchase {
		return Ok(());
	}
	let inv = invoice_store(app)?
		.invoice_by_uid(None, invoice)
		.await?
		.ok_or(Error::NotFound)?;
	if inv.paid_amount != Money::ZERO {
		return Ok(());
	}
	let ctx = Ctx::system("plans").with_org(link.org_id);
	Entitle::from_app(app)?
		.cut_to(&ctx, link.org_id, Source::Purchase, &purchase_ref(invoice), Timestamp::now())
		.await?;
	Ok(())
}

// vim: ts=4
