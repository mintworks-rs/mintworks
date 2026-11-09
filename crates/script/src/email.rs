// SPDX-License-Identifier: MPL-2.0
//! `email::send(ctx, #{to, template, lang?, vars?, attachments?, replyTo?, fromName?})` — queues one `SEND_EMAIL`. A row write, so
//! allowed inside `tx::with` and sent only on commit, refused inside a bare `db::tx`. No dedup and
//! no rate limit: trusted code.

use std::sync::Arc;

use mintworks_core::ids::AccountId;
use mintworks_email::{Attachment, SendEmail};
use mintworks_invoice::store::InvoiceStore;
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
/// before `email.template_dir`. `attachments` reference blobs by sha256 (e.g.
/// `invoices::document(ctx, uid).document.sha256`); `replyTo` must be a bare address;
/// `fromName` overrides only the From display name, never the address. `cc` is an array of at
/// most 10 bare addresses (validated by `enqueue`).
/// Init and job handlers (`Actor::System` other than `sys::SOURCE`, per `api::system_only`) may
/// attach any org's blob by sha256; request handlers are confined to their org's blobs and the
/// PDFs of their org's invoices, which live in the seller's org.
#[rune::function]
async fn send(c: Ref<ScriptCtx>, arg: Value) -> Result<Option<i64>, ScriptError> {
	crate::tx::outside_app_tx("email::send")?;
	let app = c.app()?.clone();
	let ctx = c.ctx().clone();
	drop(c);
	let arg = to_json(&arg)?;
	let to = arg["to"].as_str().ok_or_else(|| bad("`to` is required"))?;
	let template = arg["template"].as_str().ok_or_else(|| bad("`template` is required"))?;
	// Single-use links in the first three are minted only by `mintworks_auth::job`, and the invoice
	// mails' download link only by `mintworks_invoice::mail`; a script must not forge one.
	if matches!(
		template,
		"activation" | "password_reset" | "org_invite" | "invoice_mail" | "storno_mail"
	) {
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
	// `[#{sha256, filename, mime}]`, read from the blob store at send time.
	let attachments: Vec<Attachment> = match &arg["attachments"] {
		Json::Null => Vec::new(),
		v => serde_json::from_value(v.clone()).map_err(|e| {
			bad(format!("`attachments` must be [#{{sha256, filename, mime}}]: {e}"))
		})?,
	};
	if crate::api::system_only(&ctx).is_err() && !attachments.is_empty() {
		let org = ctx.org().map_err(ScriptError)?;
		let invoices = app.extensions.get::<Arc<dyn InvoiceStore>>().cloned();
		for a in &attachments {
			let own = app.store.blob_sha_in_org(&a.sha256, org).await.map_err(ScriptError)?
				|| match &invoices {
					Some(s) => {
						s.invoice_document_in_org(&a.sha256, org).await.map_err(ScriptError)?
					}
					None => false,
				};
			if !own {
				return Err(ScriptError(mintworks_core::error::Error::NotFound));
			}
		}
	}
	let opt_str = |key: &str| -> Result<Option<String>, ScriptError> {
		match &arg[key] {
			Json::Null => Ok(None),
			Json::String(s) => Ok(Some(s.clone())),
			_ => Err(bad(format!("`{key}` must be a string"))),
		}
	};
	// `replyTo` is validated by `enqueue`; `fromName` is sanitized when the mail is built.
	let req = SendEmail {
		to,
		template: template.to_owned(),
		lang,
		vars,
		attachments,
		reply_to: opt_str("replyTo")?,
		cc: match &arg["cc"] {
			Json::Null => Vec::new(),
			v => serde_json::from_value(v.clone())
				.map_err(|e| bad(format!("`cc` must be an array of addresses: {e}")))?,
		},
		from_name: opt_str("fromName")?,
	};
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
