//! What one `nav_submissions` row is, and the named transitions that move it.
//!
//! `job.rs` owns the orchestration — the payload, the batch it assembles, the POST and the branch
//! on what NAV answered — and calls a transition here instead of inlining `nav.finish(…)` /
//! `nav.release_batch(…)` / `nav.record_fault(…)`. The SQL stays in
//! `adapters/store-sqlite/src/nav.rs`: its guards are the concurrency contract.

use mintworks_core::{App, job, prelude::*};

use crate::store::NavStore;
use crate::submission::{NavSubmission, NavVerdict};

/// The state of one `nav_submissions` row, derived — never stored. The only owner of the
/// `open`/`pending`/`settled` split the HTTP views publish and `NavStore::awaiting_operator`
/// re-expresses in SQL; the two must agree, and this is the side a reader checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
	/// No row ever left the process for this invoice: `report` may send.
	Unsent,
	/// Somebody else's batch files it. A member never files itself.
	Member,
	/// With NAV under a `transactionId`; the poll decides.
	Sent,
	/// NAV answered and nothing is outstanding: `DONE`/`WARN`, or a refusal an operator has
	/// already dealt with through `NavStore::resolve`.
	Settled,
	/// Needs a person: a `REJECTED`/`FAILED` verdict, or no verdict and a recorded fault.
	Open,
}

/// The `open` test comes **before** the batch one, because `awaiting_operator` counts a row
/// whatever batch it sits in: an open member is still an invoice a person must look at.
#[must_use]
pub fn state(row: &NavSubmission, invoice_uid: &str) -> State {
	let needs_person = matches!(row.verdict, Some(NavVerdict::Rejected | NavVerdict::Failed))
		|| (row.verdict.is_none() && row.error_code.is_some());
	if needs_person && row.resolved_at.is_none() {
		return State::Open;
	}
	if row.verdict.is_some() {
		return State::Settled;
	}
	if row.batch_uid.as_deref().is_some_and(|leader| leader != invoice_uid) {
		return State::Member;
	}
	if row.transaction_id.is_some() {
		return State::Sent;
	}
	State::Unsent
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
/// * A row carrying another invoice's `batch_uid` is a batch member, and **a member never
///   files itself**: only the leader POSTs. Its own job completes `Ok`, so
///   `job_redrive`'s `AND status = 'FAILED'` never matches it either — `Nav::submit` on a
///   member answers `E-NAV-NOT-REDRIVABLE`, and the only way to move a stranded batch on is to
///   re-drive the **leader** named by `batch_uid`.
/// * Anything else is an attempt that died before NAV took the filing, so `report` resends it
///   on the same row under the same `requestId`. A recorded fault is one of those: the row is
///   [`State::Open`] for an operator, and the retry is still the job row's own business.
pub async fn may_send(
	app: &App,
	nav: &dyn NavStore,
	invoice_id: i64,
	invoice_uid: &str,
) -> ClResult<bool> {
	let Some(prev) = nav.submission_by_invoice(invoice_id).await? else {
		return Ok(true);
	};
	if prev.batch_uid.as_deref().is_some_and(|leader| leader != invoice_uid) {
		tracing::debug!(
			invoice = invoice_id,
			leader = prev.batch_uid.as_deref().unwrap_or("-"),
			"this invoice is filed by its batch leader"
		);
		return Ok(false);
	}
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
		crate::job::enqueue_poll(app, prev.id, transaction_id).await?;
		return Ok(false);
	}
	Ok(true)
}

/// NAV took the batch: stamp every claimed row with the `transactionId` and its 1-based
/// `<index>`. `false` when the **leader's** row already carried one — another runner won, and
/// it enqueued the poll for the id NAV actually issued.
///
/// `rows` is `(submission_id, invoice_id)` in wire order, so position `i` is `<index> i + 1`.
/// Through [`job::thrice`], because a lost `transactionId` makes the retry resend under the
/// same `requestId`, which NAV refuses as `REQUEST_ID_NOT_UNIQUE`.
pub async fn record_sent(
	nav: &dyn NavStore,
	rows: &[(i64, i64)],
	leader: i64,
	transaction_id: &str,
) -> ClResult<bool> {
	let pairs: Vec<(i64, i64)> = rows
		.iter()
		.enumerate()
		.map(|(i, (sub_id, _))| (*sub_id, i64::try_from(i + 1).unwrap_or(i64::MAX)))
		.collect();
	// One transaction for the whole batch: a partial stamp leaves a member with no transaction
	// id, no verdict and no error code, which no query in the crate can see.
	let applied =
		job::thrice(|| nav.set_sent_batch(&pairs, transaction_id))
			.await
			.map_err(|last| {
				last.unwrap_or_else(|| Error::internal("mintworks-nav: set_sent_batch did not run"))
			})?;
	let mut leader_applied = false;
	for (sub_id, inv_id) in rows {
		if *inv_id == leader {
			leader_applied = applied.contains(sub_id);
		} else if !applied.contains(sub_id) {
			tracing::warn!(
				submission = sub_id,
				invoice = inv_id,
				"this batch member already carries a transactionId"
			);
		}
	}
	Ok(leader_applied)
}

