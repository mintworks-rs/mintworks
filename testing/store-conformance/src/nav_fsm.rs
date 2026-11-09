// SPDX-License-Identifier: MPL-2.0
//! `NavStore` conformance: every transition of `claude-docs/nav-filing-design.md` §4 as one table,
//! [`transition_matrix`], then the focused cases one cell cannot state.
//!
//! §4 names R5's method `confirm_not_filed`; in the store R5 is `refile(rows, Some(code), at)`, R7
//! is `refile(rows, None, at)`, and R4 and R9 are both `attach_transaction`.
//!
//! The list rides in `invoice_tests!`: this module needs `invoice`'s fixture, and every adapter
//! that can issue an invoice already expands that macro.

use mintworks_core::prelude::Timestamp;
use mintworks_invoice::{
	invoice_job_key,
	store::{InvoiceStore, IssueInvoice},
};
use mintworks_nav::{
	AdoptResult, BatchState as B, FilingState, NavBatch, NavOp, NavSubmission, NavVerdict,
	RowState as R, store::NavStore,
};
use serde_json::json;

use crate::{
	Harness, fresh,
	invoice::{self, SELLER},
};

/// When a fixture's own writes happen. Distinct from [`AT`], so a `fault_at` or `resolved_at` says
/// which of the two wrote it.
const T0: Timestamp = Timestamp(1_000);
/// When the transition under test happens.
const AT: Timestamp = Timestamp(2_000);

/// Where a filing stands before the transition, each reached through the store's own methods.
#[derive(Clone, Copy, Debug)]
enum Start {
	NoRow,
	Claimed,
	Posting,
	InDoubt,
	/// `IN_DOUBT` with a fault, which refuses `clear_doubt`.
	Faulted,
	/// Batch `HELD`, row `ACTIVE`.
	Held,
	Sent,
	Accepted,
	/// Row `HELD` by R3, its batch closed.
	RowHeld,
	Rejected,
	Failed,
	/// `Rejected`, then `resolve`.
	Resolved,
	/// Batch closed by `spend_batch`, a live row in the new batch.
	Spent,
	/// Row `CANCELLED` out of a batch.
	Cancelled,
	/// Row `CANCELLED` that never had a batch.
	Stopped,
}

const STARTS: [Start; 15] = [
	Start::NoRow,
	Start::Claimed,
	Start::Posting,
	Start::InDoubt,
	Start::Faulted,
	Start::Held,
	Start::Sent,
	Start::Accepted,
	Start::RowHeld,
	Start::Rejected,
	Start::Failed,
	Start::Resolved,
	Start::Spent,
	Start::Cancelled,
	Start::Stopped,
];

/// One `NavStore` write, with the arguments [`apply`] gives it.
#[derive(Clone, Copy, Debug)]
enum Op {
	Claim,
	BeginPost,
	AbortPost,
	MarkInDoubt,
	SetSent,
	ClearDoubt,
	HoldBatch,
	SpendBatch,
	Finish,
	FinishHeld,
	ReleaseRow,
	Cancel,
	Reopen,
	/// R5.
	Confirm,
	/// R7.
	Retry,
	Attach,
	Resolve,
	RecordFault,
}

const OPS: [Op; 18] = [
	Op::Claim,
	Op::BeginPost,
	Op::AbortPost,
	Op::MarkInDoubt,
	Op::SetSent,
	Op::ClearDoubt,
	Op::HoldBatch,
	Op::SpendBatch,
	Op::Finish,
	Op::FinishHeld,
	Op::ReleaseRow,
	Op::Cancel,
	Op::Reopen,
	Op::Confirm,
	Op::Retry,
	Op::Attach,
	Op::Resolve,
	Op::RecordFault,
];

/// The invoice's newest row when it sits in a batch other than the fixture's.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Next {
	batch: B,
	row: R,
	idx: Option<i64>,
	tx: bool,
}

/// What `refile`, `spend_batch` and a fresh claim leave.
const REFILED: Next = Next { batch: B::Claimed, row: R::Active, idx: None, tx: false };

/// What `attach_transaction` leaves.
const ATTACHED: Next = Next { batch: B::Sent, row: R::Active, idx: Some(7), tx: true };

/// The fixture's batch and row. `row: None` is deleted, or was never there.
#[derive(Clone, Copy, Debug)]
struct Want {
	batch: Option<B>,
	row: Option<R>,
	verdict: Option<NavVerdict>,
	tx: bool,
	batch_error: Option<&'static str>,
	row_error: Option<&'static str>,
	fault_at: Option<Timestamp>,
	resolved_at: Option<Timestamp>,
	next: Option<Next>,
}

const NOTHING: Want = Want {
	batch: None,
	row: None,
	verdict: None,
	tx: false,
	batch_error: None,
	row_error: None,
	fault_at: None,
	resolved_at: None,
	next: None,
};

/// What [`drive`] leaves.
fn base(start: Start) -> Want {
	let t0 = Some(T0);
	let active = |batch| Want { batch: Some(batch), row: Some(R::Active), ..NOTHING };
	let settled = |row, verdict, row_error| Want {
		batch: Some(B::Closed),
		row: Some(row),
		verdict: Some(verdict),
		tx: true,
		row_error,
		..NOTHING
	};
	match start {
		Start::NoRow => NOTHING,
		Start::Claimed => active(B::Claimed),
		Start::Posting => active(B::Posting),
		Start::InDoubt => active(B::InDoubt),
		Start::Faulted => Want { batch_error: Some("F0"), fault_at: t0, ..active(B::InDoubt) },
		Start::Held => Want { batch_error: Some("H0"), fault_at: t0, ..active(B::Held) },
		Start::Sent => Want { tx: true, ..active(B::Sent) },
		Start::Accepted => settled(R::Accepted, NavVerdict::Done, None),
		Start::RowHeld => settled(R::Held, NavVerdict::Rejected, Some("D0")),
		Start::Rejected => settled(R::Rejected, NavVerdict::Rejected, Some("V0")),
		Start::Failed => settled(R::Failed, NavVerdict::Failed, Some("V0")),
		Start::Resolved => Want { resolved_at: t0, ..base(Start::Rejected) },
		Start::Spent => Want {
			batch: Some(B::Closed),
			row: Some(R::Failed),
			verdict: Some(NavVerdict::Failed),
			batch_error: Some("S0"),
			row_error: Some("S0"),
			fault_at: t0,
			resolved_at: t0,
			next: Some(REFILED),
			..NOTHING
		},
		Start::Cancelled => Want { batch: Some(B::Closed), row: Some(R::Cancelled), ..NOTHING },
		Start::Stopped => Want { row: Some(R::Cancelled), ..NOTHING },
	}
}

