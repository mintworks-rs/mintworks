//! Rewards: an offer's entitlements granted free (`source REWARD`) for a ref use, to either
//! side. The offer comes from a root-owned ref's `params.reward.{inviter,invitee}`, else the
//! setting `plans.reward.<ref_type>.<side>`; blank means no reward.

use saas_core::app::App;
use saas_core::auth_mw::require_operator;
use saas_core::ctx::Ctx;
use saas_core::event::Event;
use saas_core::ids::RefId;
use saas_core::prelude::*;
use saas_core::refs::{RefUse, Refs};
use saas_entitle::{Entitle, GrantReq, Source};
use serde::{Deserialize, Serialize};

use crate::service::Plans;
use crate::store::{Interval, Offer};

const DAY: i64 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
	/// The ref's owner org (`refs.org_id`).
	Inviter,
	/// The redeemer's org (`ref_uses.org_id`).
	Invitee,
}

impl Side {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Inviter => "inviter",
			Self::Invitee => "invitee",
		}
	}
}

/// The use of ref `ref_uid` by account `account_uid`.
async fn use_of(plans: &Plans, ref_uid: &str, account_uid: &str) -> ClResult<RefUse> {
	let refs = Refs::from_app(&plans.app)?;
	let r = refs.by_uid(&RefId::parse(ref_uid)?).await?.ok_or(Error::NotFound)?;
	let auth = saas_auth::routes::store(&plans.app)?;
	let account = auth
		.account_by_uid(&AccountId::parse(account_uid)?)
		.await?
		.ok_or(Error::NotFound)?;
	refs.use_of(r.id, account.id).await?.ok_or(Error::NotFound)
}

/// Operator or system: rewards the use of `ref_uid` by `account_uid`. `Ok(false)` when no
/// reward offer is configured for that side.
pub(crate) async fn reward_ref_use(
	plans: &Plans,
	ctx: &Ctx,
	ref_uid: &str,
	account_uid: &str,
	side: Side,
) -> ClResult<bool> {
	require_operator(&plans.app, ctx).await?;
	let used = use_of(plans, ref_uid, account_uid).await?;
	reward_use(plans, &used, side).await
}

/// Idempotent on `source_ref = "ruse:{id}:{side}"`.
async fn reward_use(plans: &Plans, used: &RefUse, side: Side) -> ClResult<bool> {
	let r = Refs::from_app(&plans.app)?.by_id(used.ref_id).await?.ok_or(Error::NotFound)?;
	// A coupon or invite earns nothing, and nobody is rewarded for referring themselves.
	if !matches!(r.ref_type.as_str(), "signup" | "affiliate")
		|| used.org_id == r.org_id
		|| r.created_by == Some(used.account_id)
	{
		return Ok(false);
	}
	// Any org admin mints refs: only the operator's own may name the reward offer.
	let root = plans.app.store.root_org_id().await?;
	let from_params = r
		.params
		.pointer(&format!("/reward/{}", side.as_str()))
		.and_then(|v| v.as_str())
		.filter(|_| r.org_id == root);
	let code = match from_params {
		Some(c) => c.to_owned(),
		None => {
			plans
				.app
				.settings
				.text(&format!("plans.reward.{}.{}", r.ref_type, side.as_str()))
				.await?
		}
	};
	let code = code.trim();
	if code.is_empty() {
		return Ok(false);
	}
	let org_id = match side {
		Side::Inviter => r.org_id,
		Side::Invitee => used.org_id,
	};
	let offer = declared_offer(plans, org_id, code).await?;
	grant(plans, org_id, &offer, &format!("ruse:{}:{}", used.id, side.as_str())).await?;
	Ok(true)
}

/// Operator or system: `offer_code`'s entitlements free to the org `org_uid`, idempotent on
/// `idem` (the grants' `source_ref`).
pub(crate) async fn reward(
	plans: &Plans,
	ctx: &Ctx,
	org_uid: &str,
	offer_code: &str,
	idem: &str,
) -> ClResult<()> {
	require_operator(&plans.app, ctx).await?;
	let org_id = plans.store.plan_org_id(&OrgId::parse(org_uid)?).await?.ok_or(Error::NotFound)?;
	let offer = declared_offer(plans, org_id, offer_code).await?;
	grant(plans, org_id, &offer, idem).await
}

/// The recipient's seller's offer, as a quote resolves it; an inactive one still rewards.
async fn declared_offer(plans: &Plans, org_id: i64, code: &str) -> ClResult<Offer> {
	let seller = saas_invoice::invoice_store(&plans.app)?
		.seller_for_org(org_id)
		.await?
		.ok_or(Error::NotFound)?;
	plans
		.store
		.offer_by_code(seller.org_id, code)
		.await?
		.ok_or_else(|| Error::validation(format!("reward offer '{code}' is not in the catalogue")))
}

async fn grant(plans: &Plans, org_id: i64, offer: &Offer, source_ref: &str) -> ClResult<()> {
	let now = Timestamp::now();
	// ponytail: one interval is 30/365 days, not a calendar month; renewals own calendar math.
	let days = offer.validity_days.or_else(|| {
		let per = match offer.interval? {
			Interval::Month => 30,
			Interval::Year => 365,
		};
		Some(per * offer.interval_count.unwrap_or(1))
	});
	let ctx = Ctx::system("plans").with_org(org_id);
	let entitle = Entitle::from_app(&plans.app)?;
	for e in &offer.entitlements {
		entitle
			.grant_to(
				&ctx,
				org_id,
				&GrantReq {
					key: e.key.clone(),
					amount: e.amount,
					valid_from: Some(now),
					valid_until: days.map(|d| Timestamp(now.0 + d * DAY)),
					source: Source::Reward,
					source_ref: Some(source_ref.to_owned()),
				},
			)
			.await?;
	}
	Ok(())
}

/// The `plans.reward_on` auto-trigger, from [`crate::events::handle`]. `first_payment` fires
/// on every settle of a plan invoice with a positive gross; the grants' idempotency makes all
/// but the first a no-op.
pub(crate) async fn on_event(app: &App, ev: &Event) -> ClResult<()> {
	let on = app.settings.text("plans.reward_on").await?;
	let uses = match (on.as_str(), ev) {
		("first_payment", Event::PaymentSettled { invoice, .. }) => {
			let plans = Plans::from_app(app)?;
			let Some(link) = plans.store.plan_invoice_get(invoice).await? else {
				return Ok(());
			};
			// A free or fully discounted invoice is no payment: burner orgs would farm it.
			let inv = saas_invoice::invoice_store(app)?.invoice_by_uid(None, invoice).await?;
			if inv.is_none_or(|i| i.gross <= Money::ZERO) {
				return Ok(());
			}
			Refs::from_app(app)?.uses_of_org(link.org_id).await?
		}
		_ => return Ok(()),
	};
	let plans = Plans::from_app(app)?;
	for used in &uses {
		for side in [Side::Inviter, Side::Invitee] {
			// One bad reward offer must not starve the other uses of theirs.
			if let Err(e) = reward_use(&plans, used, side).await {
				tracing::warn!(ref_use = used.id, side = side.as_str(), error = %e, "reward failed");
			}
		}
	}
	Ok(())
}

// vim: ts=4
