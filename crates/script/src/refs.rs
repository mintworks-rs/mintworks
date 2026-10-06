//! `refs::` — `mintworks_core::refs` for script: create, preview, list, revoke and reactivate redeemable codes.

use mintworks_core::ids::RefId;
use mintworks_core::prelude::*;
use mintworks_core::refs::{CreateRef, Refs};
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
	let uid = RefId::parse(&uid).map_err(ScriptError)?;
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
	let uid = RefId::parse(&uid).map_err(ScriptError)?;
	Refs::from_app(&app)
		.map_err(ScriptError)?
		.reactivate(&ctx, &uid)
		.await
		.map_err(ScriptError)?;
	Ok(())
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
	Ok(m)
}

// vim: ts=4