/// The table: the cells that apply, with what they leave. Every other `(Start, Op)` is refused,
/// and a refusal writes nothing.
fn want(start: Start, op: Op) -> Option<Want> {
	let was = base(start);
	let at = Some(AT);
	let fault = Want { batch_error: Some("F1"), fault_at: was.fault_at.or(at), ..was };
	let refiled = Want { resolved_at: was.resolved_at.or(at), next: Some(REFILED), ..was };
	Some(match (start, op) {
		// B1: a resolved row leaves room for a claim, as no row does.
		(Start::NoRow, Op::Claim) => Want { row: Some(R::Active), next: Some(REFILED), ..was },
		(Start::Resolved, Op::Claim) => Want { next: Some(REFILED), ..was },
		// B2
		(Start::Claimed, Op::BeginPost) => Want { batch: Some(B::Posting), ..was },
		// B3
		(Start::Posting, Op::AbortPost) => Want { batch: Some(B::Claimed), ..fault },
		// B4
		(Start::Posting, Op::MarkInDoubt) => Want { batch: Some(B::InDoubt), ..fault },
		// B5, B7
		(Start::Posting | Start::InDoubt | Start::Faulted, Op::SetSent) => {
			Want { batch: Some(B::Sent), tx: true, batch_error: None, fault_at: None, ..was }
		}
		// B6
		(Start::Posting, Op::SpendBatch) => Want {
			batch: Some(B::Closed),
			row: Some(R::Failed),
			verdict: Some(NavVerdict::Failed),
			batch_error: Some("S1"),
			row_error: Some("S1"),
			fault_at: at,
			resolved_at: at,
			next: Some(REFILED),
			..was
		},
		// B8
		(Start::InDoubt, Op::ClearDoubt) => Want { batch: Some(B::Claimed), ..was },
		// B9: `fault_at` is stamped only when unset.
		(Start::InDoubt | Start::Faulted, Op::HoldBatch) => Want {
			batch: Some(B::Held),
			batch_error: Some("H1"),
			fault_at: was.fault_at.or(at),
			..was
		},
		// R1, R2
		(Start::Sent, Op::Finish) => Want {
			batch: Some(B::Closed),
			row: Some(R::Rejected),
			verdict: Some(NavVerdict::Rejected),
			row_error: Some("V1"),
			..was
		},
		// R3
		(Start::Sent, Op::FinishHeld) => Want {
			batch: Some(B::Closed),
			row: Some(R::Held),
			verdict: Some(NavVerdict::Rejected),
			row_error: Some("D1"),
			..was
		},
		// R4, R9: the row stands, resolved on its batch, and a new row is inserted.
		(Start::Held, Op::Attach) => Want {
			batch: Some(B::Closed),
			row: Some(R::Cancelled),
			resolved_at: at,
			next: Some(ATTACHED),
			..was
		},
		(Start::RowHeld, Op::Attach) => {
			Want { row: Some(R::Rejected), resolved_at: at, next: Some(ATTACHED), ..was }
		}
		// R5
		(Start::Held, Op::Confirm) => Want {
			batch: Some(B::Closed),
			row: Some(R::Failed),
			verdict: Some(NavVerdict::Failed),
			row_error: Some("C1"),
			..refiled
		},
		(Start::RowHeld, Op::Confirm) => Want { row: Some(R::Rejected), ..refiled },
		// R6
		(Start::NoRow, Op::Cancel) => Want { row: Some(R::Cancelled), ..was },
		(Start::Claimed, Op::Cancel) => {
			Want { batch: Some(B::Closed), row: Some(R::Cancelled), ..was }
		}
		// The refiled row is `ACTIVE` in a `CLAIMED` batch; the spent row stands.
		(Start::Spent, Op::Cancel) => {
			let row = R::Cancelled;
			Want { next: Some(Next { batch: B::Closed, row, ..REFILED }), ..was }
		}
		// R7: an existing `resolved_at` stands.
		(Start::Rejected | Start::Failed | Start::Resolved, Op::Retry) => refiled,
		// R8
		(Start::Cancelled | Start::Stopped, Op::Reopen) => Want { row: None, ..was },
		// §3: a row that no longer builds.
		(Start::Claimed, Op::ReleaseRow) => Want { batch: Some(B::Closed), row: None, ..was },
		(Start::Rejected | Start::Failed | Start::RowHeld, Op::Resolve) => {
			Want { resolved_at: at, ..was }
		}
		(
			Start::Claimed
			| Start::Posting
			| Start::InDoubt
			| Start::Faulted
			| Start::Held
			| Start::Sent,
			Op::RecordFault,
		) => fault,
		_ => return None,
	})
}

/// One invoice's filing, driven to a [`Start`]. `row: None` reads the invoice's newest row.
struct Fixture {
	invoice: i64,
	row: Option<i64>,
	batch: Option<i64>,
	batch_uid: Option<String>,
}

async fn drive<S: InvoiceStore + NavStore>(store: &S, start: Start) -> Fixture {
	let invoice = invoice::issued(store).await.id;
	match start {
		Start::NoRow => return Fixture { invoice, row: None, batch: None, batch_uid: None },
		Start::Stopped => {
			assert!(store.cancel_filing(invoice, NavOp::Create, T0).await.unwrap());
			let row = store.filing_by_invoice(invoice).await.unwrap().unwrap().row.id;
			return Fixture { invoice, row: Some(row), batch: None, batch_uid: None };
		}
		_ => {}
	}
	let claim = store.claim_batch(SELLER, NavOp::Create, &[invoice], false).await.unwrap();
	let claimed = claim.unwrap().0;
	let (batch, batch_uid) = (claimed.id, claimed.uid.as_str().to_owned());
	let row = store.batch_rows(batch).await.unwrap()[0].id;

	let entered = match start {
		Start::Claimed => true,
		Start::Cancelled => store.cancel_filing(invoice, NavOp::Create, T0).await.unwrap(),
		_ => store.begin_post(batch, "<request/>", &[(row, 1)], T0).await.unwrap(),
	};
	assert!(entered, "{start:?}: first step");

	let tx = format!("TX{row}");
	let left = match start {
		Start::InDoubt => store.mark_in_doubt(batch, None, T0).await.unwrap(),
		Start::Faulted | Start::Held => {
			store.mark_in_doubt(batch, Some(("F0", "m")), T0).await.unwrap()
		}
		Start::Sent
		| Start::Accepted
		| Start::RowHeld
		| Start::Rejected
		| Start::Failed
		| Start::Resolved => store.set_sent(batch, &tx).await.unwrap(),
		Start::Spent => store.spend_batch(batch, "S0", "m", T0).await.unwrap().is_some(),
		_ => true,
	};
	assert!(left, "{start:?}: second step");

	let error = Some(("V0", "m"));
	let settled = match start {
		Start::Held => store.hold_batch(batch, "H0", "m", T0).await.unwrap(),
		Start::Accepted => store.finish(row, NavVerdict::Done, None, T0).await.unwrap(),
		Start::RowHeld => store.finish_held(row, "D0", "m", T0).await.unwrap(),
		Start::Rejected | Start::Resolved => {
			store.finish(row, NavVerdict::Rejected, error, T0).await.unwrap()
		}
		Start::Failed => store.finish(row, NavVerdict::Failed, error, T0).await.unwrap(),
		_ => true,
	};
	assert!(settled, "{start:?}: third step");

	if matches!(start, Start::Resolved) {
		assert!(store.resolve(row, T0).await.unwrap());
	}
	Fixture { invoice, row: Some(row), batch: Some(batch), batch_uid: Some(batch_uid) }
}

/// `-1` is an id no row or batch has, so a fixture with neither is refused like a stale id.
async fn apply<S: NavStore>(store: &S, fx: &Fixture, op: Op) -> bool {
	let (row, batch) = (fx.row.unwrap_or(-1), fx.batch.unwrap_or(-1));
	let tx = format!("TX{op:?}{row}");
	let create = NavOp::Create;
	match op {
		Op::Claim => {
			let got = store.claim_batch(SELLER, create, &[fx.invoice], false).await.unwrap();
			got.is_some()
		}
		Op::BeginPost => store.begin_post(batch, "<again/>", &[(row, 2)], AT).await.unwrap(),
		Op::AbortPost => store.abort_post(batch, Some(("F1", "m")), AT).await.unwrap(),
		Op::MarkInDoubt => store.mark_in_doubt(batch, Some(("F1", "m")), AT).await.unwrap(),
		Op::SetSent => store.set_sent(batch, &tx).await.unwrap(),
		Op::ClearDoubt => store.clear_doubt(batch).await.unwrap(),
		Op::HoldBatch => store.hold_batch(batch, "H1", "m", AT).await.unwrap(),
		Op::SpendBatch => store.spend_batch(batch, "S1", "m", AT).await.unwrap().is_some(),
		Op::Finish => {
			let error = Some(("V1", "m"));
			store.finish(row, NavVerdict::Rejected, error, AT).await.unwrap()
		}
		Op::FinishHeld => store.finish_held(row, "D1", "m", AT).await.unwrap(),
		Op::ReleaseRow => store.release_row(row).await.unwrap(),
		Op::Cancel => store.cancel_filing(fx.invoice, create, AT).await.unwrap(),
		Op::Reopen => store.reopen_cancelled(row).await.unwrap(),
		Op::Confirm => store.refile(&[row], Some("C1"), AT).await.unwrap().is_some(),
		Op::Retry => store.refile(&[row], None, AT).await.unwrap().is_some(),
		Op::Attach => {
			let got = store
				.attach_transaction(&tx, SELLER, create, &[(row, 7)], AT, AdoptResult::Match)
				.await
				.unwrap();
			got.is_some()
		}
		Op::Resolve => store.resolve(row, AT).await.unwrap(),
		Op::RecordFault => store.record_batch_fault(batch, "F1", "m", AT).await.unwrap(),
	}
}

/// More than [`Want`] names, so a refusal is held to every column a transition can reach.
#[derive(Debug, PartialEq)]
struct Seen {
	batch: Option<B>,
	tx: Option<String>,
	batch_error: Option<String>,
	fault_at: Option<Timestamp>,
	posted_at: Option<Timestamp>,
	closed_at: Option<Timestamp>,
	row: Option<R>,
	verdict: Option<NavVerdict>,
	row_error: Option<String>,
	idx: Option<i64>,
	done_at: Option<Timestamp>,
	resolved_at: Option<Timestamp>,
	next: Option<Next>,
}

