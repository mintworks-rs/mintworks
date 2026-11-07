// SPDX-License-Identifier: MPL-2.0
//! The background jobs of a filing. Nothing here is reachable from the invoice issue path: issue
//! enqueues `NAV_REPORT` and returns, so a NAV outage can never block or fail an invoice.
//!
//! A batch is one NAV request, and its uid is the `requestId`. Each job owns a span of its life
//! (`claude-docs/nav-filing-design.md` §3):
//!
//! * `NAV_REPORT` gets an invoice into a batch. Every invoice's job is the same: the first to run
//!   assembles, and the others find their row and finish.
//! * `NAV_SEND` owns `CLAIMED` and `POSTING`. `POSTING` and the archive commit before the POST, so
//!   a crash at any later point leaves a state saying NAV may hold the batch.
//! * `NAV_RECONCILE` owns `IN_DOUBT`, `NAV_POLL` owns `SENT`, `NAV_ADOPT` checks a row NAV aborted
//!   on a number it already holds, and `NAV_SWEEP` repairs whichever of them went missing.
//!
//! **Every decision reads the batch and its rows, never a job's status**; only the sweep asks
//! whether an owner job is live. A transition is one guarded [`NavStore`] write, and
//! [`job::ensure`], through the `filing::ensure_*` wrappers, is the only thing that re-queues work.
//!
//! Every failure propagates as `Err`, and `Runner::tick` reads `Error::retry()` to back off or to
//! stop; a stopped job under an open batch is the sweep's to revive. Nothing here gives up: every
//! `jobs.max_attempts.NAV_*` is `0`, `jobs.alert_after.*` is what tells a human, and
//! `Nav::cancel_filing` is how one stops a filing that can never succeed.
//!
//! An unbounded retry of a statutory filing is safe because a resend cannot become a second
//! filing, on two layers:
//!
//! * Inside ±1 day, the `requestId`: a batch keeps its uid across every attempt, and NAV refuses
//!   an id it has already processed (`REQUEST_ID_NOT_UNIQUE`).
//! * Beyond it, the invoice number: NAV aborts the invoice with `INVOICE_NUMBER_NOT_UNIQUE`, and
//!   `NAV_ADOPT` or a person takes it from there.
//!
//! NAV's verdict on the invoice itself is not available on `manageInvoice` at all: it arrives
//! on the poll path as `invoiceStatus = ABORTED`.

use mintworks_core::{
	App,
	job::{self, Job, Next, Runner},
	prelude::*,
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use mintworks_invoice::{InvoiceKind, InvoiceStore, KIND_NAV_REPORT, Seller, SellerVersion};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::auth;
use crate::auth::{Answer, NavAuth};
use crate::client::{
	self, Accepted, Disposition, InvoiceDataResult, Outcome, accepted, invoice_data_result,
	outcomes,
};
use crate::filing;
use crate::reply::Reply;
use crate::service_api::{Nav, group_by};
use crate::store::NavStore;
use crate::submission::{BatchState, NavBatch, NavOp, NavSubmission, NavVerdict, RowState};
use crate::xml::invoice_data;

pub const KIND_NAV_SEND: &str = "NAV_SEND";
pub const KIND_NAV_POLL: &str = "NAV_POLL";
pub const KIND_NAV_SWEEP: &str = "NAV_SWEEP";
pub const KIND_NAV_RECONCILE: &str = "NAV_RECONCILE";
pub const KIND_NAV_ADOPT: &str = "NAV_ADOPT";

/// How much base64 `invoiceData` one `manageInvoice` carries: NAV refuses a request over 10 MB
/// (§1.6.5), and this leaves room for the envelope. Not tunable, since NAV's limit is not.
pub const BATCH_BYTE_BUDGET: usize = 8 * 1024 * 1024;

/// How often `NAV_SWEEP` runs. An hour: a missed statutory filing is measured in days.
const SWEEP_EVERY_SECS: i64 = 3600;

/// How many rows one sweep read takes: a `supervised` page, and a tick's orphaned invoices and
/// adoptable batches. A literal rather than a setting: nobody tunes a crash window's backlog.
const SWEEP_BATCH: i64 = 100;

/// How long after `manageInvoice` the first `queryTransactionStatus` runs. Every poll after it
/// is the job runner's own backoff up to `jobs.backoff_cap.NAV_POLL`.
pub(crate) const POLL_FIRST_DELAY_SECS: i64 = 5;

/// How long after a lost `manageInvoice` reply `NAV_RECONCILE` asks NAV what landed. The
/// specification's §1.9.2 five minutes: until then the submission is still alive server-side and
/// the commit-or-rollback is not decided, so an earlier question can only get an answer that
/// changes.
const RECONCILE_DELAY_SECS: i64 = 300;

/// How far *before* the batch's own `created_at` the `queryTransactionList` window opens. It
/// absorbs the clock skew between this process and NAV's `insDate`, which is the only reason the
/// window is not exactly `[created_at, now]`; §1.9.2's own example is "the last 10 minutes".
const RECONCILE_WINDOW_SECS: i64 = 600;

/// The widest `queryTransactionList`: NAV refuses over 35 days (`BAD_QUERY_PARAM_RANGE_EXCEEDED`).
/// A longer window is walked, not clamped, so an old filing is still fully searched.
const RECONCILE_SLICE_SECS: i64 = 30 * 86_400;

/// `[from, to]` for the batch's `queryTransactionList`, in unix seconds. Skew is absorbed on
/// **both** sides: NAV stamping `insDate` ahead of this host dropped the transaction out of a
/// window that ended at `now`, and an empty `unknown` is read as "NAV never took the batch". A
/// `dateTimeTo` in the future is harmless — NAV has nothing stamped there to return.
const fn reconcile_window(opened: i64, now: i64) -> (i64, i64) {
	(opened - RECONCILE_WINDOW_SECS, now + RECONCILE_WINDOW_SECS)
}

/// `[from, to]` in slices of at most [`RECONCILE_SLICE_SECS`], newest first, touching so no
/// transaction falls between two.
fn reconcile_slices(from: i64, to: i64) -> Vec<(i64, i64)> {
	let mut slices = Vec::new();
	let mut end = to;
	while end > from {
		let start = (end - RECONCILE_SLICE_SECS).max(from);
		slices.push((start, end));
		end = start;
	}
	slices
}

/// Slept before every query of `NAV_ADOPT` and `NAV_RECONCILE`, a run's first included, because a
/// restarted handler has no memory of its last request: NAV allows one request per second per IP
/// (§1.6.11), and the tenth is margin for this host's timer.
const QUERY_PACE: Duration = Duration::from_millis(1100);

/// How long after NAV's abort "no result" may only mean the first filing is still processing.
const ADOPT_WAIT_SECS: i64 = 3600;

/// How long `NAV_ADOPT` waits before asking again inside [`ADOPT_WAIT_SECS`].
const ADOPT_RECHECK_SECS: i64 = 600;

/// The `NAV_REPORT` payload the store enqueues in `InvoiceStore::issue`'s transaction.
#[derive(Deserialize)]
struct ReportPayload {
	#[serde(rename = "invoiceId")]
	invoice_id: i64,
}

/// What [`batch_payload`] spells.
#[derive(Deserialize)]
struct BatchPayload {
	#[serde(rename = "batchUid")]
	batch_uid: String,
}

/// The handlers deserialize their payload and make exactly one call into [`Nav`], which is the
/// same rule a route handler follows. `Nav::new` resolves both stores from `app.extensions`, so
/// nothing is threaded in here.
pub fn register(runner: &mut Runner, app: App) {
	let handle = Nav::new(app);

	let h = handle.clone();
	// `register_next`: a seller that is held or not connected yet defers without an error.
	runner.register_next(KIND_NAV_REPORT, move |job: Job| {
		let h = h.clone();
		async move {
			let p: ReportPayload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("mintworks-nav: bad {KIND_NAV_REPORT} payload: {e}"))
			})?;
			h.run_report(p.invoice_id).await
		}
	});

	let h = handle.clone();
	// `register_next`, for the same deferral as `NAV_REPORT`.
	runner.register_next(KIND_NAV_SEND, move |job: Job| {
		let h = h.clone();
		async move {
			let p: BatchPayload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("mintworks-nav: bad {KIND_NAV_SEND} payload: {e}"))
			})?;
			h.run_send(&p.batch_uid).await
		}
	});

	let h = handle.clone();
	// `register_next`: a poll NAV has not answered yet is a success with a schedule, not a failure
	// — as an `Err` it raised `A-JOB-STALE`; `NAV_SWEEP` records `E-NAV-POLL-STALE` on the batch.
	runner.register_next(KIND_NAV_POLL, move |job: Job| {
		let h = h.clone();
		async move {
			let p: BatchPayload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("mintworks-nav: bad {KIND_NAV_POLL} payload: {e}"))
			})?;
			h.run_poll(&job, &p.batch_uid).await
		}
	});

	let h = handle.clone();
	runner.register(KIND_NAV_RECONCILE, move |job: Job| {
		let h = h.clone();
		async move {
			let p: BatchPayload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("mintworks-nav: bad {KIND_NAV_RECONCILE} payload: {e}"))
			})?;
			h.run_reconcile(&p.batch_uid).await
		}
	});

	let h = handle.clone();
	// `register_next`: NAV not knowing a number yet is a wait, not a failure.
	runner.register_next(KIND_NAV_ADOPT, move |job: Job| {
		let h = h.clone();
		async move {
			let p: BatchPayload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("mintworks-nav: bad {KIND_NAV_ADOPT} payload: {e}"))
			})?;
			h.run_adopt(&p.batch_uid).await
		}
	});

	runner.register_periodic(KIND_NAV_SWEEP, SWEEP_EVERY_SECS, move |_job| {
		let h = handle.clone();
		async move { h.run_sweep().await }
	});
}

