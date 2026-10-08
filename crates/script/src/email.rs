// SPDX-License-Identifier: MPL-2.0
//! `email::send(ctx, #{to, template, lang?, vars})` — queues one `SEND_EMAIL`. A row write, so
//! allowed inside `tx::with` and sent only on commit, refused inside a bare `db::tx`. No dedup and
//! no rate limit: trusted code.

use mintworks_core::ids::AccountId;
use mintworks_email::SendEmail;
use rune::{ContextError, Module, Value, runtime::Ref};
use serde_json::Value as Json;

use crate::{
	ScriptCtx, error,
	value::{ScriptError, to_json},
};

fn bad(msg: impl Into<String>) -> ScriptError {
	ScriptError(error::runtime(format!("email::send: {}", msg.into())))
}

/// `to` is an `acc_` uid — resolved to its address, its locale the default `lang` — or a raw
/// address. Answers the queued job id. Templates resolve per file under `email.app_template_dir`
/// before `email.template_dir`.
#[rune::function]
async fn send(c: Ref<ScriptCtx>, arg: Value) -> Result<Option<i64>, ScriptError> {
	crate::tx::outside_app_tx("email::send")?;
	let app = c.app()?.clone();
	drop(c);
	let arg = to_json(&arg)?;
	let to = arg["to"].as_str().ok_or_else(|| bad("`to` is required"))?;
	let template = arg["template"].as_str().ok_or_else(|| bad("`template` is required"))?;
	// Single-use links in these are minted only by `mintworks_auth::job`; a script must not forge one.
	if matches!(template, "activation" | "password_reset" | "org_invite") {
		return Err(bad(format!("template `{template}` is reserved for the framework")));
	}
	let lang = arg["lang"].as_str().map(str::to_owned);
	let (to, lang) = if to.starts_with("acc_") {
		let uid = AccountId::parse(to).map_err(ScriptError)?;
		let account = mintworks_auth::routes::store(&app)
			.map_err(ScriptError)?
			.account_by_uid(&uid)
			.await
			.map_err(ScriptError)?
			.ok_or_else(|| bad(format!("no account {to}")))?;
		(account.email, lang.unwrap_or(account.locale))
	} else if to.contains('@') {
		(to.to_owned(), lang.unwrap_or_else(|| "en".to_owned()))
	} else {
		return Err(bad("`to` is neither an acc_ uid nor an address"));
	};
	let vars = match &arg["vars"] {
		Json::Null => Json::Object(serde_json::Map::new()),
		v => v.clone(),
	};
	let req = SendEmail { to, template: template.to_owned(), lang, vars };
	mintworks_email::job::enqueue(&app.store, &req).await.map_err(ScriptError)
}

/// # Errors
/// Whatever Rune raises registering the function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["email"])?;
	m.function_meta(send)?;
	Ok(m)
}

// vim: ts=4