/// Record a reason on an open row without settling it: the row becomes [`State::Open`] and
/// `awaiting_operator` counts it, while the job's own retry is untouched.
///
/// Best-effort, like [`archive_reply`]: the caller is on a path with an error in hand that
/// must survive, and `finish` — which stamps a `done_at` nothing clears — is never the answer
/// for a filing NAV has not given a verdict on.
pub async fn park(nav: &dyn NavStore, id: i64, code: &str, message: &str) {
	if let Err(e) = nav.record_fault(id, code, message).await {
		tracing::warn!(submission = id, %code, error = %e, "could not record the NAV fault");
	}
}

/// NAV's verdict on one invoice. `false` when the row was settled while this was in flight —
/// a verdict is the most terminal state in the system, so the first one stands.
pub async fn settle(
	nav: &dyn NavStore,
	id: i64,
	verdict: NavVerdict,
	error: Option<(&str, &str)>,
) -> ClResult<bool> {
	nav.finish(id, Some(verdict), error, Timestamp::now()).await
}

/// A batch that was never filed: release every member and put each back on its own
/// `NAV_REPORT`, in that order — the reverse strands them in a dead leader's batch. Returns
/// the invoice ids released.
///
/// Releasing the row is not enough: a member stood down inside [`may_send`] and returned `Ok`,
/// so the runner marked its `NAV_REPORT` `DONE` and `nav:invoice:{id}` is spent.
pub async fn abandon(
	app: &App,
	nav: &dyn NavStore,
	batch_uid: &str,
	leader_id: i64,
) -> ClResult<Vec<i64>> {
	let released = nav.release_batch(batch_uid, leader_id).await?;
	crate::job::refile_released(app, &released).await;
	Ok(released)
}

/// Archives a NAV reply, best-effort, like `mintworks_core::audit::log`.
///
/// The archive is a diagnostic; the reply in hand is the truth. Propagating a failure here
/// would resend a filing NAV has already accepted, or — on the poll path — discard a verdict
/// NAV has already given and re-query it, which a `Retry::Never` mapping strands for good.
pub async fn archive_reply(nav: &dyn NavStore, id: i64, invoice_id: i64, response_xml: &str) {
	if let Err(e) = nav.archive_response(id, response_xml).await {
		tracing::error!(
			submission = id,
			invoice = invoice_id,
			error = %e,
			"could not archive the NAV response; continuing with the reply in hand"
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::submission::NavOp;

	fn row() -> NavSubmission {
		NavSubmission {
			id: 1,
			invoice_id: 1,
			op: NavOp::Create,
			transaction_id: None,
			idx: None,
			verdict: None,
			error_code: None,
			error_msg: None,
			created_at: Timestamp(0),
			done_at: None,
			batch_uid: None,
			resolved_at: None,
		}
	}

	/// Every `State` a submission row can be read as. This restates
	/// `NavStore::awaiting_operator`'s rule — `verdict IN ('REJECTED','FAILED') OR (verdict IS
	/// NULL AND error_code IS NOT NULL)`, minus the rows `resolve` has settled — it does not
	/// read the statement, so the two can still drift; `crates/nav/tests/nav.rs`'s
	/// `awaiting_operator_counts_only_navs_rejections` is what covers the SQL side.
	#[test]
	fn every_state_a_submission_row_reads_as() {
		const ME: &str = "inv_me";

		assert_eq!(state(&row(), ME), State::Unsent);

		let mut sent = row();
		sent.transaction_id = Some("TX1".into());
		assert_eq!(state(&sent, ME), State::Sent);

		let mut member = row();
		member.batch_uid = Some("inv_someone_else".into());
		assert_eq!(state(&member, ME), State::Member);
		// A row carrying its *own* uid is the leader, not a member.
		member.batch_uid = Some(ME.into());
		assert_eq!(state(&member, ME), State::Unsent);

		for verdict in [NavVerdict::Done, NavVerdict::Warn] {
			let mut settled = row();
			settled.verdict = Some(verdict);
			assert_eq!(state(&settled, ME), State::Settled);
		}

		for verdict in [NavVerdict::Rejected, NavVerdict::Failed] {
			let mut open = row();
			open.verdict = Some(verdict);
			assert_eq!(state(&open, ME), State::Open);
			// `resolve` is what takes it back out of the count, and out of this state.
			open.resolved_at = Some(Timestamp(1));
			assert_eq!(state(&open, ME), State::Settled);
		}

		// No verdict but a recorded reason: the `REQUEST_ID_NOT_UNIQUE` shape, and the two
		// poll markers. `open` means "a person should look", not "nothing is running".
		let mut parked = row();
		parked.error_code = Some("E-NAV-UNKNOWN-STATUS".into());
		parked.transaction_id = Some("TX1".into());
		assert_eq!(state(&parked, ME), State::Open);

		// And an open member is still open: `awaiting_operator` does not look at `batch_uid`.
		let mut open_member = row();
		open_member.batch_uid = Some("inv_someone_else".into());
		open_member.error_code = Some("REQUEST_ID_NOT_UNIQUE".into());
		assert_eq!(state(&open_member, ME), State::Open);
	}
}

// vim: ts=4
