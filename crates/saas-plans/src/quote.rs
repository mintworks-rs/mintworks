//! Quote: prices an offer for the acting org and signs the result as a stateless token that
//! checkout re-derives. No quotes table: the token carries everything, as activation does.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use saas_core::app::App;
use saas_core::crypto::{ct_eq, hmac_hex};
use saas_core::ctx::Ctx;
use saas_core::error::StatusCode;
use saas_core::ids::SubscriptionId;
use saas_core::prelude::*;
use saas_invoice::store::InvoiceStatus;
use saas_invoice::{ComputedInvoice, Currency, DraftLine, Invoices, Service, invoice_store};
use serde::{Deserialize, Serialize};

use crate::coupon::{self, Coupon};
use crate::renew;
use crate::service::Plans;
use crate::store::{Offer, OfferKind, SubStatus, Subscription};
use crate::subscribe::{self, DAY};

pub const E_NO_PRICE: &str = "E-PLAN-NO-PRICE";
pub const E_QUOTE_EXPIRED: &str = "E-PLAN-QUOTE-EXPIRED";
pub const E_QUOTE_STALE: &str = "E-PLAN-QUOTE-STALE";
pub const E_COUPON_INVALID: &str = "E-PLAN-COUPON-INVALID";
pub const E_FAMILY_MISMATCH: &str = "E-PLAN-FAMILY-MISMATCH";
pub const E_CHANGE_INVALID: &str = "E-PLAN-CHANGE-INVALID";

const KEY_NAME: &str = "plans.quote_key";
const TTL_SECONDS: i64 = 900;
const MAX_QTY: i64 = 10_000;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteReq {
	pub offer: String,
	pub qty: Option<i64>,
	pub currency: Option<String>,
	pub coupon: Option<String>,
	pub subscription: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteLine {
	pub description: String,
	pub unit: String,
	pub qty: i64,
	pub unit_price: MoneyWire,
	pub net: MoneyWire,
	pub vat: MoneyWire,
	pub gross: MoneyWire,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
	pub lines: Vec<QuoteLine>,
	pub net: MoneyWire,
	pub vat: MoneyWire,
	pub gross: MoneyWire,
	pub currency: CurrencyCode,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub period_start: Option<Timestamp>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub period_end: Option<Timestamp>,
	/// `now`, or `period_end` for a change that waits for the period to end (commerce-4).
	pub effective: &'static str,
	pub quote_token: String,
}

/// What the token signs. Checkout rebuilds it from current state and compares field by field,
/// so a price, currency or offer change since the quote is `E-PLAN-QUOTE-STALE`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Claims {
	pub org: i64,
	/// `purchase` (ONE_TIME), `subscribe` (RECURRING), `upgrade` or `downgrade`.
	pub action: String,
	pub offer: String,
	pub qty: i64,
	pub currency: String,
	pub coupon: Option<String>,
	/// The quote promised a trial: checkout refuses with `E-PLAN-TRIAL-USED` once it no longer
	/// holds, unless the live trial is this token's own replay.
	#[serde(default)]
	pub trial: bool,
	/// Minor units.
	pub gross: i64,
	pub exp: i64,
	/// A tier change: the sub's uid and `updated_at`, so any write to it since is stale.
	#[serde(default)]
	pub subscription: Option<String>,
	#[serde(default)]
	pub sub_updated: i64,
	/// The quote's instant: the proration is fixed at it, so checkout re-derives the same amount.
	#[serde(default)]
	pub at: i64,
}

/// An offer priced for one org: what both the quote and the checkout draft are built from.
pub(crate) struct Priced {
	pub offer: Offer,
	pub currency: Currency,
	pub qty: i64,
	pub unit_price: Money,
	pub line: DraftLine,
	pub computed: ComputedInvoice,
	pub coupon: Option<Coupon>,
}