/// Seeds the periodic sweep, and refuses to start on a `software` block NAV would reject.
/// Call once at boot from `AppBuilder::on_init`, next to `mintworks_invoice::draft::seed`.
///
/// The settings check belongs here rather than at filing time: a missing `nav.software_*`
/// key makes every `manageInvoice` fail on a schema error, and a faulted request is
/// retryable, so the failure is otherwise invisible and permanent.
pub async fn seed(app: &App) -> ClResult<()> {
	auth::check_seller(app).await?;
	job::seed_periodic(&app.store, KIND_NAV_SWEEP).await
}

/// The job that owns `batch` in its state, as a dedup key.
fn owner_key(batch: &NavBatch) -> String {
	let uid = batch.uid.as_str();
	match batch.state {
		BatchState::InDoubt => reconcile_key(uid),
		BatchState::Sent => poll_key(uid),
		_ => send_key(uid),
	}
}

/// Step 2 of [`sweep`]: every open batch has a live owner job, or gets one.
async fn supervise(app: &App, nav: &dyn NavStore) -> ClResult<()> {
	let now = Timestamp::now().0;
	let lease = app.settings.int("jobs.timeout_secs.NAV_SEND").await? + 60;
	let stale_after = app.settings.int("jobs.alert_after.NAV_POLL").await?;
	let mut after_id = 0;
	loop {
		let page = nav.supervised(after_id, SWEEP_BATCH).await?;
		let Some(last) = page.last() else {
			return Ok(());
		};
		after_id = last.id;
		let keys: Vec<String> = page.iter().map(owner_key).collect();
		let live: HashSet<String> = app
			.store
			.job_statuses_by_keys(&keys)
			.await?
			.into_iter()
			.filter(|(_, status)| matches!(status.as_str(), "PENDING" | "RUNNING"))
			.map(|(key, _)| key)
			.collect();

		for (batch, key) in page.iter().zip(&keys) {
			let uid = batch.uid.as_str();
			let posted = batch.posted_at.unwrap_or(batch.created_at);
			// Ahead of the owner check: a poll that keeps faulting, or a reply that omits a row,
			// is a live job. `A-JOB-STALE` cannot see a poll deferring on `Next::Again`.
			if batch.state == BatchState::Sent && stale_after > 0 && now - posted.0 > stale_after {
				let why = "NAV has not settled this batch within jobs.alert_after.NAV_POLL; the \
				           poll continues";
				mark(nav, batch, E_NAV_POLL_STALE, why).await;
			}
			if live.contains(key) {
				continue;
			}
			let ensured = match batch.state {
				BatchState::Claimed => filing::ensure_send(app, uid, false).await,
				BatchState::InDoubt => {
					filing::ensure_reconcile(app, uid, reconcile_at(posted), false).await
				}
				BatchState::Sent => filing::ensure_poll(app, uid, false).await,
				// Younger than the lease, a worker may still hold the POST.
				BatchState::Posting => {
					if now - posted.0 > lease {
						doubt(app, nav, batch, None, posted).await;
					}
					continue;
				}
				BatchState::Held | BatchState::Closed => continue,
			};
			match ensured {
				Ok(_) => tracing::warn!(batch = uid, "this open batch had no live job; queued one"),
				Err(e) => {
					tracing::error!(batch = uid, error = %e, "could not queue this batch's job");
				}
			}
		}
	}
}

/// The level-triggered supervisor, over every seller (`claude-docs/nav-filing-design.md` §5): log
/// what needs a person, then give whatever durable state owes a job that job. A batch's own gate
/// makes a job queued here harmless, and `wake = false` leaves every backoff and deferral alone.
///
/// **Nothing in here propagates.** The signature stays `ClResult<()>` because that is the
/// runner's handler type, but every failure logs and gives up on this tick: the sweep is
/// periodic, and nothing re-seeds a periodic kind except `seed_periodic`, which runs at boot.
pub async fn sweep(app: &App, nav: &dyn NavStore) -> ClResult<()> {
	// The one thing no retry can fix, so loud every tick rather than one `error!` when NAV said so;
	// `service_api::alerts` raises `A-NAV-REJECTED` off the same count.
	let awaiting = async {
		let grace = app.settings.int("nav.fault_alert_after").await?;
		nav.awaiting_operator(None, Timestamp(Timestamp::now().0 - grace)).await
	};
	match awaiting.await {
		Err(e) => tracing::error!(error = %e, "could not count the filings that need a person"),
		Ok(0) => {}
		Ok(waiting) => tracing::error!(
			invoices = waiting,
			"issued invoices ended without a NAV acceptance and need a person: either NAV \
			 rejected them, or their status with NAV is unknown — check each before acting"
		),
	}

	if let Err(e) = supervise(app, nav).await {
		tracing::error!(error = %e, "could not supervise every open batch this tick");
	}

	match nav.orphaned_invoices(SWEEP_BATCH).await {
		Err(e) => tracing::error!(error = %e, "could not read orphaned invoices this tick"),
		Ok(ids) => {
			for invoice in ids {
				match filing::ensure_report(app, invoice, false).await {
					Ok(_) => {
						tracing::warn!(invoice, "an issued invoice had no NAV filing; queued it");
					}
					Err(e) => tracing::error!(invoice, error = %e, "could not queue a NAV filing"),
				}
			}
		}
	}

	match nav.adoptable(SWEEP_BATCH).await {
		Err(e) => tracing::error!(error = %e, "could not read adoptable batches this tick"),
		Ok(uids) => {
			for uid in uids {
				let uid = uid.as_str();
				if let Err(e) = filing::ensure_adopt(app, uid, false).await {
					tracing::error!(batch = uid, error = %e, "could not queue NAV_ADOPT");
				}
			}
		}
	}
	Ok(())
}

