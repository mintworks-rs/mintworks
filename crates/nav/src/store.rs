// SPDX-License-Identifier: MPL-2.0
//! The persistence this crate needs: `nav_batches`, `nav_submissions`, `nav_seller_holds`, plus
//! the two invoice-range selections the statutory audit export runs.
//!
//! The export's selections live here rather than on `InvoiceStore` because they are this crate's
//! obligation, not `mintworks-invoice`'s. They read `invoices` only — never `nav_submissions` — so
//! an invoice that failed to report still appears in the export.
//!
//! **Only these methods write `nav_batches.state` or `nav_submissions.state`**, each as one write
//! transaction with a state guard. A failed guard writes nothing and answers `false` or `None`:
//! the policy is the caller's, the adapter only reports whether the write applied. `B*` and `R*`
//! name the transitions of `claude-docs/nav-filing-design.md` §4.
//!
//! **Nothing here schedules work.** No method writes a job: the caller runs
//! `mintworks_core::job::ensure` after the transition, and `NAV_SWEEP` repairs a lost one.

use async_trait::async_trait;
use mintworks_core::{
	ids::NavBatchId,
	prelude::{ClResult, Timestamp},
};

use crate::submission::{
	FilingState, NavArchive, NavBatch, NavFiling, NavOp, NavSellerHold, NavSubmission, NavVerdict,
};

