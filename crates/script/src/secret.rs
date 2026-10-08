// SPDX-License-Identifier: MPL-2.0
//! `secret::get(ctx, key)` — a secret this app declared with `app.secret`. Only `app.*` keys the
//! app declared are readable, so a framework secret (`auth.jwt_key`, `nav.sign_key`) is not.

use std::sync::Arc;

use mintworks_core::settings::env_name;
use rune::{ContextError, Module, Value, runtime::Ref};
use serde_json::Value as Json;

use crate::{
	ScriptCtx, ScriptRuntime, error,
	io::TestEnv,
	tx,
	value::{ScriptError, from_json},
};

/// # Errors
/// Whatever Rune raises registering the function.
pub(crate) fn module(test_env: TestEnv) -> Result<Module, ContextError> {
	let mut m = Module::with_item(["secret"])?;
	m.function("get", move |c: Ref<ScriptCtx>, key: String| {
		let test_env = Arc::clone(&test_env);
		let app = c.app().cloned();
		drop(c);
		async move {
			tx::outside_tx("secret::")?;
			let app = app?;
			let declared = app
				.extensions
				.get::<ScriptRuntime>()
				.is_some_and(|rt| rt.secrets.contains(&key));
			if !key.starts_with("app.") || !declared {
				return Err(ScriptError(error::runtime(format!(
					"{key}: not a secret this app declared with app.secret"
				))));
			}
			// Under a suite `app.test_env` stands in for the process environment, which is the
			// layer that wins over the row anyway.
			let planted = test_env.get().and_then(|t| t.get(&env_name(&key)).cloned());
			let value = match planted.filter(|v| !v.trim().is_empty()) {
				Some(v) => Some(v),
				None => app
					.secrets
					.get(&key)
					.await
					.map_err(ScriptError)?
					.map(|b| String::from_utf8_lossy(&b).into_owned()),
			};
			match value {
				Some(v) => from_json(&Json::String(v)).map_err(ScriptError),
				None => Ok::<_, ScriptError>(Value::from(())),
			}
		}
	})
	.build()?;
	Ok(m)
}

// vim: ts=4
