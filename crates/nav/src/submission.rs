// SPDX-License-Identifier: MPL-2.0
//! The filing model: `nav_batches`, one row per NAV request, and `nav_submissions`, one row per
//! invoice per batch. Request facts live on the batch, invoice facts on the row, and
//! [`FilingState`] is the pure function over both that services and the wire see.
//!
//! A job's retry lives on the `jobs` row, never here. The request and reply are archived verbatim
//! in `nav_batch_xml` *before* anything interprets them, so a NAV dispute is settled from the
//! archive, not from a reconstruction. There is one archive per batch: an invoice's own part is
//! sliced out on read by [`NavSubmission::idx`].

use mintworks_core::{ids::NavBatchId, prelude::Timestamp};
use serde::Serialize;

/// `nav_batches.op` and `nav_submissions.op` — NAV's `invoiceOperation` set as far as this crate
/// files it. A correction is storno + reissue, so `MODIFY` is never emitted, and technical
/// annulment has no code path. `as_str` is also the `invoiceOperation` string sent on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavOp {
	Create,
	Storno,
}

mintworks_core::str_enum!(NavOp { Create => "CREATE", Storno => "STORNO" });

/// `nav_submissions.verdict` — the row's final word on the invoice. Every variant is final by
/// construction, which is why there is no `is_terminal`.
///
/// The column is nullable and the row carries no verdict until the filing ends; where it stands
/// meanwhile is [`NavSubmission::state`] and its batch's [`NavBatch::state`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavVerdict {
	/// Accepted.
	Done,
	/// Accepted with warnings — still reported, still filed.
	Warn,
	/// NAV examined the invoice and refused it (`invoiceStatus = ABORTED`) — its verdict, and
	/// the only rejection the protocol has. Identical data can only produce another `ABORTED`, so
	/// a person must correct it and re-issue, unless the refusal was
	/// `client::INVOICE_NUMBER_NOT_UNIQUE`, which holds the row instead.
	Rejected,
	/// The filing ended with nothing filed and no verdict from NAV: a spent `requestId`, or an
	/// operator's confirmation. Recording it as `REJECTED` would put a refusal NAV never made in
	/// the archive.
	Failed,
}

// The tags are the `verdict` CHECK in both store adapters' `schema.rs`.
mintworks_core::str_enum!(NavVerdict {
	Done => "DONE",
	Warn => "WARN",
	Rejected => "REJECTED",
	Failed => "FAILED",
});

/// `nav_batches.state` — what is on the wire. Only [`crate::store::NavStore`] writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchState {
	/// Rows are in it and NAV does not hold it, as far as anyone can prove.
	Claimed,
	/// The write-ahead committed: a POST may be on the wire.
	Posting,
	/// NAV may hold it; reconciliation decides.
	InDoubt,
	/// Unproven, and a person must choose. No job runs.
	Held,
	/// NAV holds a transaction; the poll decides.
	Sent,
	/// Nothing more happens under this request.
	Closed,
}

mintworks_core::str_enum!(BatchState {
	Claimed => "CLAIMED",
	Posting => "POSTING",
	InDoubt => "IN_DOUBT",
	Held => "HELD",
	Sent => "SENT",
	Closed => "CLOSED",
});

/// `nav_submissions.state` — what happened to one invoice. Only [`crate::store::NavStore`] writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
	/// Riding its batch: the batch's state says where the filing stands.
	Active,
	/// NAV aborted it on a number it already holds, so a person or `NAV_ADOPT` must decide.
	Held,
	Accepted,
	Rejected,
	Failed,
	/// An operator stopped it, and the row refuses `claim_batch` until it is reopened; or
	/// `attach_transaction` replaced it, already resolved, keeping it for the archive.
	Cancelled,
}

mintworks_core::str_enum!(RowState {
	Active => "ACTIVE",
	Held => "HELD",
	Accepted => "ACCEPTED",
	Rejected => "REJECTED",
	Failed => "FAILED",
	Cancelled => "CANCELLED",
});

/// `nav_submissions.adopt_result` — what comparing NAV's copy of the invoice number found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptResult {
	/// Field for field ours, and bindable: `NAV_ADOPT` attached the row.
	Match,
	/// NAV holds other fields under the number; `adopt_detail` says which.
	Different,
	/// NAV holds nothing under the number.
	NotFound,
	/// NAV's copy is compressed, so it cannot be compared.
	Compressed,
	/// No archived request, or one side is no document to compare.
	NoArchive,
	/// NAV's copy names no transaction to poll.
	NoTransaction,
	/// The transaction is already bound to a batch, or a row was no longer eligible.
	AlreadyBound,
	/// A person bound the row with `Nav::confirm_ours`.
	Confirmed,
	/// Reconciliation bound the row to a transaction it found.
	Attached,
}