#[async_trait]
pub trait NavStore: Send + Sync + 'static {
	/// B1: opens a `CLAIMED` batch of `seller_id`, minting its `uid`, with one `ACTIVE` row per
	/// invoice of `invoice_ids` that has no live row. Returns the batch and the invoice ids it
	/// got; `None` when it got none, and no batch is left behind.
	///
	/// Create-only: a live row of any state keeps its invoice out, and a `CANCELLED` row is how an
	/// operator's cancel beats the first POST (R6). One `op` covers the claim. `electronic` is the
	/// `invoiceAppearance` stored on every row, which the audit export reports.
	async fn claim_batch(
		&self,
		seller_id: i64,
		op: NavOp,
		invoice_ids: &[i64],
		electronic: bool,
	) -> ClResult<Option<(NavBatch, Vec<i64>)>>;

	/// B2, the write-ahead, committed before the POST: `CLAIMED` to `POSTING`, `posted_at = at`,
	/// each row's `idx`, and the archive in one transaction. `rows` is `(row_id, idx)`, `idx` the
	/// 1-based `<index>` on the wire. `request_xml` must already have been through
	/// [`crate::auth::redact`]: the raw envelope carries a replayable credential.
	///
	/// The archive is overwritten and its `response_xml` cleared: a batch back in `CLAIMED` was
	/// proved not filed, so this envelope is the only one NAV can hold. A fault stays.
	///
	/// `false` when the batch is not `CLAIMED`, or `rows` is not exactly its `ACTIVE` rows: a
	/// cancel took one, and this envelope no longer describes the batch.
	async fn begin_post(
		&self,
		batch_id: i64,
		request_xml: &str,
		rows: &[(i64, i64)],
		at: Timestamp,
	) -> ClResult<bool>;

	/// B3: `POSTING` to `CLAIMED`, the reply having proved nothing was filed. `fault` is
	/// `(code, message)`, written as [`Self::record_batch_fault`] writes it; `None` leaves any
	/// fault as it is, so a persistent one keeps ageing across attempts. `posted_at` stays.
	async fn abort_post(
		&self,
		batch_id: i64,
		fault: Option<(&str, &str)>,
		at: Timestamp,
	) -> ClResult<bool>;

	/// B4: `POSTING` to `IN_DOUBT`, the reply lost or the run dead. Unlike [`Self::abort_post`] this
	/// replaces the fault: `None` clears `error_code`, `error_msg` and `fault_at`, since one left
	/// by an earlier attempt says nothing of this one and would refuse [`Self::clear_doubt`].
	async fn mark_in_doubt(
		&self,
		batch_id: i64,
		fault: Option<(&str, &str)>,
		at: Timestamp,
	) -> ClResult<bool>;

	/// B5, B7: `POSTING` or `IN_DOUBT` to `SENT`, recording `transaction_id` and clearing the
	/// fault. `false` also when another batch carries `transaction_id`.
	async fn set_sent(&self, batch_id: i64, transaction_id: &str) -> ClResult<bool>;

	/// B8: `IN_DOUBT` with no fault to `CLAIMED`. A recorded fault means NAV may hold the request
	/// whatever the search found, and that batch goes to [`Self::hold_batch`] instead.
	async fn clear_doubt(&self, batch_id: i64) -> ClResult<bool>;

	/// B9: `IN_DOUBT` to `HELD`, replacing `error_code` and `error_msg`, stamping `fault_at` only
	/// when unset. A person leaves it through [`Self::attach_transaction`] or [`Self::refile`].
	async fn hold_batch(
		&self,
		batch_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<bool>;

	/// B6, in one transaction: `POSTING` to `CLOSED` with the fault written and `closed_at = at`;
	/// every `ACTIVE` row to `FAILED`, verdict `FAILED`, carrying `code` and `message`, resolved;
	/// a new `ACTIVE` row for each in one new `CLAIMED` batch; and the seller's hold unless one
	/// exists, the first one standing, so the new batch does not send. While a hold exists,
	/// `NAV_REPORT` and `NAV_SEND` wait, so nothing of the seller reaches NAV. Returns the new batch.
	/// `None` when the batch is not `POSTING`.
	async fn spend_batch(
		&self,
		batch_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<Option<NavBatch>>;

	/// R1, R2: an `ACTIVE` row of a `SENT` batch takes NAV's verdict. `Done`/`Warn` make it
	/// `ACCEPTED`, `Rejected` `REJECTED`, `Failed` `FAILED`. `error` is `(code, message)`. Sets
	/// `done_at`, and closes the batch when no `ACTIVE` row remains. `false` when the row was
	/// settled meanwhile: a verdict is the most terminal state in the system, so the first stands.
	async fn finish(
		&self,
		row_id: i64,
		verdict: NavVerdict,
		error: Option<(&str, &str)>,
		at: Timestamp,
	) -> ClResult<bool>;

	/// R3: an `ACTIVE` row of a `SENT` batch goes to `HELD` with verdict `REJECTED`, NAV having
	/// aborted it with `client::INVOICE_NUMBER_NOT_UNIQUE`. Sets `done_at`, and closes the batch
	/// when no `ACTIVE` row remains, as [`Self::finish`] does.
	async fn finish_held(
		&self,
		row_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<bool>;

	/// Deletes an `ACTIVE` row of a `CLAIMED` batch that no longer builds, closing the batch when
	/// no `ACTIVE` row remains. The caller then ensures the invoice's `NAV_REPORT`.
	async fn release_row(&self, row_id: i64) -> ClResult<bool>;

	/// R6. With no row at all for `(invoice_id, op)`, inserts a `CANCELLED` row with no batch; with
	/// an `ACTIVE` row in a `CLAIMED` batch, makes it `CANCELLED` and closes the batch when no
	/// `ACTIVE` row remains. Sets `done_at`. `false` otherwise: this and [`Self::begin_post`]
	/// serialize in the store, so a batch already `POSTING` refuses the cancel.
	async fn cancel_filing(&self, invoice_id: i64, op: NavOp, at: Timestamp) -> ClResult<bool>;

	/// R8: deletes a `CANCELLED` row, so the invoice can be claimed again. The caller then ensures
	/// its `NAV_REPORT`.
	async fn reopen_cancelled(&self, row_id: i64) -> ClResult<bool>;

	/// R5 and R7: moves `row_ids` as new `ACTIVE` rows into one new `CLAIMED` batch of the old
	/// rows' seller and `op`, copying `electronic`, and returns it. Each old row ends resolved; one
	/// already resolved keeps its `resolved_at`.
	///
	/// `code: Some` is R5, a person confirming nothing was filed: each row must be `HELD`, or
	/// `ACTIVE` in a `HELD` batch. A row with no verdict becomes `FAILED`, verdict `FAILED`, with
	/// `code` as its `error_code` and `done_at = at`; a row with NAV's verdict becomes `REJECTED`
	/// and its error stands. A `HELD` batch closes when no `ACTIVE` row remains.
	///
	/// `code: None` is R7: each row must be `REJECTED` or `FAILED`, and is left as it is.
	///
	/// `None` writes nothing: `row_ids` is empty or spans more than one seller or `op`, a row is
	/// not in the state above, or another live row exists for its `(invoice_id, op)`.
	async fn refile(
		&self,
		row_ids: &[i64],
		code: Option<&str>,
		at: Timestamp,
	) -> ClResult<Option<NavBatch>>;

	/// R4, R9: opens a `SENT` batch carrying `transaction_id` with `posted_at = at`, and returns it.
	/// `rows` is `(row_id, idx)`, `idx` being NAV's index. Each must be `HELD`, or `ACTIVE` in a
	/// `HELD` batch. A row with a verdict becomes `REJECTED`, one without `CANCELLED`; either way it
	/// is resolved and stays on its batch, so its record and the archive it reaches stand, and a
	/// new `ACTIVE` row with that `idx` is inserted. Rows not listed stay as they are. A `HELD` batch
	/// closes when no `ACTIVE` row remains.
	///
	/// `None` writes nothing: a batch already carries `transaction_id`, `rows` is empty, or a row
	/// is no longer eligible.
	async fn attach_transaction(
		&self,
		transaction_id: &str,
		seller_id: i64,
		op: NavOp,
		rows: &[(i64, i64)],
		at: Timestamp,
	) -> ClResult<Option<NavBatch>>;

	/// `NAV_ADOPT` is finished with these rows: stamps `adopt_checked_at` on each still `HELD`,
	/// which a row adopted meanwhile is not. Returns how many it stamped.
	async fn stamp_adopt_checked(&self, row_ids: &[i64], at: Timestamp) -> ClResult<u64>;

	/// Records that a person settled a `REJECTED` or `FAILED` row: writes `resolved_at` and nothing
	/// else, so it stops counting in [`Self::awaiting_operator`]. The verdict, the error and the
	/// archive are the statutory record of what NAV said, and a judgement does not amend them.
	/// `false` when the row is in any other state or already resolved.
	async fn resolve(&self, row_id: i64, at: Timestamp) -> ClResult<bool>;

	/// Records a request-level fault without moving the batch: `error_code` and `error_msg` are
	/// replaced, `fault_at` stamped only when unset. `false` on a `CLOSED` batch.
	async fn record_batch_fault(
		&self,
		batch_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<bool>;

	/// Archives a reply verbatim, before anything parses it. Upserts `nav_batch_xml`; the latest
	/// reply wins. `response_xml` must already have been through [`crate::auth::redact`].
	async fn archive_response(&self, batch_id: i64, response_xml: &str) -> ClResult<()>;

	/// Removes the hold. `false` when there was none.
	async fn release_seller(&self, seller_id: i64) -> ClResult<bool>;

	async fn seller_hold(&self, seller_id: i64) -> ClResult<Option<NavSellerHold>>;

	/// How many sellers are held, for `A-NAV-SELLER-HELD`.
	async fn seller_holds_count(&self) -> ClResult<i64>;

	async fn batch_by_uid(&self, uid: &str) -> ClResult<Option<NavBatch>>;

	/// The batch carrying `transaction_id`. `idx_nav_batch_transaction` admits at most one.
	async fn batch_by_transaction(&self, transaction_id: &str) -> ClResult<Option<NavBatch>>;

	/// The archived request and reply, or `None` when nothing is archived. Separate from
	/// [`Self::batch`] because it is the only read that touches the blobs.
	async fn batch_archive(&self, batch_id: i64) -> ClResult<Option<NavArchive>>;

	/// Every row of a batch in any state, ascending `id`.
	async fn batch_rows(&self, batch_id: i64) -> ClResult<Vec<NavSubmission>>;

	/// The invoice's newest row with its batch, which is the live one when there is one:
	/// `idx_nav_submission_live` admits one live row per `(invoice_id, op)`, and a new row is
	/// created only once the previous one is resolved. `None` is [`FilingState::None`].
	///
	/// **One operation per invoice is an invariant of the callers, not of the schema.** "The
	/// newest row" is *the* record only because `report` derives `op` from `invoice.kind`, and an
	/// invoice is either Normal or Storno; a second operation would need this signature widened.
	async fn filing_by_invoice(&self, invoice_id: i64) -> ClResult<Option<NavFiling>>;

	/// Every filing of an invoice, newest first: [`Self::filing_by_invoice`] plus resolved history.
	async fn filings_by_invoice(&self, invoice_id: i64) -> ClResult<Vec<NavFiling>>;

	/// Live filings of any seller whose [`NavFiling::state`] is in `states`, every one when `states`
	/// is empty, by keyset: ascending row `id` above `after_id`. In SQL the state is `CASE WHEN
	/// row.state = 'ACTIVE' THEN batch.state ELSE row.state END`, whose tags are [`FilingState`]'s.
	async fn list_filings(
		&self,
		states: &[FilingState],
		after_id: i64,
		limit: i64,
	) -> ClResult<Vec<NavFiling>>;

	/// Batches in `CLAIMED`, `POSTING`, `IN_DOUBT` or `SENT`, every seller's, by keyset: ascending
	/// `id` above `after_id`. Each has an owner job, which `NAV_SWEEP` checks.
	async fn supervised(&self, after_id: i64, limit: i64) -> ClResult<Vec<NavBatch>>;

	/// Issued, non-test-mode invoice ids of **every** seller with no row at all and no `PENDING` or
	/// `RUNNING` job under `nav:invoice:{id}`. Oldest invoice first.
	async fn orphaned_invoices(&self, limit: i64) -> ClResult<Vec<i64>>;

	/// Uids of batches with a row `HELD` and `adopt_checked_at IS NULL`, whatever the batch's own
	/// state. Each is owed a `NAV_ADOPT`.
	async fn adoptable(&self, limit: i64) -> ClResult<Vec<NavBatchId>>;

	/// Ready-to-file invoice ids of `seller_id` that could join a batch: issued, `NORMAL`, not test
	/// mode, with no `nav_submissions` row at all, excluding `exclude_invoice_id` (the invoice being
	/// assembled for). `require_document` additionally demands an `invoice_documents` row, for the
	/// sellers whose filing waits on the PDF. Oldest invoice first, `limit` applied in SQL.
	async fn batch_candidates(
		&self,
		seller_id: i64,
		exclude_invoice_id: i64,
		require_document: bool,
		limit: i64,
	) -> ClResult<Vec<i64>>;

	/// Issued, non-test-mode invoice ids of `seller_id` with no `nav_submissions` row **at all**:
	/// the backlog waiting on `NAV_REPORT`. `limit` is applied in SQL. Oldest invoice first.
	async fn unfiled_invoices(&self, seller_id: i64, limit: i64) -> ClResult<Vec<i64>>;

	/// How many issued invoices need a person, `seller_id`'s or (`None`) the whole deployment's:
	/// row `HELD` with `adopt_checked_at` set, unresolved `REJECTED` or `FAILED`, or an `ACTIVE`
	/// row in a batch `HELD`, in a batch `SENT` with an `error_code`, or in a batch `CLAIMED` with
	/// `fault_at < fault_before`. That last bound is the grace `nav.fault_alert_after` buys, so a
	/// transient fault pages nobody. Read for the sweep's log and for `A-NAV-REJECTED`.
	async fn awaiting_operator(
		&self,
		seller_id: Option<i64>,
		fault_before: Timestamp,
	) -> ClResult<i64>;

	/// What `seller_id` has not yet put on the wire: invoices with no row, plus `ACTIVE` rows in a
	/// batch with `posted_at IS NULL`.
	async fn backlog(&self, seller_id: i64) -> ClResult<i64>;

	/// The `error_code` of `seller_id`'s batch not `CLOSED` with the oldest `fault_at`.
	async fn oldest_open_fault(&self, seller_id: i64) -> ClResult<Option<String>>;

	/// The job keys `nav:send:{uid}` of `seller_id`'s `CLAIMED` batches, for `job_wake` once a hold
	/// is released or credentials change.
	async fn open_batch_keys(&self, seller_id: i64) -> ClResult<Vec<String>>;

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
pub fn store(app: &mintworks_core::App) -> ClResult<std::sync::Arc<dyn NavStore>> {
	app.extensions.get::<std::sync::Arc<dyn NavStore>>().cloned().ok_or_else(|| {
		mintworks_core::prelude::Error::internal(
			"mintworks-nav: no NavStore was registered on the app",
		)
	})
}

// vim: ts=4
