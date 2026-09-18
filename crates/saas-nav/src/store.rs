//! The persistence this crate needs: the `nav_submissions` filing record, plus the two
//! invoice-range selections the statutory audit export runs.
//!
//! The export's selections live here rather than on `InvoiceStore` because they are this crate's
//! obligation, not `saas-invoice`'s. They read `invoices` only — never `nav_submissions` — so an
//! invoice that failed to report still appears in the export.
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

	/// Archives one row's slice of the request. [`Self::claim_batch`] inserts rows before the
	/// request exists — it is built from the rows the claim returned — so the archiving
	/// contract on [`Self::create_submission`] is kept by calling this, and committing it,
	/// **before** the POST.
	async fn archive_request(&self, id: i64, request_xml: &str) -> ClResult<()>;

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

	/// [`Self::set_sent`] for a whole batch, in **one** write transaction: NAV holds the batch
	/// under one `transactionId`, so a partial write leaves the unwritten members with no
	/// transaction id, no verdict and no error code — invisible to `may_send`,
	/// [`Self::submissions_by_transaction`], [`Self::awaiting_operator`] and
	/// [`Self::unfiled_invoices`] alike.
	///
	/// `rows` is `(submission_id, idx)`, `idx` being the 1-based `<index>` on the wire. Returns
	/// the submission ids the write applied to; a row already carrying a `transaction_id` is
	/// skipped, exactly as [`Self::set_sent`] answers `false`.
	async fn set_sent_batch(&self, rows: &[(i64, i64)], transaction_id: &str)
	-> ClResult<Vec<i64>>;

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

	/// Records that an operator dealt with a filing that needs a person, so
	/// [`Self::awaiting_operator`] stops counting it.
	///
	/// `false` when the row is not one that needs a person — no verdict and no recorded fault,
	/// a verdict NAV was happy with, or a row already resolved. The caller decides what that
	/// means; the adapter only reports whether the write applied, as [`Self::set_sent`] and
	/// [`Self::finish`] do.
	///
	/// Writes `resolved_at` and nothing else. The verdict, the error and both XML archives are
	/// the statutory record of what NAV said, and an operator's judgement does not amend them.
	async fn resolve(&self, id: i64, at: Timestamp) -> ClResult<bool>;

	/// Ready-to-file invoice ids of `seller_id` that could join a batch: issued, `NORMAL`, with
	/// no `nav_submissions` row at all, excluding `exclude_invoice_id` (the leader). Oldest
	/// invoice first, `limit` applied in SQL.
	///
	/// Read-only and deliberately unfiltered by job state: the caller must drop any candidate
	/// without a live `PENDING` `NAV_REPORT` job before claiming it, or batching would
	/// file an invoice whose filing an operator stopped through `Nav::cancel_filing`.
	/// `require_document` additionally demands an `invoice_documents` row, for the sellers whose
	/// filing waits on the PDF.
	async fn batch_candidates(
		&self,
		seller_id: i64,
		exclude_invoice_id: i64,
		require_document: bool,
		limit: i64,
	) -> ClResult<Vec<i64>>;

	/// Claims `ids` into `batch_uid` in **one** write transaction, returning the
	/// `(submission_id, invoice_id)` pairs actually inserted — which is what the request is then
	/// built from.
	///
	/// The leader is get-or-claim and the members are create-only: a leader retrying a
	/// filing that predates batching already owns a row with `batch_uid IS NULL`, and a
	/// create-only insert would collide on `idx_nav_submission_live` and skip the leader out of
	/// its own batch. `idx` is not written here — [`Self::set_sent`] is its only writer, because
	/// membership can shift between attempts and the poll matches NAV's `index` against it.
	///
	/// One `op` covers the whole claim: `job::report` batches only when `op` is `Create`, so a
	/// `Storno` is always a lone leader with no members.
	async fn claim_batch(
		&self,
		leader_invoice_id: i64,
		op: NavOp,
		batch_uid: &str,
		ids: &[i64],
	) -> ClResult<Vec<(i64, i64)>>;

	async fn submissions_by_batch(&self, batch_uid: &str) -> ClResult<Vec<NavSubmission>>;

	/// Every row NAV answered under one `transactionId`. The poll settles the whole
	/// transaction, not just the row whose job is running.
	async fn submissions_by_transaction(
		&self,
		transaction_id: &str,
	) -> ClResult<Vec<NavSubmission>>;

	/// Releases the member rows of a batch that was never filed, returning the **invoice ids** of
	/// the members it touched. Never touches `leader_submission_id`, and never a row that already
	/// carries a `transaction_id` or a verdict.
	///
	/// The ids are what the caller re-drives: a member that stood down through `job::may_send`
	/// completed its own `NAV_REPORT` `Ok`, which spends `nav:invoice:{id}` for good, so
	/// releasing the row alone leaves the invoice unfilable by every API path.
	///
	/// Two outcomes, both required, in one transaction. A member with `done_at IS NULL` is
	/// **deleted**, so it looks never-attempted and `unfiled_invoices` offers it again. A member
	/// that [`NavStore::finish`] left open with a reason — `done_at` set, still no verdict — keeps
	/// its row and only has `batch_uid` cleared: that shape is what `awaiting_operator` counts as
	/// needing a person, so deleting it would erase a recorded fault. It keeps its archived
	/// `request_xml` too, which is the record of what was sent on its behalf.
	///
	/// The test is `done_at`, never `error_code`: only [`NavStore::finish`] settles a member, and
	/// a row carrying an `error_code` but no `done_at` is still pristine — keeping it would make
	/// the invoice unfilable.
	///
	/// A terminal fault that burns only the leader's `requestId` must leave every member
	/// filable: a `verdict = FAILED` on them would be unfilable three ways over, since
	/// `may_send` reads a verdict as settled, `unfiled_invoices` skips any invoice with a row,
	/// and the members' own jobs completed `Ok`.
	///
	/// Deleting rows whose `request_xml` was archived before the POST does not breach the
	/// archiving contract above: the leader's row keeps the whole redacted envelope, so every
	/// released member's `<invoiceOperation>` is still on record under `batch_uid`.
	async fn release_batch(&self, batch_uid: &str, leader_submission_id: i64)
	-> ClResult<Vec<i64>>;

	/// Drop one invoice out of a batch it was claimed into, leaving its row otherwise untouched.
	/// `false` when the row is no longer releasable — it carries a `transactionId` or a verdict,
	/// so the filing is at NAV and the member is not this batch's to give back.
	///
	/// Unlike [`Self::release_batch`] the row is kept whatever its `done_at`: the caller is
	/// `job::report` dropping a member it could not build, which parks a reason on the row first
	/// so `awaiting_operator` shows it.
	async fn release_member(&self, batch_uid: &str, invoice_id: i64) -> ClResult<bool>;

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
	/// forever. Rows [`Self::resolve`] has settled are excluded — without that the count, and
	/// the alert, could never come down.
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
