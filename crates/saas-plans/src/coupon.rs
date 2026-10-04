//! Coupons: a `coupon` ref whose params `{offers: [code]|null, discount: {percentBp} | {amount,
//! currency}, periods: n|null}` become the line `Discount` — never a negative line. Every
//! refusal is the same `E-PLAN-COUPON-INVALID`, with no reason, like the ref preview.

use saas_core::ctx::Ctx;
use saas_core::error::StatusCode;
use saas_core::prelude::*;
use saas_core::refs::{Ref, Refs};
use saas_invoice::Discount;
use serde::Deserialize;

use crate::quote::E_COUPON_INVALID;
use crate::service::Plans;

pub const REF_TYPE: &str = "coupon";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Params {
	#[serde(default)]
	offers: Option<Vec<String>>,
	discount: DiscountParam,
	#[serde(default)]
	periods: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged, rename_all = "camelCase")]
enum DiscountParam {
	#[serde(rename_all = "camelCase")]
	Percent { percent_bp: u32 },
	/// Minor units, off the line total.
	Amount { amount: i64, currency: String },
}

/// A coupon that applies to one offer in one currency.
#[derive(Clone, Debug)]
pub(crate) struct Coupon {
	pub r: Ref,
	pub discount: Discount,
	/// Renewal periods the discount lasts; `None` = every period.
	pub periods: Option<i64>,
}

pub(crate) fn invalid() -> Error {
	Error::coded(StatusCode::UNPROCESSABLE_ENTITY, E_COUPON_INVALID, "invalid coupon")
}

/// Shape, owner, offer and currency only. Redeemability is [`usable`]'s: a replayed checkout
/// re-prices after its own redeem may have exhausted the ref.
pub(crate) async fn resolve(
	plans: &Plans,
	seller_org: i64,
	code: &str,
	offer_code: &str,
	currency: &CurrencyCode,
) -> ClResult<Coupon> {
	let r = Refs::from_app(&plans.app)?.by_code(code.trim()).await?.ok_or_else(invalid)?;
	// Any org admin mints refs of any open type: only the seller's own coupons discount.
	if r.ref_type != REF_TYPE || r.org_id != seller_org {
		return Err(invalid());
	}
	let p: Params = serde_json::from_value(r.params.clone()).map_err(|_| invalid())?;
	if p.offers.as_ref().is_some_and(|os| !os.iter().any(|o| o == offer_code)) {
		return Err(invalid());
	}
	let discount = match p.discount {
		DiscountParam::Percent { percent_bp } if (1..=10_000).contains(&percent_bp) => {
			Discount::Percent(percent_bp)
		}
		DiscountParam::Amount { amount, currency: c } if amount > 0 && c == currency.as_str() => {
			Discount::Amount(Money(amount))
		}
		_ => return Err(invalid()),
	};
	if p.periods.is_some_and(|n| n < 1) {
		return Err(invalid());
	}
	Ok(Coupon { r, discount, periods: p.periods })
}

/// Redeemable now, by this account, and no member of `org_id` has used it yet. The org check
/// here is the quote's preview: `RefStore::ref_redeem` repeats it under the write lock.
pub(crate) async fn usable(plans: &Plans, ctx: &Ctx, c: &Coupon, org_id: i64) -> ClResult<()> {
	if !c.r.is_redeemable(Timestamp::now()) || !email_matches(plans, ctx, &c.r).await? {
		return Err(invalid());
	}
	let uses = Refs::from_app(&plans.app)?.uses_of_org(org_id).await?;
	if uses.iter().any(|u| u.ref_id == c.r.id) {
		return Err(invalid());
	}
	Ok(())
}

/// [`usable`], then `(ref_id, account_id, org_id)` for `PlanStore::plan_invoice_link` to
/// redeem with the link; `None` when the coupon is not usable.
pub(crate) async fn redeem_args(
	plans: &Plans,
	ctx: &Ctx,
	c: &Coupon,
	org_id: i64,
) -> ClResult<Option<(i64, i64, i64)>> {
	match usable(plans, ctx, c, org_id).await {
		Ok(()) => Ok(ctx.actor.account_id().map(|a| (c.r.id, a, org_id))),
		Err(Error::Coded { code: E_COUPON_INVALID, .. }) => Ok(None),
		Err(e) => Err(e),
	}
}

/// [`usable`], then the use row. `Refs::redeem` refusing (a race on the last use) is invalid
/// too, and so is a use this call did not insert: this account or another member of `org_id`
/// spent it before, possibly in a concurrent checkout.
pub(crate) async fn redeem(plans: &Plans, ctx: &Ctx, c: &Coupon, org_id: i64) -> ClResult<()> {
	usable(plans, ctx, c, org_id).await?;
	let account = ctx.actor.account_id().ok_or_else(invalid)?;
	match Refs::from_app(&plans.app)?.redeem(ctx, c.r.id, account, org_id).await? {
		Some((_, true)) => Ok(()),
		_ => Err(invalid()),
	}
}

async fn email_matches(plans: &Plans, ctx: &Ctx, r: &Ref) -> ClResult<bool> {
	let Some(email) = &r.email else {
		return Ok(true);
	};
	let Some(account) = ctx.actor.account_id() else {
		return Ok(false);
	};
	let store = saas_auth::routes::store(&plans.app)?;
	Ok(store
		.account_by_id(account)
		.await?
		.is_some_and(|a| a.email.eq_ignore_ascii_case(email)))
}

// vim: ts=4
