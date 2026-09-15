//! The two background jobs. Nothing here is reachable from the invoice issue path: issue
//! enqueues `NAV_REPORT` and returns, so a NAV outage can never block or fail an invoice.
//!
//! **Retry lives on the `jobs` row and nowhere else.** Every failure propagates as `Err`;
//! `Runner::tick` reads `Error::retry()` and either backs off or terminates the chain.
//! `nav_submissions` keeps no attempt count, no next-try time and no state — one row per
//! `(invoice, operation)`, updated in place, carrying NAV's verdict once NAV has one.
//!
//! Two things make an unbounded retry of a statutory filing safe:
//!
//! * The NAV `requestId` is the invoice's own uid, stable across every attempt, and NAV
//!   refuses a request id it has already processed (NAV interface spec HU v3.0). A resend
//!   cannot become a second filing.
//! * [`may_send`] refuses an invoice whose row already carries a verdict or a
//!   `transactionId`, so once NAV holds the filing the retry stops sending and starts polling.
//!
//! So nothing here gives up: `jobs.max_attempts.NAV_REPORT` and `.NAV_POLL` are `0`.
//! `jobs.alert_after.NAV_POLL` is what tells a human, and `Nav::cancel_filing` is how one
//! stops a filing that can never succeed.
//!
//! NAV's verdict on the invoice itself is not available on `manageInvoice` at all: it arrives
//! on the poll path as `invoiceStatus = ABORTED`.

use saas_core::{
	App, Retry,
	job::{self, Job, Runner},
	prelude::*,
};
use saas_invoice::{InvoiceKind, InvoiceStore, KIND_NAV_REPORT};
use serde::Deserialize;

use crate::auth;
use crate::auth::NavAuth;
use crate::client::{self, Accepted, Outcome, accepted, outcome};
use crate::service_api::Nav;
use crate::store::NavStore;
use crate::submission::{NavOp, NavVerdict};
use crate::xml::invoice_data;

pub const KIND_NAV_POLL: &str = "NAV_POLL";
pub const KIND_NAV_SWEEP: &str = "NAV_SWEEP";

/// How often `NAV_SWEEP` runs. An hour: a missed statutory filing is measured in days, and
/// every sweep is two indexed reads.
const SWEEP_EVERY_SECS: i64 = 3600;

/// How many never-enqueued filings one sweep tick picks up. A literal rather than a setting:
/// the backlog it drains is a crash window, not a backlog anybody tunes.
const SWEEP_BATCH: i64 = 100;

/// How long after `manageInvoice` the first `queryTransactionStatus` runs. Every poll after it
/// is the job runner's own backoff up to `jobs.backoff_cap.NAV_POLL`.
const POLL_FIRST_DELAY_SECS: i64 = 5;

/// The `NAV_REPORT` payload `saas-invoice` enqueues (`issue.rs::enqueue_jobs`).
#[derive(Deserialize)]
struct ReportPayload {
	#[serde(rename = "invoiceId")]
	invoice_id: i64,
}

#[derive(Deserialize)]
struct PollPayload {
	#[serde(rename = "submissionId")]
	submission_id: i64,
}

/// The three handlers deserialize their payload and make exactly one call into [`Nav`],
/// which is the same rule a route handler follows. `Nav::new` resolves both stores from
/// `app.extensions`, so nothing is threaded in here.
pub fn register(runner: &mut Runner, app: App) {
	let handle = Nav::new(app);

	let h = handle.clone();
	runner.register(KIND_NAV_REPORT, move |job: Job| {
		let h = h.clone();
		async move {
			let p: ReportPayload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("saas-nav: bad {KIND_NAV_REPORT} payload: {e}"))
			})?;
			h.run_report(p.invoice_id).await
		}
	});

	let h = handle.clone();
	runner.register(KIND_NAV_POLL, move |job: Job| {
		let h = h.clone();
		async move {
			let p: PollPayload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("saas-nav: bad {KIND_NAV_POLL} payload: {e}"))
			})?;
			h.run_poll(p.submission_id).await
		}
	});

	runner.register_periodic(KIND_NAV_SWEEP, SWEEP_EVERY_SECS, move |_job| {
		let h = handle.clone();
		async move { h.run_sweep().await }
	});
}

