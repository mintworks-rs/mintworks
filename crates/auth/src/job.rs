// SPDX-License-Identifier: MPL-2.0
//! `AUTH_LINK_EMAIL` — the two mails whose body *is* a credential.
//!
//! The token is minted **when the job runs**, not when it is queued, so `jobs.payload` never
//! holds a live link. `Runner::terminate` deliberately keeps a `FAILED` payload as the
//! diagnostic, so an SMTP outage outlasting `max_attempts` would otherwise park a live
//! account-takeover credential in the database indefinitely. An account uid and a link kind
//! are neither a credential nor a worse diagnostic.
//!
//! **The token's TTL therefore starts at delivery rather than at enqueue** — the more correct
//! semantic for a link a person receives: a mail delayed an hour in the queue no longer arrives
//! with an hour already spent off a 2 h reset window.
//!
//! Wired by the application with `AppBuilder::jobs(mintworks_auth::job::register)`, next to
//! `mintworks_email::job::register` — the plain `SEND_EMAIL` kind still carries every mail that
//! has no link in it (the welcome mail).

use std::sync::Arc;

use mintworks_core::{
	App, ClResult, Error,
	ids::AccountId,
	job::{Job, Runner},
	store::CoreStore,
	types::Timestamp,
};
use mintworks_email::SendEmail;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::store::Account;
use crate::{activate, register, reset, routes};

pub const KIND: &str = "AUTH_LINK_EMAIL";

/// Which link to mint. The template name follows from it, so the payload cannot ask for a
/// reset token to be rendered into the activation mail.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LinkKind {
	Activation,
	PasswordReset,
}

/// The `jobs.payload` shape for [`KIND`]. Deliberately holds nothing secret.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkMail {
	pub account_uid: String,
	pub kind: LinkKind,
}

/// Queue one link mail. Un-deduplicated for the same reason `mintworks_email::job::enqueue` is:
/// a resend-activation and a second reset request are legitimate repeats.
pub async fn enqueue(
	store: &Arc<dyn CoreStore>,
	account: &Account,
	kind: LinkKind,
) -> ClResult<()> {
	let mail = LinkMail { account_uid: account.uid.as_str().to_owned(), kind };
	let payload = serde_json::to_string(&mail)
		.map_err(|e| Error::internal(format!("cannot serialise {KIND}: {e}")))?;
	// No dedup key, so this cannot collide and the id is of no use here.
	let _id = mintworks_core::job::enqueue(store, KIND, &payload, None, Timestamp::now()).await?;
	Ok(())
}

/// `AppBuilder::jobs(mintworks_auth::job::register)`.
pub fn register(runner: &mut Runner, app: App) {
	runner.register(KIND, move |job| {
		let app = app.clone();
		async move { run(&app, &job).await }
	});
}

async fn run(app: &App, job: &Job) -> ClResult<()> {
	let mail: LinkMail = serde_json::from_str(&job.payload)
		.map_err(|e| Error::validation(format!("bad {KIND} payload: {e}")))?;
	mintworks_email::job::deliver(app, render(app, &mail).await?).await
}

/// Mint the token and build the mail. Public so a test can drive the same path the handler
/// takes rather than reaching behind it for a token.
pub async fn render(app: &App, mail: &LinkMail) -> ClResult<SendEmail> {
	let store = routes::store(app)?;
	let uid = AccountId::parse(&mail.account_uid)
		.map_err(|e| Error::validation(format!("bad {KIND} account uid: {e}")))?;
	let account = store.account_by_uid(&uid).await?.ok_or_else(|| {
		// `Retry::Never`: the account will not come back, so the runner terminates the row
		// on attempt one rather than spending the budget.
		Error::validation(format!("no account {} for a {KIND} job", mail.account_uid))
	})?;
	let base = app.config.base_url.trim_end_matches('/');

	Ok(match mail.kind {
		LinkKind::Activation => SendEmail {
			to: account.email.clone(),
			template: "activation".to_owned(),
			lang: account.locale.clone(),
			vars: json!({
				"name": register::display_name(&account),
				"activation_link":
					format!("{base}/activate?token={}", activate::mint(app, &account).await?),
			}),
		},
		LinkKind::PasswordReset => SendEmail {
			to: account.email.clone(),
			template: "password_reset".to_owned(),
			lang: account.locale.clone(),
			vars: json!({
				"name": register::display_name(&account),
				"reset_link": format!(
					"{base}/reset-password?token={}",
					reset::mint(app, &account).await?
				),
				"expire_hours": reset::TTL_SECONDS / 3600,
			}),
		},
	})
}

// vim: ts=4
