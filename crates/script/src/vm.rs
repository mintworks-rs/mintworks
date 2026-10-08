// SPDX-License-Identifier: MPL-2.0
//! Compiling a bundle once, and running one invocation on a fresh VM under both bounds.

use std::{
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use mintworks_core::{
	error::{ClResult, Error},
	settings::Settings,
};
use rune::{
	Context, Diagnostics, Source, Sources, ToTypeHash, Unit, Value, Vm,
	diagnostics::Diagnostic,
	runtime::{Args, RuntimeContext, VmError, budget},
};

use crate::{error, value::ScriptError};

/// The two bounds every invocation runs under, read from settings once at load.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
	/// Rune instructions, metered by `budget::with` inside the VM's own instruction loop.
	pub instructions: usize,
	pub deadline: Duration,
}

impl Default for Limits {
	fn default() -> Self {
		Self { instructions: 10_000_000, deadline: Duration::from_secs(5) }
	}
}

impl Limits {
	/// Read once per bundle, never per call: a settings lookup is a store read, and the
	/// invariant forbids mutable script globals to cache it in.
	///
	/// # Errors
	/// Whatever `Settings::int` raises for `script.budget` or `script.timeout_ms`.
	pub async fn load(settings: &Settings) -> ClResult<Self> {
		let instructions = settings.int("script.budget").await?;
		let ms = settings.int("script.timeout_ms").await?;
		Ok(Self {
			instructions: usize::try_from(instructions).unwrap_or(usize::MAX),
			deadline: Duration::from_millis(u64::try_from(ms).unwrap_or(0)),
		})
	}
}

/// A compiled bundle.
///
/// `Unit` and `RuntimeContext` are built once and `Arc`-shared while every invocation gets a
/// fresh `Vm`, so no state survives a call and no lock is taken on the hot path.
pub struct Script {
	unit: Arc<Unit>,
	runtime: Arc<RuntimeContext>,
	limits: Limits,
}

impl Script {
	/// Compile `sources` — `(name, text)` pairs — against the host `context`.
	///
	/// # Errors
	/// `E-SCRIPT-COMPILE`, carrying Rune's fatal diagnostics as the logged detail.
	///
	/// `visitor` is how `#[test]` discovery rides along: rune 0.14's `Options::test` is a
	/// no-op, so the attribute is only visible to a `CompileVisitor`, and it has to be *this*
	/// compile or the collected hashes would belong to a unit nothing runs.
	pub fn compile(
		context: &Context,
		sources: &[(String, String)],
		limits: Limits,
		visitor: Option<&mut dyn rune::compile::CompileVisitor>,
	) -> ClResult<Self> {
		let mut set = Sources::new();
		for (name, text) in sources {
			let source =
				Source::new(name, text).map_err(|e| error::compile(format!("{name}: {e}")))?;
			set.insert(source).map_err(|e| error::compile(format!("{name}: {e}")))?;
		}

		let mut diagnostics = Diagnostics::new();
		let mut build =
			rune::prepare(&mut set).with_context(context).with_diagnostics(&mut diagnostics);
		if let Some(visitor) = visitor {
			build = build.with_visitor(visitor).map_err(|e| error::compile(e.to_string()))?;
		}
		let unit = build
			.build()
			.map_err(|e| error::compile(fatals(&diagnostics).unwrap_or_else(|| e.to_string())))?;
		let runtime = context.runtime().map_err(|e| error::compile(e.to_string()))?;

		Ok(Self { unit: Arc::new(unit), runtime: Arc::new(runtime), limits })
	}

	/// Run `entry` on a fresh VM, bounded by the bundle's budget and deadline.
	///
	/// A script raises a framework error by returning `Err(err::app(…))` or any other
	/// `ScriptError`; anything else it returns is the result.
	///
	/// # Errors
	/// `E-SCRIPT-BUDGET`, `E-SCRIPT-TIMEOUT`, the `E-APP-*` `err::app` minted for a script raise,
	/// or `E-SCRIPT-RUNTIME`. A Rune failure never panics the host.
	pub async fn invoke<T>(&self, entry: impl ToTypeHash, args: impl Args + Send) -> ClResult<T>
	where
		T: rune::FromValue,
	{
		self.invoke_with(entry, args, None).await
	}

