// SPDX-License-Identifier: MIT-0
//! `cargo run -p native-module -- examples/native-module/app` (or `-- test …`): the stock
//! `mintworks` CLI plus a native `greet::` module and one `configure` extension.

use mintworks::Host;
use mintworks_script::{ScriptCtx, ScriptError};
use rune::{ContextError, Module, runtime::Ref};

/// Put on the `AppBuilder` by `configure`, read back by `greet::hello`.
#[derive(Clone)]
pub struct Greeting(pub String);

/// `greet::hello(ctx, name)` — `"<greeting>, <name>!"`, with the greeting from `configure`.
// Rune hands arguments over by value.
#[allow(clippy::needless_pass_by_value)]
#[rune::function]
fn hello(c: Ref<ScriptCtx>, name: String) -> Result<String, ScriptError> {
	let greeting = c.app()?.extensions.get::<Greeting>().map_or("hello", |g| g.0.as_str());
	Ok(format!("{greeting}, {name}!"))
}

/// Registers `greet::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["greet"])?;
	m.function_meta(hello)?;
	Ok(m)
}

#[must_use]
pub fn host() -> Host {
	Host::new()
		.module(module)
		.configure(|b| b.extension(Greeting("szia".to_owned())))
}

fn main() -> std::process::ExitCode {
	host().main()
}

// vim: ts=4
