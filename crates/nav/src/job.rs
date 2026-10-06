// SPDX-License-Identifier: MPL-2.0
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
//! * [`filing::may_send`] refuses an invoice whose row already carries a verdict or a
//!   `transactionId`, so once NAV holds the filing the retry stops sending and starts polling.
//!
//! So nothing here gives up: `jobs.max_attempts.NAV_REPORT` and `.NAV_POLL` are `0`.
//! `jobs.alert_after.NAV_POLL` is what tells a human, and `Nav::cancel_filing` is how one
//! stops a filing that can never succeed.
//!
//! NAV's verdict on the invoice itself is not available on `manageInvoice` at all: it arrives
//! on the poll path as `invoiceStatus = ABORTED`.

use mintworks_core::{
	App,
	job::{self, Job, Next, Runner},
	prelude::*,
};
use std::collections::HashMap;

use mintworks_invoice::{InvoiceKind, InvoiceStore, KIND_NAV_REPORT, invoice_job_payload};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::auth;
use crate::auth::{Answer, NavAuth};
use crate::client::{self, Accepted, Disposition, Outcome, accepted, outcomes};
use crate::filing;
use crate::reply::Reply;
use crate::service_api::Nav;
use crate::store::NavStore;
use crate::submission::{NavOp, NavSubmission, NavVerdict};
use crate::xml::invoice_data;

pub const KIND_NAV_POLL: &str = "NAV_POLL";
pub const KIND_NAV_SWEEP: &str = "NAV_SWEEP";
pub const KIND_NAV_RECONCILE: &str = "NAV_RECONCILE";

/// How often `NAV_SWEEP` runs. An hour: a missed statutory filing is measured in days, and
/// every sweep is two indexed reads.
const SWEEP_EVERY_SECS: i64 = 3600;

/// How many never-enqueued filings one sweep tick picks up. A literal rather than a setting:
/// the backlog it drains is a crash window, not a backlog anybody tunes.
const SWEEP_BATCH: i64 = 100;

/// How long after `manageInvoice` the first `queryTransactionStatus` runs. Every poll after it
/// is the job runner's own backoff up to `jobs.backoff_cap.NAV_POLL`.
const POLL_FIRST_DELAY_SECS: i64 = 5;

/// How long after a lost `manageInvoice` reply `NAV_RECONCILE` asks NAV what landed. The
/// specification's §1.9.2 five minutes: until then the submission is still alive server-side and
/// the commit-or-rollback is not decided, so an earlier question can only get an answer that
/// changes.
const RECONCILE_DELAY_SECS: i64 = 300;

/// How far *before* the batch's own `created_at` the `queryTransactionList` window opens. It
/// absorbs the clock skew between this process and NAV's `insDate`, which is the only reason the
/// window is not exactly `[created_at, now]`; §1.9.2's own example is "the last 10 minutes".
const RECONCILE_WINDOW_SECS: i64 = 600;

/// `[from, to]` for the batch's `queryTransactionList`, in unix seconds. Skew is absorbed on
/// **both** sides: NAV stamping `insDate` ahead of this host dropped the transaction out of a
/// window that ended at `now`, and an empty `unknown` is read as "NAV never took the batch". A
/// `dateTimeTo` in the future is harmless — NAV has nothing stamped there to return.
const fn reconcile_window(opened: i64, now: i64) -> (i64, i64) {
	(opened - RECONCILE_WINDOW_SECS, now + RECONCILE_WINDOW_SECS)
}

/// The `NAV_REPORT` payload `mintworks-invoice` enqueues (`issue.rs::enqueue_jobs`).
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

#[derive(Deserialize)]
struct ReconcilePayload {
	#[serde(rename = "batchUid")]
	batch_uid: String,
}

