// SPDX-License-Identifier: MPL-2.0
//! The `SEND_EMAIL` job: the only way mail leaves this framework.
//!
//! `mintworks-core` reserves the kind but registers no handler; the application wires this one
//! in with `AppBuilder::jobs(mintworks_email::job::register)`. Retry, backoff and the terminal
//! `FAILED` state all belong to `mintworks_core::job::Runner`.

use std::{path::PathBuf, sync::Arc};

use mintworks_core::{
	App, ClResult, Error,
	blob::{blob_path, sha_path},
	error::StatusCode,
	job::{Job, Runner},
	store::CoreStore,
	types::Timestamp,
};
use serde_json::Value;

use crate::{Attachment, SendEmail, sender, template};

pub const KIND: &str = "SEND_EMAIL";

/// Queues one mail. Deliberately un-deduplicated: resend-activation and a second password
/// reset are legitimate repeats of an identical payload.
///
/// # Errors
/// `Error::Validation` when `reply_to` is set but is not a bare email address.
pub async fn enqueue(store: &Arc<dyn CoreStore>, req: &SendEmail) -> ClResult<Option<i64>> {
	validate(req)?;
	let payload = serde_json::to_string(req)
		.map_err(|e| Error::internal(format!("cannot serialise {KIND}: {e}")))?;
	mintworks_core::job::enqueue(store, KIND, &payload, None, Timestamp::now()).await
}

/// The most Cc addresses one mail carries.
pub const MAX_CC: usize = 10;

/// What `enqueue` can refuse up front, so a bad producer value raises instead of burning a job.
fn validate(req: &SendEmail) -> ClResult<()> {
	if let Some(r) = &req.reply_to
		&& r.parse::<lettre::Address>().is_err()
	{
		return Err(Error::validation(format!("reply_to is not an email address: '{r}'")));
	}
	if req.cc.len() > MAX_CC {
		return Err(Error::validation(format!("at most {MAX_CC} cc addresses")));
	}
	if let Some(c) = req.cc.iter().find(|c| c.parse::<lettre::Address>().is_err()) {
		return Err(Error::validation(format!("cc is not an email address: '{c}'")));
	}
	Ok(())
}

/// Reads every attachment of one mail, refusing more than `cap` bytes in total.
///
/// A missing blob is `E-EMAIL-ATTACHMENT-MISSING` and **retryable**: an app may enqueue before
/// the PDF it attaches has finished rendering. Over the cap is `E-EMAIL-ATTACHMENT-TOO-LARGE`,
/// permanent — the mail fails visibly rather than going out without the file.
///
/// Falls back to `documents/…/{sha}.pdf`, where invoice PDFs rendered before they moved into
/// `blobs/` still live (the same fallback as `mintworks_invoice`'s `Invoices::document`).
pub(crate) fn load_attachments(
	data_dir: &str,
	atts: &[Attachment],
	cap: u64,
) -> ClResult<Vec<(String, String, Vec<u8>)>> {
	let mut out = Vec::with_capacity(atts.len());
	let mut total = 0u64;
	for a in atts {
		// The sha becomes a path: anything but 64 hex digits could walk out of `data_dir`.
		if a.sha256.len() != 64 || !a.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
			return Err(Error::validation(format!("attachment '{}': bad sha256", a.filename)));
		}
		let blob = blob_path(data_dir, &a.sha256)?;
		let path =
			if blob.is_file() { blob } else { sha_path(data_dir, "documents", &a.sha256, ".pdf")? };
		let io = |e: std::io::Error| {
			if e.kind() == std::io::ErrorKind::NotFound {
				Error::coded_retry(
					StatusCode::SERVICE_UNAVAILABLE,
					"E-EMAIL-ATTACHMENT-MISSING",
					format!("attachment '{}': blob {} not found", a.filename, a.sha256),
				)
			} else {
				Error::Unavailable(format!("attachment '{}': {e}", a.filename))
			}
		};
		total += std::fs::metadata(&path).map_err(io)?.len();
		if total > cap {
			return Err(Error::coded(
				StatusCode::PAYLOAD_TOO_LARGE,
				"E-EMAIL-ATTACHMENT-TOO-LARGE",
				format!("attachments exceed email.max_attachment_bytes ({cap})"),
			));
		}
		out.push((a.filename.clone(), a.mime.clone(), std::fs::read(&path).map_err(io)?));
	}
	Ok(out)
}

/// `AppBuilder::jobs(mintworks_email::job::register)`.
pub fn register(runner: &mut Runner, app: App) {
	runner.register(KIND, move |job| {
		let app = app.clone();
		async move { run(&app, &job).await }
	});
}