async fn seen<S: NavStore>(store: &S, fx: &Fixture) -> Seen {
	let batch = match &fx.batch_uid {
		Some(uid) => store.batch_by_uid(uid).await.unwrap(),
		None => None,
	};
	let filings = store.filings_by_invoice(fx.invoice).await.unwrap();
	let row = filings.iter().map(|f| &f.row).find(|r| fx.row.is_none_or(|id| r.id == id));
	let next = filings.first().filter(|f| f.row.batch_id != fx.batch).map(|f| {
		let batch = f.batch.as_ref().unwrap();
		Next {
			batch: batch.state,
			row: f.row.state,
			idx: f.row.idx,
			tx: batch.transaction_id.is_some(),
		}
	});
	Seen {
		batch: batch.as_ref().map(|b| b.state),
		tx: batch.as_ref().and_then(|b| b.transaction_id.clone()),
		batch_error: batch.as_ref().and_then(|b| b.error_code.clone()),
		fault_at: batch.as_ref().and_then(|b| b.fault_at),
		posted_at: batch.as_ref().and_then(|b| b.posted_at),
		closed_at: batch.as_ref().and_then(|b| b.closed_at),
		row: row.map(|r| r.state),
		verdict: row.and_then(|r| r.verdict),
		row_error: row.and_then(|r| r.error_code.clone()),
		idx: row.and_then(|r| r.idx),
		done_at: row.and_then(|r| r.done_at),
		resolved_at: row.and_then(|r| r.resolved_at),
		next,
	}
}

fn check(got: &Seen, want: &Want, label: &str) {
	assert_eq!(
		(
			got.batch,
			got.row,
			got.verdict,
			got.tx.is_some(),
			got.batch_error.as_deref(),
			got.row_error.as_deref(),
			got.fault_at,
			got.resolved_at,
			got.next,
		),
		(
			want.batch,
			want.row,
			want.verdict,
			want.tx,
			want.batch_error,
			want.row_error,
			want.fault_at,
			want.resolved_at,
			want.next,
		),
		"{label}"
	);
}

async fn totals<H: Harness>(h: &H) -> (i64, i64) {
	let batches = h.scalar_i64("SELECT COUNT(*) FROM nav_batches", &[]).await;
	(batches, h.scalar_i64("SELECT COUNT(*) FROM nav_submissions", &[]).await)
}

/// Every [`Start`] against every [`Op`]: a cell of [`want`] applies and leaves what it says, and
/// any other is refused and leaves everything as it was.
pub async fn transition_matrix<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-matrix");
	invoice::setup(&h).await;
	let store = h.store();
	for start in STARTS {
		for op in OPS {
			let label = format!("{start:?} x {op:?}");
			let fx = drive(store, start).await;
			let before = seen(store, &fx).await;
			check(&before, &base(start), &format!("{label}: the fixture"));
			let counted = totals(&h).await;

			let applied = apply(store, &fx, op).await;

			let after = seen(store, &fx).await;
			if let Some(want) = want(start, op) {
				assert!(applied, "{label}: refused");
				check(&after, &want, &label);
			} else {
				assert!(!applied, "{label}: applied");
				assert_eq!(after, before, "{label}: a refusal wrote");
				assert_eq!(totals(&h).await, counted, "{label}: a refusal left rows");
			}
		}
	}
}

async fn claim<S: NavStore>(
	store: &S,
	seller: i64,
	op: NavOp,
	invoices: &[i64],
) -> (NavBatch, Vec<NavSubmission>) {
	let (batch, got) = store.claim_batch(seller, op, invoices, true).await.unwrap().unwrap();
	assert_eq!(got, invoices);
	let rows = store.batch_rows(batch.id).await.unwrap();
	(batch, rows)
}

/// `n` issued invoices of [`SELLER`] in one `CLAIMED` batch.
async fn claimed<S: InvoiceStore + NavStore>(
	store: &S,
	n: usize,
) -> (NavBatch, Vec<NavSubmission>) {
	let mut invoices = Vec::with_capacity(n);
	for _ in 0..n {
		invoices.push(invoice::issued(store).await.id);
	}
	claim(store, SELLER, NavOp::Create, &invoices).await
}

/// B2 for every row, `idx` following the order given.
async fn post<S: NavStore>(store: &S, batch: &NavBatch, rows: &[NavSubmission]) {
	let indexed: Vec<(i64, i64)> = rows.iter().zip(1..).map(|(r, idx)| (r.id, idx)).collect();
	assert!(store.begin_post(batch.id, "<request/>", &indexed, T0).await.unwrap());
}

/// B6 on a fresh one-row batch: a hold is only ever written by a spend.
async fn spend_one<S: InvoiceStore + NavStore>(store: &S, code: &str, at: Timestamp) {
	let (batch, rows) = claimed(store, 1).await;
	post(store, &batch, &rows).await;
	assert!(store.spend_batch(batch.id, code, "no", at).await.unwrap().is_some());
}

/// [`claim`], [`post`] and B5.
async fn filed<S: NavStore>(
	store: &S,
	seller: i64,
	op: NavOp,
	invoices: &[i64],
) -> (NavBatch, Vec<NavSubmission>) {
	let (batch, rows) = claim(store, seller, op, invoices).await;
	post(store, &batch, &rows).await;
	assert!(store.set_sent(batch.id, &format!("TX{}", batch.id)).await.unwrap());
	(batch, rows)
}

/// `n` issued invoices of [`SELLER`] in one `SENT` batch.
async fn sent<S: InvoiceStore + NavStore>(store: &S, n: usize) -> (NavBatch, Vec<NavSubmission>) {
	let (batch, rows) = claimed(store, n).await;
	post(store, &batch, &rows).await;
	assert!(store.set_sent(batch.id, &format!("TX{}", batch.id)).await.unwrap());
	(batch, rows)
}

/// `n` `ACTIVE` rows in a batch `HELD`.
async fn held<S: InvoiceStore + NavStore>(store: &S, n: usize) -> (NavBatch, Vec<NavSubmission>) {
	let (batch, rows) = claimed(store, n).await;
	post(store, &batch, &rows).await;
	assert!(store.mark_in_doubt(batch.id, None, T0).await.unwrap());
	assert!(store.hold_batch(batch.id, "H0", "m", T0).await.unwrap());
	(batch, rows)
}

async fn batch_of<S: NavStore>(store: &S, uid: &str) -> NavBatch {
	store.batch_by_uid(uid).await.unwrap().unwrap()
}

/// The row `id` of `batch_id`, in whatever state it reached.
async fn row_of<S: NavStore>(store: &S, batch_id: i64, id: i64) -> NavSubmission {
	let rows = store.batch_rows(batch_id).await.unwrap();
	rows.into_iter().find(|r| r.id == id).unwrap()
}

async fn issued_test_mode<S: InvoiceStore>(store: &S) -> i64 {
	let d = invoice::draft(store, None).await;
	let input = IssueInvoice { test_mode: true, ..invoice::issue_input(d.id, 100_000) };
	store.issue(d.id, &input, d.version).await.unwrap().id
}

/// `job::poll` matches NAV's index against `idx` and the archive is sliced by it, so one without
/// the other cannot be read back.
pub async fn begin_post_writes_archive_and_idx_together<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-begin-post");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = claimed(store, 2).await;
	let (a, b) = (rows[0].id, rows[1].id);

	assert!(!store.begin_post(batch.id, "<short/>", &[(a, 1)], T0).await.unwrap());
	assert!(store.batch_archive(batch.id).await.unwrap().is_none());
	assert!(store.batch_rows(batch.id).await.unwrap().iter().all(|r| r.idx.is_none()));
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Claimed);

	assert!(store.begin_post(batch.id, "<first/>", &[(b, 2), (a, 1)], T0).await.unwrap());
	let idx: Vec<_> = store.batch_rows(batch.id).await.unwrap().iter().map(|r| r.idx).collect();
	assert_eq!(idx, [Some(1), Some(2)]);
	let archive = store.batch_archive(batch.id).await.unwrap().unwrap();
	assert_eq!(archive.request_xml.as_deref(), Some("<first/>"));
	assert_eq!(archive.response_xml, None);
	let posting = batch_of(store, batch.uid.as_str()).await;
	assert_eq!((posting.state, posting.posted_at), (B::Posting, Some(T0)));
}

/// A batch back in `CLAIMED` was proved not filed, so the new envelope is the only one NAV can
/// hold: the old request and its reply go, and the fault stays.
pub async fn a_second_begin_post_overwrites_the_archive<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-repost");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = claimed(store, 1).await;
	post(store, &batch, &rows).await;
	store.archive_response(batch.id, "<reply/>").await.unwrap();
	assert!(store.abort_post(batch.id, Some(("F0", "m")), T0).await.unwrap());

	assert!(store.begin_post(batch.id, "<second/>", &[(rows[0].id, 1)], AT).await.unwrap());

	let archive = store.batch_archive(batch.id).await.unwrap().unwrap();
	assert_eq!(archive.request_xml.as_deref(), Some("<second/>"));
	assert_eq!(archive.response_xml, None);
	let posting = batch_of(store, batch.uid.as_str()).await;
	assert_eq!(posting.error_code.as_deref(), Some("F0"));
	assert_eq!((posting.posted_at, posting.fault_at), (Some(AT), Some(T0)));
}

