// SPDX-License-Identifier: MIT-0
//! `cargo run -p native-module -- examples/native-module/app` (or `-- test …`): the stock
//! `mintworks` CLI plus a native `greet::` module, one `configure` extension and one host
//! command, `greet-count`.

use mintworks::Host;
use mintworks_core::{App, ClResult};
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

/// `greet::nap(ms)` — awaits `ms` milliseconds, so a suite case can outlast a deadline.
#[rune::function]
async fn nap(ms: u64) {
	tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/// Registers `greet::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["greet"])?;
	m.function_meta(hello)?;
	m.function_meta(nap)?;
	Ok(m)
}

#[must_use]
pub fn host() -> Host {
	Host::new()
		.module(module)
		.configure(|b| b.extension(Greeting("szia".to_owned())))
		.command("greet-count", |app, names| Box::pin(greet_count(app, names)))
}

/// `native-module greet-count <app-dir> <name>…`: greets each name with the `configure`d
/// greeting, then prints how many it greeted.
///
/// # Errors
/// None; the `ClResult` is the [`Host::command`] shape.
#[allow(clippy::unused_async)]
pub async fn greet_count(app: App, names: Vec<String>) -> ClResult<()> {
	let greeting = app.extensions.get::<Greeting>().map_or("hello", |g| g.0.as_str());
	for name in &names {
		println!("{greeting}, {name}!");
	}
	println!("{} greeted", names.len());
	Ok(())
}

fn main() -> std::process::ExitCode {
	host().main()
}

// vim: ts=4