/// Seeds the periodic sweep, and refuses to start on a `software` block NAV would reject.
/// Call once at boot from `AppBuilder::on_init`, next to `saas_invoice::draft::seed`.
///
/// The settings check belongs here rather than at filing time: a missing `nav.software_*`
/// key makes every `manageInvoice` fail on a schema error, and a faulted request is
/// retryable, so the failure is otherwise invisible and permanent.
pub async fn seed(app: &App) -> ClResult<()> {
	auth::check_software_settings(app).await?;
	auth::check_seller(app).await?;
	job::seed_periodic(&app.store, KIND_NAV_SWEEP).await
}

/// Whether a rejected re-enqueue is the one that needs a person, given the status of the row
/// holding the `nav:invoice:{id}` key.
///
/// `report` opens its `nav_submissions` row only after a full `tokenExchange`, so during a NAV
/// or token outage every in-flight filing matches `UNFILED` while its key is live — and the
/// one alarm that means "act" became hourly noise exactly when an operator was watching.
#[must_use]
pub fn needs_operator(status: Option<&str>) -> bool {
	!matches!(status, Some("PENDING" | "RUNNING"))
}

/// Enqueue the filings that were never enqueued **at all**: an issued invoice with no
/// `nav_submissions` row, which is the at-most-once gap at
/// `saas_invoice::issue::enqueue_jobs` — a crash between COMMIT and enqueue.
///
/// That is the whole job now. An invoice whose `NAV_REPORT` *was* enqueued is a job row the
/// runner retries on its own backoff, unbounded, so re-driving it from here would file it
/// twice; `dedup_key` survives termination, so the hourly stampede the two old circuit
/// breakers contained cannot happen; and a stranded poll is re-enqueued by [`may_send`] on
/// the report's own retry rather than by a scan here.
///
/// **Nothing in here propagates.** The signature stays `ClResult<()>` because that is the
/// runner's handler type, but every failure logs and gives up on this tick: the sweep is
/// periodic, and nothing re-seeds a periodic kind except `seed_periodic`, which runs at boot.
pub async fn sweep(app: &App, _invoices: &dyn InvoiceStore, nav: &dyn NavStore) -> ClResult<()> {
	let now = Timestamp::now();
	let seller = saas_invoice::SELLER_ID;

	// The one thing no retry can fix. Loud once a tick rather than one `error!` at the moment
	// NAV said so and silence afterwards; `service_api::alerts` raises `A-NAV-REJECTED` off the
	// same count.
	match nav.awaiting_operator(seller).await {
		Err(e) => tracing::error!(error = %e, "could not count rejected filings"),
		Ok(0) => {}
		Ok(rejected) => tracing::error!(
			invoices = rejected,
			"issued invoices ended without a NAV acceptance and need a person: either NAV \
			 rejected them, or their status with NAV is unknown — check each before acting"
		),
	}

	let unfiled = match nav.unfiled_invoices(seller, SWEEP_BATCH).await {
		Ok(ids) => ids,
		Err(e) => {
			tracing::error!(error = %e, "could not read unfiled invoices; skipping them this tick");
			Vec::new()
		}
	};
	for invoice_id in unfiled {
		let payload = saas_invoice::invoice_job_payload(invoice_id);
		let key = format!("nav:invoice:{invoice_id}");
		match job::enqueue(&app.store, KIND_NAV_REPORT, &payload, Some(&key), now).await {
			Err(e) => {
				tracing::error!(invoice = invoice_id, error = %e, "could not enqueue a NAV filing");
			}
			Ok(Some(_)) => {
				tracing::warn!(
					invoice = invoice_id,
					"an issued invoice had no NAV filing; enqueued one"
				);
			}
			// The key survives termination by design, so only a *spent* key needs a person: a
			// live one is the ordinary case, and `report` opens the `nav_submissions` row only
			// after `tokenExchange`, so every NAV outage alarmed hourly.
			Ok(None) => {
				let status = app.store.job_status_by_key(&key).await.ok().flatten();
				if needs_operator(status.as_deref()) {
					tracing::error!(
						invoice = invoice_id,
						"an issued invoice has no NAV filing and its NAV_REPORT key is already \
						 spent; it needs an operator re-drive"
					);
				} else {
					tracing::debug!(
						invoice = invoice_id,
						"a NAV filing is already in flight for this invoice"
					);
				}
			}
		}
	}
	Ok(())
}