mintworks_core::str_enum!(AdoptResult {
	Match => "MATCH",
	Different => "DIFFERENT",
	NotFound => "NOT_FOUND",
	Compressed => "COMPRESSED",
	NoArchive => "NO_ARCHIVE",
	NoTransaction => "NO_TRANSACTION",
	AlreadyBound => "ALREADY_BOUND",
	Confirmed => "CONFIRMED",
	Attached => "ATTACHED",
});

/// What services and the wire see of one filing. No store guard uses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilingState {
	/// No row: waiting or deferred. An absent [`NavFiling`], so [`NavFiling::state`] never says so.
	None,
	Claimed,
	Posting,
	InDoubt,
	/// Batch `HELD`, or row `HELD`.
	Held,
	Sent,
	Accepted,
	Rejected,
	Failed,
	Cancelled,
}

mintworks_core::str_enum!(FilingState {
	None => "NONE",
	Claimed => "CLAIMED",
	Posting => "POSTING",
	InDoubt => "IN_DOUBT",
	Held => "HELD",
	Sent => "SENT",
	Accepted => "ACCEPTED",
	Rejected => "REJECTED",
	Failed => "FAILED",
	Cancelled => "CANCELLED",
});

impl FilingState {
	/// `state=open`: every state that can still need a person, a superset of what
	/// `awaiting_operator` counts, so the `A-NAV-REJECTED` link never hides a counted filing.
	pub const OPEN: &[FilingState] = &[
		FilingState::Claimed,
		FilingState::Posting,
		FilingState::InDoubt,
		FilingState::Held,
		FilingState::Sent,
		FilingState::Rejected,
		FilingState::Failed,
	];
}

/// One NAV request.
#[derive(Clone, Debug)]
pub struct NavBatch {
	pub id: i64,
	/// The NAV `requestId`. A refile makes a new batch, so a spent id is never reused.
	pub uid: NavBatchId,
	pub seller_id: i64,
	pub op: NavOp,
	pub state: BatchState,
	pub transaction_id: Option<String>,
	/// The latest write-ahead; `None` means never sent.
	pub posted_at: Option<Timestamp>,
	/// A request-level fault or marker. A row's own [`NavSubmission::error_code`] is NAV's word on
	/// that one invoice.
	pub error_code: Option<String>,
	pub error_msg: Option<String>,
	/// When the fault was first seen; it keeps ageing across attempts.
	pub fault_at: Option<Timestamp>,
	pub created_at: Timestamp,
	pub closed_at: Option<Timestamp>,
}

/// One invoice in one batch.
#[derive(Clone, Debug)]
pub struct NavSubmission {
	pub id: i64,
	pub invoice_id: i64,
	pub op: NavOp,
	/// `None` only on a `CANCELLED` row that never had a batch.
	pub batch_id: Option<i64>,
	/// 1-based wire `<index>`, written by [`crate::store::NavStore::begin_post`], or NAV's own by
	/// [`crate::store::NavStore::attach_transaction`]. `job::poll` matches NAV's per-result index
	/// against it and the archive is sliced by it.
	pub idx: Option<i64>,
	pub state: RowState,
	/// The final word on the invoice, once there is one.
	pub verdict: Option<NavVerdict>,
	/// This invoice's own: NAV's per-index message, never a batch-level fault.
	pub error_code: Option<String>,
	pub error_msg: Option<String>,
	/// The `invoiceAppearance` filed: `ELECTRONIC` when `true`. `None` on a row that never had a
	/// batch.
	pub electronic: Option<bool>,
	/// When NAV's copy was last compared: `NAV_ADOPT` left this `HELD` row for a person, or the
	/// comparison bound this row to a transaction.
	pub adopt_checked_at: Option<Timestamp>,
	/// What it found, or how a bound row got here.
	pub adopt_result: Option<AdoptResult>,
	/// `DIFFERENT`: a [`crate::diff::NavDiff`] as JSON.
	pub adopt_detail: Option<String>,
	pub created_at: Timestamp,
	pub done_at: Option<Timestamp>,
	/// This row is no longer the invoice's live filing, so `idx_nav_submission_live` stops
	/// covering it. Set on `REJECTED` and `FAILED` rows, and on a `HELD` row a person acknowledged,
	/// which `job::report` still treats as live; the verdict and the archive stand.
	pub resolved_at: Option<Timestamp>,
}

