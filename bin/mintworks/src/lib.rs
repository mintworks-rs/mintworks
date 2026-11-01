// SPDX-License-Identifier: MPL-2.0
#![forbid(unsafe_code)]
//! `mintworks <dir>` — the whole application is the Rune sources in `<dir>`.
//!
//! There is no hot reload: every `.rn` compiles at startup and any error refuses to serve, so a
//! restart is the reload. [`Host`] is the same entry point for a consumer binary that adds its
//! own native Rune modules.

mod app;
mod test;

use std::{
	path::{Path, PathBuf},
	pin::Pin,
	process::ExitCode,
	sync::Arc,
};

use mintworks_core::{App, AppBuilder, ClResult, config::Config, error::Error};
pub use mintworks_script::ModuleFn;

type Configure = Arc<dyn Fn(AppBuilder) -> AppBuilder + Send + Sync>;

/// The same type as `futures::future::BoxFuture`, spelled out so no consumer needs `futures`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

type Command = Arc<dyn Fn(App, Vec<String>) -> BoxFuture<'static, ClResult<()>> + Send + Sync>;

/// What a consumer adds to the stock composition, threaded down to `app::compose`.
#[derive(Clone, Default)]
pub(crate) struct Ext {
	pub(crate) modules: Vec<ModuleFn>,
	pub(crate) configure: Option<Configure>,
	pub(crate) commands: Vec<(&'static str, Command)>,
}

/// The `mintworks` CLI, extensible: `Host::new().module(f).configure(|b| b).main()`.
#[derive(Default)]
pub struct Host {
	ext: Ext,
}

impl Host {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// A native module installed in every script context, served and under `test`.
	#[must_use]
	pub fn module(mut self, f: ModuleFn) -> Self {
		self.ext.modules.push(f);
		self
	}

	/// Applied to the `AppBuilder` after the stock composition. `Fn`, not `FnOnce`: `test`
	/// rebuilds the application once per case.
	#[must_use]
	pub fn configure(
		mut self,
		f: impl Fn(AppBuilder) -> AppBuilder + Send + Sync + 'static,
	) -> Self {
		self.ext.configure = Some(Arc::new(f));
		self
	}

	/// `mintworks <name> <app-dir> [args…]`: `f` runs once against the built application (store
	/// opened and migrated, job runner started, no listener), with the trailing arguments.
	/// `"test"` is reserved and refused: it is not registered, and a line says so on stderr.
	#[must_use]
	pub fn command(
		mut self,
		name: &'static str,
		f: impl Fn(App, Vec<String>) -> BoxFuture<'static, ClResult<()>> + Send + Sync + 'static,
	) -> Self {
		if name == "test" {
			eprintln!("mintworks: the command name \"test\" is reserved; not registered");
		} else {
			self.ext.commands.push((name, Arc::new(f)));
		}
		self
	}

	/// Runs the registered command `name` against the application in `dir`; `config` as
	/// `app::build` takes it, `None` resolving it from the environment and `<dir>/.env`.
	///
	/// # Errors
	/// A validation error for an unregistered name; whatever building the app or `f` raised.
	pub async fn run_command(
		&self,
		name: &str,
		dir: &Path,
		config: Option<Config>,
		args: Vec<String>,
	) -> ClResult<()> {
		let Some((_, f)) = self.ext.commands.iter().find(|(n, _)| *n == name) else {
			return Err(Error::validation(format!("no command named {name}")));
		};
		let app = app::build(dir, config, &self.ext).await?.build().await?;
		let res = f(app.clone(), args).await;
		app.shutdown_jobs().await;
		res
	}

	/// Runs the application directory's `#[test]` cases; `Ok(false)` when any failed.
	///
	/// # Errors
	/// Whatever compiling or composing the application raised.
	pub async fn run_tests(&self, dir: &Path, filter: Option<&str>) -> ClResult<bool> {
		test::run(dir, filter, &self.ext).await
	}

	/// `mintworks <app-dir>` | `mintworks test <app-dir> [name-filter]` |
	/// `mintworks <command> <app-dir> [args…]` for a [`Host::command`].
	#[must_use]
	pub fn main(self) -> ExitCode {
		let rt = match tokio::runtime::Runtime::new() {
			Ok(rt) => rt,
			Err(e) => {
				eprintln!("Error: {e}");
				return ExitCode::FAILURE;
			}
		};
		let args: Vec<String> = std::env::args().skip(1).collect();
		let argv: Vec<&str> = args.iter().map(String::as_str).collect();
		let res = match argv.as_slice() {
			[name, dir, rest @ ..] if self.ext.commands.iter().any(|(n, _)| n == name) => {
				let rest = rest.iter().map(|a| (*a).to_owned()).collect();
				rt.block_on(self.run_command(name, &PathBuf::from(dir), None, rest))
			}
			["test", dir] | ["test", dir, _] => {
				match rt.block_on(self.run_tests(&PathBuf::from(dir), argv.get(2).copied())) {
					Ok(true) => return ExitCode::SUCCESS,
					Ok(false) => return ExitCode::FAILURE,
					Err(e) => Err(e),
				}
			}
			[dir] => rt.block_on(async {
				app::build(&PathBuf::from(dir), None, &self.ext).await?.run().await
			}),
			_ => {
				let names: Vec<&str> = self.ext.commands.iter().map(|(n, _)| *n).collect();
				eprintln!(
					"usage: mintworks <app-dir> | mintworks test <app-dir> [name-filter] | \
					 mintworks <command> <app-dir> [args...]; commands: {}",
					if names.is_empty() { "(none)".to_owned() } else { names.join(", ") }
				);
				return ExitCode::from(2);
			}
		};
		match res {
			Ok(()) => ExitCode::SUCCESS,
			Err(e) => {
				eprintln!("Error: {e:?}");
				ExitCode::FAILURE
			}
		}
	}
}

// vim: ts=4