async fn run(app: &App, job: &Job) -> ClResult<()> {
	let req: SendEmail = serde_json::from_str(&job.payload)
		.map_err(|e| Error::validation(format!("bad {KIND} payload: {e}")))?;
	deliver(app, req).await
}

/// Render one [`SendEmail`] and hand it to SMTP.
///
/// Public because [`KIND`] is not the only producer: a mail whose body contains a live
/// single-use link must not have that link sitting in `jobs.payload` — `mintworks_auth::job` keeps
/// an account uid there instead, mints the token when its own handler runs, and finishes
/// through here.
pub async fn deliver(app: &App, req: SendEmail) -> ClResult<()> {
	// `retryable`: a bad `email.template_dir` must delay this mail, not destroy it — the read
	// parses, so it raises `Error::Setting`, which is `Retry::Never`.
	let dir =
		PathBuf::from(app.settings.text("email.template_dir").await.map_err(sender::retryable)?);
	let mut vars = req.vars;
	if let Value::Object(map) = &mut vars {
		map.insert("base_url".into(), Value::String(app.config.base_url.clone()));
	}
	let app_dir = app.settings.text("email.app_template_dir").await.map_err(sender::retryable)?;
	let dirs: Vec<PathBuf> = [app_dir.trim()]
		.into_iter()
		.filter(|d| !d.is_empty())
		.map(PathBuf::from)
		.chain([dir])
		.collect();
	let out = template::render_in(&dirs, &req.template, &req.lang, &vars)?;
	let attachments = if req.attachments.is_empty() {
		Vec::new()
	} else {
		let cap = app
			.settings
			.int("email.max_attachment_bytes")
			.await
			.map_err(sender::retryable)?;
		let (dir, atts) = (app.config.data_dir.clone(), req.attachments.clone());
		let cap = u64::try_from(cap).unwrap_or(0);
		tokio::task::spawn_blocking(move || load_attachments(&dir, &atts, cap))
			.await
			.map_err(|e| Error::internal(format!("mintworks-email: attachment task: {e}")))??
	};
	sender::send(
		app,
		&sender::Message {
			to: req.to,
			subject: out.subject,
			text: out.text,
			html: out.html,
			attachments,
			reply_to: req.reply_to,
			cc: req.cc,
			from_name: req.from_name,
		},
	)
	.await
}

#[cfg(test)]
mod tests {
	use mintworks_core::Retry;

	use super::{load_attachments, validate};
	use crate::{Attachment, SendEmail};

	const SHA: &str = "ab12000000000000000000000000000000000000000000000000000000000000";

	fn att(sha: &str) -> Attachment {
		Attachment {
			sha256: sha.to_owned(),
			filename: "invoice.pdf".to_owned(),
			mime: "application/pdf".to_owned(),
		}
	}

	#[test]
	fn attachments_load_by_sha_and_fail_cleanly() {
		let dir = std::env::temp_dir().join(format!("mw-email-att-{}", std::process::id()));
		let data_dir = dir.to_str().unwrap();
		let path = mintworks_core::blob::blob_path(data_dir, SHA).unwrap();
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(&path, b"%PDF-1.7").unwrap();

		let got = load_attachments(data_dir, &[att(SHA)], 8).unwrap();
		assert_eq!(
			got,
			vec![("invoice.pdf".into(), "application/pdf".into(), b"%PDF-1.7".to_vec())]
		);

		let over = load_attachments(data_dir, &[att(SHA)], 7).unwrap_err();
		assert_eq!(over.retry(), Retry::Never, "{over}");
		assert_eq!(over.parts().1, "E-EMAIL-ATTACHMENT-TOO-LARGE", "{over}");

		let missing =
			load_attachments(data_dir, &[att(&SHA.replace("ab12", "cd34"))], 8).unwrap_err();
		assert_eq!(missing.retry(), Retry::Backoff, "{missing}");
		assert_eq!(missing.parts().1, "E-EMAIL-ATTACHMENT-MISSING", "{missing}");

		load_attachments(data_dir, &[att("../../../../etc/passwd")], 8).unwrap_err();
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn reply_to_must_be_an_address() {
		let req =
			|r: Option<&str>| SendEmail { reply_to: r.map(str::to_owned), ..SendEmail::default() };
		validate(&req(None)).unwrap();
		validate(&req(Some("billing@tenant.example"))).unwrap();
		validate(&req(Some(""))).unwrap_err();
		validate(&req(Some("a@b.c\r\nBcc: x@y.z"))).unwrap_err();
	}
}

// vim: ts=4
