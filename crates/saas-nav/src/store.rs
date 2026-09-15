//! The persistence this crate needs: the `nav_submissions` filing record, plus the two
//! invoice-range selections the statutory audit export runs.
//!
//! The export's selections live here rather than on `InvoiceStore` because they are this
//! crate's obligation, not `saas-invoice`'s. They read `invoices` only —
//! never `nav_submissions` — so an invoice that failed to report still appears in the
//! export (`nav-mapping.md` §9.3).
//!
//! **Nothing here schedules a retry.** One `(invoice_id, op)` has one row, opened before the
//! request leaves the process and updated in place; whether and when to try again is the `jobs`
//! row's business.

use async_trait::async_trait;
use saas_core::prelude::{ClResult, Timestamp};

use crate::submission::{NavOp, NavSubmission, NavVerdict};

#[async_trait]
pub trait NavStore: Send + Sync + 'static {
	/// Opens a filing record with its request already archived and no verdict yet. Called
	/// *before* the request leaves the process. `request_xml` must already have been through
	/// [`crate::auth::redact`]: the raw envelope carries a replayable credential.
	///
	/// `None` means `idx_nav_submission_live` refused it: a record for this `(invoice_id, op)`
	/// already exists, so the invoice is filed or in flight and the caller must not send. This
	/// is the guard that actually serializes two runners — `job::may_send` reads through the
	/// reader pool while this writes through the writer, so the read alone cannot.
	async fn create_submission(
		&self,
		invoice_id: i64,
		op: NavOp,
		request_xml: &str,
	) -> ClResult<Option<i64>>;

	async fn submission(&self, id: i64) -> ClResult<Option<NavSubmission>>;

	/// The filing record for an invoice, verdict or not. `report` consults it before building
	/// anything: a second `manageInvoice` for the same invoice number is a duplicate statutory
	/// filing.
	///
	/// **One row per invoice is an invariant of the callers, not of the schema.**
	/// `idx_nav_submission_live` is on `(invoice_id, op)` and the CHECK permits `CREATE`,
	/// `STORNO` and `ANNUL`, so this answers "the newest row" and the newest row is the only
	/// one because `report` derives `op` from `invoice.kind` and an invoice is either Normal or
	/// Storno — one operation for the life of a row. `may_send`, the storno precondition in
	/// `job::report` and `Nav::cancel_filing` all read it as *the* record; a second operation
	/// on one invoice would need them widened before this signature is.
	async fn submission_by_invoice(&self, invoice_id: i64) -> ClResult<Option<NavSubmission>>;

	/// Archives a response verbatim. Nothing has parsed it yet.
	async fn archive_response(&self, id: i64, response_xml: &str) -> ClResult<()>;

	/// NAV accepted the submission: record the `transactionId` and the batch index. *When* to
	/// poll is the `NAV_POLL` job's schedule, not a column here.
	///
	/// `false` when the row already carries a `transactionId` — another runner's filing landed
	/// first. Guarded like `job_complete`'s `AND status = 'RUNNING'`: the policy is the
	/// caller's, so the adapter only reports whether the write applied.
	async fn set_sent(&self, id: i64, transaction_id: &str, idx: i64) -> ClResult<bool>;

	/// NAV has answered. `error` is `(error_code, error_msg)` and is `None` for `DONE`/`WARN`.
	///
	/// `false` when the row already carries a verdict. A verdict is the most terminal state in
	/// the system: an unguarded write let a late `REQUEST_ID_NOT_UNIQUE` fault mark a filing
	/// NAV had accepted as `FAILED`, which `may_send` then reads as settled forever.
	///
	/// `verdict: None` records the reason and leaves the verdict NULL — the filing is *open*,
	/// which is what `REQUEST_ID_NOT_UNIQUE` means: NAV may well hold it, so calling it `FAILED`
	/// would invite an operator to storno an invoice that is filed. An open row would otherwise
	/// be invisible in both directions — `unfiled_invoices` skips an invoice with any row — so
	/// `awaiting_operator` counts `verdict IS NULL AND error_code IS NOT NULL` as needing a
	/// person.
	async fn finish(
		&self,
		id: i64,
		verdict: Option<NavVerdict>,
		error: Option<(&str, &str)>,
		done_at: Timestamp,
	) -> ClResult<bool>;

	/// Record a fault on an open filing without settling it: `error_code`/`error_msg` only,
	/// never `verdict` or `done_at`, so the retry behaviour is unchanged.
	///
	/// Without it a `Retry::Backoff` fault — the majority, by `client::business`'s design —
	/// left both columns NULL, so the archive said nothing about why an invoice retried
	/// forever and [`Self::awaiting_operator`] could not see it.
	async fn record_fault(&self, id: i64, code: &str, message: &str) -> ClResult<()>;

	/// Issued invoice ids of `seller_id` with no `nav_submissions` row **at all** — invoices
	/// whose `NAV_REPORT` was never enqueued, which is the at-most-once gap at
	/// `saas_invoice::issue::enqueue_jobs`. An invoice that *was* enqueued is a job row the
	/// runner is retrying on its own backoff, so re-driving it from here would file it twice.
	///
	/// `limit` is applied in SQL rather than by the caller truncating a full scan. Oldest
	/// invoice first.
	async fn unfiled_invoices(&self, seller_id: i64, limit: i64) -> ClResult<Vec<i64>>;

	/// How many of `seller_id`'s issued invoices still need a person: `verdict IN
	/// ('REJECTED','FAILED')` — NAV refused the invoice, or the filing ended with no verdict on
	/// it at all — plus the open row `verdict IS NULL AND error_code IS NOT NULL`, which is the
	/// `REQUEST_ID_NOT_UNIQUE` case where NAV may hold a filing this process cannot see. None of
	/// the three is retryable. Read once per sweep tick for the log, and by
	/// [`crate::service_api::alerts`] for `A-NAV-REJECTED`: a refused filing is an operator
	/// problem and would otherwise produce one `error!` when it happened and then silence
	/// forever.
	async fn awaiting_operator(&self, seller_id: i64) -> ClResult<i64>;

	/// Issued invoice ids whose UTC issue date falls in `[from, to]`, both inclusive,
	/// `YYYY-MM-DD`. Storno invoices and the invoices they cancel are both pulled in when
	/// either end of the pair is in range. Ascending `id`.
	async fn export_ids_by_date(&self, seller_id: i64, from: &str, to: &str) -> ClResult<Vec<i64>>;

	/// The same, selecting on `invoices.number` between two rendered numbers, inclusive.
	async fn export_ids_by_number(
		&self,
		seller_id: i64,
		from: &str,
		to: &str,
	) -> ClResult<Vec<i64>>;
}

/// The registered [`NavStore`], or an internal error naming what the application forgot.
pub fn store(app: &saas_core::App) -> ClResult<std::sync::Arc<dyn NavStore>> {
	app.extensions.get::<std::sync::Arc<dyn NavStore>>().cloned().ok_or_else(|| {
		saas_core::prelude::Error::internal("saas-nav: no NavStore was registered on the app")
	})
}

// vim: ts=4