/// How long a tenant seller's filing waits before looking for credentials again.
/// Connecting pulls the backlog forward (`CoreStore::job_wake`), so this is only the fallback.
pub const NOT_CONNECTED_RECHECK_SECS: i64 = 6 * 3600;

/// How long a held seller's `NAV_REPORT` and `NAV_SEND` wait before looking again. Releasing the
/// hold wakes them, so this is only the fallback: a probe would be a POST that burns a `requestId`.
pub const HELD_RECHECK_SECS: i64 = 3600;

/// `Some(at)` when the seller files nothing for now, because a hold stands (design §7) or it is a
/// tenant that has not connected NAV: the job waits, recording no error.
pub async fn deferral(
	app: &App,
	nav: &dyn NavStore,
	seller: &Seller,
) -> ClResult<Option<Timestamp>> {
	let wait = if nav.seller_hold(seller.id).await?.is_some() {
		HELD_RECHECK_SECS
	} else if auth::connected(app, seller).await? {
		return Ok(None);
	} else {
		NOT_CONNECTED_RECHECK_SECS
	};
	Ok(Some(Timestamp(Timestamp::now().0 + wait)))
}

/// What `manage_invoice_request` takes for one invoice: its `invoiceData` and its PDF hash.
type Built = (String, Option<String>);

/// What `doc` weighs on the wire, where `invoiceData` is padded base64.
const fn wire_bytes(doc: &str) -> usize {
	doc.len().div_ceil(3) * 4
}

/// Builds each `(invoice_id, electronic)` of `wanted`. The outer `Err` is a failed read, the inner
/// one that this invoice does not build; every invoice of `wanted` has an entry.
///
/// Five bulk reads for the whole batch, not a query set per invoice. Each invoice files under the
/// seller version *it* froze at ISSUE: a batch spans invoices issued either side of a seller edit.
async fn build_documents(
	invoices: &dyn InvoiceStore,
	wanted: &[(i64, bool)],
) -> ClResult<HashMap<i64, ClResult<Built>>> {
	let ids: Vec<i64> = wanted.iter().map(|(id, _)| *id).collect();
	let (rows, lines, groups, documents) = tokio::try_join!(
		invoices.invoices_by_ids(&ids),
		invoices.invoice_lines_for(&ids),
		invoices.invoice_vat_groups_for(&ids),
		invoices.invoice_documents_for(&ids),
	)?;
	let doc_of: HashMap<i64, _> = documents.into_iter().map(|d| (d.invoice_id, d)).collect();
	let versions: Vec<i64> = rows.iter().filter_map(|i| i.seller_ver).collect();
	let version_of: HashMap<i64, _> = invoices
		.seller_versions(&versions)
		.await?
		.into_iter()
		.map(|v| (v.seller_ver, v))
		.collect();
	let by_id: HashMap<i64, _> = rows.into_iter().map(|i| (i.id, i)).collect();
	let lines_of = group_by(lines, |l| l.invoice_id);
	let groups_of = group_by(groups, |g| g.invoice_id);

	let mut built = HashMap::with_capacity(wanted.len());
	for &(id, electronic) in wanted {
		let pair: ClResult<Built> = async {
			let invoice = by_id
				.get(&id)
				.ok_or_else(|| Error::internal(format!("mintworks-nav: invoice {id} is gone")))?;
			// NAV archives this hash as the only independent proof that the buyer's PDF is the one
			// issued, and `manageInvoice` files an invoice once: a filing that raced `RENDER_PDF`
			// would be unprovable forever.
			let hash = if electronic {
				Some(doc_of.get(&id).map(|d| d.sha256.clone()).ok_or_else(|| {
					Error::Unavailable(
						"mintworks-nav: the invoice PDF is not rendered yet; retrying".into(),
					)
				})?)
			} else {
				None
			};
			let version = invoice.seller_ver.and_then(|v| version_of.get(&v)).ok_or_else(|| {
				Error::internal("mintworks-nav: an issued invoice has no seller_ver")
			})?;
			let original = match invoice.kind {
				InvoiceKind::Normal => None,
				InvoiceKind::Storno => {
					let original = invoice
						.original_invoice_id
						.ok_or_else(|| Error::internal("mintworks-nav: storno with no original"))?;
					let number =
						invoices.invoice_by_id(original).await?.and_then(|i| i.number).ok_or_else(
							|| Error::internal("mintworks-nav: the stornoed invoice has no number"),
						)?;
					Some(number)
				}
			};
			let xml = invoice_data(
				version,
				invoice,
				lines_of.get(&id).map_or(&[][..], Vec::as_slice),
				groups_of.get(&id).map_or(&[][..], Vec::as_slice),
				original.as_deref(),
				electronic,
			)?;
			Ok((xml, hash))
		}
		.await;
		built.insert(id, pair);
	}
	Ok(built)
}

/// Get the invoice into a batch and hand the batch to `NAV_SEND`. Every invoice's job is the same:
/// whichever runs first assembles, the rest find their row. A row with no live owner job is
/// `NAV_SWEEP`'s to repair, so nothing here looks at one.
///
/// The `NAV_REPORT` handler, public so it can be driven directly by a test: `Runner::tick` is
/// private and `run` never returns.
pub async fn report(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	invoice_id: i64,
) -> ClResult<Next> {
	let invoice = invoices
		.invoice_by_id(invoice_id)
		.await?
		.ok_or_else(|| Error::internal(format!("mintworks-nav: invoice {invoice_id} is gone")))?;
	// A redriven or hand-enqueued `NAV_REPORT` must not file a test-mode invoice either.
	if invoice.test_mode {
		return Ok(Next::Done);
	}
	let seller = invoices
		.seller_by_id(invoice.seller_id)
		.await?
		.ok_or_else(|| Error::internal("mintworks-nav: the invoice's seller is gone"))?;
	if let Some(at) = deferral(app, nav, &seller).await? {
		return Ok(Next::Again { at });
	}
	// A resolved row is history: R8 and `release_unbuildable` delete the live row above it, and
	// their `NAV_REPORT` must file. Only a live row means another path owns this invoice.
	if nav
		.filing_by_invoice(invoice.id)
		.await?
		.is_some_and(|f| f.row.resolved_at.is_none())
	{
		return Ok(Next::Done);
	}

	let electronic = app.settings.flag("nav.electronic_invoice").await?;
	let (op, candidates) = match invoice.kind {
		// Alone: NAV promises no processing order *within* a batch. And a STORNO must not reach NAV
		// before the invoice it cancels, or NAV answers `ABORTED`. `REJECTED` never becomes `DONE`,
		// so a storno of an invoice NAV refused waits here for a person.
		InvoiceKind::Storno => {
			let original = invoice
				.original_invoice_id
				.ok_or_else(|| Error::internal("mintworks-nav: storno with no original"))?;
			if !nav.filing_by_invoice(original).await?.is_some_and(|f| f.row.accepted()) {
				return Err(Error::Unavailable(
					"the stornoed invoice is not accepted by NAV yet; retrying — if it was \
					 rejected, it needs a person"
						.into(),
				));
			}
			(NavOp::Storno, Vec::new())
		}
		InvoiceKind::Normal => {
			let room = app.settings.int("nav.batch_max").await? - 1;
			let candidates = if room > 0 {
				nav.batch_candidates(invoice.seller_id, invoice.id, electronic, room).await?
			} else {
				Vec::new()
			};
			(NavOp::Create, candidates)
		}
	};

	let wanted: Vec<(i64, bool)> = std::iter::once(invoice.id)
		.chain(candidates.iter().copied())
		.map(|id| (id, electronic))
		.collect();
	let mut built = build_documents(invoices, &wanted).await?;
	let (doc, _) = built
		.remove(&invoice.id)
		.unwrap_or_else(|| Err(Error::internal("mintworks-nav: this invoice was not built")))?;

	// This invoice joins whatever it weighs: `send` is what faults one that is over on its own.
	let mut bytes = wire_bytes(&doc);
	let mut claim = vec![invoice.id];
	for id in candidates {
		match built.get(&id) {
			Some(Ok((doc, _))) => {
				bytes += wire_bytes(doc);
				if bytes > BATCH_BYTE_BUDGET {
					break;
				}
				claim.push(id);
			}
			// Left out, not lost: its own `NAV_REPORT` surfaces why.
			Some(Err(e)) => tracing::warn!(
				invoice = id,
				error = %e,
				"could not build this invoice's invoiceData; leaving it out of the batch"
			),
			None => {}
		}
	}

	let Some((batch, _)) = nav.claim_batch(invoice.seller_id, op, &claim, electronic).await? else {
		tracing::info!(invoice = invoice.id, "another job claimed this filing; standing down");
		return Ok(Next::Done);
	};
	filing::ensure_send(app, batch.uid.as_str(), true).await?;
	Ok(Next::Done)
}

