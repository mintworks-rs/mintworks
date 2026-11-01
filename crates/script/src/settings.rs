// SPDX-License-Identifier: MPL-2.0
//! `settings::get(ctx, key, #{org?, account?})` and `settings::set(ctx, key, value|(), #{org?,
//! account?})` — an `app.*` key at one level of its scope's chain. Who may read or write which
//! level is `mintworks_core::settings::{read_scoped, write_scoped}`'s rule, not this module's; the
//! global level is readable by any caller.

use mintworks_core::{
	App,
	error::Error,
	ids::AccountId,
	settings::{read_scoped, write_scoped},
};
use rune::{ContextError, Module, Value, runtime::Ref};

use crate::{
	ScriptCtx,
	error::{R, bad},
	tx,
	value::{ScriptError, from_json, to_json},
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Level {
	#[serde(default)]
	org: Option<String>,
	#[serde(default)]
	account: Option<String>,
}

/// The internal ids behind `#{org?, account?}`; an unknown uid is `E-CORE-NOTFOUND`.
async fn level(app: &App, key: &str, at: &Value) -> R<(Option<i64>, Option<i64>)> {
	if !key.starts_with("app.") {
		return Err(bad(format!("{key}: settings:: reaches app.* keys only")));
	}
	let json = to_json(at).map_err(ScriptError)?;
	let l: Level = serde_json::from_value(json).map_err(|e| bad(format!("settings: {e}")))?;
	let store = mintworks_auth::routes::store(app).map_err(ScriptError)?;
	let org = match l.org.as_deref() {
		None => None,
		Some(u) => Some(crate::value::org_id(app, u).await?),
	};
	let account = match l.account.as_deref() {
		None => None,
		Some(u) => {
			let uid = AccountId::parse(u).map_err(ScriptError)?;
			Some(
				store
					.account_by_uid(&uid)
					.await
					.map_err(ScriptError)?
					.ok_or(Error::NotFound)?
					.id,
			)
		}
	};
	Ok((org, account))
}

/// `settings::get(ctx, key, #{org?, account?})` — the value read from that level, resolved
/// along the key's own chain down to its default.
#[rune::function]
async fn get(c: Ref<ScriptCtx>, key: Ref<str>, at: Value) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let (org, account) = level(&app, &key, &at).await?;
	let value = read_scoped(&app, &ctx, &key, org, account).await.map_err(ScriptError)?;
	let json = serde_json::to_value(&value).map_err(|e| bad(format!("settings::get: {e}")))?;
	from_json(&json).map_err(ScriptError)
}

/// `settings::set(ctx, key, value|(), #{org?, account?})` — writes that level's row; `()`
/// deletes it, so the read falls through to the next level. Refused inside any transaction: the
/// cache is invalidated before commit, so a rollback would leave a phantom value cached.
#[rune::function]
async fn set(c: Ref<ScriptCtx>, key: Ref<str>, value: Value, at: Value) -> R<()> {
	tx::outside_tx("settings::set")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let (org, account) = level(&app, &key, &at).await?;
	let json = to_json(&value).map_err(ScriptError)?;
	let raw = if json.is_null() {
		None
	} else {
		let ty = app.settings.registry().definition(&key).map_err(ScriptError)?.ty;
		Some(mintworks_core::settings::raw_value(ty, &json).map_err(ScriptError)?)
	};
	write_scoped(&app, &ctx, &key, raw.as_deref(), org, account)
		.await
		.map_err(ScriptError)
}

/// # Errors
/// Whatever Rune raises registering the functions.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["settings"])?;
	m.function_meta(get)?;
	m.function_meta(set)?;
	Ok(m)
}

// vim: ts=4