/// A persistent fault keeps ageing across attempts, which is what `nav.fault_alert_after` reads.
pub async fn a_fault_keeps_ageing_across_attempts<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-fault-ages");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = claimed(store, 1).await;
	assert!(store.record_batch_fault(batch.id, "F0", "m", T0).await.unwrap());

	post(store, &batch, &rows).await;
	assert!(store.abort_post(batch.id, None, AT).await.unwrap());
	let kept = batch_of(store, batch.uid.as_str()).await;
	assert_eq!((kept.error_code.as_deref(), kept.fault_at), (Some("F0"), Some(T0)));
	assert_eq!(kept.posted_at, Some(T0), "`posted_at` stays");

	post(store, &batch, &rows).await;
	assert!(store.abort_post(batch.id, Some(("F1", "m")), AT).await.unwrap());
	let replaced = batch_of(store, batch.uid.as_str()).await;
	assert_eq!((replaced.error_code.as_deref(), replaced.fault_at), (Some("F1"), Some(T0)));
}

/// A fault left by an earlier attempt says nothing of this one, and would refuse `clear_doubt`.
pub async fn mark_in_doubt_with_no_fault_clears_the_old_one<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-doubt-clears");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = claimed(store, 1).await;
	post(store, &batch, &rows).await;
	assert!(store.record_batch_fault(batch.id, "F0", "m", T0).await.unwrap());

	assert!(store.mark_in_doubt(batch.id, None, AT).await.unwrap());

	let doubted = batch_of(store, batch.uid.as_str()).await;
	assert_eq!((doubted.error_code, doubted.error_msg, doubted.fault_at), (None, None, None));
	assert!(store.clear_doubt(batch.id).await.unwrap());
}

/// A fault parked by an earlier attempt must not outlive NAV taking the filing, and one recorded
/// after it stands: it is what `awaiting_operator` counts on a `SENT` batch.
pub async fn set_sent_clears_a_parked_fault<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-sent-clears-fault");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = claimed(store, 1).await;
	post(store, &batch, &rows).await;
	assert!(
		store
			.mark_in_doubt(batch.id, Some(("REQUEST_ID_NOT_UNIQUE", "used")), T0)
			.await
			.unwrap()
	);

	assert!(store.set_sent(batch.id, "TX1").await.unwrap());
	let cleared = batch_of(store, batch.uid.as_str()).await;
	assert_eq!((cleared.error_code, cleared.error_msg, cleared.fault_at), (None, None, None));

	assert!(
		store
			.record_batch_fault(batch.id, "E-NAV-POLL-STALE", "late", AT)
			.await
			.unwrap()
	);
	assert!(!store.set_sent(batch.id, "TX2").await.unwrap());
	let late = batch_of(store, batch.uid.as_str()).await;
	assert_eq!(late.error_code.as_deref(), Some("E-NAV-POLL-STALE"));
	assert_eq!(late.transaction_id.as_deref(), Some("TX1"));
}

/// `idx_nav_batch_transaction` must answer `false` here, never a unique violation.
pub async fn set_sent_refuses_a_transaction_another_batch_carries<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-sent-unique");
	invoice::setup(&h).await;
	let store = h.store();
	let (first, rows) = claimed(store, 1).await;
	post(store, &first, &rows).await;
	let (second, rows) = claimed(store, 1).await;
	post(store, &second, &rows).await;

	assert!(store.set_sent(first.id, "TX1").await.unwrap());
	assert!(!store.set_sent(second.id, "TX1").await.unwrap());

	let refused = batch_of(store, second.uid.as_str()).await;
	assert_eq!((refused.state, refused.transaction_id), (B::Posting, None));
	let carrier = store.batch_by_transaction("TX1").await.unwrap().unwrap();
	assert_eq!(carrier.id, first.id);
}

/// A row, even a never-batched one, is how an operator's cancel beats the first POST: `claim_batch`
/// is create-only, so it leaves the invoice out and no empty batch behind.
pub async fn claim_batch_skips_a_cancelled_invoice<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-cancel-no-row");
	invoice::setup(&h).await;
	let store = h.store();
	let stopped = invoice::issued(store).await.id;
	let other = invoice::issued(store).await.id;

	assert!(store.cancel_filing(stopped, NavOp::Create, T0).await.unwrap());
	let filing = store.filing_by_invoice(stopped).await.unwrap().unwrap();
	assert_eq!(filing.state(), FilingState::Cancelled);
	assert!(filing.batch.is_none());
	assert_eq!((filing.row.done_at, filing.row.resolved_at), (Some(T0), None));
	assert!(!store.cancel_filing(stopped, NavOp::Create, T0).await.unwrap());

	assert!(
		store
			.claim_batch(SELLER, NavOp::Create, &[stopped], false)
			.await
			.unwrap()
			.is_none()
	);
	assert_eq!(totals(&h).await, (0, 1));

	let (_, got) = store
		.claim_batch(SELLER, NavOp::Create, &[stopped, other], false)
		.await
		.unwrap()
		.unwrap();
	assert_eq!(got, [other]);

	assert!(store.reopen_cancelled(filing.row.id).await.unwrap());
	claim(store, SELLER, NavOp::Create, &[stopped]).await;
}

/// `begin_post` and `cancel_filing` serialize in the store: once `POSTING`, the envelope may be on
/// the wire, so the cancel is refused and nothing moves.
pub async fn cancel_filing_is_refused_against_a_posting_batch<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-cancel-posting");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = claimed(store, 2).await;
	post(store, &batch, &rows).await;

	assert!(!store.cancel_filing(rows[0].invoice_id, NavOp::Create, AT).await.unwrap());

	let row = row_of(store, batch.id, rows[0].id).await;
	assert_eq!((row.state, row.done_at), (R::Active, None));
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Posting);
}

/// Cancelling one invoice touches no other, and the envelope built before it no longer describes
/// the batch.
pub async fn cancelling_one_row_leaves_its_batch_for_the_others<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-cancel-one");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = claimed(store, 2).await;

	assert!(store.cancel_filing(rows[0].invoice_id, NavOp::Create, AT).await.unwrap());

	let cancelled = row_of(store, batch.id, rows[0].id).await;
	assert_eq!((cancelled.state, cancelled.done_at), (R::Cancelled, Some(AT)));
	assert_eq!(row_of(store, batch.id, rows[1].id).await.state, R::Active);
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Claimed);
	let stale = [(rows[0].id, 1), (rows[1].id, 2)];
	assert!(!store.begin_post(batch.id, "<stale/>", &stale, AT).await.unwrap());
	assert!(store.begin_post(batch.id, "<one/>", &[(rows[1].id, 1)], AT).await.unwrap());
}

pub async fn release_row_keeps_a_batch_with_rows_left<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-release-one");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = claimed(store, 2).await;

	assert!(store.release_row(rows[0].id).await.unwrap());

	let left = store.batch_rows(batch.id).await.unwrap();
	assert_eq!(left.iter().map(|r| r.id).collect::<Vec<_>>(), [rows[1].id]);
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Claimed);
	assert_eq!(store.unfiled_invoices(SELLER, 10).await.unwrap(), [rows[0].invoice_id]);
}

/// All of it or none: a spent `requestId` with its rows still live, or a new batch with no hold,
/// would send again under a seller NAV is refusing.
pub async fn spend_batch_moves_every_row_and_holds_the_seller<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-spend");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = claimed(store, 2).await;
	post(store, &batch, &rows).await;
	assert!(store.record_batch_fault(batch.id, "F0", "m", T0).await.unwrap());

	let new = store.spend_batch(batch.id, "FORBIDDEN", "no", AT).await.unwrap().unwrap();

	let old = batch_of(store, batch.uid.as_str()).await;
	assert_eq!((old.state, old.closed_at), (B::Closed, Some(AT)));
	assert_eq!((old.error_code.as_deref(), old.fault_at), (Some("FORBIDDEN"), Some(T0)));
	for row in store.batch_rows(batch.id).await.unwrap() {
		assert_eq!((row.state, row.verdict), (R::Failed, Some(NavVerdict::Failed)));
		assert_eq!(row.error_code.as_deref(), Some("FORBIDDEN"));
		assert_eq!((row.done_at, row.resolved_at), (Some(AT), Some(AT)));
	}

	assert_ne!(new.uid.as_str(), batch.uid.as_str());
	assert_eq!((new.state, new.seller_id, new.op), (B::Claimed, SELLER, NavOp::Create));
	let moved = store.batch_rows(new.id).await.unwrap();
	assert_eq!(
		moved.iter().map(|r| r.invoice_id).collect::<Vec<_>>(),
		rows.iter().map(|r| r.invoice_id).collect::<Vec<_>>()
	);
	for row in moved {
		assert_eq!((row.state, row.idx, row.verdict), (R::Active, None, None));
		assert_eq!(row.electronic, Some(true));
	}

	let hold = store.seller_hold(SELLER).await.unwrap().unwrap();
	assert_eq!((hold.code.as_str(), hold.held_at), ("FORBIDDEN", AT));
}