/// When `NAV_RECONCILE` may ask about a POST made at `from`. See [`RECONCILE_DELAY_SECS`].
pub(crate) const fn reconcile_at(from: Timestamp) -> Timestamp {
	Timestamp(from.0 + RECONCILE_DELAY_SECS)
}

/// Move the batch toward NAV, whatever state the last run left it in.
///
/// The `NAV_SEND` handler, public for the same reason [`report`] is. The state gate comes first:
/// a batch past `CLAIMED` owes its recovery even while its seller is held.
pub async fn send(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	batch_uid: &str,
) -> ClResult<Next> {
	let batch = nav
		.batch_by_uid(batch_uid)
		.await?
		.ok_or_else(|| Error::internal(format!("mintworks-nav: batch {batch_uid} is gone")))?;
	let posted = batch.posted_at.unwrap_or(batch.created_at);
	match batch.state {
		BatchState::Claimed => return post_batch(app, invoices, nav, &batch).await,
		// An earlier run died past the write-ahead, so NAV may hold the batch.
		BatchState::Posting => doubt(app, nav, &batch, None, posted).await,
		BatchState::InDoubt => {
			filing::ensure_reconcile(app, batch_uid, reconcile_at(posted), false).await?;
		}
		BatchState::Sent => {
			filing::ensure_poll(app, batch_uid, false).await?;
		}
		BatchState::Held | BatchState::Closed => {}
	}
	Ok(Next::Done)
}

/// [`send`] for a `CLAIMED` batch: build, write ahead, POST, and act on what NAV answered.
async fn post_batch(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	batch: &NavBatch,
) -> ClResult<Next> {
	let uid = batch.uid.as_str();
	// Two rows, two lifetimes: the filing's content comes from the version each invoice froze at
	// ISSUE, while `NavAuth::load` below authenticates as today's seller.
	let (seller, current) = seller_now(invoices, batch.seller_id).await?;
	if let Some(at) = deferral(app, nav, &seller).await? {
		return Ok(Next::Again { at });
	}

	let active = active_rows(nav, batch.id).await?;
	// What the row recorded, not the setting: the flag may have flipped since the claim.
	let wanted = active
		.iter()
		.map(|r| {
			let electronic = r.electronic.ok_or_else(|| {
				Error::internal("mintworks-nav: a batched row has no invoiceAppearance")
			})?;
			Ok((r.invoice_id, electronic))
		})
		.collect::<ClResult<Vec<(i64, bool)>>>()?;
	let mut built = build_documents(invoices, &wanted).await?;

	// `rows` and `payload` stay index-aligned: position `i` is the `<index>` `i + 1` on the wire,
	// which is the `idx` the poll matches NAV's results against. A released row takes no index.
	let mut rows: Vec<(i64, i64)> = Vec::with_capacity(active.len());
	let mut payload: Vec<Built> = Vec::with_capacity(active.len());
	for row in &active {
		match built.remove(&row.invoice_id) {
			Some(Ok(pair)) => {
				payload.push(pair);
				rows.push((row.id, i64::try_from(payload.len()).unwrap_or(i64::MAX)));
			}
			Some(Err(e)) => release_unbuildable(app, nav, row, &e).await,
			None => {
				let e = Error::internal("mintworks-nav: this row's invoice was not built");
				release_unbuildable(app, nav, row, &e).await;
			}
		}
	}
	// `release_row` closed the batch with its last row.
	if payload.is_empty() {
		return Ok(Next::Done);
	}
	// Assembly keeps a batch under the budget, so this is one invoice that needs the gzip this
	// crate never sends. `CLAIMED`, faulted and alarmed is the known ceiling (design §3).
	let bytes: usize = payload.iter().map(|(doc, _)| wire_bytes(doc)).sum();
	if bytes > BATCH_BYTE_BUDGET {
		let why = format!("{bytes} bytes of invoiceData exceed {BATCH_BYTE_BUDGET}");
		filing::park(nav, batch.id, E_NAV_OVERSIZE, &why).await;
		return Err(Error::Unavailable(format!("batch {uid}: {why}")));
	}

	// As late as it can be: NAV sets the token's validity window and nothing caches it, so the
	// builds above would run it down. On the batch, since the row exists through an outage and
	// `Nav::credentials_status` reads `E-NAV-CREDENTIALS` from here.
	let exchanged = async {
		let client = NavAuth::load(app, &seller, &current).await?;
		let token = client.token_exchange().await?;
		Ok::<_, Error>((client, token))
	}
	.await;
	let (client, token) = match exchanged {
		Ok(pair) => pair,
		Err(e) => {
			filing::park(nav, batch.id, e.parts().1, &e.to_string()).await;
			return Err(e);
		}
	};
	let request = client.manage_invoice_request(batch.op, uid, &payload, &token)?;

	// Through `redact`: the envelope authenticates with an unsalted SHA-512 password hash, and an
	// archive that kept it would be a replayable NAV credential.
	if !nav
		.begin_post(batch.id, &auth::redact(&request), &rows, Timestamp::now())
		.await?
	{
		return Err(Error::Unavailable(format!(
			"a row of batch {uid} moved before the POST; assembling the request again"
		)));
	}

	let (status, reply) = match client.post("manageInvoice", &request).await {
		Ok(Answer::Reply { status, xml }) => (status, xml),
		// Demonstrably never reached NAV, so the next attempt is a plain resend.
		Ok(Answer::Unavailable) => {
			abort(nav, batch, None).await;
			return Err(auth::unavailable());
		}
		Ok(Answer::Throttled { retry_after }) => {
			abort(nav, batch, None).await;
			return Err(Error::RateLimit(retry_after));
		}
		// §1.9.2: NAV may hold the batch, and only reconciliation may say otherwise.
		Ok(Answer::Indeterminate) => {
			doubt(app, nav, batch, None, Timestamp::now()).await;
			return Err(auth::indeterminate());
		}
		// Nothing proves this stopped short of NAV, so it counts as a lost reply.
		Err(e) => {
			doubt(app, nav, batch, None, Timestamp::now()).await;
			return Err(e);
		}
	};
	let redacted = auth::redact(&reply);
	let parsed = accepted(status, &Reply::parse(&reply));

	match parsed {
		Ok(Accepted::Ok { transaction_id }) => {
			// **Before the archive**, which is only a diagnostic: this is the one part of the reply
			// that cannot be reconstructed.
			let sent =
				job::thrice(|| nav.set_sent(batch.id, &transaction_id)).await.map_err(|last| {
					last.unwrap_or_else(|| Error::internal("mintworks-nav: set_sent did not run"))
				});
			filing::archive_reply(nav, batch.id, &redacted).await;
			match sent {
				Ok(true) => {
					filing::ensure_poll(app, uid, false).await?;
					Ok(Next::Done)
				}
				// Either another run moved the batch, and the re-run's gate finds out where, or
				// another batch carries this id, and it goes to reconciliation like a lost reply.
				Ok(false) => {
					doubt(app, nav, batch, None, Timestamp::now()).await;
					Err(Error::Unavailable(format!(
						"could not record transaction {transaction_id} on batch {uid}"
					)))
				}
				Err(e) => {
					doubt(app, nav, batch, None, Timestamp::now()).await;
					Err(e)
				}
			}
		}
		Ok(Accepted::Fault { code, message }) => {
			// Archived first: there is no `transactionId` to race the write against.
			filing::archive_reply(nav, batch.id, &redacted).await;
			tracing::error!(batch = uid, %code, %message, "manageInvoice faulted");
			let fault = Some((code.as_str(), message.as_str()));
			match client::disposition(&code) {
				Disposition::Retry => abort(nav, batch, fault).await,
				Disposition::PossiblyFiled => {
					doubt(app, nav, batch, fault, Timestamp::now()).await;
				}
				Disposition::SpendsRequestId => spend(app, nav, batch, &code, &message).await,
			}
			Err(client::business(&code, &message))
		}
		// Unreadable, or `OK` with no `transactionId`: §1.9.2 lost replies both, with the archive
		// as the only trace of what NAV said.
		Err(e) => {
			filing::archive_reply(nav, batch.id, &redacted).await;
			doubt(app, nav, batch, None, Timestamp::now()).await;
			Err(e)
		}
	}
}

