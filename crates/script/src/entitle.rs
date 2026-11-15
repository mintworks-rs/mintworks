// SPDX-License-Identifier: MPL-2.0
//! `entitle::` — `mintworks_entitle::Entitle` for script: checks, metered debits, grants, the
//! summary. A refusal is the service's own 402 (`E-ENT-DENIED`/`E-ENT-EXHAUSTED`), raised as an
//! error.

use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use mintworks_entitle::{Entitle, GrantReq};
use rune::{ContextError, Module, Value, runtime::Ref};

use crate::{
	ctx::ScriptCtx,
	error::R,
	tx,
	value::{ScriptError, to_json, wire},
};

pub(crate) fn handle(c: Ref<ScriptCtx>) -> R<(Entitle, Ctx)> {
	let (app, ctx): (App, Ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	Ok((Entitle::from_app(&app).map_err(ScriptError)?, ctx))
}

/// `entitle::has(ctx, key)` — `true` while the org holds the key, whatever its kind.
#[rune::function]
async fn has(c: Ref<ScriptCtx>, key: String) -> R<bool> {
	let (e, ctx) = handle(c)?;
	e.has(&ctx, &key).await.map_err(ScriptError)
}

/// `entitle::limit(ctx, key)` — the largest active amount, or `()` with no grant.
#[rune::function]
async fn limit(c: Ref<ScriptCtx>, key: String) -> R<Option<i64>> {
	let (e, ctx) = handle(c)?;
	e.limit(&ctx, &key).await.map_err(ScriptError)
}

/// `entitle::balance(ctx, key)` — a meter's balance; negative after an overdrawing `charge`.
#[rune::function]
async fn balance(c: Ref<ScriptCtx>, key: String) -> R<i64> {
	let (e, ctx) = handle(c)?;
	e.balance(&ctx, &key).await.map_err(ScriptError)
}

/// `entitle::require(ctx, key)` — raises 402 `E-ENT-DENIED` unless `has`.
#[rune::function]
async fn require(c: Ref<ScriptCtx>, key: String) -> R<()> {
	let (e, ctx) = handle(c)?;
	e.require(&ctx, &key).await.map_err(ScriptError)
}

/// `entitle::consume(ctx, key, n, idem)` — the balance after, or 402 `E-ENT-EXHAUSTED`.
#[rune::function]
async fn consume(c: Ref<ScriptCtx>, key: String, n: i64, idem: String) -> R<i64> {
	tx::outside_tx("entitle::")?;
	let (e, ctx) = handle(c)?;
	e.consume(&ctx, &key, n, &idem).await.map_err(ScriptError)
}

/// `entitle::charge(ctx, key, n, idem)` — `consume` that may overdraw.
#[rune::function]
async fn charge(c: Ref<ScriptCtx>, key: String, n: i64, idem: String) -> R<i64> {
	tx::outside_tx("entitle::")?;
	let (e, ctx) = handle(c)?;
	e.charge(&ctx, &key, n, &idem).await.map_err(ScriptError)
}

/// `entitle::grant(ctx, #{key, amount, validFrom?, validUntil?, source, sourceRef?})` — operator
/// or system only; the grant in its wire shape, the existing one on a repeated source.
#[rune::function]
async fn grant(c: Ref<ScriptCtx>, req: Value) -> R<Value> {
	tx::outside_tx("entitle::")?;
	let (e, ctx) = handle(c)?;
	let req: GrantReq = serde_json::from_value(to_json(&req)?)
		.map_err(|e| ScriptError(Error::validation(format!("entitle::grant: {e}"))))?;
	wire(e.grant(&ctx, &req).await.map_err(ScriptError)?)
}

/// `entitle::summary(ctx)` — `#{features, limits, meters}`, as `GET /api/entitlements`.
#[rune::function]
async fn summary(c: Ref<ScriptCtx>) -> R<Value> {
	let (e, ctx) = handle(c)?;
	wire(e.summary(&ctx).await.map_err(ScriptError)?)
}

/// `handle`, re-scoped to the active org `org_uid` names. An init or job handler reads any org; a
/// request reads only an org its user is a member of (else `E-CORE-NOTFOUND`). An escalated ctx
/// is refused (403 `E-SCRIPT-INIT-ONLY`): it passes the membership check of any org. An unknown
/// or suspended org is `E-CORE-NOTFOUND`.
async fn handle_in(c: Ref<ScriptCtx>, org_uid: &str) -> R<(Entitle, Ctx)> {
	let app = c.app()?.clone();
	let (e, ctx) = handle(c)?;
	crate::api::not_escalated(&ctx)?;
	let uid = mintworks_core::ids::OrgId::parse(org_uid).map_err(ScriptError)?;
	let org = crate::sys::active_org(&app, &ctx, &uid).await.map_err(ScriptError)?;
	Ok((e, ctx.with_org(org)))
}

/// `entitle::has_in(sys, orgUid, key)` — `has` for the named org; an init or job handler,
/// or a member of the org; never escalated.
#[rune::function]
async fn has_in(c: Ref<ScriptCtx>, org_uid: String, key: String) -> R<bool> {
	let (e, ctx) = handle_in(c, &org_uid).await?;
	e.has(&ctx, &key).await.map_err(ScriptError)
}

/// `entitle::summary_in(sys, orgUid)` — `summary` for the named org; an init or job
/// handler, or a member of the org; never escalated.
#[rune::function]
async fn summary_in(c: Ref<ScriptCtx>, org_uid: String) -> R<Value> {
	let (e, ctx) = handle_in(c, &org_uid).await?;
	wire(e.summary(&ctx).await.map_err(ScriptError)?)
}

/// Registers `entitle::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["entitle"])?;
	m.function_meta(has)?;
	m.function_meta(limit)?;
	m.function_meta(balance)?;
	m.function_meta(require)?;
	m.function_meta(consume)?;
	m.function_meta(charge)?;
	m.function_meta(grant)?;
	m.function_meta(summary)?;
	m.function_meta(has_in)?;
	m.function_meta(summary_in)?;
	Ok(m)
}

// vim: ts=4