impl Priced {
	pub fn claims(&self, org: i64, coupon: Option<String>, trial: bool, exp: i64) -> Claims {
		Claims {
			org,
			action: match self.offer.kind {
				OfferKind::OneTime => "purchase",
				OfferKind::Recurring => "subscribe",
			}
			.to_owned(),
			offer: self.offer.code.clone(),
			qty: self.qty,
			currency: self.currency.code.to_string(),
			coupon,
			trial,
			gross: self.computed.gross.0,
			exp,
			subscription: None,
			sub_updated: 0,
			at: 0,
		}
	}
}

/// Prices `offer_code × qty` in the org's (or the asked) currency. The seller is the buyer
/// org's resolved seller, so an offer of another seller reads as absent.
pub(crate) async fn price(
	plans: &Plans,
	ctx: &Ctx,
	offer_code: &str,
	qty: Option<i64>,
	currency: Option<&str>,
	coupon: Option<&str>,
) -> ClResult<Priced> {
	let org = ctx.org()?;
	// The buyer's own role cannot read the seller's catalogue or draft on it.
	let sys = ctx.clone().as_system("plans");
	let qty = qty.unwrap_or(1);
	if !(1..=MAX_QTY).contains(&qty) {
		return Err(Error::validation(format!("qty must be 1..={MAX_QTY}")));
	}
	let seller = invoice_store(&plans.app)?.seller_for_org(org).await?.ok_or(Error::NotFound)?;
	let offer = plans
		.store
		.offer_by_code(seller.org_id, offer_code)
		.await?
		.filter(|o| o.active)
		.ok_or(Error::NotFound)?;
	let invoices = Invoices::new(plans.app.clone());
	let asked = currency.map(CurrencyCode::parse).transpose()?;
	let currency = invoices.currency(&sys, asked.as_ref()).await?;
	let unit_price = offer
		.prices
		.iter()
		.find(|p| p.currency == currency.code)
		.map(|p| p.amount)
		.ok_or_else(|| {
			Error::coded(
				StatusCode::UNPROCESSABLE_ENTITY,
				E_NO_PRICE,
				format!("offer '{}' has no {} price", offer.code, currency.code),
			)
		})?;
	let coupon = match coupon {
		Some(code) => {
			Some(coupon::resolve(plans, seller.org_id, code, &offer.code, &currency.code).await?)
		}
		None => None,
	};
	let service = service_of(plans, &sys, &offer).await?;
	let line = DraftLine {
		service_id: Some(service.id),
		description: service.name.clone(),
		unit: service.unit.clone(),
		qty: Qty(qty * 1_000_000),
		unit_price,
		vat_code: service.vat_code,
		discount: coupon.as_ref().map(|c| c.discount),
		discount_description: coupon.as_ref().map(|c| c.r.code.clone()),
		note: None,
	};
	let computed =
		saas_invoice::compute(std::slice::from_ref(&line), None, currency.price_round_step)?;
	Ok(Priced { offer, currency, qty, unit_price, line, computed, coupon })
}

/// The offer's `services` row, read as `sys` (the buyer's role cannot see the seller's catalogue).
pub(crate) async fn service_of(plans: &Plans, sys: &Ctx, offer: &Offer) -> ClResult<Service> {
	Invoices::new(plans.app.clone())
		.list_services(sys, false)
		.await?
		.into_iter()
		.find(|s| s.id == offer.service_id)
		.ok_or_else(|| Error::internal(format!("offer '{}': service row missing", offer.code)))
}

pub(crate) async fn quote(plans: &Plans, ctx: &Ctx, req: &QuoteReq) -> ClResult<Quote> {
	if let Some(uid) = &req.subscription {
		return change_quote(plans, ctx, req, uid).await;
	}
	let p = price(plans, ctx, &req.offer, req.qty, req.currency.as_deref(), req.coupon.as_deref())
		.await?;
	if let Some(c) = &p.coupon {
		coupon::usable(plans, ctx, c, ctx.org()?).await?;
	}
	let org = ctx.org()?;
	let trial = subscribe::trial_eligible(plans, org, &p.offer).await?;
	let now = Timestamp::now();
	let period_end = match (p.offer.kind, trial) {
		(OfferKind::OneTime, _) => None,
		(OfferKind::Recurring, true) => Some(Timestamp(now.0 + p.offer.trial_days * DAY)),
		(OfferKind::Recurring, false) => Some(renew::next_period_end(&p.offer, now)?),
	};
	let claims = p.claims(org, req.coupon.clone(), trial, now.0 + TTL_SECONDS);
	let code = &p.currency.code;
	// A trial charges nothing now; the line still shows what the first renewal will cost.
	let total = |m: Money| if trial { Money::ZERO.to_wire(code) } else { m.to_wire(code) };
	Ok(Quote {
		lines: vec![line(&p)?],
		net: total(p.computed.net),
		vat: total(p.computed.vat),
		gross: total(p.computed.gross),
		currency: code.clone(),
		period_start: period_end.map(|_| now),
		period_end,
		effective: "now",
		quote_token: mint(&plans.app, &claims).await?,
	})
}