pub async fn spend_batch_leaves_an_earlier_hold_standing<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-spend-held");
	invoice::setup(&h).await;
	let store = h.store();
	spend_one(store, "FIRST", T0).await;
	let (batch, rows) = claimed(store, 1).await;
	post(store, &batch, &rows).await;

	assert!(store.spend_batch(batch.id, "SECOND", "m", AT).await.unwrap().is_some());

	let hold = store.seller_hold(SELLER).await.unwrap().unwrap();
	assert_eq!((hold.code.as_str(), hold.held_at), ("FIRST", T0));
}

pub async fn a_refused_spend_holds_nobody<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-spend-refused");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, _) = claimed(store, 1).await;

	assert!(store.spend_batch(batch.id, "FORBIDDEN", "m", AT).await.unwrap().is_none());

	assert!(store.seller_hold(SELLER).await.unwrap().is_none());
	assert_eq!(totals(&h).await, (1, 1));
}

/// `close_if_empty` waits for the last `ACTIVE` row, and a row `HELD` by R3 no longer counts.
pub async fn finish_closes_the_batch_with_its_last_active_row<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-finish-closes");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = sent(store, 4).await;

	assert!(store.finish(rows[0].id, NavVerdict::Done, None, AT).await.unwrap());
	assert!(store.finish_held(rows[1].id, "D0", "m", AT).await.unwrap());
	let error = Some(("V0", "m"));
	assert!(store.finish(rows[2].id, NavVerdict::Failed, error, AT).await.unwrap());
	let open = batch_of(store, batch.uid.as_str()).await;
	assert_eq!((open.state, open.closed_at), (B::Sent, None));

	assert!(store.finish(rows[3].id, NavVerdict::Warn, None, AT).await.unwrap());
	let closed = batch_of(store, batch.uid.as_str()).await;
	assert_eq!((closed.state, closed.closed_at), (B::Closed, Some(AT)));

	let states: Vec<_> = store
		.batch_rows(batch.id)
		.await
		.unwrap()
		.iter()
		.map(|r| (r.state, r.verdict, r.done_at, r.resolved_at))
		.collect();
	assert_eq!(
		states,
		[
			(R::Accepted, Some(NavVerdict::Done), Some(AT), None),
			(R::Held, Some(NavVerdict::Rejected), Some(AT), None),
			(R::Failed, Some(NavVerdict::Failed), Some(AT), None),
			(R::Accepted, Some(NavVerdict::Warn), Some(AT), None),
		]
	);
}

/// R5 for a batch a person called: one new batch takes every row, the appearance filed included.
pub async fn refile_moves_held_rows_into_one_new_batch<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-refile-three");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = held(store, 3).await;
	let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();

	let new = store.refile(&ids, Some("C0"), AT).await.unwrap().unwrap();

	let old = batch_of(store, batch.uid.as_str()).await;
	assert_eq!((old.state, old.closed_at), (B::Closed, Some(AT)));
	for row in store.batch_rows(batch.id).await.unwrap() {
		assert_eq!((row.state, row.verdict), (R::Failed, Some(NavVerdict::Failed)));
		assert_eq!(row.error_code.as_deref(), Some("C0"));
		assert_eq!((row.done_at, row.resolved_at), (Some(AT), Some(AT)));
	}
	assert_eq!((new.state, new.seller_id, new.op), (B::Claimed, SELLER, NavOp::Create));
	let moved = store.batch_rows(new.id).await.unwrap();
	assert_eq!(moved.len(), 3);
	assert!(moved.iter().all(|r| r.state == R::Active && r.idx.is_none()));
	assert!(moved.iter().all(|r| r.electronic == Some(true)));
}

pub async fn refile_leaves_a_held_batch_open_for_its_other_rows<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-refile-partial");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = held(store, 2).await;

	assert!(store.refile(&[rows[0].id], Some("C0"), AT).await.unwrap().is_some());
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Held);
	assert_eq!(row_of(store, batch.id, rows[1].id).await.state, R::Active);

	assert!(store.refile(&[rows[1].id], Some("C0"), AT).await.unwrap().is_some());
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Closed);
}

/// NAV's word is the statutory record: a person's confirmation does not amend it.
pub async fn refile_keeps_what_nav_said_on_a_held_row<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-refile-verdict");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = sent(store, 1).await;
	assert!(store.finish_held(rows[0].id, "D0", "dup", T0).await.unwrap());

	assert!(store.refile(&[rows[0].id], Some("C0"), AT).await.unwrap().is_some());

	let row = row_of(store, batch.id, rows[0].id).await;
	assert_eq!((row.state, row.verdict), (R::Rejected, Some(NavVerdict::Rejected)));
	assert_eq!((row.error_code.as_deref(), row.error_msg.as_deref()), (Some("D0"), Some("dup")));
	assert_eq!((row.done_at, row.resolved_at), (Some(T0), Some(AT)));
}

/// `idx_nav_submission_live` is partial: one open row per `(invoice_id, op)`, and resolving it
/// leaves room for exactly one successor. Resolved history is still a filing.
pub async fn a_retry_admits_one_row<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-retry-row");
	invoice::setup(&h).await;
	let store = h.store();
	let inv = invoice::issued(store).await;
	invoice::fail_submission(store, SELLER, inv.id).await;
	let first = store.filing_by_invoice(inv.id).await.unwrap().unwrap();
	assert!(
		store
			.claim_batch(SELLER, NavOp::Create, &[inv.id], false)
			.await
			.unwrap()
			.is_none()
	);

	let new = store.refile(&[first.row.id], None, AT).await.unwrap().unwrap();

	let newest = store.filing_by_invoice(inv.id).await.unwrap().unwrap();
	assert_ne!(newest.row.id, first.row.id);
	assert_eq!(newest.row.batch_id, Some(new.id));
	assert_eq!(newest.state(), FilingState::Claimed);
	assert_eq!(newest.row.electronic, Some(false), "the appearance is copied");
	let batches = totals(&h).await;
	assert!(store.refile(&[first.row.id], None, AT).await.unwrap().is_none(), "a live row stands");
	assert_eq!(totals(&h).await, batches);
	assert!(
		store
			.claim_batch(SELLER, NavOp::Create, &[inv.id], false)
			.await
			.unwrap()
			.is_none()
	);
	assert!(!store.unfiled_invoices(SELLER, 10).await.unwrap().contains(&inv.id));
}

/// The new batch is filed under one seller and one `op`, so a set spanning either has no answer.
pub async fn refile_refuses_a_set_that_is_not_one_batch<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-refile-span");
	invoice::setup(&h).await;
	let store = h.store();
	let ver = invoice::second_seller(&h).await;
	let error = Some(("V0", "m"));

	let mine = invoice::issued(store).await.id;
	let (_, mine) = filed(store, SELLER, NavOp::Create, &[mine]).await;
	let theirs = invoice::issued_second(store, ver).await.id;
	let (_, theirs) = filed(store, 2, NavOp::Create, &[theirs]).await;
	let storno = invoice::issued(store).await.id;
	let (_, storno) = filed(store, SELLER, NavOp::Storno, &[storno]).await;
	for row in [&mine[0], &theirs[0], &storno[0]] {
		assert!(store.finish(row.id, NavVerdict::Rejected, error, T0).await.unwrap());
	}
	let counted = totals(&h).await;

	assert!(store.refile(&[], None, AT).await.unwrap().is_none());
	assert!(store.refile(&[mine[0].id, theirs[0].id], None, AT).await.unwrap().is_none());
	assert!(store.refile(&[mine[0].id, storno[0].id], None, AT).await.unwrap().is_none());
	assert!(store.refile(&[mine[0].id, -1], None, AT).await.unwrap().is_none());

	assert_eq!(totals(&h).await, counted);
	let row = store.filing_by_invoice(mine[0].invoice_id).await.unwrap().unwrap().row;
	assert_eq!((row.id, row.resolved_at), (mine[0].id, None));
}

/// R9: NAV holds the filing, so its record stands and a new row carries NAV's index to the poll.
pub async fn attach_keeps_a_verdict_row_and_inserts_a_new_one<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-attach-verdict");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = sent(store, 2).await;
	assert!(store.finish_held(rows[0].id, "D0", "dup", T0).await.unwrap());
	assert!(store.finish_held(rows[1].id, "D0", "dup", T0).await.unwrap());

	let pair = [(rows[1].id, 5), (rows[0].id, 4)];
	let new = store
		.attach_transaction("TXA", SELLER, NavOp::Create, &pair, AT, AdoptResult::Match)
		.await
		.unwrap()
		.unwrap();

	assert_eq!((new.state, new.posted_at), (B::Sent, Some(AT)));
	assert_eq!(new.transaction_id.as_deref(), Some("TXA"));
	assert!(store.batch_archive(new.id).await.unwrap().is_none(), "nothing of ours was filed");
	let carrier = store.batch_by_transaction("TXA").await.unwrap().unwrap();
	assert_eq!(carrier.id, new.id);

	for row in store.batch_rows(batch.id).await.unwrap() {
		assert_eq!((row.state, row.verdict), (R::Rejected, Some(NavVerdict::Rejected)));
		assert_eq!(row.error_code.as_deref(), Some("D0"));
		assert_eq!(row.resolved_at, Some(AT));
	}
	let inserted = store.batch_rows(new.id).await.unwrap();
	let idx: Vec<_> = inserted.iter().map(|r| (r.invoice_id, r.idx)).collect();
	assert_eq!(idx, [(rows[0].invoice_id, Some(4)), (rows[1].invoice_id, Some(5))]);
	for row in inserted {
		assert_eq!((row.state, row.verdict, row.error_code), (R::Active, None, None));
		assert_eq!(row.electronic, Some(true));
	}
}