/// B3. Best-effort: the caller holds the error that must survive, and a batch left `POSTING` only
/// costs a reconciliation, which `send`'s re-run starts.
async fn abort(nav: &dyn NavStore, batch: &NavBatch, fault: Option<(&str, &str)>) {
	match nav.abort_post(batch.id, fault, Timestamp::now()).await {
		Ok(true) => {}
		Ok(false) => tracing::warn!(batch = batch.id, "this batch left POSTING during the POST"),
		Err(e) => tracing::error!(batch = batch.id, error = %e, "could not end this POST"),
	}
}

/// B4, then `NAV_RECONCILE` for a POST made at `posted`. Best-effort like [`abort`], with
/// `NAV_SWEEP` behind a lost enqueue.
async fn doubt(
	app: &App,
	nav: &dyn NavStore,
	batch: &NavBatch,
	fault: Option<(&str, &str)>,
	posted: Timestamp,
) {
	let uid = batch.uid.as_str();
	match nav.mark_in_doubt(batch.id, fault, Timestamp::now()).await {
		Ok(true) => {
			if let Err(e) = filing::ensure_reconcile(app, uid, reconcile_at(posted), false).await {
				tracing::error!(batch = uid, error = %e, "could not queue NAV_RECONCILE");
			}
		}
		Ok(false) => tracing::warn!(batch = uid, "this batch left POSTING during the POST"),
		Err(e) => tracing::error!(batch = uid, error = %e, "could not mark this batch in doubt"),
	}
}

/// B6, then `NAV_SEND` for the new batch, which waits on the hold B6 wrote. Best-effort like
/// [`abort`]; a lost write costs a reconciliation, then an operator.
async fn spend(app: &App, nav: &dyn NavStore, batch: &NavBatch, code: &str, message: &str) {
	let uid = batch.uid.as_str();
	match nav.spend_batch(batch.id, code, message, Timestamp::now()).await {
		Ok(Some(next)) => {
			if let Err(e) = filing::ensure_send(app, next.uid.as_str(), true).await {
				tracing::error!(batch = %next.uid.as_str(), error = %e, "could not queue NAV_SEND");
			}
		}
		Ok(None) => tracing::warn!(batch = uid, "this batch left POSTING during the POST"),
		Err(e) => tracing::error!(batch = uid, error = %e, "could not spend this batch"),
	}
}

/// `nav_batches.error_code` markers that are not HTTP errCodes: a reason recorded on an open batch,
/// so `NavStore::awaiting_operator` and `A-NAV-REJECTED` surface it.
pub const E_NAV_UNKNOWN_STATUS: &str = "E-NAV-UNKNOWN-STATUS";
pub const E_NAV_POLL_STALE: &str = "E-NAV-POLL-STALE";

/// A batch over [`BATCH_BYTE_BUDGET`], which stays `CLAIMED` until a person cancels it.
pub const E_NAV_OVERSIZE: &str = "E-NAV-OVERSIZE";

/// Reconciliation could not prove what NAV holds for this batch, so it is `PossiblyFiled` and
/// only an operator ends it.
pub const E_NAV_RECONCILE_UNPROVEN: &str = "E-NAV-RECONCILE-UNPROVEN";

/// Give a row that no longer builds back to its own `NAV_REPORT`; nothing feeding `invoice_data`
/// is frozen with the claim. Best-effort: a row left in place makes `begin_post` refuse this
/// envelope, and the re-run tries again.
async fn release_unbuildable(app: &App, nav: &dyn NavStore, row: &NavSubmission, err: &Error) {
	let invoice = row.invoice_id;
	tracing::warn!(invoice, error = %err, "this invoice no longer builds; releasing it");
	match nav.release_row(row.id).await {
		Ok(true) => {
			if let Err(e) = filing::ensure_report(app, invoice, false).await {
				tracing::error!(invoice, error = %e, "could not queue the released NAV_REPORT");
			}
		}
		Ok(false) => tracing::warn!(invoice, "this row left the batch before it could be released"),
		Err(e) => tracing::error!(invoice, error = %e, "could not release this row"),
	}
}

/// Record a marker on an open batch, once, without moving it. Best-effort like [`filing::park`]:
/// neither the poll's schedule nor the sweep may depend on it.
async fn mark(nav: &dyn NavStore, batch: &NavBatch, code: &str, message: &str) {
	// Once: a re-mark every round would overwrite whatever reason the batch already carries.
	if batch.error_code.is_none() {
		filing::park(nav, batch.id, code, message).await;
	}
}

/// What opens one operation block; see [`operation_slice`] for why the tag alone will not do.
const OPERATION_OPEN: &str = "<invoiceOperation><index>";

/// The `idx`th `<invoiceOperation>` block of a built `ManageInvoiceRequest`: one row's part of its
/// batch's archive.
///
/// Sliced on the `<index>` markers, not on the tag: `invoiceOperation` is nested inside itself
/// (`<invoiceOperation><index>1</index><invoiceOperation>CREATE</invoiceOperation>…`), so
/// matching the tag finds the operation string, not the block.
pub(crate) fn operation_slice(request: &str, idx: usize) -> Option<&str> {
	let from = request.find(&format!("{OPERATION_OPEN}{idx}</index>"))?;
	let rest = &request[from + OPERATION_OPEN.len()..];
	let to = rest
		.find(&format!("{OPERATION_OPEN}{}</index>", idx + 1))
		.or_else(|| rest.find("</invoiceOperations>"))
		.unwrap_or(rest.len());
	Some(&request[from..from + OPERATION_OPEN.len() + to])
}

