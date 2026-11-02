// SPDX-License-Identifier: MPL-2.0
//! `sys::escalate(ctx)` — the script spelling of `Ctx::as_system`, and the only privilege
//! escalation a script has — plus `sys::in_org(ctx, orgUid)`, which moves a job's or init
//! handler's system ctx into a tenant org, and `sys::orgs(ctx)`, which lists the orgs it accepts.
//!
//! An application-level script *is* the composition root, and
//! `examples/booking/app-rust/src/bookings.rs` writes `ctx.clone().as_system("checkout")` in Rust
//! for exactly this call: the seller belongs to the root org, so a customer's own role never
//! covers raising the invoice their checkout just asked for. There is still no Rune constructor
//! for a `Ctx` — this derives one from the ctx an entry point handed out, so `Actor` stays
//! unmintable.

use mintworks_auth::store::OrgStatus;
use mintworks_core::{ctx::Ctx, error::Error, ids::OrgId};
use rune::{ContextError, Module, runtime::Ref};

use crate::{ctx::ScriptCtx, value::ScriptError};

/// `Actor::System { source }` takes a `&'static str`, so the source is this constant and never
/// a script-supplied string — the rule `err::app` needs an intern table to keep.
pub(crate) const SOURCE: &str = "script.escalate";

/// The caller's ctx re-actored as the framework. `org_id` is kept and the caller is recorded in
/// `on_behalf_of`, so the call stays confined to the acting org and the audit trail still names
/// the user.
#[rune::function]
fn escalate(c: Ref<ScriptCtx>) -> Result<ScriptCtx, ScriptError> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	Ok(ScriptCtx::new(app, ctx.as_system(SOURCE)))
}

/// The source of every ctx [`in_org`] hands out.
pub(crate) const IN_ORG_SOURCE: &str = "script.in_org";

/// A fresh system ctx acting in the org `org_uid` names: how a job or init handler, which acts
/// for the root org, works for each tenant. The guard is `api::system_only`'s — a request
/// handler's ctx is not `System`, and an [`escalate`]d one carries [`SOURCE`], so neither can
/// leave the acting org. An unknown or suspended org is `E-CORE-NOTFOUND`.
#[rune::function]
async fn in_org(c: Ref<ScriptCtx>, org_uid: Ref<str>) -> Result<ScriptCtx, ScriptError> {
	let app = c.app()?.clone();
	crate::api::system_only(c.ctx())?;
	drop(c);
	let store = mintworks_auth::routes::store(&app).map_err(ScriptError)?;
	let uid = OrgId::parse(&org_uid).map_err(ScriptError)?;
	let org = store
		.org_by_uid(&uid)
		.await
		.map_err(ScriptError)?
		.filter(|o| o.status == OrgStatus::Active)
		.ok_or(Error::NotFound)?;
	Ok(ScriptCtx::new(app, Ctx::system(IN_ORG_SOURCE).with_org(org.id)))
}

/// The uids of the active orgs [`in_org`] accepts, minus the root org a system ctx already acts
/// in, for a job that works for each tenant. Behind the same `system_only` guard as [`in_org`], so
/// a request cannot enumerate tenants. Ordering and paging: `AuthStore::active_tenant_org_uids`.
#[rune::function]
async fn orgs(c: Ref<ScriptCtx>) -> Result<Vec<String>, ScriptError> {
	let app = c.app()?.clone();
	crate::api::system_only(c.ctx())?;
	drop(c);
	let store = mintworks_auth::routes::store(&app).map_err(ScriptError)?;
	let uids = store.active_tenant_org_uids().await.map_err(ScriptError)?;
	Ok(uids.into_iter().map(|u| u.as_str().to_owned()).collect())
}

/// Registers `sys::`. Never a member of the base set — an unregistered module is a *compile*
/// error in the script rather than a runtime permission check, which is the property that lets
/// an org-level profile exist at all (`io.rs`).
///
/// # Errors
/// Whatever Rune raises registering a module or a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["sys"])?;
	m.function_meta(escalate)?;
	m.function_meta(in_org)?;
	m.function_meta(orgs)?;
	Ok(m)
}

#[cfg(test)]
mod tests {
	use crate::{IoProfile, vm::Script};

	fn compiles(io: &IoProfile) -> bool {
		compiles_src(io, "pub fn main(ctx) { sys::escalate(ctx) }")
	}

	fn compiles_src(io: &IoProfile, body: &str) -> bool {
		let mut c = rune::Context::with_default_modules().unwrap();
		c.install(crate::ctx::module().unwrap()).unwrap();
		c.install(crate::value::module().unwrap()).unwrap();
		#[cfg(feature = "ai")]
		c.install(crate::llm::module().unwrap()).unwrap();
		for m in io.modules(&crate::io::TestEnv::default()).unwrap() {
			c.install(m).unwrap();
		}
		let src = [("t".to_owned(), body.to_owned())];
		Script::compile(&c, &src, None).is_ok()
	}

	/// The gate is *registration*, not a runtime check: without `IoProfile.sys` the call is an
	/// unresolved item, which is the property that lets an org-level profile exist at all.
	#[test]
	fn escalate_is_unreachable_without_the_sys_module() {
		assert!(!compiles(&IoProfile::sandboxed()));
		assert!(compiles(&IoProfile { sys: true, ..IoProfile::default() }));
	}

	#[test]
	fn in_org_is_unreachable_without_the_sys_module() {
		let src = "pub async fn main(ctx) { sys::in_org(ctx, \"org_x\").await }";
		assert!(!compiles_src(&IoProfile::sandboxed(), src));
		assert!(compiles_src(&IoProfile { sys: true, ..IoProfile::default() }, src));
	}

	#[test]
	fn orgs_is_unreachable_without_the_sys_module() {
		let src = "pub async fn main(ctx) { sys::orgs(ctx).await }";
		assert!(!compiles_src(&IoProfile::sandboxed(), src));
		assert!(compiles_src(&IoProfile { sys: true, ..IoProfile::default() }, src));
	}

	#[cfg(feature = "ai")]
	#[test]
	fn usage_report_is_unreachable_without_the_sys_module() {
		let src = "pub async fn main(ctx) { llm::usage_report(ctx, #{ since: 0, by: [] }).await }";
		assert!(!compiles_src(&IoProfile::sandboxed(), src));
		assert!(compiles_src(&IoProfile { sys: true, ..IoProfile::default() }, src));
	}
}

// vim: ts=4
