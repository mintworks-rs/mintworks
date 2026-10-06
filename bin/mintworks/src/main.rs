// SPDX-License-Identifier: MPL-2.0
#![forbid(unsafe_code)]
//! `mintworks <dir>` — the whole application is the Rune sources in `<dir>`.
//!
//! There is no hot reload: every `.rn` compiles at startup and any error refuses to serve, so a
//! restart is the reload.

mod app;
mod test;

use std::path::PathBuf;

use mintworks_core::ClResult;

#[tokio::main]
async fn main() -> ClResult<()> {
	let args: Vec<String> = std::env::args().skip(1).collect();
	let argv: Vec<&str> = args.iter().map(String::as_str).collect();
	match argv.as_slice() {
		["test", dir] | ["test", dir, _] => {
			let filter = argv.get(2).copied();
			if !test::run(&PathBuf::from(dir), filter).await? {
				std::process::exit(1);
			}
			Ok(())
		}
		[dir] => app::build(&PathBuf::from(dir), None).await?.run().await,
		_ => {
			eprintln!("usage: mintworks <app-dir> | mintworks test <app-dir> [name-filter]");
			std::process::exit(2);
		}
	}
}

// vim: ts=4
