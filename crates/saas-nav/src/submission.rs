//! The `nav_submissions` row: the filing record for one `(invoice, operation)` pair.
//!
//! Exactly one row per pair, opened before the first `manageInvoice` leaves the process and
//! updated in place afterwards — retry lives on the `jobs` row, never here. `request_xml` and
//! `response_xml` are archived verbatim *before* anything interprets them, so a NAV dispute is
//! settled from the row, not from a reconstruction.
//!
//! In a batch that archive is split by row: each member holds its own `<invoiceOperation>` and,
//! after the poll, its own `processingResult`. The envelope and the whole reply live on the
//! leader row — the one whose `invoice_id` the `batch_uid` names — so a dispute over one member
//! is settled from that member's row plus its leader's.

use saas_core::prelude::Timestamp;

/// `nav_submissions.op` — exactly NAV's `invoiceOperation` set. A correction is storno +
/// reissue, so `MODIFY` is never emitted, and the technical annulment `ANNUL` the CHECK
/// constraint still allows has no code path — `from_str` refuses it like any unknown tag.
/// `as_str` is also the `invoiceOperation` string sent on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavOp {
	Create,
	Storno,
}

saas_core::str_enum!(NavOp { Create => "CREATE", Storno => "STORNO" });

/// `nav_submissions.verdict` — what NAV said about the invoice, and nothing else. Every variant
/// is final by construction, which is why there is no `is_terminal`.
///
/// The column is nullable and the row carries no verdict until NAV gives one; the filing's own
/// state (pending, sent, failed) is job state and lives on the `jobs` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavVerdict {
	/// Accepted.
	Done,
	/// Accepted with warnings — still reported, still filed.
	Warn,
	/// NAV examined the invoice and refused it (`invoiceStatus = ABORTED`) — its verdict, and
	/// the only rejection the protocol has. Nothing more can be sent for this invoice as it
	/// stands: identical data can only produce another `ABORTED`, so a person must correct it
	/// and re-issue. Deliberately *not* retryable.
	Rejected,
	/// The filing ended without NAV giving a verdict on the invoice at all: a fault that spends
	/// the `requestId` (`client::Disposition::SpendsRequestId`). A signature or clock-skew fault
	/// filed nothing; `REQUEST_ID_NOT_UNIQUE` may mean it *was* filed. Either way a person decides,
	/// and recording it as `REJECTED` would put a refusal NAV never made in the archive.
	Failed,
}

// The tags are `004_nav.sql`'s `CHECK` constraint on `nav_submissions.verdict`.
saas_core::str_enum!(NavVerdict {
	Done => "DONE",
	Warn => "WARN",
	Rejected => "REJECTED",
	Failed => "FAILED",
});

#[derive(Clone, Debug)]
pub struct NavSubmission {
	pub id: i64,
	pub invoice_id: i64,
	pub op: NavOp,
	pub transaction_id: Option<String>,
	/// 1-based index of this invoice within the NAV batch, written by
	/// [`crate::store::NavStore::set_sent`] only. `job::poll` matches NAV's per-result
	/// `<index>` against it, so a row whose batch membership shifted between attempts must
	/// not carry an `idx` from the earlier one.
	pub idx: Option<i64>,
	/// NAV's answer, once there is one. `None` while the filing is in flight or faulted —
	/// that state is on the `jobs` row.
	pub verdict: Option<NavVerdict>,
	pub request_xml: Option<String>,
	pub response_xml: Option<String>,
	pub error_code: Option<String>,
	pub error_msg: Option<String>,
	pub created_at: Timestamp,
	pub done_at: Option<Timestamp>,
	/// The uid of the batch leader's invoice, which is also the NAV `requestId` the batch was
	/// filed under. `None` on a row written before batching, or by the storno path.
	pub batch_uid: Option<String>,
	/// When an operator recorded that they had dealt with a filing NAV refused or left without
	/// a verdict. Set only by [`crate::store::NavStore::resolve`]; it subtracts the row from
	/// [`crate::store::NavStore::awaiting_operator`] and changes nothing else — the verdict and
	/// both archives stand, and the invoice stays out of `unfiled_invoices` either way.
	pub resolved_at: Option<Timestamp>,
}

// vim: ts=4
