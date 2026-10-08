// SPDX-License-Identifier: MPL-2.0
//! `tx::with(ctx, |ctx| { … })` — one transaction spanning everything inside it, service calls
//! included.
//!
//! The body runs inside the transaction the consumer's `TxHook` opened, and everything it calls
//! joins that transaction **by task** — so the closure's `ctx` is the outer `ctx`, and there is
//! nothing to rebind. `mintworks-script` never names the store adapter: reaching
//! `SqliteStore::begin()` here would make the script runtime SQLite-only, which the adapter
//! contract exists to prevent.
//!
//! A `tx::with` block holds the **only** writer connection in the process, so the block carries
//! its own deadline, far shorter than the per-call budget, and `fs`/`http`/`nav::` are refused
//! inside it — a remote round trip in here stalls every write in the application.

use std::{
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use mintworks_core::error::{Error, StatusCode};
use rune::{
	ContextError, Module, Value,
	runtime::{FromValue, Function, Ref, RuntimeError},
};

use crate::{
	ScriptRuntime,
	ctx::ScriptCtx,
	error::{self, R},
	value::{ScriptError, from_json, to_json},
};

/// The block outran `script.tx_timeout_ms` and was rolled back.
pub const E_TX_TIMEOUT: &str = "E-SCRIPT-TX-TIMEOUT";
/// A remote call — `nav::`, `fs`, `http` — was made inside the block.
pub const E_TX_REMOTE: &str = "E-SCRIPT-TX-REMOTE";

tokio::task_local! {
	/// One invocation is one VM on one task, so a task-local *is* the block's scope. This is
	/// what lets [`crate::io`] refuse `fs` and `http` inside a transaction without every host
	/// module having to be handed the state.
	static OPEN: ();

	/// Set by `tx::with` alone. Lock order is script first: `db::tx` may wrap `tx::with`, never
	/// the reverse, or two invocations deadlock on the two single-writer pools (ABBA).
	static FRAMEWORK: ();

	/// Set while a block's commit is under way, so the invocation deadline in `vm.rs` waits for
	/// it instead of cancelling it and reporting a timeout for a write that landed.
	pub(crate) static COMMITTING: Arc<AtomicBool>;
}

/// Sets [`COMMITTING`] for this invocation; a no-op outside `Script::invoke`.
pub(crate) fn committing(flag: bool) {
	let _ = COMMITTING.try_with(|c| c.store(flag, Ordering::SeqCst));
}

/// Whether the calling task is inside a `tx::with` block.
#[must_use]
pub fn in_block() -> bool {
	OPEN.try_with(|()| ()).is_ok()
}

/// Whether the calling task is inside `tx::with` specifically — where `db::` writes are refused.
#[must_use]
pub(crate) fn in_framework_block() -> bool {
	FRAMEWORK.try_with(|()| ()).is_ok()
}

/// Runs `f` with the block flag set. `db::tx` sets the **same** flag: that block holds the script
/// database's only writer connection, so a remote round trip inside it stalls writes just the same.
/// `without_remote` covers the calls a host function makes on the script's behalf (VIES, MNB).
pub(crate) async fn scope_open<F: Future>(f: F) -> F::Output {
	OPEN.scope((), mintworks_core::http::without_remote(f)).await
}

/// A block holds a single-writer connection, so a remote round trip inside one stalls every
/// write behind it.
pub(crate) fn outside_tx(what: &str) -> R<()> {
	if in_block() {
		return Err(coded(E_TX_REMOTE, format!("a {what} call is not allowed inside a tx block")));
	}
	Ok(())
}

/// Inside `db::tx` but not `tx::with`, a core-DB write commits on its own and would outlive a
/// rollback of the block.
pub(crate) fn outside_app_tx(what: &str) -> R<()> {
	if in_block() && !in_framework_block() {
		return Err(coded(
			E_TX_REMOTE,
			format!(
				"{what} inside db::tx would commit even if the block rolls back; use tx::with or move it out"
			),
		));
	}
	Ok(())
}

pub(crate) fn coded(code: &'static str, msg: impl Into<String>) -> ScriptError {
	ScriptError(Error::coded(StatusCode::INTERNAL_SERVER_ERROR, code, msg))
}

/// The block's return value, bridged to JSON **inside** `FromValue`.
///
/// `Function::async_send_call` needs a `Send` output and `rune::Value` is not `Send`, so the
/// conversion cannot happen after the call. A failure travels in the payload rather than as a
/// `RuntimeError`, which this crate has no constructor for.
pub(crate) struct Bridged(pub(crate) mintworks_core::error::ClResult<serde_json::Value>);

impl FromValue for Bridged {
	fn from_value(value: Value) -> Result<Self, RuntimeError> {
		// A `?` inside the block makes the closure **return** `Err(e)` rather than unwind, so the
		// `Result` is unwrapped here as `Vm::invoke` does, never serialized as the block's value.
		let value = match rune::from_value::<Result<Value, Value>>(value.clone()) {
			Ok(Ok(inner)) => inner,
			Ok(Err(raised)) => return Ok(Self(Err(crate::vm::raised(raised)))),
			Err(_) => value,
		};
		Ok(Self(to_json(&value)))
	}
}

/// `tx::with(ctx, closure)` — commits on success, rolls back on any error or on its deadline.
#[rune::function]
pub async fn with(c: Ref<ScriptCtx>, body: Function) -> R<Value> {
	let app = c.app()?.clone();
	let inner = (*c).clone();
	drop(c);

	let rt =
		app.extensions.get::<ScriptRuntime>().cloned().ok_or_else(|| {
			ScriptError(error::runtime("no ScriptRuntime extension is registered"))
		})?;
	// Not a panic: an application that registered no hook is a misconfiguration, and the
	// envelope names the missing piece.
	let hook = rt.tx.clone().ok_or_else(|| {
		ScriptError(error::compile("tx::with needs a TxHook and the application registered none"))
	})?;

	let ms = app.settings.int("script.tx_timeout_ms").await.map_err(ScriptError)?;
	let deadline = Duration::from_millis(u64::try_from(ms).unwrap_or(DEFAULT_TX_TIMEOUT_MS));

	// The deadline goes on the body, never around `hook.run`: a timeout outside the hook would
	// cancel the commit. Elapsed, the body resolves to an error and the hook rolls back.
	//
	// The `rune::Value` the block yields is converted inside the call, so nothing non-`Send` is
	// alive across the commit the hook performs.
	let called = body.async_send_call::<_, Bridged>((inner,));
	let fut: crate::TxBody<'_> = Box::pin(async move {
		let out = match tokio::time::timeout(deadline, OPEN.scope((), FRAMEWORK.scope((), called)))
			.await
		{
			Err(_) => Err(coded(E_TX_TIMEOUT, "the tx::with block outran script.tx_timeout_ms").0),
			Ok(vm) => match vm.into_result() {
				Err(e) => Err(error::runtime(e.to_string())),
				Ok(Bridged(v)) => v,
			},
		};
		committing(out.is_ok());
		out
	});

	let json = hook.run(&app, fut).await;
	committing(false);
	let json = json.map_err(ScriptError)?;
	from_json(&json).map_err(ScriptError)
}

