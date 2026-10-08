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
	process::ExitCode,
	sync::Arc,
};

use mintworks_core::{AppBuilder, ClResult};
pub use mintworks_script::ModuleFn;

type Configure = Arc<dyn Fn(AppBuilder) -> AppBuilder + Send + Sync>;

/// What a consumer adds to the stock composition, threaded down to `app::compose`.
#[derive(Clone, Default)]
pub(crate) struct Ext {
	pub(crate) modules: Vec<ModuleFn>,
	pub(crate) configure: Option<Configure>,
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

	/// Runs the application directory's `#[test]` cases; `Ok(false)` when any failed.
	///
	/// # Errors
	/// Whatever compiling or composing the application raised.
	pub async fn run_tests(&self, dir: &Path, filter: Option<&str>) -> ClResult<bool> {
		test::run(dir, filter, &self.ext).await
	}

	/// `mintworks <app-dir>` | `mintworks test <app-dir> [name-filter]`.
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
				eprintln!("usage: mintworks <app-dir> | mintworks test <app-dir> [name-filter]");
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
