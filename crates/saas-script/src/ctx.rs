//! `Ctx` as an opaque Rune handle: read accessors only, and no constructor script can reach.

use rune::{Any, ContextError, Module};
use saas_core::{Actor, App, Ctx};

use crate::value::ScriptError;

/// What a script holds when a handler hands it a `ctx`.
///
/// The `App` travels with the `Ctx` because a Rune host function is a free `fn` that captures
/// nothing, and every bound service method needs one to build its handle. It is the same `App`
/// inside a `tx::with` block as outside: the transaction is joined by task, not by handle.
///
/// There is no Rune constructor for this type and there must never be one — that is what makes
/// `Actor` unmintable from script, and `Actor::System` is the most privileged actor there is.
#[derive(Clone, Any)]
pub struct ScriptCtx {
	app: App,
	ctx: Ctx,
}

impl ScriptCtx {
	/// Rust-only. The route, job and init entry points mint one; script cannot.
	#[must_use]
	pub fn new(app: App, ctx: Ctx) -> Self {
		Self { app, ctx }
	}

	/// Infallible: a call inside `tx::with` joins the transaction whichever ctx made it. The
	/// `Result` only keeps the call sites uniform.
	#[allow(clippy::unnecessary_wraps)]
	pub(crate) fn app(&self) -> Result<&App, ScriptError> {
		Ok(&self.app)
	}

	pub(crate) fn ctx(&self) -> &Ctx {
		&self.ctx
	}
}

/// `ctx.actor()` — the actor kind, as a string.
///
/// The kind is all a script gets. `ctx.org()` and `ctx.account()` are also specified to return
/// uids, and neither is bound: `Ctx` carries only the internal
/// `org_id` / `account_id`, handing those to script would put a primary key somewhere a script
/// can echo it into a response, and `saas-core` has no id-to-uid accessor to convert with. A
/// script that needs the org's uid reads `auth::org(ctx).uid`.
#[rune::function(instance)]
fn actor(this: &ScriptCtx) -> &'static str {
	match this.ctx.actor {
		Actor::User { .. } => "user",
		Actor::Operator { .. } => "operator",
		Actor::Key { .. } => "key",
		Actor::System { .. } => "system",
		Actor::Public { .. } => "public",
	}
}

/// Registers the `ctx` handle and its accessors.
///
/// # Errors
/// Whatever Rune raises registering the type or a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::new();
	m.ty::<ScriptCtx>()?;
	m.function_meta(actor)?;
	Ok(m)
}

// vim: ts=4