/// What `script.tx_timeout_ms` falls back to if its value will not fit a `u64`.
pub(crate) const DEFAULT_TX_TIMEOUT_MS: u64 = 2_000;

/// Registers `tx::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["tx"])?;
	m.function_meta(with)?;
	Ok(m)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn the_block_flag_is_scoped_to_the_block() {
		assert!(!in_block());
		OPEN.scope((), async {
			assert!(in_block());
		})
		.await;
		assert!(!in_block());
	}

	#[tokio::test]
	async fn db_tx_is_not_a_framework_block() {
		scope_open(async { assert!(!in_framework_block()) }).await;
		FRAMEWORK.scope((), async { assert!(in_framework_block()) }).await;
	}

	#[tokio::test]
	async fn a_remote_call_is_refused_inside_a_block() {
		assert!(outside_tx("billing::").is_ok());
		let Err(ScriptError(Error::Coded { code, .. })) =
			scope_open(async { outside_tx("billing::") }).await
		else {
			panic!("a remote call inside a block was allowed");
		};
		assert_eq!(code, E_TX_REMOTE);
		// And a host function's own call (VIES, MNB) meets `E-CORE-REMOTE-IN-TX`.
		assert!(scope_open(async { mintworks_core::http::remote_forbidden() }).await);
	}
}

// vim: ts=4