fn line(p: &Priced) -> ClResult<QuoteLine> {
	let code = &p.currency.code;
	let cl = p.computed.lines.first().ok_or_else(|| Error::internal("quote: no line"))?;
	Ok(QuoteLine {
		description: p.line.description.clone(),
		unit: p.line.unit.clone(),
		qty: p.line.qty.0 / 1_000_000,
		unit_price: p.unit_price.to_wire(code),
		net: cl.net.to_wire(code),
		vat: cl.vat.to_wire(code),
		gross: cl.gross.to_wire(code),
	})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeKind {
	/// Invoiced for the rest of the period and applied now.
	Upgrade,
	/// Free, queued in `next_offer_id`/`next_qty` for `renew_due` at `period_end`.
	Downgrade,
}

pub(crate) struct Change {
	pub kind: ChangeKind,
	pub sub: Subscription,
	/// The target offer's current per-seat price, which the sub carries from an upgrade on.
	pub seat_price: Money,
}

impl Change {
	fn stamp(&self, claims: &mut Claims, at: Timestamp) {
		match self.kind {
			ChangeKind::Upgrade => "upgrade",
			ChangeKind::Downgrade => "downgrade",
		}
		.clone_into(&mut claims.action);
		claims.subscription = Some(self.sub.uid.to_string());
		claims.sub_updated = self.sub.updated_at.0;
		claims.at = at.0;
	}
}

fn change_invalid(msg: &str) -> Error {
	Error::coded(StatusCode::UNPROCESSABLE_ENTITY, E_CHANGE_INVALID, msg.to_owned())
}

/// The acting org's sub by uid; another org's is `NotFound`.
pub(crate) async fn own_sub(plans: &Plans, ctx: &Ctx, uid: &str) -> ClResult<Subscription> {
	let sub = plans
		.store
		.sub_by_uid(&SubscriptionId::parse(uid)?)
		.await?
		.ok_or(Error::NotFound)?;
	if sub.org_id != ctx.org()? {
		return Err(Error::NotFound);
	}
	Ok(sub)
}

/// Classifies and prices moving `sub` to `offer_code × qty` as of `at`. Only within a family:
/// rank up (or more seats of the same offer) is an upgrade, rank down (or fewer seats) a
/// downgrade. An upgrade's [`Priced`] is its single prorated line (qty 1).
pub(crate) async fn change(
	plans: &Plans,
	ctx: &Ctx,
	sub: Subscription,
	offer_code: &str,
	qty: Option<i64>,
	at: Timestamp,
) -> ClResult<(Change, Priced)> {
	use std::cmp::Ordering::{Equal, Greater, Less};
	if sub.status != SubStatus::Active {
		return Err(change_invalid("only an ACTIVE subscription can change tier"));
	}
	let currency = sub.currency.to_string();
	let qty = qty.or(Some(sub.qty));
	let mut p = price(plans, ctx, offer_code, qty, Some(&currency), None).await?;
	let cur = plans.store.offer_get(sub.offer_id).await?.ok_or(Error::NotFound)?;
	let kind = if p.offer.id == cur.id {
		match p.qty.cmp(&sub.qty) {
			Greater => ChangeKind::Upgrade,
			Less => ChangeKind::Downgrade,
			Equal => return Err(change_invalid("the subscription already has this offer and qty")),
		}
	} else {
		if p.offer.kind != OfferKind::Recurring
			|| sub.family.is_none()
			|| p.offer.family != sub.family
		{
			return Err(Error::coded(
				StatusCode::UNPROCESSABLE_ENTITY,
				E_FAMILY_MISMATCH,
				"a change across families is a new subscription",
			));
		}
		match p.offer.rank.cmp(&cur.rank) {
			Greater => ChangeKind::Upgrade,
			Less => ChangeKind::Downgrade,
			Equal => return Err(change_invalid("the offers have the same rank")),
		}
	};
	let seat_price = p.unit_price;
	if kind == ChangeKind::Upgrade {
		let range = || Error::validation("amount out of range");
		let old = sub.price.0.checked_mul(sub.qty).ok_or_else(range)?;
		let new = seat_price.0.checked_mul(p.qty).ok_or_else(range)?;
		let amount = prorate(
			new - old,
			sub.period_end.0 - at.0,
			sub.period_end.0 - sub.period_start.0,
			p.currency.price_round_step,
		)?;
		// Rank up with fewer seats can cost nothing; money never flows back through an invoice.
		if amount <= 0 {
			return Err(change_invalid("the change costs nothing now; it is not an upgrade"));
		}
		let day = |t: Timestamp| {
			t.to_rfc3339().and_then(|s| s.get(..10).map(str::to_owned)).unwrap_or_default()
		};
		p.line.description =
			format!("{} ×{}, {} – {}", p.line.description, p.qty, day(at), day(sub.period_end));
		p.line.qty = Qty(1_000_000);
		p.line.unit_price = Money(amount);
		p.unit_price = Money(amount);
		p.computed = saas_invoice::compute(
			std::slice::from_ref(&p.line),
			None,
			p.currency.price_round_step,
		)?;
	}
	Ok((Change { kind, sub, seat_price }, p))
}

/// `max(0, diff) × remaining / period`, rounded half-up once, to the currency's
/// `price_round_step` (an ad-hoc unit price off the step is `E-INV-LINE`).
fn prorate(diff: i64, remaining: i64, period: i64, step: i64) -> ClResult<i64> {
	let n = i128::from(diff.max(0)) * i128::from(remaining.max(0));
	let d = i128::from(period.max(1)) * i128::from(step.max(1));
	let steps = (2 * n + d) / (2 * d);
	i64::try_from(steps * i128::from(step.max(1)))
		.map_err(|_| Error::validation("amount out of range"))
}

async fn change_quote(plans: &Plans, ctx: &Ctx, req: &QuoteReq, uid: &str) -> ClResult<Quote> {
	if req.coupon.is_some() {
		return Err(change_invalid("a coupon does not apply to a tier change"));
	}
	let at = Timestamp::now();
	let sub = own_sub(plans, ctx, uid).await?;
	let (c, p) = change(plans, ctx, sub, &req.offer, req.qty, at).await?;
	// Quote-time only: checkout re-runs `change`, and a replay would find its own draft.
	if c.kind == ChangeKind::Upgrade {
		for up in plans.store.plan_invoice_upgrades(c.sub.id, c.sub.period_start).await? {
			let inv = invoice_store(&plans.app)?.invoice_by_uid(None, &up.invoice_uid).await?;
			if inv.is_some_and(|i| {
				matches!(i.status, InvoiceStatus::Draft | InvoiceStatus::Pending)
					&& i.paid_amount < i.gross
			}) {
				return Err(change_invalid("an upgrade is awaiting payment"));
			}
		}
	}
	let mut claims = p.claims(ctx.org()?, None, false, at.0 + TTL_SECONDS);
	c.stamp(&mut claims, at);
	let code = &p.currency.code;
	let up = c.kind == ChangeKind::Upgrade;
	// A downgrade charges nothing now; the line shows what its first renewal will cost.
	let total = |m: Money| if up { m.to_wire(code) } else { Money::ZERO.to_wire(code) };
	let (start, end, effective) = if up {
		(at, c.sub.period_end, "now")
	} else {
		let start = c.sub.period_end;
		(start, renew::next_period_end(&p.offer, start)?, "period_end")
	};
	Ok(Quote {
		lines: vec![line(&p)?],
		net: total(p.computed.net),
		vat: total(p.computed.vat),
		gross: total(p.computed.gross),
		currency: code.clone(),
		period_start: Some(start),
		period_end: Some(end),
		effective,
		quote_token: mint(&plans.app, &claims).await?,
	})
}

/// Opens a token for the acting org: the signature, expiry and org, nothing re-priced yet.
pub(crate) async fn opened(plans: &Plans, ctx: &Ctx, token: &str) -> ClResult<Claims> {
	let claims = open(&plans.app, token).await?;
	if claims.exp < Timestamp::now().0 {
		return Err(Error::coded(StatusCode::GONE, E_QUOTE_EXPIRED, "the quote has expired"));
	}
	if claims.org != ctx.org()? {
		return Err(stale());
	}
	Ok(claims)
}

/// Re-prices a tier change's claims against the sub's current row.
pub(crate) async fn verify_change(
	plans: &Plans,
	ctx: &Ctx,
	claims: &Claims,
	sub: Subscription,
) -> ClResult<(Change, Priced)> {
	if sub.updated_at.0 != claims.sub_updated {
		return Err(stale());
	}
	let at = Timestamp(claims.at);
	let (c, p) = change(plans, ctx, sub, &claims.offer, Some(claims.qty), at).await?;
	let mut expect = p.claims(claims.org, None, false, claims.exp);
	c.stamp(&mut expect, at);
	if expect != *claims {
		return Err(stale());
	}
	Ok((c, p))
}

/// Re-prices what opened claims say for the acting org. The returned [`Priced`] is current
/// state, never the token's word.
pub(crate) async fn verify(plans: &Plans, ctx: &Ctx, claims: Claims) -> ClResult<(Claims, Priced)> {
	let p = price(
		plans,
		ctx,
		&claims.offer,
		Some(claims.qty),
		Some(&claims.currency),
		claims.coupon.as_deref(),
	)
	.await?;
	if p.claims(claims.org, claims.coupon.clone(), claims.trial, claims.exp) != claims {
		return Err(stale());
	}
	Ok((claims, p))
}

pub(crate) fn stale() -> Error {
	Error::coded(StatusCode::CONFLICT, E_QUOTE_STALE, "the quote no longer matches; quote again")
}

/// `b64url(json).hex_hmac`, the activation token's shape.
async fn mint(app: &App, claims: &Claims) -> ClResult<String> {
	let json = serde_json::to_string(claims).map_err(|e| Error::internal(e.to_string()))?;
	let key = app.secrets.get_or_create(KEY_NAME, 32).await?;
	Ok(format!("{}.{}", B64.encode(json.as_bytes()), hmac_hex(&key, &json)?))
}

async fn open(app: &App, token: &str) -> ClResult<Claims> {
	let bad = || Error::validation("invalid quote token");
	let (encoded, sig) = token.split_once('.').ok_or_else(bad)?;
	let json = String::from_utf8(B64.decode(encoded).map_err(|_| bad())?).map_err(|_| bad())?;
	let key = app.secrets.get_or_create(KEY_NAME, 32).await?;
	if !ct_eq(hmac_hex(&key, &json)?.as_bytes(), sig.as_bytes()) {
		return Err(bad());
	}
	serde_json::from_str(&json).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
	use super::prorate;

	#[test]
	fn prorate_rounds_half_up_to_the_step() {
		// 4000 HUF × 10/30 = 1333.33 → 1333 on a 1-forint step (100 minor).
		assert_eq!(prorate(400_000, 10, 30, 100).unwrap(), 133_300);
		// Exactly half a step rounds up: 150 minor × 1/1 on step 100 → 200.
		assert_eq!(prorate(150, 1, 1, 100).unwrap(), 200);
		assert_eq!(prorate(-5, 10, 30, 1).unwrap(), 0);
		assert_eq!(prorate(900, 0, 30, 1).unwrap(), 0);
	}
}

// vim: ts=4
