// SPDX-License-Identifier: MPL-2.0
//! `time::now()` — unix seconds. Calendar math stays in SQL; this is only the clock, so that
//! `test::set_now` can pin it.

use mintworks_core::types::Timestamp;
use rune::{ContextError, Module};

/// `time::now() -> i64`: the pinned instant under `mintworks test` after `test::set_now`, else
/// the wall clock.
#[rune::function]
fn now() -> i64 {
	crate::testing::now_override().unwrap_or_else(|| Timestamp::now().0)
}

/// # Errors
/// Whatever Rune raises registering the function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["time"])?;
	m.function_meta(now)?;
	Ok(m)
}

// vim: ts=4
