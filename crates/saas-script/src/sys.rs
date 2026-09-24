//! `sys::escalate(ctx)` — the script spelling of `Ctx::as_system`, and the only privilege
//! escalation a script has.
//!
//! An application-level script *is* the composition root, and
//! `examples/booking/backend/src/bookings.rs` writes `ctx.clone().as_system("checkout")` in Rust
//! for exactly this call: the seller belongs to the root org, so a customer's own role never
//! covers raising the invoice their checkout just asked for. There is still no Rune constructor
//! for a `Ctx` — this derives one from the ctx an entry point handed out, so `Actor` stays
//! unmintable.

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

/// Registers `sys::`. Never a member of the base set — an unregistered module is a *compile*
/// error in the script rather than a runtime permission check, which is the property that lets
/// an org-level profile exist at all (`io.rs`).
///
/// # Errors
/// Whatever Rune raises registering a module or a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["sys"])?;
	m.function_meta(escalate)?;
	Ok(m)
}

#[cfg(test)]
mod tests {
	use crate::{IoProfile, Limits, vm::Script};

	fn compiles(io: &IoProfile) -> bool {
		let mut c = rune::Context::with_default_modules().unwrap();
		c.install(crate::ctx::module().unwrap()).unwrap();
		c.install(crate::value::module().unwrap()).unwrap();
		for m in io.modules().unwrap() {
			c.install(m).unwrap();
		}
		let src = [("t".to_owned(), "pub fn main(ctx) { sys::escalate(ctx) }".to_owned())];
		Script::compile(&c, &src, Limits::default(), None).is_ok()
	}

	/// The gate is *registration*, not a runtime check: without `IoProfile.sys` the call is an
	/// unresolved item, which is the property that lets an org-level profile exist at all.
	#[test]
	fn escalate_is_unreachable_without_the_sys_module() {
		assert!(!compiles(&IoProfile::sandboxed()));
		assert!(compiles(&IoProfile { sys: true, ..IoProfile::default() }));
	}
}

// vim: ts=4
