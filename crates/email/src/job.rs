//! The `SEND_EMAIL` job: the only way mail leaves this framework.
//!
//! `mintworks-core` reserves the kind but registers no handler; the application wires this one
//! in with `AppBuilder::jobs(mintworks_email::job::register)`. Retry, backoff and the terminal
//! `FAILED` state all belong to `mintworks_core::job::Runner`.

use std::{path::PathBuf, sync::Arc};

use mintworks_core::{
	App, ClResult, Error,
	job::{Job, Runner},
	store::CoreStore,
	types::Timestamp,
};
use serde_json::Value;

use crate::{SendEmail, sender, template};

pub const KIND: &str = "SEND_EMAIL";

/// Queues one mail. Deliberately un-deduplicated: resend-activation and a second password
/// reset are legitimate repeats of an identical payload.
pub async fn enqueue(store: &Arc<dyn CoreStore>, req: &SendEmail) -> ClResult<Option<i64>> {
	let payload = serde_json::to_string(req)
		.map_err(|e| Error::internal(format!("cannot serialise {KIND}: {e}")))?;
	mintworks_core::job::enqueue(store, KIND, &payload, None, Timestamp::now()).await
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
		map.entry("base_url")
			.or_insert_with(|| Value::String(app.config.base_url.clone()));
	}
	let out = template::render(&dir, &req.template, &req.lang, &vars)?;
	sender::send(
		app,
		&sender::Message { to: req.to, subject: out.subject, text: out.text, html: out.html },
	)
	.await
}

// vim: ts=4