/// Build the invoice's `invoiceData`, submit it, and hand the `transactionId` to `NAV_POLL`.
///
/// The `NAV_REPORT` handler, public so it can be driven directly by a test: `Runner::tick` is
/// private and `run` never returns. Calling it twice for one invoice is safe — see
/// [`may_send`], which is the whole point.
pub async fn report(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	invoice_id: i64,
) -> ClResult<()> {
	if !may_send(app, nav, invoice_id).await? {
		return Ok(());
	}
	let invoice = invoices
		.invoice_by_id(invoice_id)
		.await?
		.ok_or_else(|| Error::internal(format!("saas-nav: invoice {invoice_id} is gone")))?;
	let (seller, lines, groups) = tokio::try_join!(
		invoices.seller_by_id(invoice.seller_id),
		invoices.invoice_lines(invoice.id),
		invoices.invoice_vat_groups(invoice.id),
	)?;
	let seller = seller.ok_or_else(|| Error::internal("saas-nav: the invoice's seller is gone"))?;

	let (op, original) = match invoice.kind {
		InvoiceKind::Storno => {
			let id = invoice
				.original_invoice_id
				.ok_or_else(|| Error::internal("saas-nav: storno with no original"))?;
			let original =
				invoices.invoice_by_id(id).await?.and_then(|i| i.number).ok_or_else(|| {
					Error::internal("saas-nav: the stornoed invoice has no number")
				})?;

			// A STORNO must not reach NAV before the invoice it cancels: both jobs share a
			// `run_at` and two workers can send the storno first, which NAV answers `ABORTED`
			// and `may_send` then refuses to resend forever. `Unavailable` and *before*
			// `create_submission`, so the storno holds no row while the backoff retries it.
			match nav.submission_by_invoice(id).await?.and_then(|s| s.verdict) {
				Some(NavVerdict::Done | NavVerdict::Warn) => {}
				// `REJECTED` will never become `DONE`, so this storno waits forever and shows
				// up in `awaiting_operator`'s count. That is correct: a storno of an invoice
				// NAV never accepted needs a person, and `Nav::cancel_filing` stops the job.
				other => {
					return Err(Error::Unavailable(format!(
						"the stornoed invoice is not filed with NAV yet ({other:?}); \
						 retrying — if it was rejected, it needs a person"
					)));
				}
			}
			(NavOp::Storno, Some(original))
		}
		InvoiceKind::Normal => (NavOp::Create, None),
	};

	let doc = invoice_data(&seller, &invoice, &lines, &groups, original.as_deref())?;
	let client = NavAuth::load(app, &seller).await?;
	// The invoice's own uid is the NAV `requestId`: it is what makes a retry idempotent, and
	// it is the only identifier here that no cleanup or rebuild can reissue. See
	// [`NavAuth::manage_invoice_request`].
	let request = client.manage_invoice_request(op, invoice.uid.as_str(), &doc).await?;

	// Archived before it leaves the process, and the reply before anything reads it. Both go
	// through `redact`: the envelope authenticates with an unsalted SHA-512 password hash,
	// and an archive that kept it would be a replayable NAV credential per row.
	let created = nav.create_submission(invoice.id, op, &auth::redact(&request)).await?;
	let id = if let Some(id) = created {
		id
	} else {
		// One row per `(invoice, op)`, not one per attempt; the archived `request_xml` stays the
		// first attempt's, being the statutory dispute record. Re-read rather than trusting
		// `may_send`: that read went to the reader pool and a `tokenExchange` has elapsed, so a
		// verdict now on the row means posting again would be a second statutory filing.
		let prev = nav
			.submission_by_invoice(invoice.id)
			.await?
			.ok_or_else(|| Error::internal("saas-nav: the filing record vanished mid-send"))?;
		if prev.verdict.is_some() || prev.transaction_id.is_some() {
			tracing::info!(
				submission = prev.id,
				invoice = invoice.id,
				"another runner filed this invoice while this attempt was authenticating"
			);
			return Ok(());
		}
		prev.id
	};

	// No row write on failure: the row stays open with its request archived, and the failure
	// is the `jobs` row's business — `last_error` and `err_code` record it, `Error::retry()`
	// decides whether it is tried again.
	let (status, reply) = client.post("manageInvoice", &request).await?;
	let redacted = auth::redact(&reply);

	let parsed = match accepted(status, &reply) {
		Ok(parsed) => parsed,
		// Unreadable, or `OK` with no `transactionId`: nothing to record, so the archive keeps
		// its place as the only trace of what NAV said.
		Err(e) => {
			archive(nav, id, invoice.id, &redacted).await;
			return Err(e);
		}
	};
	match parsed {
		Accepted::Ok { transaction_id } => {
			// **Before the archive**, which is only a diagnostic: this is the one part of the
			// reply that cannot be reconstructed, and a lost `transactionId` makes the retry
			// resend under the same `requestId`, which NAV refuses as `REQUEST_ID_NOT_UNIQUE`.
			let applied =
				job::thrice(|| nav.set_sent(id, &transaction_id, 1)).await.map_err(|last| {
					last.unwrap_or_else(|| Error::internal("saas-nav: set_sent did not run"))
				})?;
			archive(nav, id, invoice.id, &redacted).await;
			if !applied {
				// The winner enqueued the poll for its own `transactionId`; a second one here
				// would poll a transaction NAV never issued for this row.
				tracing::warn!(
					submission = id,
					invoice = invoice.id,
					"another runner already recorded a transactionId for this filing"
				);
				return Ok(());
			}
			enqueue_poll(app, id, &transaction_id).await
		}
		Accepted::Fault { code, message } => {
			// Archived first here: there is no `transactionId` to race the write against.
			archive(nav, id, invoice.id, &redacted).await;
			// Unconditionally, before the `Retry::Never` branch: a retrying fault left both
			// columns NULL, so an invoice NAV would never accept retried forever with the archive
			// silent. Best-effort — a `?` would reclass a terminal fault as retryable and skip
			// the `finish` below, leaving the row open.
			if let Err(e) = nav.record_fault(id, &code, &message).await {
				tracing::warn!(submission = id, error = %e, "could not record the NAV fault");
			}
			// NAV answered and did not accept, so nothing was filed and a fresh attempt is safe —
			// and necessary, or bad credentials drop every invoice issued while the fault lasts.
			// The exception is `client::BURNS_REQUEST_ID`: a fresh attempt carries the same spent
			// `invoice.uid` and is refused identically, so that one terminates.
			tracing::error!(
				submission = id,
				invoice = invoice.id,
				%code,
				%message,
				"manageInvoice faulted; the invoice was not filed"
			);
			let err = client::business(&code, &message);
			// A terminal fault ends the filing, so settle the row: `unfiled_invoices` skips an
			// invoice that has any row, so an open one here is an unfiled invoice nothing can
			// ever see again. Re-read first — the guard above the POST cannot see an in-flight
			// sibling, so a `REQUEST_ID_NOT_UNIQUE` refusal would flip an accepted filing to
			// `FAILED` permanently.
			if err.retry() == Retry::Never {
				let cur = nav.submission(id).await?;
				// The one fault meaning NAV *probably holds this filing*: a POST that succeeded
				// then lost its `transactionId` retries into exactly it. `FAILED` here would be
				// an archive saying a filed invoice was never filed.
				let unknown = err.parts().1 == "E-NAV-REQUEST-ID-REUSED";
				if cur.is_some_and(|s| s.verdict.is_some() || s.transaction_id.is_some()) {
					tracing::info!(
						submission = id,
						invoice = invoice.id,
						"another runner's filing landed while this attempt was refused; \
						 verdict kept"
					);
				} else if unknown {
					tracing::error!(
						submission = id,
						invoice = invoice.id,
						%code,
						"NAV has already processed a request under this invoice's requestId; \
						 its status with NAV is unknown. Query the transaction before anything \
						 else, and do not storno it on the strength of this error"
					);
					// Verdict left NULL — the filing is open — with the reason on the row, which
					// is what `awaiting_operator` counts it by.
					nav.finish(id, None, Some((&code, &message)), Timestamp::now()).await?;
				} else if !nav
					.finish(id, Some(NavVerdict::Failed), Some((&code, &message)), Timestamp::now())
					.await?
				{
					tracing::warn!(
						submission = id,
						invoice = invoice.id,
						"the filing already carries a verdict; the fault is the job row's only"
					);
				}
			}
			Err(err)
		}
	}
}