	/// [`Self::invoke`] with `deadline` replacing the bundle's `script.timeout_ms` — a route's
	/// `.timeout_ms(n)`, already clamped by the caller.
	///
	/// # Errors
	/// As [`Self::invoke`].
	pub async fn invoke_with<T>(
		&self,
		entry: impl ToTypeHash,
		args: impl Args + Send,
		deadline: Option<Duration>,
	) -> ClResult<T>
	where
		T: rune::FromValue,
	{
		let vm = Vm::new(Arc::clone(&self.runtime), Arc::clone(&self.unit));
		// `send_execute`, not `async_call`: Rune's ordinary async futures are not `Send` and
		// an axum handler needs one that is.
		let exec = vm.send_execute(entry, args).map_err(|e| Self::vm_error(&e))?;

		// Neither bound replaces the other. The budget meters instructions inside the VM loop,
		// which a deadline cannot bound because a CPU-bound script never awaits; the deadline
		// bounds a host call, which the budget never sees.
		let committing = Arc::new(AtomicBool::new(false));
		let running = crate::tx::COMMITTING.scope(
			Arc::clone(&committing),
			budget::with(self.limits.instructions, exec.async_complete()),
		);
		let expired = expire(deadline.unwrap_or(self.limits.deadline), &committing);
		let result = tokio::select! {
			result = running => result,
			() = expired => return Err(error::timeout("script invocation exceeded its deadline")),
		};
		let value = result.into_result().map_err(|e| Self::vm_error(&e))?;

		let value = match rune::from_value::<Result<Value, Value>>(value.clone()) {
			Ok(Ok(inner)) => inner,
			Ok(Err(value)) => return Err(raised(value)),
			Err(_) => value,
		};
		rune::from_value(value).map_err(|e| error::runtime(e.to_string()))
	}

	/// `VmErrorKind` and `VmHaltInfo` are `pub(crate)` in rune 0.14, so the budget halt is
	/// reachable only through `Display`, which renders `VmHalt::Limited` as `limited`.
	fn vm_error(err: &VmError) -> Error {
		let msg = err.to_string();
		if msg.contains("limited") { error::budget(msg) } else { error::runtime(msg) }
	}
}

/// Resolves at `deadline`, or once a commit in flight there finishes: the commit is never
/// cancelled, as it may already have landed, but the script after it is still bounded.
async fn expire(deadline: Duration, committing: &AtomicBool) {
	tokio::time::sleep(deadline).await;
	while committing.load(Ordering::SeqCst) {
		tokio::time::sleep(Duration::from_millis(5)).await;
	}
}

/// The error a script raised, out of the value it returned. `err::*` and every fallible host
/// call answer a [`ScriptError`], which is every way a script can fail — both here and inside a
/// `tx::with` block, whose `Bridged::from_value` calls this too.
pub(crate) fn raised(raised: Value) -> Error {
	match rune::from_value::<ScriptError>(raised) {
		Ok(ScriptError(err)) => err,
		Err(_) => error::runtime("script raised a value that is not an error"),
	}
}

/// Rune's own `emit` renders diagnostics to a colour terminal; a framework error carries a
/// string, so the fatal ones are joined instead.
fn fatals(diagnostics: &Diagnostics) -> Option<String> {
	let msg = diagnostics
		.diagnostics()
		.iter()
		.filter_map(|d| match d {
			Diagnostic::Fatal(fatal) => Some(fatal.to_string()),
			_ => None,
		})
		.collect::<Vec<_>>()
		.join("; ");
	(!msg.is_empty()).then_some(msg)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn script(src: &str, limits: Limits) -> ClResult<Script> {
		let context = Context::with_default_modules().unwrap();
		Script::compile(&context, &[("test".to_string(), src.to_string())], limits, None)
	}

	#[tokio::test(start_paused = true)]
	async fn the_deadline_fires_once_a_commit_finishes() {
		let committing = Arc::new(AtomicBool::new(true));
		let flag = Arc::clone(&committing);
		tokio::spawn(async move {
			tokio::time::sleep(Duration::from_secs(3)).await;
			flag.store(false, Ordering::SeqCst);
		});
		let start = tokio::time::Instant::now();
		expire(Duration::from_secs(1), &committing).await;
		let waited = start.elapsed();
		assert!(waited >= Duration::from_secs(3) && waited < Duration::from_secs(4), "{waited:?}");
	}

	#[tokio::test]
	async fn runs_a_trivial_async_function() {
		let s = script("pub async fn main() { 41 + 1 }", Limits::default()).unwrap();
		let n: i64 = s.invoke(["main"], ()).await.unwrap();
		assert_eq!(n, 42);
	}

	#[test]
	fn a_syntax_error_is_e_script_compile() {
		let err = script("pub fn main( {", Limits::default()).map(|_| ()).unwrap_err();
		assert_eq!(err.parts().1, error::E_COMPILE);
	}

	#[tokio::test]
	async fn a_runaway_loop_exhausts_the_budget() {
		let limits = Limits { instructions: 1_000, ..Limits::default() };
		let s = script("pub fn main() { let n = 0; while true { n += 1 } }", limits).unwrap();
		let err = s.invoke::<Value>(["main"], ()).await.unwrap_err();
		assert_eq!(err.parts().1, error::E_BUDGET);
	}
}

// vim: ts=4
