// SPDX-License-Identifier: MPL-2.0
//! `refs::` — `mintworks_core::refs` for script: create, preview, list, revoke and reactivate redeemable codes.

use mintworks_core::refs::{AUTH_TYPES, CreateRef, Refs};
use mintworks_core::{error::StatusCode, prelude::*};
use rune::{ContextError, Module, Value, runtime::Ref};

use crate::{
	ctx::ScriptCtx,
	error::R,
	tx,
	value::{ScriptError, to_json, wire},
};

/// `refs::create(ctx, #{type, code?, target?, email?, params?, usesLeft?, expiresAt?})` — the
/// new ref in its wire shape (`uid`, `code`, …).
#[rune::function]
async fn create(c: Ref<ScriptCtx>, req: Value) -> R<Value> {
	tx::outside_tx("refs::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let req: CreateRef = serde_json::from_value(to_json(&req)?)
		.map_err(|e| ScriptError(Error::validation(format!("refs::create: {e}"))))?;
	let r = Refs::from_app(&app)
		.map_err(ScriptError)?
		.create(&ctx, &req)
		.await
		.map_err(ScriptError)?;
	wire(r)
}

/// `refs::get(ctx, code)` — the public preview `#{type, valid, orgName?}`.
#[rune::function]
async fn get(c: Ref<ScriptCtx>, code: String) -> R<Value> {
	let app = c.app()?.clone();
	drop(c);
	let p = Refs::from_app(&app)
		.map_err(ScriptError)?
		.preview(&code)
		.await
		.map_err(ScriptError)?;
	wire(p)
}

/// `refs::list(ctx, type)` — the org's refs, newest first; `()` for every type.
#[rune::function]
async fn list(c: Ref<ScriptCtx>, ref_type: Value) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let ref_type = to_json(&ref_type)?;
	let refs = Refs::from_app(&app)
		.map_err(ScriptError)?
		.list(&ctx, ref_type.as_str())
		.await
		.map_err(ScriptError)?;
	wire(refs)
}

/// `refs::revoke(ctx, "ref_…")`.
#[rune::function]
async fn revoke(c: Ref<ScriptCtx>, uid: String) -> R<()> {
	tx::outside_tx("refs::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	Refs::from_app(&app)
		.map_err(ScriptError)?
		.revoke(&ctx, &uid)
		.await
		.map_err(ScriptError)?;
	Ok(())
}

/// `refs::reactivate(ctx, "ref_…")`.
#[rune::function]
async fn reactivate(c: Ref<ScriptCtx>, uid: String) -> R<()> {
	tx::outside_tx("refs::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	Refs::from_app(&app)
		.map_err(ScriptError)?
		.reactivate(&ctx, &uid)
		.await
		.map_err(ScriptError)?;
	Ok(())
}

/// `refs::redeem(ctx, type, code)` — records the acting account, in the acting org, as a use of
/// `code`: `#{uid, type, usedAt, fresh}`. A repeat use answers the first with `fresh: false`; a
/// revoked, exhausted or expired code is 410. A code of another type, an auth type (`signup`,
/// `org_invite`: activation's business) or bound to another email is 404 like an unknown one.
#[rune::function]
async fn redeem(c: Ref<ScriptCtx>, ref_type: String, code: String) -> R<Value> {
	tx::outside_tx("refs::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let account_id = ctx.actor.account_id().ok_or_else(|| {
		ScriptError(Error::coded(StatusCode::FORBIDDEN, "E-AUTH-FORBIDDEN", "no acting account"))
	})?;
	let org_id = ctx.org().map_err(ScriptError)?;
	let invalid =
		|| ScriptError(Error::coded(StatusCode::NOT_FOUND, "E-CORE-REF-INVALID", "unknown code"));
	let refs = Refs::from_app(&app).map_err(ScriptError)?;
	let r = refs.by_code(&code).await.map_err(ScriptError)?.ok_or_else(invalid)?;
	if r.ref_type != ref_type || AUTH_TYPES.contains(&r.ref_type.as_str()) {
		return Err(invalid());
	}
	if let Some(bound) = &r.email {
		let account = mintworks_auth::routes::store(&app)
			.map_err(ScriptError)?
			.account_by_id(account_id)
			.await
			.map_err(ScriptError)?
			.ok_or_else(invalid)?;
		if account.email.to_lowercase() != *bound {
			return Err(invalid());
		}
	}
	let (u, fresh) = refs
		.redeem(&ctx, r.id, account_id, org_id)
		.await
		.map_err(ScriptError)?
		.ok_or_else(|| {
			ScriptError(Error::coded(
				StatusCode::GONE,
				"E-CORE-REF-GONE",
				"this code is no longer valid",
			))
		})?;
	wire(serde_json::json!({ "uid": r.uid, "type": r.ref_type, "usedAt": u.at, "fresh": fresh }))
}

/// Registers `refs::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["refs"])?;
	m.function_meta(create)?;
	m.function_meta(get)?;
	m.function_meta(list)?;
	m.function_meta(revoke)?;
	m.function_meta(reactivate)?;
	m.function_meta(redeem)?;
	Ok(m)
}

// vim: ts=4
