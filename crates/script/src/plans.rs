//! `plans::` — `mintworks_plans::Plans` for script: the catalogue, quote → checkout, subscriptions,
//! rewards, the renewal sweep at an explicit time, and operator cancel/reprice.

use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use mintworks_plans::{AdminCancelReq, CheckoutReq, Plans, QuoteReq, RepriceReq, Side};
use rune::{ContextError, Module, Value, runtime::Ref};
use serde::de::DeserializeOwned;

use crate::{
	ctx::ScriptCtx,
	error::R,
	tx,
	value::{ScriptError, to_json, wire},
};

fn arg<T: DeserializeOwned>(what: &str, json: serde_json::Value) -> R<T> {
	serde_json::from_value(json)
		.map_err(|e| ScriptError(Error::validation(format!("plans::{what}: {e}"))))
}

fn handle(c: Ref<ScriptCtx>) -> R<(Plans, Ctx)> {
	let (app, ctx): (App, Ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	Ok((Plans::from_app(&app).map_err(ScriptError)?, ctx))
}

/// `plans::offers(ctx)` — the active offers, as `GET /api/plans/offers` lists them.
#[rune::function]
async fn offers(c: Ref<ScriptCtx>) -> R<Value> {
	let (p, ctx) = handle(c)?;
	wire(p.offers(&ctx).await.map_err(ScriptError)?)
}

/// `plans::quote(ctx, #{offer, qty?, currency?, coupon?, subscription?})` — as
/// `POST /api/plans/quote`; `subscription` quotes a tier change of that sub.
#[rune::function]
async fn quote(c: Ref<ScriptCtx>, req: Value) -> R<Value> {
	let (p, ctx) = handle(c)?;
	let req: QuoteReq = arg("quote", to_json(&req)?)?;
	wire(p.quote(&ctx, &req).await.map_err(ScriptError)?)
}

/// `plans::checkout(ctx, quoteToken, payMethod)` — `#{invoiceUid?, subscriptionUid?, next}`.
#[rune::function]
async fn checkout(c: Ref<ScriptCtx>, token: String, pay_method: String) -> R<Value> {
	tx::outside_tx("plans::")?;
	let (p, ctx) = handle(c)?;
	let req: CheckoutReq =
		arg("checkout", serde_json::json!({ "quoteToken": token, "payMethod": pay_method }))?;
	wire(p.checkout(&ctx, &req).await.map_err(ScriptError)?)
}

/// `plans::subscriptions(ctx)` — the org's subscriptions, newest first.
#[rune::function]
async fn subscriptions(c: Ref<ScriptCtx>) -> R<Value> {
	let (p, ctx) = handle(c)?;
	wire(p.subscriptions(&ctx).await.map_err(ScriptError)?)
}

/// `plans::cancel(ctx, uid)` — cancel at period end.
#[rune::function]
async fn cancel(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
	let (p, ctx) = handle(c)?;
	wire(p.cancel(&ctx, &uid).await.map_err(ScriptError)?)
}

/// `plans::resume(ctx, uid)` — undo a pending cancel.
#[rune::function]
async fn resume(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
	let (p, ctx) = handle(c)?;
	wire(p.resume(&ctx, &uid).await.map_err(ScriptError)?)
}

/// `plans::cancel_change(ctx, uid)` — take a queued downgrade back.
#[rune::function]
async fn cancel_change(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
	let (p, ctx) = handle(c)?;
	wire(p.cancel_change(&ctx, &uid).await.map_err(ScriptError)?)
}

/// `plans::reward_ref_use(ctx, refUid, accountUid, "inviter"|"invitee")` — rewards that
/// account's use of the ref; `false` when none configured.
#[rune::function]
async fn reward_ref_use(
	c: Ref<ScriptCtx>,
	ref_uid: String,
	account_uid: String,
	side: String,
) -> R<bool> {
	tx::outside_tx("plans::")?;
	let (p, ctx) = handle(c)?;
	let side: Side = arg("reward_ref_use", serde_json::Value::String(side))?;
	p.reward_ref_use(&ctx, &ref_uid, &account_uid, side).await.map_err(ScriptError)
}

/// `plans::reward(ctx, orgUid, offerCode, idem)` — operator or system; grants the offer free.
#[rune::function]
async fn reward(c: Ref<ScriptCtx>, org_uid: String, offer: String, idem: String) -> R<()> {
	tx::outside_tx("plans::")?;
	let (p, ctx) = handle(c)?;
	p.reward(&ctx, &org_uid, &offer, &idem).await.map_err(ScriptError)
}

/// `plans::renew_due(ctx, unixSeconds)` — the `SUBSCRIPTION_RENEW` body at an explicit time, for
/// suites that time-travel. Operator or system only.
#[rune::function]
async fn renew_due(c: Ref<ScriptCtx>, ts: i64) -> R<()> {
	tx::outside_tx("plans::")?;
	let (p, ctx) = handle(c)?;
	p.renew_due(&ctx, Timestamp(ts)).await.map_err(ScriptError)
}

/// `plans::admin_cancel(ctx, uid, #{immediate, refund})` — operator cancel, as
/// `POST /api/admin/subscriptions/{uid}/cancel`; `refund` is `"none"` or `"prorated"`.
#[rune::function]
async fn admin_cancel(c: Ref<ScriptCtx>, uid: String, req: Value) -> R<Value> {
	tx::outside_tx("plans::")?;
	let req: AdminCancelReq = arg("admin_cancel", to_json(&req)?)?;
	let (p, ctx) = handle(c)?;
	wire(p.admin_cancel(&ctx, &uid, &req).await.map_err(ScriptError)?)
}

/// `plans::reprice(ctx, offerCode, #{currency, amount})` — operator reprice, as
/// `POST /api/admin/offers/{code}/reprice`; the rewritten subscriptions.
#[rune::function]
async fn reprice(c: Ref<ScriptCtx>, code: String, req: Value) -> R<Value> {
	tx::outside_tx("plans::")?;
	let req: RepriceReq = arg("reprice", to_json(&req)?)?;
	let (p, ctx) = handle(c)?;
	wire(p.reprice(&ctx, &code, &req).await.map_err(ScriptError)?)
}

/// Registers `plans::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["plans"])?;
	m.function_meta(offers)?;
	m.function_meta(quote)?;
	m.function_meta(checkout)?;
	m.function_meta(subscriptions)?;
	m.function_meta(cancel)?;
	m.function_meta(resume)?;
	m.function_meta(cancel_change)?;
	m.function_meta(reward_ref_use)?;
	m.function_meta(reward)?;
	m.function_meta(renew_due)?;
	m.function_meta(admin_cancel)?;
	m.function_meta(reprice)?;
	Ok(m)
}

// vim: ts=4