/// A partial transaction: rows NAV did not return stay `HELD` for a person.
pub async fn attach_leaves_the_rows_it_did_not_name<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-attach-partial");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = held(store, 3).await;

	let pair = [(rows[0].id, 1), (rows[1].id, 2)];
	let new = store
		.attach_transaction("TXA", SELLER, NavOp::Create, &pair, AT, AdoptResult::Match)
		.await
		.unwrap()
		.unwrap();

	let moved = store.batch_rows(new.id).await.unwrap();
	let invoices = [rows[0].invoice_id, rows[1].invoice_id];
	assert_eq!(moved.iter().map(|r| r.invoice_id).collect::<Vec<_>>(), invoices);
	assert_eq!(moved.iter().map(|r| r.idx).collect::<Vec<_>>(), [Some(1), Some(2)]);
	assert!(moved.iter().all(|r| r.state == R::Active));
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Held);
	let left = store.batch_rows(batch.id).await.unwrap();
	let stood: Vec<_> = left.iter().map(|r| (r.id, r.state, r.resolved_at)).collect();
	let (cancelled, active) = (R::Cancelled, R::Active);
	assert_eq!(
		stood,
		[
			(rows[0].id, cancelled, Some(AT)),
			(rows[1].id, cancelled, Some(AT)),
			(rows[2].id, active, None)
		],
		"a replaced row stays on its batch, resolved, and the unnamed one stays live"
	);

	let last = [(rows[2].id, 1)];
	assert!(
		store
			.attach_transaction("TXB", SELLER, NavOp::Create, &last, AT, AdoptResult::Match)
			.await
			.unwrap()
			.is_some()
	);
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Closed);
}

/// `close_held` closes `HELD` batches only: a row `HELD` by R3 leaves a `SENT` batch whose
/// siblings still await the poll.
pub async fn attach_leaves_a_sent_batch_to_its_poll<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-attach-sent");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = sent(store, 2).await;
	assert!(store.finish_held(rows[0].id, "D0", "m", T0).await.unwrap());

	let pair = [(rows[0].id, 1)];
	assert!(
		store
			.attach_transaction("TXA", SELLER, NavOp::Create, &pair, AT, AdoptResult::Match)
			.await
			.unwrap()
			.is_some()
	);

	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Sent);
	assert_eq!(row_of(store, batch.id, rows[1].id).await.state, R::Active);
}

pub async fn attach_refuses_what_it_cannot_bind<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-attach-refused");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = held(store, 1).await;
	let (carrier, active) = sent(store, 1).await;
	let carried = format!("TX{}", carrier.id);
	let row = [(rows[0].id, 1)];
	let counted = totals(&h).await;

	let create = NavOp::Create;
	assert!(
		store
			.attach_transaction(&carried, SELLER, create, &row, AT, AdoptResult::Match)
			.await
			.unwrap()
			.is_none()
	);
	assert!(
		store
			.attach_transaction("TXA", SELLER, create, &[], AT, AdoptResult::Match)
			.await
			.unwrap()
			.is_none()
	);
	assert!(
		store
			.attach_transaction("TXA", 2, create, &row, AT, AdoptResult::Match)
			.await
			.unwrap()
			.is_none()
	);
	let storno = NavOp::Storno;
	assert!(
		store
			.attach_transaction("TXA", SELLER, storno, &row, AT, AdoptResult::Match)
			.await
			.unwrap()
			.is_none()
	);
	let both = [(rows[0].id, 1), (active[0].id, 2)];
	assert!(
		store
			.attach_transaction("TXA", SELLER, create, &both, AT, AdoptResult::Match)
			.await
			.unwrap()
			.is_none()
	);

	assert_eq!(totals(&h).await, counted);
	assert_eq!(row_of(store, batch.id, rows[0].id).await.state, R::Active);
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Held);
}

/// A batch is owed a `NAV_ADOPT` while any row is `HELD` and unchecked, whatever its own state.
pub async fn adoptable_lists_a_held_row_whatever_the_batch_state<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-adoptable");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = sent(store, 3).await;
	sent(store, 1).await;
	let adoptable = async || -> Vec<String> {
		let uids = store.adoptable(10).await.unwrap();
		uids.iter().map(|u| u.as_str().to_owned()).collect()
	};
	let uid = batch.uid.as_str().to_owned();
	assert!(adoptable().await.is_empty());

	assert!(store.finish_held(rows[0].id, "D0", "m", T0).await.unwrap());
	assert_eq!(adoptable().await, std::slice::from_ref(&uid), "in a batch still SENT");

	assert!(store.finish_held(rows[1].id, "D0", "m", T0).await.unwrap());
	assert!(store.finish(rows[2].id, NavVerdict::Done, None, T0).await.unwrap());
	assert_eq!(batch_of(store, batch.uid.as_str()).await.state, B::Closed);
	assert_eq!(adoptable().await, std::slice::from_ref(&uid), "once, and in a batch CLOSED");

	assert_eq!(
		store
			.stamp_adopt_checked(&[rows[0].id], AdoptResult::Different, None, AT)
			.await
			.unwrap(),
		1
	);
	assert_eq!(adoptable().await, [uid], "one row is still unchecked");

	assert_eq!(
		store
			.stamp_adopt_checked(&[rows[1].id], AdoptResult::Different, None, AT)
			.await
			.unwrap(),
		1
	);
	assert!(adoptable().await.is_empty());
}

/// A crash between attach and stamp repeats the job, and an adopted row is no longer `HELD`.
pub async fn stamp_adopt_checked_leaves_a_moved_row_alone<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-stamp");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = sent(store, 2).await;
	assert!(store.finish_held(rows[0].id, "D0", "m", T0).await.unwrap());
	assert!(store.finish_held(rows[1].id, "D0", "m", T0).await.unwrap());
	let pair = [(rows[0].id, 1)];
	assert!(
		store
			.attach_transaction("TXA", SELLER, NavOp::Create, &pair, AT, AdoptResult::Match)
			.await
			.unwrap()
			.is_some()
	);

	let ids = [rows[0].id, rows[1].id];
	assert_eq!(store.stamp_adopt_checked(&ids, AdoptResult::Different, None, AT).await.unwrap(), 1);
	assert_eq!(store.stamp_adopt_checked(&[], AdoptResult::Different, None, AT).await.unwrap(), 0);

	assert_eq!(row_of(store, batch.id, rows[0].id).await.adopt_checked_at, None);
	assert_eq!(row_of(store, batch.id, rows[1].id).await.adopt_checked_at, Some(AT));
}

/// A person's re-check forgets what the job stamped; `attach_transaction` writes the reason on the
/// row that replaces it, which is the one `filing_by_invoice` answers.
pub async fn the_adopt_result_stamps_clears_and_survives_attach<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-adopt-result");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = sent(store, 1).await;
	let (id, invoice) = (rows[0].id, rows[0].invoice_id);
	assert!(!store.clear_adopt_checked(id).await.unwrap(), "only a HELD row");
	assert!(store.finish_held(id, "D0", "m", T0).await.unwrap());

	assert_eq!(
		store
			.stamp_adopt_checked(&[id], AdoptResult::Different, Some("d"), T0)
			.await
			.unwrap(),
		1
	);
	let row = row_of(store, batch.id, id).await;
	assert_eq!(row.adopt_result, Some(AdoptResult::Different));
	assert_eq!(row.adopt_detail.as_deref(), Some("d"));
	// A person acknowledged it; a re-check must count again in `awaiting_operator`.
	assert!(store.resolve(id, T0).await.unwrap());

	assert!(store.clear_adopt_checked(id).await.unwrap());
	let row = row_of(store, batch.id, id).await;
	assert_eq!((row.adopt_checked_at, row.adopt_result, row.adopt_detail), (None, None, None));
	assert_eq!(row.resolved_at, None, "a re-check re-opens an acknowledged row");

	let pair = [(id, 1)];
	let got =
		store.attach_transaction("TXA", SELLER, NavOp::Create, &pair, AT, AdoptResult::Confirmed);
	assert!(got.await.unwrap().is_some());
	let live = store.filing_by_invoice(invoice).await.unwrap().unwrap().row;
	assert_ne!(live.id, id);
	assert_eq!(live.adopt_result, Some(AdoptResult::Confirmed));
	assert_eq!(live.adopt_checked_at, Some(AT));
	assert_eq!(live.adopt_detail, None, "a replacement row starts with no stale diff");
}

