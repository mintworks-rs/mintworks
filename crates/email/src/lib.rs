// SPDX-License-Identifier: MPL-2.0
//! Transactional email: handlebars templates on disk, SMTP delivery through a retrying
//! `SEND_EMAIL` job.
//!
//! Producers never render and never talk to SMTP. They enqueue a [`SendEmail`] with
//! [`job::enqueue`]; the handler renders and delivers at fire time, so a template fix or
//! an SMTP credential change reaches jobs that are already queued, and every transient
//! delivery failure is retried by `mintworks-core`'s job runner rather than lost inline.
//!
//! An application calls [`check_email_settings`] from `AppBuilder::on_init`, alongside
//! `AppBuilder::jobs(mintworks_email::job::register)`.

#![forbid(unsafe_code)]

pub mod job;
pub(crate) mod sender;
pub(crate) mod template;

pub use job::{deliver, enqueue, register};

use mintworks_core::prelude::*;
use serde::{Deserialize, Serialize};

/// Namespaced like the `email.*` settings beside it, so one `EMAIL_SMTP_*` family covers the
/// username and the password. Declared for the registry in [`SECRETS`].
pub(crate) const PASSWORD_SECRET: &str = "email.smtp.password";

/// Refuse to start without the settings a delivery needs. Call it from
/// `AppBuilder::on_init`, alongside `AppBuilder::jobs(mintworks_email::job::register)`.
///
/// `sender::send` used to answer `Ok(())` with no SMTP host, so the job reached `DONE`, its
/// payload was blanked and the activation link was gone — a deployment could drop every mail
/// it sent and no alert would say so.
///
/// Only the two conditions a declaration cannot express: a directory's existence is not a
/// property of a string, and the SMTP password lives in `secrets`, which carries names but no
/// values to validate. Everything else is [`SETTINGS`], which `AppBuilder::build` checks itself.
///
/// # Errors
/// `E-CORE-SETTING` for a missing template directory or an unusable password secret.
pub async fn check_email_settings(app: &mintworks_core::app::App) -> ClResult<()> {
	let dir = app.settings.text("email.template_dir").await?;
	if !std::path::Path::new(&dir).is_dir() {
		return Err(Error::Setting(format!(
			"setting 'email.template_dir': '{dir}' is not a directory"
		)));
	}

	// The same condition `sender::checked_credentials` tests at send time, moved to boot: a
	// `MASTER_KEY` rotation that lost the row is caught before any mail queues behind it.
	if !app.settings.text("email.smtp.username").await?.is_empty() {
		let password = app.secrets.get(PASSWORD_SECRET).await?;
		if !password.is_some_and(|p| !p.is_empty() && std::str::from_utf8(&p).is_ok()) {
			return Err(Error::Setting(format!(
				"secret '{PASSWORD_SECRET}' is unset, empty or not UTF-8, but \
				 'email.smtp.username' is configured"
			)));
		}
	}
	Ok(())
}

/// One queued email. This is the `jobs.payload` shape for kind `SEND_EMAIL`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SendEmail {
	pub to: String,
	/// Template base name: `activation` resolves to `activation.{html,txt}.hbs` under the
	/// `email.template_dir` setting.
	pub template: String,
	/// Locale infix. `hu` picks `activation.hu.html.hbs`; anything without its own file
	/// falls back to the un-infixed English template.
	pub lang: String,
	/// Template variables, normally a JSON object. The handler adds `base_url` unless the
	/// producer set it.
	pub vars: serde_json::Value,
}

use mintworks_core::settings::{SettingDef, check_address};

/// This crate's declared settings, registered with
/// `AppBuilder::settings(mintworks_email::SETTINGS)`.
pub static SETTINGS: &[SettingDef] = &[
	SettingDef::text("email.from", "", "Sender address on every transactional mail.")
		.required()
		.check(check_address),
	SettingDef::text("email.from.name", "", "Display name beside email.from."),
	SettingDef::text("email.smtp.host", "", "SMTP host.").required(),
	SettingDef::int("email.smtp.port", "587", "SMTP port.").range(1, 65535),
	SettingDef::text("email.smtp.username", "", "SMTP username; blank means no authentication."),
	SettingDef::choice(
		"email.smtp.tls_mode",
		&["none", "starttls", "tls"],
		"starttls",
		"How the SMTP connection is secured.",
	),
	SettingDef::int("email.smtp.timeout_seconds", "30", "Seconds a delivery attempt may take.")
		.range(1, 600),
	SettingDef::text(
		"email.template_dir",
		"./templates/email",
		"Directory holding the handlebars mail templates.",
	),
	// A mail waits out a misconfiguration rather than dying on attempt one: nothing re-drives a
	// `SEND_EMAIL` row, so a terminal failure here loses an activation link for good. 14 attempts
	// under the 3600 s cap is roughly half a day.
	SettingDef::int("jobs.max_attempts.SEND_EMAIL", "14", "Attempts before a queued mail is lost.")
		.range(0, 1_000),
];

/// Declared so the unprefixed environment namespace cannot collide with a setting's variable.
pub static SECRETS: &[&str] = &[PASSWORD_SECRET];

// vim: ts=4
