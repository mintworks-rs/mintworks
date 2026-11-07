// SPDX-License-Identifier: MPL-2.0
//! What the jobs of a filing share, so `job.rs` keeps only the orchestration.
//!
//! Each `ensure_*` is the one spelling of a job's kind, payload, key and first run over
//! [`job::ensure`], the only way NAV work is re-queued. **The caller must have proved from the
//! batch or the row that the run is needed.** `wake` also pulls a `PENDING` job forward to that
//! first run, so the supervisor passes `false` and never defeats a backoff or a deferral.
//!
//! The transitions themselves are `NavStore` methods: their guards are the concurrency contract.

use mintworks_core::{
	App,
	job::{self, Ensured},
	prelude::*,
};
use mintworks_invoice::{KIND_NAV_REPORT, invoice_job_key, invoice_job_payload};

use crate::job::{
	KIND_NAV_ADOPT, KIND_NAV_POLL, KIND_NAV_RECONCILE, KIND_NAV_SEND, POLL_FIRST_DELAY_SECS,
	adopt_key, batch_payload, poll_key, reconcile_key, send_key,
};
use crate::store::NavStore;

/// The invoice's `NAV_REPORT`, due now.
pub async fn ensure_report(app: &App, invoice_id: i64, wake: bool) -> ClResult<Ensured> {
	let (payload, key) = (invoice_job_payload(invoice_id), invoice_job_key(invoice_id));
	job::ensure(&app.store, KIND_NAV_REPORT, &payload, &key, Timestamp::now(), wake).await
}

/// The batch's `NAV_SEND`, due now.
pub async fn ensure_send(app: &App, batch_uid: &str, wake: bool) -> ClResult<Ensured> {
	let (payload, key) = (batch_payload(batch_uid), send_key(batch_uid));
	job::ensure(&app.store, KIND_NAV_SEND, &payload, &key, Timestamp::now(), wake).await
}

/// The batch's `NAV_POLL`, due once NAV has had a moment with the transaction.
pub async fn ensure_poll(app: &App, batch_uid: &str, wake: bool) -> ClResult<Ensured> {
	let (payload, key) = (batch_payload(batch_uid), poll_key(batch_uid));
	let at = Timestamp(Timestamp::now().0 + POLL_FIRST_DELAY_SECS);
	job::ensure(&app.store, KIND_NAV_POLL, &payload, &key, at, wake).await
}

/// The batch's `NAV_RECONCILE`, due at `at`: §1.9.2's five minutes run from the POST, which only
/// the caller knows.
pub async fn ensure_reconcile(
	app: &App,
	batch_uid: &str,
	at: Timestamp,
	wake: bool,
) -> ClResult<Ensured> {
	let (payload, key) = (batch_payload(batch_uid), reconcile_key(batch_uid));
	job::ensure(&app.store, KIND_NAV_RECONCILE, &payload, &key, at, wake).await
}

/// The batch's `NAV_ADOPT`, due now.
pub async fn ensure_adopt(app: &App, batch_uid: &str, wake: bool) -> ClResult<Ensured> {
	let (payload, key) = (batch_payload(batch_uid), adopt_key(batch_uid));
	job::ensure(&app.store, KIND_NAV_ADOPT, &payload, &key, Timestamp::now(), wake).await
}

/// Record a reason on an open batch without moving it: `awaiting_operator` counts it, while the
/// job's own retry is untouched.
///
/// Best-effort, like [`archive_reply`]: the caller is on a path with an error in hand that must
/// survive.
pub async fn park(nav: &dyn NavStore, batch_id: i64, code: &str, message: &str) {
	if let Err(e) = nav.record_batch_fault(batch_id, code, message, Timestamp::now()).await {
		tracing::warn!(batch = batch_id, %code, error = %e, "could not record the NAV fault");
	}
}

/// Archives a NAV reply, best-effort, like `mintworks_core::audit::log`.
///
/// The archive is a diagnostic; the reply in hand is the truth. Propagating a failure here
/// would resend a filing NAV has already accepted, or — on the poll path — discard a verdict
/// NAV has already given and re-query it, which a `Retry::Never` mapping strands for good.
pub async fn archive_reply(nav: &dyn NavStore, batch_id: i64, response_xml: &str) {
	if let Err(e) = nav.archive_response(batch_id, response_xml).await {
		tracing::error!(
			batch = batch_id,
			error = %e,
			"could not archive the NAV response; continuing with the reply in hand"
		);
	}
}

// vim: ts=4