impl NavSubmission {
	/// NAV took this filing.
	#[must_use]
	pub fn accepted(&self) -> bool {
		matches!(self.verdict, Some(NavVerdict::Done | NavVerdict::Warn))
	}
}

/// A row with its batch, the one thing [`Self::state`] needs. `batch` is `None` only for a
/// `CANCELLED` row that never had one.
#[derive(Clone, Debug)]
pub struct NavFiling {
	pub row: NavSubmission,
	pub batch: Option<NavBatch>,
}

impl NavFiling {
	#[must_use]
	pub fn state(&self) -> FilingState {
		match self.row.state {
			RowState::Held => FilingState::Held,
			RowState::Accepted => FilingState::Accepted,
			RowState::Rejected => FilingState::Rejected,
			RowState::Failed => FilingState::Failed,
			RowState::Cancelled => FilingState::Cancelled,
			RowState::Active => match self.batch.as_ref().map(|b| b.state) {
				Some(BatchState::Claimed) => FilingState::Claimed,
				Some(BatchState::Posting) => FilingState::Posting,
				Some(BatchState::InDoubt) => FilingState::InDoubt,
				Some(BatchState::Sent) => FilingState::Sent,
				// No transition leaves an ACTIVE row closed or batchless; HELD is the one answer
				// that lets nothing automatic touch it.
				Some(BatchState::Held | BatchState::Closed) | None => FilingState::Held,
			},
		}
	}
}

/// `nav_seller_holds`: a seller whose `requestId`s NAV is spending, so nothing of it reaches NAV.
#[derive(Clone, Debug)]
pub struct NavSellerHold {
	pub seller_id: i64,
	pub code: String,
	pub message: String,
	pub held_at: Timestamp,
}

/// The archived NAV exchange for one batch, read separately because it is cold and large.
/// At least one side is always set — no write path creates a row with neither — so `None` from
/// [`crate::store::NavStore::batch_archive`] is the only "nothing archived".
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NavArchive {
	pub request_xml: Option<String>,
	pub response_xml: Option<String>,
}

#[cfg(test)]
mod tests {
	use super::*;

	fn filing(row: RowState, batch: Option<BatchState>) -> NavFiling {
		NavFiling {
			row: NavSubmission {
				id: 1,
				invoice_id: 1,
				op: NavOp::Create,
				batch_id: batch.map(|_| 1),
				idx: None,
				state: row,
				verdict: None,
				error_code: None,
				error_msg: None,
				electronic: None,
				adopt_checked_at: None,
				adopt_result: None,
				adopt_detail: None,
				created_at: Timestamp(0),
				done_at: None,
				resolved_at: None,
			},
			batch: batch.map(|state| NavBatch {
				id: 1,
				uid: NavBatchId::generate(),
				seller_id: 1,
				op: NavOp::Create,
				state,
				transaction_id: None,
				posted_at: None,
				error_code: None,
				error_msg: None,
				fault_at: None,
				created_at: Timestamp(0),
				closed_at: None,
			}),
		}
	}

	#[test]
	fn an_active_row_takes_its_state_from_the_batch() {
		for (batch, want) in [
			(Some(BatchState::Claimed), FilingState::Claimed),
			(Some(BatchState::Posting), FilingState::Posting),
			(Some(BatchState::InDoubt), FilingState::InDoubt),
			(Some(BatchState::Held), FilingState::Held),
			(Some(BatchState::Sent), FilingState::Sent),
			(Some(BatchState::Closed), FilingState::Held),
			(None, FilingState::Held),
		] {
			assert_eq!(filing(RowState::Active, batch).state(), want, "{batch:?}");
		}
	}

	#[test]
	fn any_other_row_state_wins_over_the_batch() {
		for (row, want) in [
			(RowState::Held, FilingState::Held),
			(RowState::Accepted, FilingState::Accepted),
			(RowState::Rejected, FilingState::Rejected),
			(RowState::Failed, FilingState::Failed),
			(RowState::Cancelled, FilingState::Cancelled),
		] {
			for batch in [
				None,
				Some(BatchState::Claimed),
				Some(BatchState::Posting),
				Some(BatchState::InDoubt),
				Some(BatchState::Held),
				Some(BatchState::Sent),
				Some(BatchState::Closed),
			] {
				assert_eq!(filing(row, batch).state(), want, "{row:?} in {batch:?}");
			}
		}
	}
}

// vim: ts=4
