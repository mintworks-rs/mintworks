//! Transactional email: handlebars templates on disk, SMTP delivery through a retrying
//! `SEND_EMAIL` job.
//!
//! Producers never render and never talk to SMTP. They enqueue a [`SendEmail`] with
//! [`job::enqueue`]; the handler renders and delivers at fire time, so a template fix or
//! an SMTP credential change reaches jobs that are already queued, and every transient
//! delivery failure is retried by `saas-core`'s job runner rather than lost inline.
//!
//! An application calls [`check_email_settings`] from `AppBuilder::on_init`, alongside
//! `AppBuilder::jobs(saas_email::job::register)`.

#![forbid(unsafe_code)]

pub mod job;
pub(crate) mod sender;
pub(crate) mod template;

pub use job::{deliver, enqueue, register};

use saas_core::prelude::*;
use serde::{Deserialize, Serialize};

/// Refuse to start without the settings a delivery needs. Call it from
/// `AppBuilder::on_init`, alongside `AppBuilder::jobs(saas_email::job::register)`.
///
/// `sender::send` used to answer `Ok(())` with no SMTP host, so the job reached `DONE`, its
/// payload was blanked and the activation link was gone — a deployment could drop every mail
/// it sent and no alert would say so.
///
/// `check_required` parses every declared `email.*` key. The two conditions it cannot express
/// are checked here: a directory's existence is not a property of a string, and the SMTP
/// password lives in `secrets`, outside the settings registry.
pub async fn check_email_settings(app: &saas_core::app::App) -> ClResult<()> {
	app.settings.check_required("email.").await?;

	let dir = app.settings.text("email.template_dir").await?;
	if !std::path::Path::new(&dir).is_dir() {
		return Err(Error::Setting(format!(
			"setting 'email.template_dir': '{dir}' is not a directory"
		)));
	}

	// The same condition `sender::checked_credentials` tests at send time, moved to boot: a
	// `MASTER_KEY` rotation that lost the row is caught before any mail queues behind it.
	if !app.settings.text("email.smtp.username").await?.is_empty() {
		let password = app.secrets.get("smtp.password").await?;
		if !password.is_some_and(|p| !p.is_empty() && std::str::from_utf8(&p).is_ok()) {
			return Err(Error::Setting(
				"secret 'smtp.password' is unset, empty or not UTF-8, but \
				 'email.smtp.username' is configured"
					.to_owned(),
			));
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

// vim: ts=4