/// An acknowledged `HELD` row stays acknowledged once another row is live for its invoice and op.
pub async fn clearing_the_adopt_check_refuses_beside_another_live_row<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-adopt-clear-live");
	invoice::setup(&h).await;
	let store = h.store();
	let (batch, rows) = sent(store, 1).await;
	let (id, invoice) = (rows[0].id, rows[0].invoice_id);
	assert!(store.finish_held(id, "D0", "m", T0).await.unwrap());
	let ids = [id];
	let stamped = store.stamp_adopt_checked(&ids, AdoptResult::Different, Some("d"), T0);
	assert_eq!(stamped.await.unwrap(), 1);
	assert!(store.resolve(id, T0).await.unwrap());
	let claim = store.claim_batch(SELLER, NavOp::Create, &[invoice], false).await.unwrap();
	assert!(claim.is_some(), "a resolved row frees the live slot");

	assert!(!store.clear_adopt_checked(id).await.unwrap());
	assert!(row_of(store, batch.id, id).await.resolved_at.is_some());
}

/// Each listed batch has an owner job `NAV_SWEEP` checks; `HELD` runs none and `CLOSED` needs none.
pub async fn supervised_pages_by_keyset<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-supervised");
	invoice::setup(&h).await;
	let store = h.store();
	let mut owed = Vec::new();
	for start in [Start::Claimed, Start::Posting, Start::InDoubt, Start::Sent] {
		drive(store, Start::Held).await;
		owed.push(drive(store, start).await.batch.unwrap());
		drive(store, Start::Accepted).await;
	}
	let page = async |after, limit| -> Vec<i64> {
		let batches = store.supervised(after, limit).await.unwrap();
		batches.iter().map(|b| b.id).collect()
	};

	assert_eq!(page(0, 3).await, owed[..3]);
	assert_eq!(page(owed[2], 3).await, owed[3..]);
	assert!(page(owed[3], 3).await.is_empty());
}

/// What `NAV_SWEEP` re-enqueues. A live job already owns its invoice, a finished one does not, and
/// a row of any state means the filing has an owner elsewhere.
pub async fn orphaned_invoices_spans_every_seller<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-orphaned");
	invoice::setup(&h).await;
	let store = h.store();
	let ver = invoice::second_seller(&h).await;

	let mine = invoice::issued(store).await.id;
	let theirs = invoice::issued_second(store, ver).await.id;
	claimed(store, 1).await;
	let cancelled = invoice::issued(store).await.id;
	assert!(store.cancel_filing(cancelled, NavOp::Create, T0).await.unwrap());
	issued_test_mode(store).await;
	invoice::draft(store, None).await;
	let d = invoice::draft(store, None).await;
	let input = IssueInvoice { nav_report_at: Some(T0), ..invoice::issue_input(d.id, 100_000) };
	let queued = store.issue(d.id, &input, d.version).await.unwrap().id;
	let key = json!(invoice_job_key(queued));

	assert_eq!(store.orphaned_invoices(10).await.unwrap(), [mine, theirs]);
	assert_eq!(store.orphaned_invoices(1).await.unwrap(), [mine]);

	h.exec("UPDATE jobs SET status = 'RUNNING' WHERE dedup_key = ?", std::slice::from_ref(&key))
		.await;
	assert_eq!(store.orphaned_invoices(10).await.unwrap(), [mine, theirs]);

	h.exec("UPDATE jobs SET status = 'FAILED' WHERE dedup_key = ?", &[key]).await;
	assert_eq!(store.orphaned_invoices(10).await.unwrap(), [mine, theirs, queued]);
}

/// Each state's answer, then the ones that turn on a stamp or on the grace. `POSTING` and
/// `IN_DOUBT` have an owner job, so a fault there never pages anyone.
pub async fn awaiting_operator_counts_each_state_and_the_grace<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-awaiting");
	invoice::setup(&h).await;
	let store = h.store();
	let count = async |before| store.awaiting_operator(Some(SELLER), before).await.unwrap();

	for (start, counted) in [
		(Start::NoRow, false),
		(Start::Claimed, false),
		(Start::Posting, false),
		(Start::InDoubt, false),
		(Start::Faulted, false),
		(Start::Held, true),
		(Start::Sent, false),
		(Start::Accepted, false),
		(Start::RowHeld, false),
		(Start::Rejected, true),
		(Start::Failed, true),
		(Start::Resolved, false),
		(Start::Spent, false),
		(Start::Cancelled, false),
		(Start::Stopped, false),
	] {
		let was = count(AT).await;
		drive(store, start).await;
		assert_eq!(count(AT).await - was, i64::from(counted), "{start:?}");
	}

	let was = count(AT).await;
	let fx = drive(store, Start::Posting).await;
	assert!(store.record_batch_fault(fx.batch.unwrap(), "F0", "m", T0).await.unwrap());
	assert_eq!(count(AT).await, was, "a fault on POSTING");

	let fx = drive(store, Start::Sent).await;
	assert!(store.record_batch_fault(fx.batch.unwrap(), "F0", "m", T0).await.unwrap());
	assert_eq!(count(AT).await, was + 1, "a fault on SENT");

	let was = count(AT).await;
	let fx = drive(store, Start::RowHeld).await;
	assert_eq!(count(AT).await, was, "unchecked");
	assert_eq!(
		store
			.stamp_adopt_checked(&[fx.row.unwrap()], AdoptResult::Different, None, T0)
			.await
			.unwrap(),
		1
	);
	assert_eq!(count(AT).await, was + 1, "checked and left for a person");

	let was = count(AT).await;
	let fx = drive(store, Start::Claimed).await;
	assert!(store.record_batch_fault(fx.batch.unwrap(), "F0", "m", T0).await.unwrap());
	assert_eq!(count(T0).await, was, "inside the grace: `fault_at` is not before the bound");
	assert_eq!(count(Timestamp(T0.0 + 1)).await, was + 1, "past the grace");

	let was = count(AT).await;
	let fx = drive(store, Start::Rejected).await;
	assert_eq!(count(AT).await, was + 1);
	assert!(store.resolve(fx.row.unwrap(), AT).await.unwrap());
	assert_eq!(count(AT).await, was, "a person settled it");
	// 1000: earlier iterations leave unfiled invoices, and the query reads oldest first.
	assert!(
		!store.unfiled_invoices(SELLER, 1000).await.unwrap().contains(&fx.invoice),
		"resolving must not offer the invoice to the sweep again"
	);
}

/// `posted_at` never clears, so a batch aborted back to `CLAIMED` stays out of the count: it has
/// been on the wire.
pub async fn backlog_counts_what_has_not_reached_the_wire<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-backlog");
	invoice::setup(&h).await;
	let store = h.store();
	let ver = invoice::second_seller(&h).await;
	let backlog = async || store.backlog(SELLER).await.unwrap();
	assert_eq!(backlog().await, 0);

	invoice::issued(store).await;
	invoice::issued_second(store, ver).await;
	issued_test_mode(store).await;
	invoice::draft(store, None).await;
	assert_eq!(backlog().await, 1, "no row: only this seller's real issued invoice");

	let (batch, rows) = claimed(store, 2).await;
	assert_eq!(backlog().await, 3, "ACTIVE in a batch never posted");

	post(store, &batch, &rows).await;
	assert_eq!(backlog().await, 1);
	assert!(store.abort_post(batch.id, None, AT).await.unwrap());
	assert_eq!(backlog().await, 1, "aborted back to CLAIMED");

	let (_, rows) = claimed(store, 1).await;
	assert_eq!(backlog().await, 2);
	assert!(store.cancel_filing(rows[0].invoice_id, NavOp::Create, AT).await.unwrap());
	assert_eq!(backlog().await, 1, "a cancelled row is not waiting");
}

pub async fn a_seller_hold_round_trips<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-hold");
	invoice::setup(&h).await;
	let store = h.store();
	assert!(store.seller_hold(SELLER).await.unwrap().is_none());
	assert_eq!(store.seller_holds_count().await.unwrap(), 0);
	assert!(!store.release_seller(SELLER).await.unwrap());

	spend_one(store, "FORBIDDEN", T0).await;
	spend_one(store, "LATER", AT).await;

	let hold = store.seller_hold(SELLER).await.unwrap().unwrap();
	assert_eq!(hold.seller_id, SELLER);
	assert_eq!(
		(hold.code.as_str(), hold.message.as_str()),
		("FORBIDDEN", "no"),
		"the first stands"
	);
	assert_eq!(hold.held_at, T0);
	assert_eq!(store.seller_holds_count().await.unwrap(), 1);

	assert!(store.release_seller(SELLER).await.unwrap());
	assert!(store.seller_hold(SELLER).await.unwrap().is_none());
	assert_eq!(store.seller_holds_count().await.unwrap(), 0);

	spend_one(store, "LATER", AT).await;
	let hold = store.seller_hold(SELLER).await.unwrap().unwrap();
	assert_eq!(hold.code, "LATER", "and can be held again");
}