/// What every job of a batch carries. `job_revive` writes it back, and one owner of the spelling
/// keeps every enqueue of a batch's job agreeing with it.
#[must_use]
pub fn batch_payload(batch_uid: &str) -> String {
	format!(r#"{{"batchUid":"{batch_uid}"}}"#)
}

/// The `NAV_SEND` dedup key: one sender per batch. A refile is a new batch, so a new key.
#[must_use]
pub fn send_key(batch_uid: &str) -> String {
	format!("nav:send:{batch_uid}")
}

/// The `NAV_POLL` dedup key: one poll chain per batch, whose backoff is the poll schedule.
#[must_use]
pub fn poll_key(batch_uid: &str) -> String {
	format!("nav:poll:{batch_uid}")
}

/// The `NAV_RECONCILE` dedup key: one reconciliation per batch.
#[must_use]
pub fn reconcile_key(batch_uid: &str) -> String {
	format!("nav:reconcile:{batch_uid}")
}

/// The `NAV_ADOPT` dedup key: per batch, not per row, so rows of one lost request bind one
/// transaction together.
#[must_use]
pub fn adopt_key(batch_uid: &str) -> String {
	format!("nav:adopt:{batch_uid}")
}

/// `seller_id` and its current version: what [`NavAuth::load`] authenticates.
async fn seller_now(
	invoices: &dyn InvoiceStore,
	seller_id: i64,
) -> ClResult<(Seller, SellerVersion)> {
	let (seller, current) = tokio::try_join!(
		invoices.seller_by_id(seller_id),
		invoices.current_seller_version(seller_id),
	)?;
	let seller =
		seller.ok_or_else(|| Error::internal("mintworks-nav: the batch's seller is gone"))?;
	let current = current
		.ok_or_else(|| Error::internal("mintworks-nav: the seller has no published version"))?;
	Ok((seller, current))
}

/// A client for `seller_id` as it stands today: NAV authenticates the current seller, whatever
/// version a filing froze at ISSUE.
pub(crate) async fn client_of(
	app: &App,
	invoices: &dyn InvoiceStore,
	seller_id: i64,
) -> ClResult<NavAuth> {
	let (seller, current) = seller_now(invoices, seller_id).await?;
	NavAuth::load(app, &seller, &current).await
}

/// The batch's `ACTIVE` rows; `batch_rows` answers any state.
async fn active_rows(nav: &dyn NavStore, batch_id: i64) -> ClResult<Vec<NavSubmission>> {
	let mut rows = nav.batch_rows(batch_id).await?;
	rows.retain(|r| r.state == RowState::Active);
	Ok(rows)
}

/// `invoices.number`, which is what NAV knows a filing by.
async fn invoice_number(invoices: &dyn InvoiceStore, invoice_id: i64) -> ClResult<String> {
	invoices
		.invoice_by_id(invoice_id)
		.await?
		.ok_or_else(|| Error::internal(format!("mintworks-nav: invoice {invoice_id} is gone")))?
		.number
		.ok_or_else(|| Error::internal("mintworks-nav: a filed invoice has no number"))
}

/// B9: NAV may hold this batch and reconciliation cannot prove how, so a person takes it. That is
/// a proved outcome, so the job ends; a batch that moved meanwhile needs nothing from here.
async fn hold_batch(nav: &dyn NavStore, batch: &NavBatch, why: &str) -> ClResult<()> {
	let uid = batch.uid.as_str();
	tracing::error!(batch = uid, why, "reconciliation is holding this batch for a person");
	if !nav
		.hold_batch(batch.id, E_NAV_RECONCILE_UNPROVEN, why, Timestamp::now())
		.await?
	{
		tracing::warn!(batch = uid, "this batch left IN_DOUBT during reconciliation");
	}
	Ok(())
}

/// Every transaction NAV lists in `[from, to]` that no batch here carries.
async fn unknown_transactions(
	client: &NavAuth,
	nav: &dyn NavStore,
	from: &str,
	to: &str,
) -> ClResult<Vec<String>> {
	let mut unknown = Vec::new();
	let mut page = 1i64;
	loop {
		tokio::time::sleep(QUERY_PACE).await;
		let request = client.query_transaction_list_request(page, from, to);
		let (_, reply) = client
			.post("queryTransactionList", &request)
			.await?
			.body("queryTransactionList")?;
		let (ids, available) = client::transaction_list(&Reply::parse(&reply))?;
		for id in ids {
			if nav.batch_by_transaction(&id).await?.is_none() {
				unknown.push(id);
			}
		}
		if page >= available {
			return Ok(unknown);
		}
		page += 1;
	}
}

/// §1.9.2 lost-transaction recovery: a `manageInvoice` that got no answer, so nobody knows whether
/// NAV holds the batch. Ends `Ok` on every proved outcome, `Err` only when NAV could not be asked.
///
/// The `NAV_RECONCILE` handler, public for the same reason [`report`] is: `Runner::tick` is
/// private, so a test drives it directly.
///
/// NAV's transaction list carries no `requestId`, so a transaction is this batch's only when the
/// invoice data it echoes back is **exactly** the `ACTIVE` rows, each at its `idx`. Any overlap
/// short of that goes to a person, and so does nothing found after NAV said it took this
/// `requestId`. Nothing found otherwise means NAV never took the batch, and §1.9.2 requires
/// repeating it immediately.
pub async fn reconcile(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	batch_uid: &str,
) -> ClResult<()> {
	let batch = nav
		.batch_by_uid(batch_uid)
		.await?
		.ok_or_else(|| Error::internal(format!("mintworks-nav: batch {batch_uid} is gone")))?;
	if batch.state != BatchState::InDoubt {
		return Ok(());
	}
	let client = client_of(app, invoices, batch.seller_id).await?;
	let mut wanted = Vec::new();
	for row in active_rows(nav, batch.id).await? {
		wanted.push((row.idx, invoice_number(invoices, row.invoice_id).await?));
	}

	// Every transaction of this taxpayer since the claim, not since now: a reconciliation delayed
	// by a restart or its own backoff would query a window the transaction has fallen out of.
	let (from_at, to_at) = reconcile_window(batch.created_at.0, Timestamp::now().0);
	let stamp = |t: i64| {
		auth::stamps(
			OffsetDateTime::from_unix_timestamp(t).unwrap_or_else(|_| OffsetDateTime::now_utc()),
		)
		.0
	};
	for (from, to) in reconcile_slices(from_at, to_at) {
		for transaction_id in unknown_transactions(&client, nav, &stamp(from), &stamp(to)).await? {
			tokio::time::sleep(QUERY_PACE).await;
			let request = client.query_status_original_request(&transaction_id);
			let (_, reply) = client
				.post("queryTransactionStatus", &request)
				.await?
				.body("queryTransactionStatus")?;
			let filed = match client::original_invoice_numbers(&Reply::parse(&reply)) {
				Ok(filed) => filed,
				// A fault in the question says nothing about any invoice, and the job retries.
				Err(fault) => {
					return Err(Error::Unavailable(format!(
						"queryTransactionStatus faulted during reconciliation ({fault:?}); \
						 the batch's status with NAV is still unknown"
					)));
				}
			};
			if !wanted.iter().any(|(_, n)| filed.iter().any(|(_, number)| number == n)) {
				continue;
			}
			// Part of this batch, more than it, or other indexes: `poll` would hand one invoice
			// another's verdict, and guessing what landed is how an invoice gets filed twice.
			let exact = filed.len() == wanted.len()
				&& wanted.iter().all(|(idx, n)| {
					filed.iter().any(|(i, number)| Some(*i) == *idx && number == n)
				});
			if !exact {
				let why = "NAV holds a transaction that does not match this batch exactly";
				return hold_batch(nav, &batch, why).await;
			}
			let sent =
				job::thrice(|| nav.set_sent(batch.id, &transaction_id)).await.map_err(|last| {
					last.unwrap_or_else(|| Error::internal("mintworks-nav: set_sent did not run"))
				})?;
			if !sent {
				return Err(Error::Unavailable(format!(
					"could not record transaction {transaction_id} on batch {batch_uid}"
				)));
			}
			tracing::warn!(
				batch = batch_uid,
				transaction = %transaction_id,
				invoices = wanted.len(),
				"recovered a lost manageInvoice reply; the batch is on the ordinary poll path"
			);
			filing::ensure_poll(app, batch_uid, false).await?;
			return Ok(());
		}
	}

	// NAV itself said it took this `requestId`, so an empty window is a miss, not an absence:
	// resending would earn the same refusal forever.
	if batch.error_code.is_some() {
		let why = "NAV refused this requestId as used, but no transaction here carries the batch";
		return hold_batch(nav, &batch, why).await;
	}
	// Same uid, so the same `requestId`: if this conclusion is ever wrong NAV refuses the resend
	// rather than filing twice.
	if nav.clear_doubt(batch.id).await? {
		filing::ensure_send(app, batch_uid, true).await?;
	} else {
		tracing::warn!(batch = batch_uid, "this batch left IN_DOUBT during reconciliation");
	}
	Ok(())
}

/// One `queryTransactionStatus` round. [`Next::Done`] only when NAV has reached a verdict for
/// every `ACTIVE` row of the batch; [`Next::Again`] while it has not, which is a success carrying
/// its own next run. An `Err` is reserved for a round that genuinely failed.
///
/// The `NAV_POLL` handler, public for the same reason [`report`] is: `Runner::tick` is
/// private, so a test drives the pair directly.
pub async fn poll(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	job: &Job,
	batch_uid: &str,
) -> ClResult<Next> {
	let batch = nav
		.batch_by_uid(batch_uid)
		.await?
		.ok_or_else(|| Error::internal(format!("mintworks-nav: batch {batch_uid} is gone")))?;
	if batch.state != BatchState::Sent {
		return Ok(Next::Done);
	}
	let transaction_id = batch
		.transaction_id
		.as_deref()
		.ok_or_else(|| Error::internal("mintworks-nav: a SENT batch has no transactionId"))?;

	let client = client_of(app, invoices, batch.seller_id).await?;
	let (_, reply) = client
		.post("queryTransactionStatus", &client.query_status_request(transaction_id))
		.await?
		.body("queryTransactionStatus")?;
	filing::archive_reply(nav, batch.id, &auth::redact(&reply)).await;

	let results = match outcomes(&Reply::parse(&reply)) {
		Ok(results) => results,
		// A fault in the *question*. It says nothing about any invoice, which NAV may already
		// have accepted and filed, so it must not finish a row.
		Err(Outcome::Unavailable { code, message }) => {
			return Err(Error::Unavailable(format!(
				"queryTransactionStatus faulted ({code}: {message}); \
				 the invoices' status with NAV is still unknown"
			)));
		}
		Err(other) => {
			return Err(Error::Unavailable(format!("queryTransactionStatus faulted: {other:?}")));
		}
	};

	// The runner's own backoff formula to the second: this cap plus the id-keyed jitter.
	let cap = app.settings.int("jobs.backoff_cap.NAV_POLL").await?;

	let active = active_rows(nav, batch.id).await?;
	let mut pending = 0usize;
	let mut held = false;
	for row in &active {
		let Some((_, outcome, _)) = results.iter().find(|(i, ..)| Some(*i) == row.idx) else {
			// Nothing is settled by omission: an index the reply does not mention is pending.
			tracing::warn!(
				submission = row.id,
				idx = row.idx.unwrap_or(0),
				"NAV's reply carries no result for this row; leaving it open"
			);
			pending += 1;
			continue;
		};
		let settled = match outcome {
			Outcome::Done => nav.finish(row.id, NavVerdict::Done, None, Timestamp::now()).await?,
			Outcome::Warn => nav.finish(row.id, NavVerdict::Warn, None, Timestamp::now()).await?,
			// R3: NAV already holds the number, usually through our own lost filing. Whether it is
			// ours is `NAV_ADOPT`'s to prove, so the row must not count as a plain rejection.
			Outcome::Failed { code, message }
				if code.as_str() == client::INVOICE_NUMBER_NOT_UNIQUE =>
			{
				tracing::warn!(
					submission = row.id,
					invoice = row.invoice_id,
					%message,
					"NAV already holds this invoice number; checking whether the filing is ours"
				);
				held = true;
				nav.finish_held(row.id, code, message, Timestamp::now()).await?
			}
			// `invoiceStatus = ABORTED`: NAV's verdict on the invoice, and the only rejection
			// there is. Retrying a verdict can only earn the same verdict, so this is where the
			// automation stops and `awaiting_operator` starts counting.
			Outcome::Failed { code, message } => {
				tracing::error!(
					submission = row.id,
					invoice = row.invoice_id,
					%code,
					%message,
					"NAV rejected the invoice; it needs a person to correct and re-issue it"
				);
				let error = Some((code.as_str(), message.as_str()));
				nav.finish(row.id, NavVerdict::Rejected, error, Timestamp::now()).await?
			}
			// Never settled: an unknown status may be one NAV has just added, and terminating a
			// statutory filing on it is worse than polling on. The marker stops it being silent,
			// and counts in `awaiting_operator` until the batch closes.
			Outcome::Unknown { status } => {
				mark(
					nav,
					&batch,
					E_NAV_UNKNOWN_STATUS,
					&format!(
						"NAV reported invoiceStatus '{status}', which is outside \
						 InvoiceStatusType; the filing is still being polled"
					),
				)
				.await;
				pending += 1;
				continue;
			}
			Outcome::Pending | Outcome::Unavailable { .. } => {
				pending += 1;
				continue;
			}
		};
		if !settled {
			tracing::warn!(submission = row.id, "the filing was settled while this poll ran");
		}
	}
	if held {
		filing::ensure_adopt(app, batch_uid, false).await?;
	}
	if pending > 0 {
		let base = job::backoff_secs(job.attempts, cap);
		let at = Timestamp(Timestamp::now().0 + base + job.id.rem_euclid(base / 4 + 1));
		tracing::debug!(
			batch = batch_uid,
			pending,
			of = active.len(),
			at = at.0,
			"NAV is still processing this transaction; asking again"
		);
		return Ok(Next::Again { at });
	}
	Ok(Next::Done)
}

/// `(transactionId, index)` when `found` proves the filing is this row's: bindable to a poll, not
/// compressed, and decoding to the bytes the batch's archive holds at `idx`. The number alone
/// proves nothing, since another program may hold it.
fn adoption(
	found: &InvoiceDataResult,
	request: Option<&str>,
	idx: Option<i64>,
) -> Option<(String, i64)> {
	if found.compressed {
		return None;
	}
	let slice = operation_slice(request?, usize::try_from(idx?).ok()?)?;
	let ours = B64.decode(auth::element_text(slice, "invoiceData")?).ok()?;
	let theirs = B64.decode(&found.invoice_data).ok()?;
	if ours != theirs {
		return None;
	}
	Some((found.transaction_id.clone()?, found.index?))
}

/// R9: the one automatic check of a row NAV aborted on a number it already holds. Every read comes
/// before any write, so rows of one lost request land in one `SENT` batch, and a re-run repeats
/// only reads. A row left unproved stays `HELD` for a person.
///
/// The `NAV_ADOPT` handler, public for the same reason [`report`] is. No gate on the batch's state:
/// R3 on its last row leaves it `CLOSED`.
pub async fn adopt(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	batch_uid: &str,
) -> ClResult<Next> {
	let batch = nav
		.batch_by_uid(batch_uid)
		.await?
		.ok_or_else(|| Error::internal(format!("mintworks-nav: batch {batch_uid} is gone")))?;
	let mut rows = nav.batch_rows(batch.id).await?;
	rows.retain(|r| r.state == RowState::Held && r.adopt_checked_at.is_none());
	if rows.is_empty() {
		return Ok(Next::Done);
	}
	let client = client_of(app, invoices, batch.seller_id).await?;
	let request = nav.batch_archive(batch.id).await?.and_then(|a| a.request_xml);

	let mut proved: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
	for row in &rows {
		tokio::time::sleep(QUERY_PACE).await;
		let number = invoice_number(invoices, row.invoice_id).await?;
		let (_, reply) = client
			.post("queryInvoiceData", &client.query_invoice_data_request(&number))
			.await?
			.body("queryInvoiceData")?;
		let now = Timestamp::now().0;
		match invoice_data_result(&Reply::parse(&reply))? {
			Some(found) => {
				if let Some((transaction_id, idx)) = adoption(&found, request.as_deref(), row.idx) {
					proved.entry(transaction_id).or_default().push((row.id, idx));
				}
			}
			// The first filing may still be processing, and this row has no answer yet.
			None if row.done_at.is_some_and(|at| now - at.0 < ADOPT_WAIT_SECS) => {
				return Ok(Next::Again { at: Timestamp(now + ADOPT_RECHECK_SECS) });
			}
			None => {}
		}
	}

	for (transaction_id, pairs) in &proved {
		let at = Timestamp::now();
		let attached = nav
			.attach_transaction(transaction_id, batch.seller_id, batch.op, pairs, at)
			.await?;
		if let Some(adopted) = attached {
			let uid = adopted.uid.as_str();
			tracing::warn!(
				batch = batch_uid,
				adopted = uid,
				transaction = %transaction_id,
				invoices = pairs.len(),
				"NAV holds these invoices as our own filing; adopted it"
			);
			filing::ensure_poll(app, uid, false).await?;
		} else {
			tracing::warn!(
				batch = batch_uid,
				transaction = %transaction_id,
				"could not adopt this transaction; leaving its rows for a person"
			);
		}
	}
	let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
	nav.stamp_adopt_checked(&ids, Timestamp::now()).await?;
	Ok(Next::Done)
}

#[cfg(test)]
mod tests {
	use super::{
		RECONCILE_SLICE_SECS, RECONCILE_WINDOW_SECS, adoption, operation_slice, reconcile_slices,
		reconcile_window, wire_bytes,
	};
	use crate::client::{InvoiceDataResult, invoice_data_result};
	use crate::reply::Reply;
	use base64::{Engine, engine::general_purpose::STANDARD as B64};

	/// The byte budget is NAV's limit on the encoded request, so it must weigh what goes out.
	#[test]
	fn wire_bytes_is_the_padded_base64_length() {
		for doc in ["", "a", "ab", "abc", "abcd"] {
			assert_eq!(wire_bytes(doc), B64.encode(doc).len(), "{doc:?}");
		}
	}

	/// `Nav::filing_archives` shows a row this slice alone: one block too many hands a tenant
	/// another org's invoice.
	#[test]
	fn a_slice_is_one_operation_block() {
		let two = "<invoiceOperations><invoiceOperation><index>1</index>\
			<invoiceOperation>CREATE</invoiceOperation><invoiceData>QQ==</invoiceData>\
			</invoiceOperation><invoiceOperation><index>2</index>\
			<invoiceOperation>CREATE</invoiceOperation><invoiceData>Qg==</invoiceData>\
			</invoiceOperation></invoiceOperations>";
		assert!(operation_slice(two, 1).is_some_and(|s| s.contains("QQ==") && !s.contains("Qg==")));
		assert!(operation_slice(two, 2).is_some_and(|s| s.contains("Qg==") && !s.contains("QQ==")));
	}

	/// The `to` side used to be `now` exactly, so NAV stamping `insDate` ahead of this host put
	/// the transaction outside the window and reconciliation resent under a burned `requestId`.
	#[test]
	fn the_reconcile_window_has_skew_slack_on_both_sides() {
		let (from, to) = reconcile_window(1_000, 2_000);
		assert!(1_000 - from >= RECONCILE_WINDOW_SECS, "no slack before the batch opened");
		assert!(to - 2_000 >= RECONCILE_WINDOW_SECS, "no slack after now");
	}

	/// A missing marker must slice nothing: falling back to the whole envelope leaks every
	/// other org's `invoiceData`, and `redact` fails closed by truncating.
	#[test]
	fn a_missing_operation_marker_slices_nothing() {
		let one = "<invoiceOperations><invoiceOperation><index>1</index>\
			<invoiceOperation>CREATE</invoiceOperation><invoiceData>QQ==</invoiceData>\
			</invoiceOperation></invoiceOperations>";
		assert!(operation_slice(one, 1).is_some_and(|s| s.contains("QQ==")));
		assert!(operation_slice(one, 2).is_none(), "nothing to archive, not everything");
		assert!(operation_slice("", 1).is_none());
	}

	/// A gap between slices hides a filing, and a slice over NAV's limit is refused outright.
	#[test]
	fn the_reconcile_window_is_walked_in_slices_newest_first() {
		let to = 3 * RECONCILE_SLICE_SECS - 5;
		let slices = reconcile_slices(0, to);
		assert_eq!(slices.len(), 3);
		assert_eq!(slices[0].1, to, "the newest slice goes first");
		assert_eq!(slices[2].0, 0, "the oldest slice reaches the window's start");
		assert!(slices.windows(2).all(|p| p[0].0 == p[1].1), "the slices must touch");
		assert!(slices.iter().all(|(from, to)| to - from <= RECONCILE_SLICE_SECS));
		assert_eq!(reconcile_slices(100, 200), vec![(100, 200)]);
	}

	/// The number alone proves nothing: it may be another program's filing.
	#[test]
	fn only_our_own_bytes_in_a_bindable_filing_are_proved() {
		let request = "<invoiceOperations><invoiceOperation><index>1</index>\
			<invoiceOperation>CREATE</invoiceOperation><invoiceData>QQ==</invoiceData>\
			</invoiceOperation></invoiceOperations>";
		let ours = InvoiceDataResult {
			invoice_data: "QQ==".to_owned(),
			transaction_id: Some("T1".to_owned()),
			index: Some(7),
			compressed: false,
		};
		assert_eq!(adoption(&ours, Some(request), Some(1)), Some(("T1".to_owned(), 7)));

		let theirs = InvoiceDataResult { invoice_data: "Qg==".to_owned(), ..ours.clone() };
		assert_eq!(adoption(&theirs, Some(request), Some(1)), None);
		let compressed = InvoiceDataResult { compressed: true, ..ours.clone() };
		assert_eq!(adoption(&compressed, Some(request), Some(1)), None);
		let unbound = InvoiceDataResult { transaction_id: None, ..ours.clone() };
		assert_eq!(adoption(&unbound, Some(request), Some(1)), None);
		let no_index = InvoiceDataResult { index: None, ..ours.clone() };
		assert_eq!(adoption(&no_index, Some(request), Some(1)), None);
		assert_eq!(adoption(&ours, None, Some(1)), None, "no archive proves nothing");
		assert_eq!(adoption(&ours, Some(request), Some(2)), None);
	}

	/// `invoiceDataResult` is optional, so its absence is an answer and a fault is not.
	#[test]
	fn no_invoice_data_result_is_a_value_and_a_fault_is_an_error() {
		let head = "<QueryInvoiceDataResponse><result><funcCode>OK</funcCode></result>";
		let found = format!(
			"{head}<invoiceDataResult><invoiceData>QQ==</invoiceData><auditData>\
			 <transactionId>T1</transactionId><index>2</index></auditData>\
			 <compressedContentIndicator>false</compressedContentIndicator>\
			 </invoiceDataResult></QueryInvoiceDataResponse>"
		);
		assert_eq!(
			invoice_data_result(&Reply::parse(&found)).unwrap(),
			Some(InvoiceDataResult {
				invoice_data: "QQ==".to_owned(),
				transaction_id: Some("T1".to_owned()),
				index: Some(2),
				compressed: false,
			})
		);
		let empty = format!("{head}</QueryInvoiceDataResponse>");
		assert_eq!(invoice_data_result(&Reply::parse(&empty)).unwrap(), None);
		let fault = "<GeneralErrorResponse><result><funcCode>ERROR</funcCode></result>\
			</GeneralErrorResponse>";
		assert!(invoice_data_result(&Reply::parse(fault)).is_err());
	}
}

// vim: ts=4