/// Archives a NAV reply, best-effort, like `saas_core::audit::log`.
///
/// The archive is a diagnostic; the reply in hand is the truth. Propagating a failure here
/// would resend a filing NAV has already accepted, or — on the poll path — discard a verdict
/// NAV has already given and re-query it, which a `Retry::Never` mapping strands for good.
async fn archive(nav: &dyn NavStore, id: i64, invoice_id: i64, response_xml: &str) {
	if let Err(e) = nav.archive_response(id, response_xml).await {
		tracing::error!(
			submission = id,
			invoice = invoice_id,
			error = %e,
			"could not archive the NAV response; continuing with the reply in hand"
		);
	}
}

/// Whether this invoice may be sent to NAV at all. One read.
///
/// `Runner::fail` re-runs `report` from the top for as long as the filing keeps failing —
/// unbounded, because a statutory filing is not something to give up on — and the
/// `nav:invoice:{id}` dedup key stops a double *enqueue*, not a double *execution*. This is
/// the double-execution guard.
///
/// * A row with a verdict is settled: NAV has answered about this invoice.
/// * A row with a `transactionId` is with NAV and the poll decides. The poll is re-enqueued
///   here in case the enqueue is what was lost — now that the sweep re-drives only filings
///   that were never enqueued, this is the only thing that recovers a stranded poll.
/// * Anything else is an attempt that died before NAV took the filing, so `report` resends it
///   on the same row under the same `requestId`.
pub(crate) async fn may_send(app: &App, nav: &dyn NavStore, invoice_id: i64) -> ClResult<bool> {
	let Some(prev) = nav.submission_by_invoice(invoice_id).await? else {
		return Ok(true);
	};
	if let Some(verdict) = prev.verdict {
		if matches!(verdict, NavVerdict::Rejected) {
			tracing::error!(
				submission = prev.id,
				invoice = invoice_id,
				"NAV rejected this invoice; it needs correcting and re-issuing, not resending"
			);
		}
		return Ok(false);
	}
	if let Some(transaction_id) = &prev.transaction_id {
		enqueue_poll(app, prev.id, transaction_id).await?;
		return Ok(false);
	}
	Ok(true)
}