/// The four handlers deserialize their payload and make exactly one call into [`Nav`],
/// which is the same rule a route handler follows. `Nav::new` resolves both stores from
/// `app.extensions`, so nothing is threaded in here.
pub fn register(runner: &mut Runner, app: App) {
	let handle = Nav::new(app);

	let h = handle.clone();
	// `register_next`: a tenant that has not connected NAV yet defers without an error.
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
	// `register_next`: a poll NAV has not answered yet is a success with a schedule, not a failure
	// — as an `Err` it raised `A-JOB-STALE`; `poll` records `E-NAV-POLL-STALE` on the filing.
	runner.register_next(KIND_NAV_POLL, move |job: Job| {
		let h = h.clone();
		async move {
			let p: PollPayload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("mintworks-nav: bad {KIND_NAV_POLL} payload: {e}"))
			})?;
			h.run_poll(&job, p.submission_id).await
		}
	});

	let h = handle.clone();
	runner.register(KIND_NAV_RECONCILE, move |job: Job| {
		let h = h.clone();
		async move {
			let p: ReconcilePayload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("mintworks-nav: bad {KIND_NAV_RECONCILE} payload: {e}"))
			})?;
			h.run_reconcile(&p.batch_uid).await
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
/// `mintworks_invoice::issue::enqueue_jobs` — a crash between COMMIT and enqueue.
///
/// That is the whole job now. An invoice whose `NAV_REPORT` *was* enqueued is a job row the
/// runner retries on its own backoff, unbounded, so re-driving it from here would file it
/// twice; `dedup_key` survives termination, so the hourly stampede the two old circuit
/// breakers contained cannot happen; and a stranded poll is re-enqueued by [`filing::may_send`] on
/// the report's own retry rather than by a scan here.
///
/// **Nothing in here propagates.** The signature stays `ClResult<()>` because that is the
/// runner's handler type, but every failure logs and gives up on this tick: the sweep is
/// periodic, and nothing re-seeds a periodic kind except `seed_periodic`, which runs at boot.
pub async fn sweep(app: &App, _invoices: &dyn InvoiceStore, nav: &dyn NavStore) -> ClResult<()> {
	let now = Timestamp::now();
	// Covers the root seller only; loop over sellers with a `nav_login` when tenant
	// invoices need the crash-window sweep too.
	let seller = match auth::deployment_seller(app).await {
		Ok(Some(s)) => s.id,
		Ok(None) => {
			tracing::debug!("the root org owns no seller; nothing to sweep");
			return Ok(());
		}
		Err(e) => {
			tracing::error!(error = %e, "could not resolve the deployment seller; nothing swept");
			return Ok(());
		}
	};

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
		let payload = mintworks_invoice::invoice_job_payload(invoice_id);
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

/// How long a tenant seller's `NAV_REPORT` waits before looking for credentials again.
/// Connecting pulls the backlog forward (`CoreStore::job_wake`), so this is only the fallback.
pub const NOT_CONNECTED_RECHECK_SECS: i64 = 6 * 3600;

/// `Some(at)` when the invoice's seller is a tenant that has not connected NAV: the filing
/// waits, recording no error and no `nav_submissions` row.
pub async fn deferral(
	app: &App,
	invoices: &dyn InvoiceStore,
	invoice_id: i64,
) -> ClResult<Option<Timestamp>> {
	let Some(invoice) = invoices.invoice_by_id(invoice_id).await? else {
		return Ok(None);
	};
	let Some(seller) = invoices.seller_by_id(invoice.seller_id).await? else {
		return Ok(None);
	};
	if auth::connected(app, &seller).await? {
		return Ok(None);
	}
	Ok(Some(Timestamp(Timestamp::now().0 + NOT_CONNECTED_RECHECK_SECS)))
}

/// Build the invoice's `invoiceData`, submit it, and hand the `transactionId` to `NAV_POLL`.
///
/// The `NAV_REPORT` handler, public so it can be driven directly by a test: `Runner::tick` is
/// private and `run` never returns. Calling it twice for one invoice is safe — see
/// [`filing::may_send`], which is the whole point.
pub async fn report(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	invoice_id: i64,
) -> ClResult<()> {
	// Read first: `may_send` needs the uid to tell this invoice's own batch from somebody else's.
	let invoice = invoices
		.invoice_by_id(invoice_id)
		.await?
		.ok_or_else(|| Error::internal(format!("mintworks-nav: invoice {invoice_id} is gone")))?;
	if !filing::may_send(app, nav, invoice_id, invoice.uid.as_str()).await? {
		return Ok(());
	}
	// A lost `manageInvoice` reply is resolved by `NAV_RECONCILE`, never by resending: §1.9.2
	// forbids the immediate repeat, and the first backoff step is one second. Any spent key counts,
	// not just a live one — resending past a `FAILED` one earns `REQUEST_ID_NOT_UNIQUE`.
	if app
		.store
		.job_status_by_key(&format!("nav:reconcile:{}", invoice.uid.as_str()))
		.await?
		.is_some()
	{
		enqueue_reconcile(app, nav, invoice.uid.as_str()).await?;
		return Err(Error::Unavailable(format!(
			"a NAV reconciliation for {} is outstanding; not resending under the same requestId",
			invoice.uid.as_str()
		)));
	}
	// Two rows, two lifetimes: the filing's content comes from the version the invoice froze at
	// ISSUE, while `NavAuth::load` below authenticates as today's seller.
	let seller_ver = invoice
		.seller_ver
		.ok_or_else(|| Error::internal("mintworks-nav: an issued invoice has no seller_ver"))?;
	let (seller, version, current, lines, groups, document) = tokio::try_join!(
		invoices.seller_by_id(invoice.seller_id),
		invoices.seller_version(seller_ver),
		invoices.current_seller_version(invoice.seller_id),
		invoices.invoice_lines(invoice.id),
		invoices.invoice_vat_groups(invoice.id),
		invoices.invoice_document(invoice.id),
	)?;
	let seller =
		seller.ok_or_else(|| Error::internal("mintworks-nav: the invoice's seller is gone"))?;
	let version = version
		.ok_or_else(|| Error::internal("mintworks-nav: the invoice's seller version is gone"))?;
	let current = current
		.ok_or_else(|| Error::internal("mintworks-nav: the seller has no published version"))?;

	// NAV archives this hash as the only independent proof that the buyer's PDF is the one
	// issued, and `manageInvoice` files an invoice once — a filing that raced `RENDER_PDF` would
	// be unprovable forever. `Unavailable` before `create_submission`, so no row is burned.
	let pdf_sha256 = if app.settings.flag("nav.electronic_invoice").await? {
		Some(document.map(|d| d.sha256).ok_or_else(|| {
			Error::Unavailable(
				"mintworks-nav: the invoice PDF is not rendered yet; retrying".into(),
			)
		})?)
	} else {
		None
	};

	let (op, original) = match invoice.kind {
		InvoiceKind::Storno => {
			let id = invoice
				.original_invoice_id
				.ok_or_else(|| Error::internal("mintworks-nav: storno with no original"))?;
			let original =
				invoices.invoice_by_id(id).await?.and_then(|i| i.number).ok_or_else(|| {
					Error::internal("mintworks-nav: the stornoed invoice has no number")
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

	let doc = invoice_data(&version, &invoice, &lines, &groups, original.as_deref())?;
	let client = NavAuth::load(app, &seller, &current).await?;

	// One exchange token covers one request however many invoices it carries (§1.1). A storno
	// files alone: NAV promises no processing order *within* a batch, and the storno
	// precondition above needs the invoice it cancels already accepted.
	let batch_max = if op == NavOp::Storno { 1 } else { app.settings.int("nav.batch_max").await? };
	// A member claimed by an earlier attempt already has a row, so `batch_candidates`' `NOT EXISTS`
	// stops offering it: re-deriving membership from candidates alone resends under the same
	// `requestId` with fewer invoices than the attempt it repeats.
	let batch_rows = nav.submissions_by_batch(invoice.uid.as_str()).await?;
	// Still open for this batch is the three columns `claim_batch`'s member upsert gates on: no
	// verdict, no `transactionId`, not the leader. `error_code` is only a diagnostic `park` records
	// on an open row; reading it as settled strands a faulted member off every later resend.
	let mut members: Vec<i64> = batch_rows
		.iter()
		.filter(|s| s.invoice_id != invoice.id && s.verdict.is_none() && s.transaction_id.is_none())
		.map(|s| s.invoice_id)
		.collect();
	// Which of them an earlier attempt already claimed, by submission id: dropping one of these
	// from the batch below has to release it, or nothing will ever POST it.
	let resumed: HashMap<i64, i64> = batch_rows
		.iter()
		.filter(|s| members.contains(&s.invoice_id))
		.map(|s| (s.invoice_id, s.id))
		.collect();
	// Membership freezes once the leader's envelope is archived: `archive_request` keeps the
	// *first* attempt, so admitting a fresh candidate on a resend would leave that archive —
	// the record `release_batch` relies on — describing a batch nobody ever sent.
	let leader_row = batch_rows.iter().find(|s| s.invoice_id == invoice.id);
	let frozen = match leader_row {
		Some(s) => nav.request_archived(s.id).await?,
		None => false,
	};
	let room =
		if frozen { 0 } else { batch_max - 1 - i64::try_from(members.len()).unwrap_or(i64::MAX) };
	if room > 0 {
		let candidates = nav
			.batch_candidates(invoice.seller_id, invoice.id, pdf_sha256.is_some(), room)
			.await?;
		let keys: Vec<String> = candidates.iter().map(|id| format!("nav:invoice:{id}")).collect();
		let live: HashMap<String, String> =
			app.store.job_statuses_by_keys(&keys).await?.into_iter().collect();
		// Only an invoice with a live `PENDING` filing joins, or batching would file one whose
		// filing an operator stopped through `Nav::cancel_filing`. `RUNNING` is excluded too —
		// its own worker is already past `may_send`.
		for (id, key) in candidates.into_iter().zip(&keys) {
			if live.get(key).map(String::as_str) == Some("PENDING") {
				members.push(id);
			}
		}
	}

	// Four bulk reads for the whole batch, not the leader's four-query `try_join!` per member.
	let (member_rows, member_lines, member_groups, member_docs) = tokio::try_join!(
		invoices.invoices_by_ids(&members),
		invoices.invoice_lines_for(&members),
		invoices.invoice_vat_groups_for(&members),
		invoices.invoice_documents_for(&members),
	)?;
	let doc_of: HashMap<i64, _> = member_docs.into_iter().map(|d| (d.invoice_id, d)).collect();
	// A fifth bulk read, for the same reason: every member files under the version *it* froze,
	// and a batch spans invoices issued either side of a seller edit.
	let member_vers: Vec<i64> = member_rows.iter().filter_map(|i| i.seller_ver).collect();
	let version_of: HashMap<i64, _> = invoices
		.seller_versions(&member_vers)
		.await?
		.into_iter()
		.map(|v| (v.seller_ver, v))
		.collect();
	let by_id: HashMap<i64, _> = member_rows.into_iter().map(|i| (i.id, i)).collect();
	let mut lines_of: HashMap<i64, Vec<_>> = HashMap::new();
	for l in member_lines {
		lines_of.entry(l.invoice_id).or_default().push(l);
	}
	let mut groups_of: HashMap<i64, Vec<_>> = HashMap::new();
	for g in member_groups {
		groups_of.entry(g.invoice_id).or_default().push(g);
	}

	// Built **before** the claim: a member claimed and then dropped has a row nothing will ever
	// POST, which `may_send` refuses, `unfiled_invoices` skips and `Nav::submit` calls
	// `E-NAV-NOT-REDRIVABLE`. Unbuilt, it is simply never claimed and its own job files it — but
	// a *resumed* member is already claimed, so `release_member` below is what gives it back.
	let mut built: HashMap<i64, (String, Option<String>)> = HashMap::new();
	let mut buildable: Vec<i64> = Vec::with_capacity(members.len());
	for inv_id in &members {
		let pair: ClResult<(String, Option<String>)> = async {
			let member = by_id.get(inv_id).ok_or_else(|| {
				Error::internal(format!("mintworks-nav: invoice {inv_id} is gone"))
			})?;
			let hash = if pdf_sha256.is_some() {
				Some(doc_of.get(inv_id).map(|d| d.sha256.clone()).ok_or_else(|| {
					Error::internal("mintworks-nav: the invoice PDF is not rendered yet")
				})?)
			} else {
				None
			};
			let member_version =
				member.seller_ver.and_then(|v| version_of.get(&v)).ok_or_else(|| {
					Error::internal("mintworks-nav: an issued invoice has no seller_ver")
				})?;
			let xml = invoice_data(
				member_version,
				member,
				lines_of.get(inv_id).map_or(&[][..], Vec::as_slice),
				groups_of.get(inv_id).map_or(&[][..], Vec::as_slice),
				None,
			)?;
			Ok((xml, hash))
		}
		.await;
		match pair {
			Ok(pair) => {
				built.insert(*inv_id, pair);
				buildable.push(*inv_id);
			}
			Err(e) => {
				tracing::warn!(
					invoice = inv_id,
					error = %e,
					"could not build this invoice's invoiceData; leaving it out of the batch"
				);
				// Nothing feeding `invoice_data` is frozen with the claim, so a member built on
				// an earlier attempt can stop building on this one — turning
				// `nav.electronic_invoice` on demands a PDF no member was screened for. Dropping
				// it while it still carries `batch_uid` is an invoice no path can ever file.
				if let Some(&sub_id) = resumed.get(inv_id) {
					release_unbuildable(app, nav, invoice.uid.as_str(), sub_id, *inv_id, &e).await;
				}
			}
		}
	}

	// As late as it can be while still preceding the claim: NAV sets the token's validity window
	// and nothing caches it, so the `invoiceData` builds above would run it down — and past the
	// claim a `tokenExchange` outage would burn a row `unfiled_invoices` can no longer see.
	let token = client.token_exchange().await?;

	// Get-or-claim for the leader, create-only for the members, in one write transaction.
	// The request is built from what came back, never from what was asked for.
	let claimed = nav.claim_batch(invoice.id, op, invoice.uid.as_str(), &buildable).await?;
	let Some((id, _)) = claimed.iter().copied().find(|(_, inv)| *inv == invoice.id) else {
		// The stand-down `create_submission` used to perform when `idx_nav_submission_live`
		// refused: the row is settled, in flight, or already a member of another leader's
		// batch — and a member never files itself.
		let leader = nav.submission_by_invoice(invoice.id).await?.and_then(|s| s.batch_uid);
		tracing::info!(
			invoice = invoice.id,
			leader = leader.as_deref().unwrap_or("-"),
			"another runner owns this invoice's filing; standing down"
		);
		return Ok(());
	};

	// `rows` and `payload` stay index-aligned: position `i` is the `<index>` `i + 1` on the
	// wire, which is the `idx` `set_sent` writes and the poll matches NAV's results against.
	let mut rows: Vec<(i64, i64)> = Vec::with_capacity(claimed.len());
	let mut payload: Vec<(String, Option<String>)> = Vec::with_capacity(claimed.len());
	for (sub_id, inv_id) in &claimed {
		if *inv_id == invoice.id {
			rows.push((*sub_id, *inv_id));
			payload.push((doc.clone(), pdf_sha256.clone()));
			continue;
		}
		let pair = built.remove(inv_id).ok_or_else(|| {
			Error::internal(format!("mintworks-nav: invoice {inv_id} was not built"))
		})?;
		rows.push((*sub_id, *inv_id));
		payload.push(pair);
	}

	// The invoice's own uid is the NAV `requestId` for the whole batch: it is what makes a
	// retry idempotent, and it is the only identifier here that no cleanup or rebuild can
	// reissue. See [`NavAuth::manage_invoice_request`].
	let request = client.manage_invoice_request(op, invoice.uid.as_str(), &payload, &token)?;

	// Archived before it leaves the process, and the reply before anything reads it. Both go
	// through `redact`: the envelope authenticates with an unsalted SHA-512 password hash,
	// and an archive that kept it would be a replayable NAV credential per row.
	let redacted_request = auth::redact(&request);
	for (i, (sub_id, inv_id)) in rows.iter().enumerate() {
		// The leader keeps the whole envelope — it is the only surviving record of a released
		// member's attempt, which is what makes `release_batch` legal — and each member keeps
		// its own operation: ~5 KB of base64 per invoice stored N times over is O(N²).
		let xml = if *inv_id == invoice.id {
			Some(redacted_request.as_str())
		} else {
			operation_slice(&redacted_request, i + 1)
		};
		// Nothing, never the whole envelope, when the slice is not found: that fallback copied
		// every other org's `invoiceData` onto this member's archive, which is exactly what
		// `Nav::filing_archive`'s operator gate hides. The leader's row still holds it all.
		if let Some(xml) = xml {
			nav.archive_request(*sub_id, xml).await?;
		} else {
			tracing::warn!(
				submission = sub_id,
				invoice = inv_id,
				index = i + 1,
				"could not slice this member's operation out of the request; archiving nothing"
			);
		}
	}

	// No row write on failure: the row stays open with its request archived, and the failure
	// is the `jobs` row's business — `last_error` and `err_code` record it, `Error::retry()`
	// decides whether it is tried again.
	let (status, reply) = match client.post("manageInvoice", &request).await? {
		Answer::Reply { status, xml } => (status, xml),
		// The submission's fate is unknown — NAV may hold the batch — which is what §1.9.2
		// exists to resolve. `Answer::Unavailable` demonstrably never reached NAV and stays on
		// the plain resend path.
		Answer::Indeterminate => {
			// Best-effort, like the sibling branch below: `Error::Timeout` is `Retry::Backoff`
			// and a store failure is `Retry::Never`, so propagating the enqueue's error instead
			// would terminate the leader's job with the batch claimed and nothing reconciling it.
			if let Err(qe) = enqueue_reconcile(app, nav, invoice.uid.as_str()).await {
				tracing::error!(invoice = invoice.id, error = %qe,
					"could not enqueue the NAV reconciliation for a lost reply");
			}
			return Err(auth::indeterminate());
		}
		Answer::Unavailable => return Err(auth::unavailable()),
		Answer::Throttled { retry_after } => return Err(Error::RateLimit(retry_after)),
	};
	let redacted = auth::redact(&reply);
	let parsed_reply = Reply::parse(&reply);

	let parsed = match accepted(status, &parsed_reply) {
		Ok(parsed) => parsed,
		// Unreadable, or `OK` with no `transactionId`: nothing to record, so the archive keeps
		// its place as the only trace of what NAV said.
		Err(e) => {
			filing::archive_reply(nav, id, invoice.id, &redacted).await;
			// Both are §1.9.2 lost replies — NAV may hold the batch — so the resend can only
			// earn `REQUEST_ID_NOT_UNIQUE` and park every member for an operator. Best-effort,
			// for the same reason `record_fault` below is: the error in hand must survive.
			if matches!(e.parts().1, "E-NAV-UNREADABLE-REPLY" | "E-NAV-NO-TRANSACTION-ID")
				&& let Err(qe) = enqueue_reconcile(app, nav, invoice.uid.as_str()).await
			{
				tracing::error!(invoice = invoice.id, error = %qe,
					"could not enqueue the NAV reconciliation for a lost reply");
			}
			return Err(e);
		}
	};
	match parsed {
		Accepted::Ok { transaction_id } => {
			// **Before the archive**, which is only a diagnostic: this is the one part of the
			// reply that cannot be reconstructed, and a lost `transactionId` makes the retry
			// resend under the same `requestId`, which NAV refuses as `REQUEST_ID_NOT_UNIQUE`.
			let applied = filing::record_sent(nav, &rows, invoice.id, &transaction_id).await?;
			filing::archive_reply(nav, id, invoice.id, &redacted).await;
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
			filing::archive_reply(nav, id, invoice.id, &redacted).await;
			// The leader's row alone. The terminal paths settle members themselves, through
			// `release_batch`'s `done_at` test or `finish`.
			filing::park(nav, id, &code, &message).await;
			// NAV answered and did not accept, so nothing was filed and a fresh attempt is safe —
			// and necessary, or bad credentials drop every invoice issued while the fault lasts.
			// `client::disposition` names the codes for which it is not.
			tracing::error!(
				submission = id,
				invoice = invoice.id,
				%code,
				%message,
				"manageInvoice faulted; the invoice was not filed"
			);
			let err = client::business(&code, &message);
			// Re-read first: the guard above the POST cannot see an in-flight sibling, so a
			// `REQUEST_ID_NOT_UNIQUE` refusal would flip an accepted filing to `FAILED` for good.
			let disposition = client::disposition(&code);
			if disposition != Disposition::Retry {
				let cur = nav.submission(id).await?;
				if cur.is_some_and(|s| s.verdict.is_some() || s.transaction_id.is_some()) {
					tracing::info!(
						submission = id,
						invoice = invoice.id,
						"another runner's filing landed while this attempt was refused; \
						 verdict kept"
					);
				} else {
					match disposition {
						// The one fault meaning NAV *probably holds this filing*: a POST that
						// succeeded then lost its `transactionId` retries into exactly it.
						// `FAILED` here would be an archive saying a filed invoice was never filed.
						Disposition::PossiblyFiled => {
							tracing::error!(
								submission = id,
								invoice = invoice.id,
								leader = %invoice.uid.as_str(),
								members = rows.len() - 1,
								%code,
								"NAV has already processed a request under this batch's \
								 requestId; its status with NAV is unknown. Query the \
								 transaction before anything else, and do not storno it on the \
								 strength of this error"
							);
							// Every row stays open, not just the leader's: NAV may hold the whole
							// batch. NAV's own text, never the leader's uid — a batch spans
							// orgs, which is why `Nav::filing` scrubs `batch_uid`. Best-effort:
							// one failed write must not leave the rest with no `error_code`.
							for (sub_id, _) in &rows {
								if let Err(e) = nav.record_fault(*sub_id, &code, &message).await {
									tracing::error!(submission = *sub_id, error = %e,
										"could not record the reused-requestId fault; this row \
										 is invisible to awaiting_operator");
								}
							}
						}
						// The leader stays **open** rather than settled: no verdict NAV never gave
						// belongs in the archive, and `finish` stamps a `done_at` nothing clears.
						Disposition::NeedsPerson => {
							tracing::error!(
								submission = id,
								invoice = invoice.id,
								%code,
								%message,
								"NAV will never accept this invoice as it stands; the filing is \
								 parked for a person and no requestId was spent"
							);
							// Best-effort like every other write on this path: a failed release
							// must not discard `err`, which is what the retry classifies on.
							if let Err(e) =
								filing::abandon(app, nav, invoice.uid.as_str(), id).await
							{
								tracing::error!(submission = id, error = %e,
									"could not release the batch; its members stay claimed");
							}
						}
						// The spent `requestId` is the leader's alone, so every member is still
						// filable. Released *before* the leader is settled — the reverse order
						// strands them in a `FAILED` batch if this dies in between.
						Disposition::SpendsRequestId => {
							if let Err(e) =
								filing::abandon(app, nav, invoice.uid.as_str(), id).await
							{
								tracing::error!(submission = id, error = %e,
									"could not release the batch; its members stay claimed");
							}
							match filing::settle(
								nav,
								id,
								NavVerdict::Failed,
								Some((&code, &message)),
							)
							.await
							{
								Ok(false) => tracing::warn!(
									submission = id,
									invoice = invoice.id,
									"the filing already carries a verdict; the fault is the job \
									 row's only"
								),
								Err(e) => tracing::error!(submission = id, error = %e,
									"could not record the spent requestId; the resend will \
									 misreport this filing as possibly held by NAV"),
								Ok(true) => {}
							}
						}
						Disposition::Retry => {}
					}
				}
			}
			Err(err)
		}
	}
}

/// `nav_submissions.error_code` markers that are not HTTP errCodes: a reason recorded on a row
/// that is still open and still being polled, so `NavStore::awaiting_operator` and
/// `A-NAV-REJECTED` surface it at once. `NavStore::finish` clears both when NAV does answer.
pub const E_NAV_UNKNOWN_STATUS: &str = "E-NAV-UNKNOWN-STATUS";
pub const E_NAV_POLL_STALE: &str = "E-NAV-POLL-STALE";
pub const E_NAV_UNBUILDABLE: &str = "E-NAV-UNBUILDABLE";

/// Give a claimed member back to its own filing path because this attempt could not build its
/// `invoiceData`: park the reason so `awaiting_operator` shows it, clear `batch_uid`, revive its
/// `NAV_REPORT`. Best-effort like the transition helpers — the batch's other members must still
/// POST.
async fn release_unbuildable(
	app: &App,
	nav: &dyn NavStore,
	batch_uid: &str,
	submission_id: i64,
	invoice_id: i64,
	err: &Error,
) {
	filing::park(nav, submission_id, E_NAV_UNBUILDABLE, &err.to_string()).await;
	match nav.release_member(batch_uid, invoice_id).await {
		Ok(true) => refile_released(app, &[invoice_id]).await,
		Ok(false) => tracing::warn!(
			invoice = invoice_id,
			"this batch member is already at NAV; leaving it in the batch"
		),
		Err(e) => tracing::error!(invoice = invoice_id, error = %e,
			"could not release an unbuildable batch member; it will not be reported"),
	}
}

/// Record a marker on an open row, once, without settling it. Best-effort like [`archive`]:
/// the poll's own schedule must not depend on it.
async fn mark(nav: &dyn NavStore, row: &NavSubmission, code: &str, message: &str) {
	// Once: a re-mark every ten minutes would overwrite whatever reason the row already
	// carries, including a fault `report` recorded.
	if row.error_code.is_none() {
		filing::park(nav, row.id, code, message).await;
	}
}

/// The `idx`th `<invoiceOperation>` block of a built `ManageInvoiceRequest`, which is what a
/// batch member archives instead of the whole envelope.
///
/// Sliced on the `<index>` markers, not on the tag: `invoiceOperation` is nested inside itself
/// (`<invoiceOperation><index>1</index><invoiceOperation>CREATE</invoiceOperation>…`), so
/// matching the tag finds the operation string, not the block.
fn operation_slice(request: &str, idx: usize) -> Option<&str> {
	const OPEN: &str = "<invoiceOperation><index>";
	let from = request.find(&format!("{OPEN}{idx}</index>"))?;
	let rest = &request[from + OPEN.len()..];
	let to = rest
		.find(&format!("{OPEN}{}</index>", idx + 1))
		.or_else(|| rest.find("</invoiceOperations>"))
		.unwrap_or(rest.len());
	Some(&request[from..from + OPEN.len() + to])
}

/// The `NAV_POLL` payload. `job_cancel` addresses rows by `payload = ?` string equality, so a
/// respelling here silently cancels nothing — one owner of the spelling.
#[must_use]
pub fn poll_payload(submission_id: i64) -> String {
	format!(r#"{{"submissionId":{submission_id}}}"#)
}

pub(crate) async fn enqueue_poll(
	app: &App,
	submission_id: i64,
	transaction_id: &str,
) -> ClResult<()> {
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
	// The key is spent, so a `FAILED` poll is a filing NAV holds that nothing else can ask
	// about. Only through the **leader's** invoice: `job_redrive` matches the leader
	// submission's payload, so a member cannot revive the chain its own filing rides on.
	if app.store.job_redrive(KIND_NAV_POLL, &payload, Timestamp::now()).await? > 0 {
		tracing::warn!(submission = submission_id, "revived a stranded NAV poll");
	}
	Ok(())
}

/// Put every released batch member back on its own filing path.
///
/// Releasing the row is not enough: a member stood down inside [`filing::may_send`] and returned `Ok`,
/// so the runner marked its `NAV_REPORT` `DONE` and `nav:invoice:{id}` is spent — the sweep's
/// re-enqueue is a no-op and `Nav::submit` answers `E-NAV-NOT-REDRIVABLE`. Enqueue first, then
/// re-drive the spent key, exactly as [`enqueue_reconcile`] does.
///
/// Best-effort and never propagating: the caller is on a terminal-fault path with an error in
/// hand that must survive, and a member left behind is an `awaiting_operator` problem rather
/// than a reason to lose the leader's verdict.
pub(crate) async fn refile_released(app: &App, invoice_ids: &[i64]) {
	for &id in invoice_ids {
		let payload = mintworks_invoice::invoice_job_payload(id);
		let key = format!("nav:invoice:{id}");
		let now = Timestamp::now();
		if let Err(e) = async {
			if job::enqueue(&app.store, KIND_NAV_REPORT, &payload, Some(&key), now)
				.await?
				.is_some()
			{
				return Ok(0);
			}
			// `FAILED` first, then the `DONE` row `may_send`'s stand-down leaves. Re-running a
			// `DONE` filing is only safe because this member was just released: `release_batch`
			// touches no row carrying a `transaction_id` or a verdict, so nothing was filed.
			let moved = app.store.job_redrive(KIND_NAV_REPORT, &payload, now).await?;
			if moved > 0 {
				return Ok(moved);
			}
			let moved = app.store.job_redrive_done(&key, &payload, now).await?;
			if moved == 0 {
				tracing::error!(
					invoice = id,
					"a released batch member has no filing job to revive; it will not be reported"
				);
			}
			Ok::<u64, Error>(moved)
		}
		.await
		{
			tracing::error!(invoice = id, error = %e,
				"could not put a released batch member back on its own filing job");
		}
	}
}

/// The `NAV_RECONCILE` payload. `job_cancel` and `job_redrive` address rows by `payload = ?`
/// string equality, so a respelling here silently matches nothing — one owner of the spelling.
#[must_use]
pub fn reconcile_payload(batch_uid: &str) -> String {
	format!(r#"{{"batchUid":"{batch_uid}"}}"#)
}

async fn enqueue_reconcile(app: &App, nav: &dyn NavStore, batch_uid: &str) -> ClResult<()> {
	let payload = reconcile_payload(batch_uid);
	let run_at = Timestamp(Timestamp::now().0 + RECONCILE_DELAY_SECS);
	let key = format!("nav:reconcile:{batch_uid}");
	if job::enqueue(&app.store, KIND_NAV_RECONCILE, &payload, Some(&key), run_at)
		.await?
		.is_some()
	{
		return Ok(());
	}
	// The key is spent: this batch lost a reply before. The earlier reconciliation either
	// resolved it — and this one stands down on the `transaction_id` it wrote — or is still
	// pending, in which case the re-drive is a no-op.
	if app.store.job_redrive(KIND_NAV_RECONCILE, &payload, run_at).await? > 0 {
		tracing::warn!(leader = batch_uid, "revived a stranded NAV reconciliation");
		return Ok(());
	}
	// A reconciliation legitimately finishes `DONE` having settled nothing, which leaves a
	// second lost reply on the same batch no recovery path at all. Only while the rows still
	// prove nothing was settled: a resolved batch must never be re-reconciled.
	let unsettled = nav
		.submissions_by_batch(batch_uid)
		.await?
		.iter()
		.all(|s| s.transaction_id.is_none() && s.verdict.is_none());
	if unsettled && app.store.job_redrive_done(&key, &payload, run_at).await? > 0 {
		tracing::warn!(leader = batch_uid, "re-ran a NAV reconciliation that settled nothing");
	}
	Ok(())
}

/// §1.9.2 lost-transaction recovery: a `manageInvoice` that got no answer, so nobody knows
/// whether NAV holds the batch. Without it, one timeout strands `nav.batch_max` invoices at
/// `E-NAV-REQUEST-ID-REUSED` with no automated way to ask NAV what landed.
///
/// The `NAV_RECONCILE` handler, public for the same reason [`report`] is: `Runner::tick` is
/// private, so a test drives it directly.
///
/// Settles nothing it cannot prove. NAV's transaction list carries no `requestId`, so a
/// transaction is claimed only when **every** member's invoice number appears in the invoice
/// data it echoes back; a partial match is an operator's problem, not a verdict.
pub async fn reconcile(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	batch_uid: &str,
) -> ClResult<()> {
	let rows = nav.submissions_by_batch(batch_uid).await?;
	if rows.is_empty() {
		return Ok(());
	}
	if rows.iter().any(|r| r.transaction_id.is_some() || r.verdict.is_some()) {
		tracing::info!(leader = batch_uid, "the batch resolved itself while reconciliation waited");
		return Ok(());
	}

	// `invoices.number` is what NAV echoes back, and the leader is the row whose invoice owns
	// the `batch_uid` — the rows themselves only carry ids.
	let mut members: Vec<(i64, i64, String)> = Vec::with_capacity(rows.len());
	let mut leader: Option<(i64, i64, i64)> = None;
	for row in &rows {
		let invoice = invoices.invoice_by_id(row.invoice_id).await?.ok_or_else(|| {
			Error::internal(format!("mintworks-nav: invoice {} is gone", row.invoice_id))
		})?;
		let number = invoice
			.number
			.ok_or_else(|| Error::internal("mintworks-nav: a filed invoice has no number"))?;
		if invoice.uid.as_str() == batch_uid {
			leader = Some((row.id, row.invoice_id, invoice.seller_id));
		}
		members.push((row.id, row.invoice_id, number));
	}
	let Some((leader_sub, leader_invoice, seller_id)) = leader else {
		tracing::error!(leader = batch_uid, "this batch has no leader row; leaving it open");
		return Ok(());
	};
	let (seller, current) = tokio::try_join!(
		invoices.seller_by_id(seller_id),
		invoices.current_seller_version(seller_id),
	)?;
	let seller =
		seller.ok_or_else(|| Error::internal("mintworks-nav: the invoice's seller is gone"))?;
	let current = current
		.ok_or_else(|| Error::internal("mintworks-nav: the seller has no published version"))?;
	let client = NavAuth::load(app, &seller, &current).await?;

	// Every transaction of this technical user in the window `nav_submissions` never heard of.
	// Anchored to the claim, not to now: a reconciliation delayed by a restart or its own backoff
	// queried a window the transaction had already fallen out of, and resent under a burned id.
	let opened = rows.iter().map(|r| r.created_at.0).min().unwrap_or_else(|| Timestamp::now().0);
	let (from_at, to_at) = reconcile_window(opened, Timestamp::now().0);
	let stamp = |t: i64| {
		auth::stamps(
			OffsetDateTime::from_unix_timestamp(t).unwrap_or_else(|_| OffsetDateTime::now_utc()),
		)
		.0
	};
	let (from, to) = (stamp(from_at), stamp(to_at));
	let mut unknown: Vec<String> = Vec::new();
	let mut page = 1i64;
	loop {
		let request = client.query_transaction_list_request(page, &from, &to);
		let (_, reply) = client
			.post("queryTransactionList", &request)
			.await?
			.body("queryTransactionList")?;
		let (ids, available) = client::transaction_list(&Reply::parse(&reply))?;
		for id in ids {
			if nav.submissions_by_transaction(&id).await?.is_empty() {
				unknown.push(id);
			}
		}
		if page >= available {
			break;
		}
		page += 1;
	}

	for transaction_id in &unknown {
		let request = client.query_status_original_request(transaction_id);
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
		if !filed.iter().any(|(_, number)| members.iter().any(|(.., ours)| ours == number)) {
			continue;
		}
		// Ours by one number, so every member must be in it. A transaction carrying part of
		// this batch is something NAV should never produce, and guessing which half landed is
		// how an invoice gets filed twice.
		let mut sent = Vec::with_capacity(members.len());
		for (sub_id, invoice_id, number) in &members {
			let Some((idx, _)) = filed.iter().find(|(_, filed)| filed == number) else {
				tracing::error!(
					leader = batch_uid,
					transaction = %transaction_id,
					invoice = invoice_id,
					"NAV holds a transaction carrying only part of this batch; settling nothing"
				);
				return Ok(());
			};
			sent.push((*sub_id, *idx));
		}
		job::thrice(|| nav.set_sent_batch(&sent, transaction_id))
			.await
			.map_err(|last| {
				last.unwrap_or_else(|| Error::internal("mintworks-nav: set_sent_batch did not run"))
			})?;
		tracing::warn!(
			leader = batch_uid,
			transaction = %transaction_id,
			invoices = members.len(),
			"recovered a lost manageInvoice reply; the batch is on the ordinary poll path"
		);
		return enqueue_poll(app, leader_sub, transaction_id).await;
	}

	// No unknown transaction in the window, so NAV never took the submission and §1.9.2 requires
	// repeating it immediately. Same `batch_uid`, so the same `requestId` and members: if this
	// conclusion is ever wrong NAV refuses the resend rather than filing twice.
	let payload = invoice_job_payload(leader_invoice);
	if app.store.job_redrive(KIND_NAV_REPORT, &payload, Timestamp::now()).await? == 0 {
		// `job_redrive` matches `FAILED` only, and `jobs.max_attempts.NAV_REPORT = 0` means the
		// leader's job is normally still PENDING on its own backoff — it resends the same batch
		// without help. Nothing to recover, so the reconciliation is done either way.
		tracing::info!(leader = batch_uid, "NAV never took this batch; its filing job resends it");
	}
	Ok(())
}

/// One `queryTransactionStatus` round. [`Next::Done`] only when NAV has reached a verdict for
/// every row of the transaction; [`Next::Again`] while it has not, which is a success carrying
/// its own next run. An `Err` is reserved for a round that genuinely failed.
///
/// The `NAV_POLL` handler, public for the same reason [`report`] is: `Runner::tick` is
/// private, so a test drives the pair directly.
pub async fn poll(
	app: &App,
	invoices: &dyn InvoiceStore,
	nav: &dyn NavStore,
	job: &Job,
	submission_id: i64,
) -> ClResult<Next> {
	let sub = nav.submission(submission_id).await?.ok_or_else(|| {
		Error::internal(format!("mintworks-nav: submission {submission_id} is gone"))
	})?;
	let Some(transaction_id) = sub.transaction_id.clone() else {
		// Settled before it ever reached NAV: there is nothing to ask about.
		if sub.verdict.is_some() {
			return Ok(Next::Done);
		}
		return Err(Error::internal("mintworks-nav: polling a submission with no transactionId"));
	};
	// The poll gates on the whole transaction, not on the row whose job is running. The
	// leader's verdict alone spent `nav:poll:{txid}` and left every other row with a
	// transactionId and no verdict — the silently unreported filing this crate exists to stop.
	let batch = nav.submissions_by_transaction(&transaction_id).await?;
	if batch.iter().all(|r| r.verdict.is_some()) {
		return Ok(Next::Done);
	}

	let invoice = invoices
		.invoice_by_id(sub.invoice_id)
		.await?
		.ok_or_else(|| Error::internal("mintworks-nav: the submission's invoice is gone"))?;
	let (seller, current) = tokio::try_join!(
		invoices.seller_by_id(invoice.seller_id),
		invoices.current_seller_version(invoice.seller_id),
	)?;
	let seller =
		seller.ok_or_else(|| Error::internal("mintworks-nav: the invoice's seller is gone"))?;
	let current = current
		.ok_or_else(|| Error::internal("mintworks-nav: the seller has no published version"))?;

	let client = NavAuth::load(app, &seller, &current).await?;
	let (_, reply) = client
		.post("queryTransactionStatus", &client.query_status_request(&transaction_id))
		.await?
		.body("queryTransactionStatus")?;
	filing::archive_reply(nav, sub.id, sub.invoice_id, &auth::redact(&reply)).await;

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

	// `A-JOB-STALE` no longer matches a deferring poll (`Next::Again` clears `last_error`), so
	// the "a human is told after a day" guarantee is kept here instead, off the same setting.
	let stale_after = app.settings.int("jobs.alert_after.NAV_POLL").await?;
	// The runner's own backoff formula to the second: this cap plus the id-keyed jitter.
	let cap = app.settings.int("jobs.backoff_cap.NAV_POLL").await?;

	let mut pending = 0usize;
	for row in &batch {
		// A row that already carries a verdict is settled — see `NavStore::finish`. The gate
		// above catches the ordinary case; this catches a row another runner settled while
		// this poll was in flight.
		if row.verdict.is_some() {
			continue;
		}
		let hit = match row.idx {
			Some(idx) => results.iter().find(|(i, ..)| *i == idx),
			// A row written before batching carries no `idx`; its reply holds one result.
			None => results.first().filter(|_| batch.len() == 1),
		};
		let Some((_, outcome, subtree)) = hit else {
			// Nothing is settled by omission: an index the reply does not mention is pending.
			tracing::warn!(
				submission = row.id,
				idx = row.idx.unwrap_or(0),
				"NAV's reply carries no result for this row; leaving it open"
			);
			pending += 1;
			continue;
		};
		// The polled row is the leader's — `enqueue_poll` runs once, from the leader — so it
		// keeps the whole reply, archived above, and each member keeps its own result, for the
		// same reason the request side splits.
		if row.id != sub.id {
			filing::archive_reply(nav, row.id, row.invoice_id, &auth::redact(subtree)).await;
		}
		let settled = match outcome {
			Outcome::Done => filing::settle(nav, row.id, NavVerdict::Done, None).await?,
			Outcome::Warn => filing::settle(nav, row.id, NavVerdict::Warn, None).await?,
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
				filing::settle(
					nav,
					row.id,
					NavVerdict::Rejected,
					Some((code.as_str(), message.as_str())),
				)
				.await?
			}
			// Never settled: an unknown status may be one NAV has just added, and terminating a
			// statutory filing on it is worse than polling on. The marker is what stops it being
			// silent, and `finish` clears it if NAV does answer.
			Outcome::Unknown { status } => {
				mark(
					nav,
					row,
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
				if Timestamp::now().0 - row.created_at.0 > stale_after {
					mark(
						nav,
						row,
						E_NAV_POLL_STALE,
						"NAV has not answered about this invoice within \
						 jobs.alert_after.NAV_POLL; the poll continues",
					)
					.await;
				}
				pending += 1;
				continue;
			}
		};
		if !settled {
			tracing::warn!(submission = row.id, "the filing was settled while this poll ran");
		}
	}
	if pending > 0 {
		let base = job::backoff_secs(job.attempts, cap);
		let at = Timestamp(Timestamp::now().0 + base + job.id.rem_euclid(base / 4 + 1));
		tracing::debug!(
			submission = sub.id,
			pending,
			of = batch.len(),
			at = at.0,
			"NAV is still processing this transaction; asking again"
		);
		return Ok(Next::Again { at });
	}
	Ok(Next::Done)
}

#[cfg(test)]
mod tests {
	use super::{RECONCILE_WINDOW_SECS, operation_slice, reconcile_window};

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
}

// vim: ts=4