/// `filing_by_invoice` is the head of this list; the rest is the history a retry leaves.
pub async fn filings_by_invoice_lists_every_row_newest_first<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-by-invoice");
	invoice::setup(&h).await;
	let store = h.store();
	let inv = invoice::issued(store).await.id;
	assert!(store.filings_by_invoice(inv).await.unwrap().is_empty());

	invoice::fail_submission(store, SELLER, inv).await;
	let first = store.filing_by_invoice(inv).await.unwrap().unwrap().row.id;
	assert!(store.resolve(first, T0).await.unwrap());
	assert!(store.refile(&[first], None, AT).await.unwrap().is_some());
	let second = store.filing_by_invoice(inv).await.unwrap().unwrap().row.id;
	invoice::fail_submission(store, SELLER, invoice::issued(store).await.id).await;

	let filings = store.filings_by_invoice(inv).await.unwrap();
	assert_eq!(filings.iter().map(|f| f.row.id).collect::<Vec<_>>(), [second, first]);
	assert_eq!(filings[1].state(), FilingState::Rejected);
}

/// The key is selected from `nav_batches`, so an unknown id is zero rows rather than `FOREIGN KEY
/// constraint failed`. The latest reply wins.
pub async fn archive_response_keeps_the_latest_reply_of_a_known_batch<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-archive-response");
	invoice::setup(&h).await;
	let store = h.store();

	store.archive_response(-1, "<lost/>").await.unwrap();
	assert!(store.batch_archive(-1).await.unwrap().is_none());

	let (batch, rows) = claimed(store, 1).await;
	post(store, &batch, &rows).await;
	store.archive_response(batch.id, "<first/>").await.unwrap();
	store.archive_response(batch.id, "<second/>").await.unwrap();
	let archive = store.batch_archive(batch.id).await.unwrap().unwrap();
	assert_eq!(archive.request_xml.as_deref(), Some("<request/>"));
	assert_eq!(archive.response_xml.as_deref(), Some("<second/>"));
}

/// Row ids `list_filings` answers, in its order.
async fn listed<S: NavStore>(
	store: &S,
	states: &[FilingState],
	after_id: i64,
	limit: i64,
) -> Vec<i64> {
	let filings = store.list_filings(states, after_id, limit).await.unwrap();
	filings.iter().map(|f| f.row.id).collect()
}

/// An `ACTIVE` row reads its batch's state, any other its own, and a `CANCELLED` row may have no
/// batch at all. Resolved rows are not live.
pub async fn list_filings_filters_on_the_filing_state<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-list");
	invoice::setup(&h).await;
	let store = h.store();
	assert!(listed(store, &[], 0, 10).await.is_empty());

	let sent = drive(store, Start::Sent).await.row.unwrap();
	let batch_held = drive(store, Start::Held).await.row.unwrap();
	let row_held = drive(store, Start::RowHeld).await.row.unwrap();
	let rejected = drive(store, Start::Rejected).await.row.unwrap();
	let stopped = drive(store, Start::Stopped).await.row.unwrap();
	drive(store, Start::Resolved).await;

	assert_eq!(listed(store, &[FilingState::Sent], 0, 10).await, [sent]);
	assert_eq!(listed(store, &[FilingState::Held], 0, 10).await, [batch_held, row_held]);
	assert_eq!(listed(store, &[FilingState::Rejected], 0, 10).await, [rejected]);
	assert_eq!(listed(store, &[FilingState::Cancelled], 0, 10).await, [stopped]);
	assert!(listed(store, &[FilingState::Accepted], 0, 10).await.is_empty());
	let either = [FilingState::Rejected, FilingState::Sent];
	assert_eq!(listed(store, &either, 0, 10).await, [sent, rejected]);
	let all = [sent, batch_held, row_held, rejected, stopped];
	assert_eq!(listed(store, &[], 0, 10).await, all, "empty means every state");
}

pub async fn list_filings_pages_by_keyset<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-list-page");
	invoice::setup(&h).await;
	let store = h.store();
	let mut rows = Vec::new();
	for _ in 0..3 {
		rows.push(drive(store, Start::Sent).await.row.unwrap());
	}
	drive(store, Start::Rejected).await;

	let sent = [FilingState::Sent];
	assert_eq!(listed(store, &sent, 0, 2).await, rows[..2]);
	assert_eq!(listed(store, &sent, rows[1], 2).await, rows[2..]);
	assert!(listed(store, &sent, rows[2], 2).await.is_empty());
}

/// The oldest `fault_at` of this seller's open batches: not the first batch, not a closed one.
pub async fn oldest_open_fault_reads_this_sellers_open_batches<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-oldest-fault");
	invoice::setup(&h).await;
	let store = h.store();
	let ver = invoice::second_seller(&h).await;
	let oldest = async || store.oldest_open_fault(SELLER).await.unwrap();
	assert_eq!(oldest().await, None);

	let (clean, _) = claimed(store, 1).await;
	assert_eq!(oldest().await, None, "a batch with no fault");

	let (young, _) = claimed(store, 1).await;
	assert!(store.record_batch_fault(young.id, "YOUNG", "m", AT).await.unwrap());
	let (old, rows) = claimed(store, 1).await;
	assert!(store.record_batch_fault(old.id, "OLD", "m", T0).await.unwrap());
	assert_eq!(oldest().await.as_deref(), Some("OLD"));

	let other = invoice::issued_second(store, ver).await.id;
	let (theirs, _) = claim(store, 2, NavOp::Create, &[other]).await;
	let older = Timestamp(T0.0 - 1);
	assert!(store.record_batch_fault(theirs.id, "THEIRS", "m", older).await.unwrap());
	assert_eq!(oldest().await.as_deref(), Some("OLD"), "another seller's is not read");
	let theirs = store.oldest_open_fault(2).await.unwrap();
	assert_eq!(theirs.as_deref(), Some("THEIRS"));

	assert!(store.cancel_filing(rows[0].invoice_id, NavOp::Create, AT).await.unwrap());
	assert_eq!(batch_of(store, old.uid.as_str()).await.state, B::Closed);
	assert_eq!(oldest().await.as_deref(), Some("YOUNG"), "a closed batch is done");
	assert_eq!(batch_of(store, clean.uid.as_str()).await.fault_at, None);
}

/// Only `CLAIMED`: every later batch has a job the hold did not park.
pub async fn open_batch_keys_lists_this_sellers_claimed_batches<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-open-keys");
	invoice::setup(&h).await;
	let store = h.store();
	let ver = invoice::second_seller(&h).await;
	assert!(store.open_batch_keys(SELLER).await.unwrap().is_empty());

	let (first, _) = claimed(store, 1).await;
	let (posted, rows) = claimed(store, 1).await;
	post(store, &posted, &rows).await;
	let (second, _) = claimed(store, 1).await;
	let other = invoice::issued_second(store, ver).await.id;
	let (theirs, _) = claim(store, 2, NavOp::Create, &[other]).await;

	let key = |b: &NavBatch| format!("nav:send:{}", b.uid.as_str());
	let keys = store.open_batch_keys(SELLER).await.unwrap();
	assert_eq!(keys, [key(&first), key(&second)]);
	assert_eq!(store.open_batch_keys(2).await.unwrap(), [key(&theirs)]);
}

/// Two workers claiming one invoice must not both file it, and the loser's batch must not stay.
pub async fn concurrent_claims_of_one_invoice_leave_one_live_row<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	const N: usize = 8;

	let h = fresh!(H, "nav-fsm-claim-race");
	invoice::setup(&h).await;
	let store = h.store();
	let id = invoice::issued(store).await.id;
	let other = h.reopen().await;

	let mut tasks = Vec::new();
	for i in 0..N {
		let s = if i % 2 == 0 { store.clone() } else { other.clone() };
		tasks.push(tokio::spawn(async move {
			s.claim_batch(SELLER, NavOp::Create, &[id], false).await.unwrap()
		}));
	}
	let mut won = 0;
	for t in tasks {
		won += usize::from(t.await.unwrap().is_some());
	}

	assert_eq!(won, 1);
	assert_eq!(totals(&h).await, (1, 1));
}

pub async fn batch_by_uid_finds_a_batch_and_answers_none_for_a_stranger<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "nav-fsm-by-uid");
	invoice::setup(&h).await;
	let store = h.store();
	assert!(store.batch_by_uid("nbt_missing").await.unwrap().is_none());

	let (batch, _) = claimed(store, 1).await;

	let found = store.batch_by_uid(batch.uid.as_str()).await.unwrap().unwrap();
	assert_eq!((found.id, found.state), (batch.id, B::Claimed));
	assert!(store.batch_by_uid("nbt_missing").await.unwrap().is_none());
}

// vim: ts=4