/// The `NAV_POLL` payload. `job_cancel` addresses rows by `payload = ?` string equality, so a
/// respelling here silently cancels nothing — one owner of the spelling.
#[must_use]
pub fn poll_payload(submission_id: i64) -> String {
	format!(r#"{{"submissionId":{submission_id}}}"#)
}

async fn enqueue_poll(app: &App, submission_id: i64, transaction_id: &str) -> ClResult<()> {
	let payload = poll_payload(submission_id);
	if job::enqueue(
		&app.store,
		KIND_NAV_POLL,
		&payload,
		// `dedup_key` is never released and the job row's backoff *is* the poll schedule, so one
		// key per transaction is one poll chain per filing. Keyed on the transaction, not the
		// submission id: a row can be resent, and each `transactionId` needs its own chain.
		Some(&format!("nav:poll:{transaction_id}")),
		Timestamp(Timestamp::now().0 + POLL_FIRST_DELAY_SECS),
	)
	.await?
	.is_some()
	{
		return Ok(());
	}
	// The key is spent. A `DONE` poll settled the verdict, so `may_send` never reaches here
	// with one; a `FAILED` one is a filing NAV holds and nothing else can ever ask about.
	// Reaching this line at all means an operator re-drove `Nav::submit`, which is audited.
	if app.store.job_redrive(KIND_NAV_POLL, &payload, Timestamp::now()).await? > 0 {
		tracing::warn!(submission = submission_id, "revived a stranded NAV poll");
	}
	Ok(())
}

/// One `queryTransactionStatus` round. Returns `Ok` only when NAV has reached a verdict;
/// anything else is an `Err`, which is how the job row schedules the next round.
///
/// The `NAV_POLL` handler, public for the same reason [`report`] is: `Runner::tick` is
/// private, so a test drives the pair directly.
pub async fn poll(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	submission_id: i64,
) -> ClResult<()> {
	let sub = nav
		.submission(submission_id)
		.await?
		.ok_or_else(|| Error::internal(format!("saas-nav: submission {submission_id} is gone")))?;
	if sub.verdict.is_some() {
		return Ok(());
	}
	let transaction_id = sub
		.transaction_id
		.clone()
		.ok_or_else(|| Error::internal("saas-nav: polling a submission with no transactionId"))?;

	let invoice = invoices
		.invoice_by_id(sub.invoice_id)
		.await?
		.ok_or_else(|| Error::internal("saas-nav: the submission's invoice is gone"))?;
	let seller = invoices
		.seller_by_id(invoice.seller_id)
		.await?
		.ok_or_else(|| Error::internal("saas-nav: the invoice's seller is gone"))?;

	let client = NavAuth::load(app, &seller).await?;
	let (_, reply) = client
		.post("queryTransactionStatus", &client.query_status_request(&transaction_id))
		.await?;
	archive(nav, sub.id, sub.invoice_id, &auth::redact(&reply)).await;

	// A row that already carries a verdict is settled, so a second one is dropped with a log —
	// see `NavStore::finish`. The early return above catches the ordinary case; this catches
	// the one where another runner settled it while this poll was in flight.
	let settle = |applied: bool| {
		if !applied {
			tracing::warn!(submission = sub.id, "the filing was settled while this poll ran");
		}
		Ok(())
	};

	match outcome(&reply)? {
		Outcome::Done => {
			settle(nav.finish(sub.id, Some(NavVerdict::Done), None, Timestamp::now()).await?)
		}
		Outcome::Warn => {
			settle(nav.finish(sub.id, Some(NavVerdict::Warn), None, Timestamp::now()).await?)
		}
		// `invoiceStatus = ABORTED`: NAV's verdict on the invoice, and the only rejection
		// there is. Retrying a verdict can only earn the same verdict, so this is where the
		// automation stops and `awaiting_operator` starts counting.
		Outcome::Failed { code, message } => {
			tracing::error!(
				submission = sub.id,
				invoice = sub.invoice_id,
				%code,
				%message,
				"NAV rejected the invoice; it needs a person to correct and re-issue it"
			);
			settle(
				nav.finish(
					sub.id,
					Some(NavVerdict::Rejected),
					Some((&code, &message)),
					Timestamp::now(),
				)
				.await?,
			)
		}
		// Not a verdict, so the job is not done. `Err` hands the schedule back to the runner,
		// which backs off to `jobs.backoff_cap.NAV_POLL` and, after
		// `jobs.alert_after.NAV_POLL`, is what tells a human.
		Outcome::Pending => {
			Err(Error::Unavailable(format!("NAV is still processing {transaction_id}")))
		}
		// A fault in the *question*. It says nothing about the invoice, which NAV may already
		// have accepted and filed, so it must not finish the row.
		Outcome::Unavailable { code, message } => Err(Error::Unavailable(format!(
			"queryTransactionStatus faulted ({code}: {message}); \
			 the invoice's status with NAV is still unknown"
		))),
	}
}

// vim: ts=4
