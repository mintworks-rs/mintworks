// SPDX-License-Identifier: MPL-2.0
//! `Nav` — the service handle a consumer application drives NAV reporting through.
//!
//! Every method takes `&Ctx` first and derives its permission from `ctx.actor`, matching
//! `Auth` and `Invoices`. Two route bundles cover HTTP: [`crate::routes::org_credentials`] is a
//! tenant's NAV connection and filing retry, and [`crate::routes::operator`] the operator's
//! filing queue.
//!
//! The handle also carries the audit writes NAV never had. `nav_submissions` is a statutory
//! archive but it produced no `audit_logs` row at all — only [`crate::export`] audited — so
//! filing an invoice with the tax authority left no trace of *who* asked for it.

use std::collections::HashMap;
use std::sync::Arc;

use mintworks_core::alert::{Alert, Severity};
use mintworks_core::ids::NavBatchId;
use mintworks_core::job::{Job, Next};
use mintworks_core::secrets::SecretStatus;
use mintworks_core::store::Role;
use mintworks_core::{App, audit, auth_mw, ctx::Actor, ctx::Ctx, error::StatusCode, prelude::*};
use mintworks_invoice::store::{InvoiceKind, InvoiceStatus};
use mintworks_invoice::{Invoice, InvoiceStore, invoice_store};
use serde::{Deserialize, Serialize};

use crate::auth::NavAuth;
use crate::client::{Taxpayer, original_invoice_numbers};
use crate::export::{Selection, original_number};
use crate::filing;
use crate::reply::Reply;
use crate::store::{NavStore, store as nav_store};
use crate::submission::{
	BatchState, FilingState, NavArchive, NavBatch, NavFiling, NavOp, NavSubmission, NavVerdict,
	RowState,
};
use crate::xml::invoice_data;

/// Invoices per batched read in [`Nav::audit_export`]. Well under SQLite's 32 766 variable
/// cap, which an unchunked year of invoices would pass.
const EXPORT_CHUNK: usize = 1000;

/// Rows keyed by whatever `key` reads off them, order preserved within each group.
pub(crate) fn group_by<T>(rows: Vec<T>, key: fn(&T) -> i64) -> HashMap<i64, Vec<T>> {
	let mut out: HashMap<i64, Vec<T>> = HashMap::new();
	for row in rows {
		out.entry(key(&row)).or_default().push(row);
	}
	out
}

/// [`Nav::cancel_filing`] on a filing NAV may hold: its batch is past `CLAIMED`, or reached
/// `POSTING` while the cancel was on its way.
pub const E_NAV_FILING_IN_FLIGHT: &str = "E-NAV-FILING-IN-FLIGHT";

/// A filing whose state refuses the act: [`Nav::resolve_filing`] on anything but an unresolved
/// `REJECTED`/`FAILED`, [`Nav::cancel_filing`] on a finished filing, attach or confirm on anything
/// but `HELD`, or attach to a transaction a batch already carries.
pub const E_NAV_SUBMISSION_STATE: &str = "E-NAV-SUBMISSION-STATE";

/// [`Nav::retry`] on a filing a tenant cannot move on: accepted, cancelled by an operator, one NAV
/// may hold, test mode, or not issued.
pub const E_NAV_NOT_RETRYABLE: &str = "E-NAV-NOT-RETRYABLE";

/// [`Nav::attach_transaction`] on a transaction that does not carry this invoice's number.
pub const E_NAV_ATTACH_MISMATCH: &str = "E-NAV-ATTACH-MISMATCH";

/// `error_code` on a row [`Nav::confirm_not_filed`] ended: a person's finding, not NAV's.
pub const E_NAV_CONFIRMED_NOT_FILED: &str = "E-NAV-CONFIRMED-NOT-FILED";

/// Most filings [`Nav::list_filings`] reads at once.
const MAX_FILINGS_PAGE: i64 = 200;

/// [`Nav::list_filings`]'s page when the caller names no `limit`.
const DEFAULT_FILINGS_PAGE: i64 = 50;

/// One page of [`Nav::list_filings`]. `next_cursor` goes back as `after`, and is `None` only on
/// an empty page: `limit` is clamped, so a short page proves nothing.
#[derive(Clone, Debug)]
pub struct FilingPage {
	pub items: Vec<(InvoiceId, NavFiling)>,
	pub next_cursor: Option<String>,
}

/// Absent is every state; an unknown tag is refused, or a typo would answer with everything.
fn filing_states(state: Option<&str>) -> ClResult<Vec<FilingState>> {
	match state {
		None => Ok(Vec::new()),
		Some("open") => Ok(FilingState::OPEN.to_vec()),
		Some(tag) => Ok(vec![tag.parse().map_err(|_| Error::validation("unknown filing state"))?]),
	}
}

fn not_retryable(msg: &str) -> Error {
	Error::coded(StatusCode::CONFLICT, E_NAV_NOT_RETRYABLE, msg)
}

fn state_refused(msg: &str) -> Error {
	Error::coded(StatusCode::CONFLICT, E_NAV_SUBMISSION_STATE, msg)
}

fn in_flight(msg: &str) -> Error {
	Error::coded(StatusCode::CONFLICT, E_NAV_FILING_IN_FLIGHT, msg)
}

fn not_issued() -> Error {
	Error::coded(
		StatusCode::BAD_REQUEST,
		"E-NAV-NOT-ISSUED",
		"only an issued invoice can be filed; a draft has no number to file",
	)
}

fn issued(invoice: &Invoice) -> bool {
	matches!(invoice.status, InvoiceStatus::Issued | InvoiceStatus::Paid | InvoiceStatus::Stornoed)
}

fn state_of(filing: Option<&NavFiling>) -> FilingState {
	filing.map_or(FilingState::None, NavFiling::state)
}

/// The row of a state that has one: every state but `NONE`.
fn row_of(filing: Option<&NavFiling>) -> ClResult<&NavSubmission> {
	filing
		.map(|f| &f.row)
		.ok_or_else(|| Error::internal("mintworks-nav: a filing has no row"))
}

/// The batch of a state only a batch can be in: `CLAIMED`, `IN_DOUBT`, `SENT`.
fn batch_of(filing: Option<&NavFiling>) -> ClResult<&NavBatch> {
	filing
		.and_then(|f| f.batch.as_ref())
		.ok_or_else(|| Error::internal("mintworks-nav: a filing in a batch state has no batch"))
}

/// A tenant seller's NAV technical user, as typed. No `Debug`: three of the four are secrets.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavCredentials {
	pub login: String,
	pub tech_password: String,
	pub sign_key: String,
	pub exchange_key: String,
}

/// A standing seller hold, as [`NavCredentialsStatus`] reports it.
#[derive(Debug, Serialize)]
pub struct NavHeld {
	pub code: String,
	pub since: Timestamp,
}

/// What the acting org may know about its seller's NAV connection. Never a secret value.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NavCredentialsStatus {
	pub login: Option<String>,
	pub connected: bool,
	/// Invoices not yet on the wire, the backlog connecting releases: no row at all, or a row in
	/// a batch nothing has posted.
	pub unreported: i64,
	/// The seller's oldest open batch fault is NAV refusing the stored credentials, so `connected`
	/// is true and nothing files until the tenant reconnects.
	pub credentials_refused: bool,
	/// NAV spent a `requestId` of this seller's, so nothing of it reaches NAV until the tenant
	/// reconnects or an operator releases the hold.
	pub held: Option<NavHeld>,
	pub tech_password: SecretStatus,
	pub sign_key: SecretStatus,
	pub exchange_key: SecretStatus,
}

#[derive(Clone)]
#[allow(clippy::struct_field_names)] // `nav` names the NavStore, not the struct
pub struct Nav {
	app: App,
	/// Both stores resolved once here, as `Invoices` does: `App` is immutable after `build()`,
	/// so the extension map cannot answer differently later.
	nav: Option<Arc<dyn NavStore>>,
	invoices: Option<Arc<dyn InvoiceStore>>,
}

impl Nav {
	pub fn new(app: App) -> Self {
		let nav = nav_store(&app).ok();
		let invoices = invoice_store(&app).ok();
		Self { app, nav, invoices }
	}

	fn nav(&self) -> ClResult<Arc<dyn NavStore>> {
		self.nav
			.clone()
			.ok_or_else(|| Error::internal("mintworks-nav: no NavStore was registered on the app"))
	}

	fn invoices(&self) -> ClResult<Arc<dyn InvoiceStore>> {
		self.invoices.clone().ok_or_else(|| {
			Error::internal("mintworks-nav: no InvoiceStore was registered on the app")
		})
	}

	/// Org-scoped for a `User`, a `Key` and a `Public`, so another org's `inv_` id reads as
	/// absent rather than as a `403`. Only an `Operator` or `System` caller with no org
	/// chosen reads unscoped — lumping `Public` in with them gave a `Ctx` carrying no org
	/// an unscoped read of any org's invoice by uid. `lookup_tax_number` below refuses
	/// `Public` outright; this is the same line drawn where a scope is what is needed.
	async fn invoice(&self, ctx: &Ctx, uid: &str) -> ClResult<Invoice> {
		let uid = InvoiceId::parse(uid)?;
		let scope = match ctx.actor {
			Actor::User { .. } | Actor::Key { .. } | Actor::Public { .. } => Some(ctx.org()?),
			_ => ctx.org_id,
		};
		self.invoices()?.invoice_by_uid(scope, &uid).await?.ok_or(Error::NotFound)
	}

	/// An operator method's lookup, always after `require_operator`: an operator is not confined
	/// to a selected org, and [`Nav::list_filings`] hands out every seller's uids. Never call this
	/// from a method a tenant reaches; those go through [`Nav::invoice`].
	async fn any_invoice(&self, uid: &str) -> ClResult<Invoice> {
		let uid = InvoiceId::parse(uid)?;
		self.invoices()?.invoice_by_uid(None, &uid).await?.ok_or(Error::NotFound)
	}

	/// An operator's push on this invoice's filing, dispatching on [`FilingState`] per design §12.
	///
	/// * `NONE`, and `CANCELLED` after R8 deletes the row: the invoice's `NAV_REPORT`.
	/// * `CLAIMED`, `IN_DOUBT`, `SENT`: the batch's `NAV_SEND`, `NAV_RECONCILE` or `NAV_POLL`.
	/// * `REJECTED`, `FAILED`: R7, a new row in a new batch, once [`Nav::resolve_filing`] settled
	///   the old one.
	///
	/// `E-NAV-NOT-REDRIVABLE` for the rest: `POSTING` is in a job's hands, `HELD` is
	/// [`Nav::attach_transaction`] or [`Nav::confirm_not_filed`], `ACCEPTED` is finished.
	pub async fn submit(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<()> {
		auth_mw::require_operator(&self.app, ctx).await?;
		// Filing a statutory return is the same class of act as `Invoices::issue`, which gates on
		// step-up too. In the service, not the handler: the guard belongs to the operation.
		auth_mw::require_stepup(&self.app, ctx).await?;
		let invoice = self.any_invoice(invoice_uid).await?;
		// A draft has no number, so the job fails terminally on attempt one — **after** spending
		// `nav:invoice:{id}`, the key `InvoiceStore::issue` needs at real issue time.
		if !issued(&invoice) {
			return Err(not_issued());
		}
		let refuse = |msg: &str| Error::coded(StatusCode::CONFLICT, "E-NAV-NOT-REDRIVABLE", msg);
		let nav = self.nav()?;
		let found = nav.filing_by_invoice(invoice.id).await?;
		let current = found.as_ref();
		let state = state_of(current);
		let target = match state {
			FilingState::None => {
				filing::ensure_report(&self.app, invoice.id, true).await?;
				"report"
			}
			FilingState::Claimed => {
				filing::ensure_send(&self.app, batch_of(current)?.uid.as_str(), true).await?;
				"send"
			}
			FilingState::InDoubt => {
				let batch = batch_of(current)?;
				// §1.9.2's five minutes run from the POST: a person's push does not shorten them.
				let at = crate::job::reconcile_at(batch.posted_at.unwrap_or_else(Timestamp::now));
				filing::ensure_reconcile(&self.app, batch.uid.as_str(), at, true).await?;
				"reconcile"
			}
			FilingState::Sent => {
				filing::ensure_poll(&self.app, batch_of(current)?.uid.as_str(), true).await?;
				"poll"
			}
			FilingState::Rejected | FilingState::Failed => {
				let row = row_of(current)?;
				if row.resolved_at.is_none() {
					return Err(refuse("settle this filing with resolve_filing first"));
				}
				if !self.refile_solo(row.id).await? {
					return Err(refuse("this filing is already being refiled"));
				}
				"refile"
			}
			FilingState::Cancelled => {
				if !nav.reopen_cancelled(row_of(current)?.id).await? {
					return Err(refuse("this filing is already being reopened"));
				}
				filing::ensure_report(&self.app, invoice.id, true).await?;
				"report"
			}
			FilingState::Posting => return Err(refuse("a job is posting this filing right now")),
			FilingState::Held => {
				return Err(refuse(
					"NAV may hold this filing: use attach_transaction or confirm_not_filed",
				));
			}
			FilingState::Accepted => return Err(refuse("NAV has accepted this invoice")),
		};
		self.audit_submit(ctx, invoice_uid, state != FilingState::None, target).await
	}

	/// `try_log`, not `log`: this row is the record of who ordered a statutory filing, and the
	/// write is past the enqueue's commit, so a lost one can never be recovered.
	async fn audit_submit(
		&self,
		ctx: &Ctx,
		invoice_uid: &str,
		redrive: bool,
		target: &str,
	) -> ClResult<()> {
		audit::try_log(
			&self.app.store,
			ctx,
			"nav_submission",
			Some(invoice_uid),
			"SUBMIT",
			Some(serde_json::json!({ "redrive": redrive, "target": target })),
		)
		.await
	}

	/// R7 on one row, then the new batch's `NAV_SEND`. `false` when the store refused: a concurrent
	/// refile got there first.
	async fn refile_solo(&self, row_id: i64) -> ClResult<bool> {
		let Some(batch) = self.nav()?.refile(&[row_id], None, Timestamp::now()).await? else {
			return Ok(false);
		};
		filing::ensure_send(&self.app, batch.uid.as_str(), true).await?;
		Ok(true)
	}

	/// End a filing NAV cannot hold yet (R6): the invoice gets a `CANCELLED` row, which refuses
	/// `claim_batch`, and leaves its batch if it had one. Nothing else in that batch moves.
	/// [`Nav::submit`] reopens it. Operator only, step-up gated.
	///
	/// NAV retry is unbounded — giving up on a statutory filing is not a recovery — so an invoice
	/// whose XML NAV will never accept would otherwise retry forever with no way to stop it short
	/// of raw SQL.
	///
	/// # Errors
	/// `E-NAV-NOT-ISSUED` on a draft, whose `CANCELLED` row would silently keep it from ever being
	/// filed once issued. `E-NAV-FILING-IN-FLIGHT` (409) once the batch is past `CLAIMED`, and on
	/// losing the race with `begin_post`. `E-NAV-SUBMISSION-STATE` (409) on a finished filing.
	pub async fn cancel_filing(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<()> {
		auth_mw::require_operator(&self.app, ctx).await?;
		auth_mw::require_stepup(&self.app, ctx).await?;
		let invoice = self.any_invoice(invoice_uid).await?;
		if !issued(&invoice) {
			return Err(not_issued());
		}
		let nav = self.nav()?;
		let state = state_of(nav.filing_by_invoice(invoice.id).await?.as_ref());
		match state {
			FilingState::None | FilingState::Claimed => {}
			FilingState::Posting | FilingState::InDoubt | FilingState::Held | FilingState::Sent => {
				return Err(in_flight("NAV may hold this filing"));
			}
			FilingState::Accepted
			| FilingState::Rejected
			| FilingState::Failed
			| FilingState::Cancelled => {
				return Err(state_refused("this filing has already ended"));
			}
		}
		let op = match invoice.kind {
			InvoiceKind::Storno => NavOp::Storno,
			InvoiceKind::Normal => NavOp::Create,
		};
		if !nav.cancel_filing(invoice.id, op, Timestamp::now()).await? {
			return Err(in_flight("this filing moved on while it was being cancelled"));
		}
		self.audit(ctx, invoice_uid, "CANCEL", serde_json::json!({ "state": state.as_str() }))
			.await;
		Ok(())
	}

	/// Record that an operator has dealt with a `REJECTED` or `FAILED` filing, so `A-NAV-REJECTED`
	/// and the hourly sweep stop counting this invoice. Operator only, step-up gated.
	///
	/// The remedy the alert recommends — correct and re-issue — produces a *new* invoice, so
	/// without this the old row alarms forever.
	///
	/// It settles the alarm, not the invoice: the verdict and the archive stand. Re-filing is
	/// [`Nav::submit`]'s R7, a separate deliberate act.
	///
	/// `note` is free text for the audit row: what the person actually did.
	///
	/// # Errors
	/// `E-CORE-NOTFOUND` when the invoice or its filing record is absent, `E-NAV-SUBMISSION-STATE`
	/// (409) in any other state, or when it was already resolved.
	pub async fn resolve_filing(&self, ctx: &Ctx, invoice_uid: &str, note: &str) -> ClResult<()> {
		auth_mw::require_operator(&self.app, ctx).await?;
		auth_mw::require_stepup(&self.app, ctx).await?;
		let invoice = self.any_invoice(invoice_uid).await?;
		let nav = self.nav()?;
		let current = nav.filing_by_invoice(invoice.id).await?.ok_or(Error::NotFound)?;
		let row = &current.row;
		let settled = "this filing is not waiting on a person, or it was already resolved";
		if !matches!(current.state(), FilingState::Rejected | FilingState::Failed)
			|| !nav.resolve(row.id, Timestamp::now()).await?
		{
			return Err(state_refused(settled));
		}
		self.audit(
			ctx,
			invoice_uid,
			"RESOLVE",
			serde_json::json!({
				"submission": row.id,
				"verdict": row.verdict.map(NavVerdict::as_str),
				"errorCode": row.error_code,
				"note": note,
			}),
		)
		.await;
		Ok(())
	}

	/// Retry this invoice's NAV filing — the tenant's counterpart of [`Nav::submit`]. Admin on the
	/// seller's org, step-up gated. Returns [`Nav::filing`] afterwards.
	///
	/// * `NONE`, `CLAIMED`, `SENT`: the job that owes the next step, revived if it died. A live job
	///   keeps its schedule, so a tenant's click defeats no backoff or deferral.
	/// * `REJECTED`, `FAILED`: R7, a new row in a new batch under a new `requestId`; the old row's
	///   verdict, error and archive stand. `ABORTED` does not consume the number at NAV, and our
	///   mapping, NAV's rules or the taxpayer registry may have changed since.
	///
	/// `E-NAV-NOT-RETRYABLE` for the rest, and on a test-mode or unissued invoice: `POSTING`,
	/// `IN_DOUBT` and `HELD` are not a tenant's to move, `ACCEPTED` is finished, and an operator
	/// cancelled a `CANCELLED` one.
	pub async fn retry(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<Option<NavFiling>> {
		let invoice = self.invoice(ctx, invoice_uid).await?;
		// The seller's org, as `Invoices::issue`/`storno` gate: the filing is the seller's.
		let seller =
			self.invoices()?.seller_by_id(invoice.seller_id).await?.ok_or(Error::NotFound)?;
		auth_mw::require_role_on(&self.app, ctx, seller.org_id, Role::Admin).await?;
		auth_mw::require_stepup(&self.app, ctx).await?;
		// Test-mode invoices are never reported to NAV.
		if invoice.test_mode {
			return Err(not_retryable("a test-mode invoice is never reported to NAV"));
		}
		if !issued(&invoice) {
			return Err(not_retryable("only an issued invoice is filed with NAV"));
		}
		let found = self.nav()?.filing_by_invoice(invoice.id).await?;
		let current = found.as_ref();
		let detail = match state_of(current) {
			FilingState::None => {
				filing::ensure_report(&self.app, invoice.id, false).await?;
				serde_json::json!({ "target": "report" })
			}
			FilingState::Claimed => {
				filing::ensure_send(&self.app, batch_of(current)?.uid.as_str(), false).await?;
				serde_json::json!({ "target": "send" })
			}
			FilingState::Sent => {
				filing::ensure_poll(&self.app, batch_of(current)?.uid.as_str(), false).await?;
				serde_json::json!({ "target": "poll" })
			}
			FilingState::Rejected | FilingState::Failed => {
				let row = row_of(current)?;
				if !self.refile_solo(row.id).await? {
					return Err(not_retryable("this filing is already being retried"));
				}
				serde_json::json!({
					"target": "refile",
					"submission": row.id,
					"verdict": row.verdict.map(NavVerdict::as_str),
					"errorCode": row.error_code,
					"note": "tenant retry",
				})
			}
			FilingState::Accepted => return Err(not_retryable("NAV has accepted this invoice")),
			FilingState::Cancelled => {
				return Err(not_retryable("an operator cancelled this filing"));
			}
			FilingState::Posting | FilingState::InDoubt | FilingState::Held => {
				return Err(not_retryable("NAV may hold this filing, so it cannot be retried"));
			}
		};
		audit::try_log(
			&self.app.store,
			ctx,
			"nav_submission",
			Some(invoice_uid),
			"RETRY",
			Some(detail),
		)
		.await?;
		self.filing(ctx, invoice_uid).await
	}

	/// This filing's `HELD` filings, batch-mates included: rows `HELD`, and rows `ACTIVE` in a
	/// `HELD` batch.
	async fn held_rows(&self, current: &NavFiling) -> ClResult<Vec<NavSubmission>> {
		let Some(batch) = &current.batch else {
			return Ok(vec![current.row.clone()]);
		};
		let mut rows = self.nav()?.batch_rows(batch.id).await?;
		rows.retain(|r| {
			r.state == RowState::Held
				|| (r.state == RowState::Active && batch.state == BatchState::Held)
		});
		Ok(rows)
	}

	/// The invoice's filing, refused unless it is `HELD`, the one state a person ends.
	async fn held_filing(&self, invoice: &Invoice) -> ClResult<NavFiling> {
		self.nav()?
			.filing_by_invoice(invoice.id)
			.await?
			.filter(|f| f.state() == FilingState::Held)
			.ok_or_else(|| state_refused("this filing is not held for a person"))
	}

	/// R4: a person names the transaction NAV holds a `HELD` filing under. Operator, step-up,
	/// audited.
	///
	/// NAV is asked first, with `returnOriginalRequest`. Every `HELD` filing of the batch whose
	/// number NAV returned joins a new `SENT` batch under NAV's index, and its `NAV_POLL` follows;
	/// the rest stay `HELD`, which is what a partial transaction leaves.
	///
	/// # Errors
	/// `E-NAV-ATTACH-MISMATCH` (409) when the transaction does not carry this invoice's number.
	/// `E-NAV-SUBMISSION-STATE` (409) when the filing is not `HELD` or a batch already carries the
	/// transaction.
	pub async fn attach_transaction(
		&self,
		ctx: &Ctx,
		invoice_uid: &str,
		transaction_id: &str,
	) -> ClResult<()> {
		auth_mw::require_operator(&self.app, ctx).await?;
		auth_mw::require_stepup(&self.app, ctx).await?;
		let invoice = self.any_invoice(invoice_uid).await?;
		let current = self.held_filing(&invoice).await?;
		let nav = self.nav()?;
		let bound = "a batch already carries this transaction";
		if nav.batch_by_transaction(transaction_id).await?.is_some() {
			return Err(state_refused(bound));
		}

		let invoices = self.invoices()?;
		let client = crate::job::client_of(&self.app, invoices.as_ref(), invoice.seller_id).await?;
		let (_, reply) = client
			.post("queryTransactionStatus", &client.query_status_original_request(transaction_id))
			.await?
			.body("queryTransactionStatus")?;
		let filed = original_invoice_numbers(&Reply::parse(&reply)).map_err(|fault| {
			Error::Unavailable(format!("queryTransactionStatus faulted: {fault:?}"))
		})?;
		if !filed.iter().any(|(_, number)| invoice.number.as_ref() == Some(number)) {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				E_NAV_ATTACH_MISMATCH,
				"NAV's transaction does not carry this invoice's number",
			));
		}

		let held = self.held_rows(&current).await?;
		let ids: Vec<i64> = held.iter().map(|r| r.invoice_id).collect();
		let numbers: HashMap<i64, Option<String>> = invoices
			.invoices_by_ids(&ids)
			.await?
			.into_iter()
			.map(|i| (i.id, i.number))
			.collect();
		let rows: Vec<(i64, i64)> = held
			.iter()
			.filter_map(|r| {
				let number = numbers.get(&r.invoice_id)?.as_ref()?;
				let (idx, _) = filed.iter().find(|(_, n)| n == number)?;
				Some((r.id, *idx))
			})
			.collect();
		let at = Timestamp::now();
		let batch = nav
			.attach_transaction(transaction_id, invoice.seller_id, current.row.op, &rows, at)
			.await?
			.ok_or_else(|| state_refused("this filing moved on, or the transaction was bound"))?;
		filing::ensure_poll(&self.app, batch.uid.as_str(), true).await?;
		audit::try_log(
			&self.app.store,
			ctx,
			"nav_submission",
			Some(invoice_uid),
			"ATTACH",
			Some(serde_json::json!({
				"transactionId": transaction_id,
				"batch": batch.uid.as_str(),
				"rows": rows.len(),
			})),
		)
		.await
	}

	/// R5: a person found that NAV holds nothing of this filing. Every `HELD` filing of its batch
	/// ends `FAILED` under `E-NAV-CONFIRMED-NOT-FILED`, or stays `REJECTED` where NAV said so, and
	/// refiles into one new batch. Operator, step-up, audited; `note` is what the person checked.
	///
	/// # Errors
	/// `E-NAV-SUBMISSION-STATE` (409) when the filing is not `HELD`.
	pub async fn confirm_not_filed(
		&self,
		ctx: &Ctx,
		invoice_uid: &str,
		note: &str,
	) -> ClResult<()> {
		auth_mw::require_operator(&self.app, ctx).await?;
		auth_mw::require_stepup(&self.app, ctx).await?;
		let invoice = self.any_invoice(invoice_uid).await?;
		let current = self.held_filing(&invoice).await?;
		let ids: Vec<i64> = self.held_rows(&current).await?.iter().map(|r| r.id).collect();
		let batch = self
			.nav()?
			.refile(&ids, Some(E_NAV_CONFIRMED_NOT_FILED), Timestamp::now())
			.await?
			.ok_or_else(|| state_refused("this filing moved on while it was being confirmed"))?;
		filing::ensure_send(&self.app, batch.uid.as_str(), true).await?;
		audit::try_log(
			&self.app.store,
			ctx,
			"nav_submission",
			Some(invoice_uid),
			"CONFIRM_NOT_FILED",
			Some(serde_json::json!({
				"batch": batch.uid.as_str(),
				"rows": ids.len(),
				"note": note,
			})),
		)
		.await
	}

	/// Live filings of every seller in `state` — a `FilingState` tag, `open`, or absent for all —
	/// each with its invoice's uid: the queue an operator works from, and what the routes take as
	/// `{uid}`. Keyset on the row `id`, ascending above `after`. Operator only, so unblanked.
	pub async fn list_filings(
		&self,
		ctx: &Ctx,
		state: Option<&str>,
		after: Option<i64>,
		limit: Option<i64>,
	) -> ClResult<FilingPage> {
		auth_mw::require_operator(&self.app, ctx).await?;
		let states = filing_states(state)?;
		let limit = limit.unwrap_or(DEFAULT_FILINGS_PAGE).clamp(1, MAX_FILINGS_PAGE);
		let filings = self.nav()?.list_filings(&states, after.unwrap_or(0), limit).await?;
		let ids: Vec<i64> = filings.iter().map(|f| f.row.invoice_id).collect();
		let uids: HashMap<i64, InvoiceId> = self
			.invoices()?
			.invoices_by_ids(&ids)
			.await?
			.into_iter()
			.map(|i| (i.id, i.uid))
			.collect();
		let items: Vec<(InvoiceId, NavFiling)> = filings
			.into_iter()
			.filter_map(|f| Some((uids.get(&f.row.invoice_id)?.clone(), f)))
			.collect();
		let next_cursor = items.last().map(|(_, f)| f.row.id.to_string());
		Ok(FilingPage { items, next_cursor })
	}

	/// This invoice's filing record, or `None` when nothing has been filed. Org-scoped through
	/// [`Nav::invoice`], so another org's uid reads as `E-CORE-NOTFOUND` and never as a 403.
	///
	/// Read-only, so no step-up and no audit row. `POST /api/invoices/{uid}/nav/retry` answers
	/// with it, and it is the blanking read every caller goes through: nothing may hand out a
	/// `NavStore` row directly. The archived XML is not on the row at all — it is
	/// [`Self::filing_archives`], which is operator-only.
	///
	/// For anyone but an operator the batch's `uid`, `transaction_id`, `error_code` and `error_msg`
	/// are blanked: a batch spans orgs, so all four are shared with another org's invoices. The
	/// row's own `error_code` and `error_msg` stay, being NAV's text about this one invoice.
	///
	/// The batch is blanked rather than dropped, and its `uid` emptied, because
	/// [`NavFiling::state`] reads the batch; `routes::filing_json` writes an empty `uid` as null.
	pub async fn filing(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<Option<NavFiling>> {
		let invoice = self.invoice(ctx, invoice_uid).await?;
		let mut found = self.nav()?.filing_by_invoice(invoice.id).await?;
		if auth_mw::require_operator(&self.app, ctx).await.is_err()
			&& let Some(batch) = found.as_mut().and_then(|f| f.batch.as_mut())
		{
			batch.uid = NavBatchId::from_trusted(String::new());
			batch.transaction_id = None;
			batch.error_code = None;
			batch.error_msg = None;
		}
		Ok(found)
	}

	/// The archived NAV exchanges for this invoice, newest first, one per filing with an archive: a
	/// retry opens a new row, and the XML NAV rejected on the old one is what a dispute reads.
	///
	/// The request is this invoice's own `<invoiceOperation>`, sliced from the batch's envelope by
	/// `idx`; the reply is the batch's. A row that never posted has no part of the envelope, and the
	/// row [`Nav::attach_transaction`] inserts sits in a batch that has no archive, so both are left
	/// out; the row it replaced keeps its batch, so what we sent NAV stays readable.
	///
	/// Operator-only, and the permission is propagated rather than blanked: the reply covers the
	/// whole batch, so it can name every other org's invoice.
	///
	/// The operator gate comes first, as in [`Self::resolve_filing`]: a non-operator gets
	/// `E-AUTH-FORBIDDEN` whatever uid they pass, which leaks nothing because that error is
	/// about the actor's role and not about the resource. An operator reads any org's invoice.
	pub async fn filing_archives(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<Vec<NavArchive>> {
		auth_mw::require_operator(&self.app, ctx).await?;
		let invoice = self.any_invoice(invoice_uid).await?;
		let nav = self.nav()?;
		let mut archives = Vec::new();
		for f in nav.filings_by_invoice(invoice.id).await? {
			let (Some(batch), Some(idx)) = (&f.batch, f.row.idx) else {
				continue;
			};
			let Some(archive) = nav.batch_archive(batch.id).await? else {
				continue;
			};
			let slice = |xml: String| {
				let idx = usize::try_from(idx).ok()?;
				crate::job::operation_slice(&xml, idx).map(str::to_owned)
			};
			archives.push(NavArchive {
				request_xml: archive.request_xml.and_then(slice),
				response_xml: archive.response_xml,
			});
		}
		Ok(archives)
	}

	/// `queryTaxpayer` — validate a Hungarian tax number and read back the registered name and
	/// address (`Taxpayer::address`).
	/// One round trip to NAV, nothing archived, no audit row: it touches no invoice and
	/// files nothing.
	///
	/// The credentials are the seller's, so this needs a seller row even though the tax
	/// number being looked up is a buyer's.
	pub async fn lookup_tax_number(&self, ctx: &Ctx, tax_number: &str) -> ClResult<Taxpayer> {
		// Not `require_operator`: this is the lookup an invoice form runs while a user types
		// a buyer's tax number. It is gated only on there being an org to act for.
		if matches!(ctx.actor, Actor::Public { .. }) {
			return Err(Error::coded(
				StatusCode::FORBIDDEN,
				"E-NAV-FORBIDDEN",
				"queryTaxpayer needs an authenticated caller",
			));
		}
		// `common:TaxpayerIdType` is `[0-9]{8}`, but a Hungarian tax number is *printed*
		// `12345678-2-41` and this is the lookup a form runs while a user types one, so NAV
		// answered `funcCode=ERROR` and the user saw a 502 for a valid number.
		let core: String = tax_number.chars().filter(char::is_ascii_digit).take(8).collect();
		if core.len() != 8 {
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-NAV-TAX-NUMBER",
				"a Hungarian tax number is 8 digits, optionally followed by -V-CC",
			));
		}
		let invoices = self.invoices()?;
		// The caller's own seller, inherited from an ancestor org when it owns none: an org
		// with no seller above it has no credentials to look anything up with.
		let seller = invoices.seller_for_org(ctx.org()?).await?.ok_or(Error::NotFound)?;
		let current = invoices
			.current_seller_version(seller.id)
			.await?
			.ok_or_else(|| Error::internal("mintworks-nav: the seller has no published version"))?;
		NavAuth::load(&self.app, &seller, &current).await?.query_taxpayer(&core).await
	}

	/// *Adóhatósági ellenőrzési adatszolgáltatás* — write the tax-authority audit export into
	/// `out` and return how many invoices it covered. Operator only.
	///
	/// It **never contacts NAV**, and it is driven from `invoices` — never from `nav_submissions` —
	/// so an invoice that failed to report still appears. Invoices are fetched and written one at a
	/// time, so a full year is never held in memory here; the sink decides what streaming means.
	///
	/// # Errors
	/// [`Error::Validation`] when an invoice in range has no mandatory NAV value — the export
	/// fails loudly rather than emitting a blank, exactly as reporting does.
	pub async fn audit_export(
		&self,
		ctx: &Ctx,
		seller_id: i64,
		selection: Selection<'_>,
		out: &mut dyn std::io::Write,
	) -> ClResult<usize> {
		auth_mw::require_operator(&self.app, ctx).await?;
		let nav = self.nav()?;
		let invoices = self.invoices()?;
		// Only for the filename below. The *content* of each `<InvoiceData>` comes from the
		// version that invoice froze — a statutory eight-year export re-serialised with today's
		// seller data is the bug this whole export used to have.
		let current = invoices.current_seller_version(seller_id).await?.ok_or(Error::NotFound)?;
		// Only for invoices never filed, or filed before the appearance was recorded.
		let electronic = self.app.settings.flag("nav.electronic_invoice").await?;

		let ids = match selection {
			Selection::IssueDate { from, to } => {
				nav.export_ids_by_date(seller_id, from, to).await?
			}
			Selection::Number { from, to } => nav.export_ids_by_number(seller_id, from, to).await?,
		};

		let io = |e: std::io::Error| Error::Unavailable(format!("audit export: {e}"));
		out.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Invoices>\n")
			.map_err(io)?;
		// Numbers of everything already emitted, so a storno's original — which the selection
		// always closes over, and which has the lower id — is a lookup rather than a query.
		let mut numbers: HashMap<i64, String> = HashMap::new();
		for chunk in ids.chunks(EXPORT_CHUNK) {
			let (rows, lines, groups) = tokio::try_join!(
				invoices.invoices_by_ids(chunk),
				invoices.invoice_lines_for(chunk),
				invoices.invoice_vat_groups_for(chunk),
			)?;
			// Joined per chunk, the same way the lines and groups are: a version is shared by
			// every invoice issued while it was live, so this is a handful of rows per chunk
			// and not one lookup per invoice.
			let vers: Vec<i64> = rows.iter().filter_map(|i| i.seller_ver).collect();
			let versions: HashMap<i64, _> = invoices
				.seller_versions(&vers)
				.await?
				.into_iter()
				.map(|v| (v.seller_ver, v))
				.collect();
			let mut by_id: HashMap<i64, _> = rows.into_iter().map(|i| (i.id, i)).collect();
			let mut lines_of = group_by(lines, |l| l.invoice_id);
			let mut groups_of = group_by(groups, |g| g.invoice_id);

			for id in chunk {
				let invoice = by_id.remove(id).ok_or_else(|| {
					Error::internal(format!("audit export: invoice {id} vanished"))
				})?;
				if let Some(n) = &invoice.number {
					numbers.insert(invoice.id, n.clone());
				}
				// Every path the map misses — a non-storno, an unnumbered or absent original —
				// falls through to `original_number`, which owns the refusals.
				let original = match invoice
					.original_invoice_id
					.filter(|_| invoice.kind == InvoiceKind::Storno)
					.and_then(|orig| numbers.get(&orig))
				{
					Some(n) => Some(n.clone()),
					None => original_number(invoices.as_ref(), &invoice).await?,
				};
				let lines = lines_of.remove(id).unwrap_or_default();
				let groups = groups_of.remove(id).unwrap_or_default();

				let version =
					invoice.seller_ver.and_then(|v| versions.get(&v)).ok_or_else(|| {
						Error::Validation(format!(
							"audit export: invoice {id} has no frozen seller version"
						))
					})?;
				let doc = invoice_data(
					version,
					&invoice,
					&lines,
					&groups,
					original.as_deref(),
					filed_appearance(nav.as_ref(), invoice.id).await?.unwrap_or(electronic),
				)?;
				// `invoice_data` emits its own declaration; the file already has one, and the
				// rendelet wants the InvoiceData elements themselves under a grouping root.
				let body = doc.split_once("<InvoiceData").map_or(doc.as_str(), |(_, rest)| rest);
				out.write_all(b"<InvoiceData").map_err(io)?;
				out.write_all(body.as_bytes()).map_err(io)?;
				out.write_all(b"\n").map_err(io)?;
			}
		}
		out.write_all(b"</Invoices>\n").map_err(io)?;

		audit::log(
			&self.app.store,
			ctx,
			"audit_export",
			None,
			"EXPORT",
			Some(serde_json::json!({
				"form": match selection {
					Selection::IssueDate { .. } => "issue_date",
					Selection::Number { .. } => "number",
				},
				"filename": selection.filename(&current.tax_number),
				"invoices": ids.len(),
			})),
		)
		.await;

		Ok(ids.len())
	}

	/// The acting org's seller, gated on Admin over the org that owns it.
	async fn admin_seller(&self, ctx: &Ctx) -> ClResult<mintworks_invoice::Seller> {
		let seller = self.invoices()?.seller_for_org(ctx.org()?).await?.ok_or(Error::NotFound)?;
		auth_mw::require_role_on(&self.app, ctx, seller.org_id, Role::Admin).await?;
		Ok(seller)
	}

	/// Whether the acting org's seller can file with NAV, and how many invoices wait for it.
	pub async fn credentials_status(&self, ctx: &Ctx) -> ClResult<NavCredentialsStatus> {
		let seller = self.admin_seller(ctx).await?;
		self.status_of(&seller).await
	}

	async fn status_of(
		&self,
		seller: &mintworks_invoice::Seller,
	) -> ClResult<NavCredentialsStatus> {
		let org = crate::auth::credential_org(&self.app, seller).await?;
		let secrets = &self.app.secrets;
		let (tech_password, sign_key, exchange_key) = tokio::try_join!(
			secrets.status_at(org, "nav.tech_password"),
			secrets.status_at(org, "nav.sign_key"),
			secrets.status_at(org, "nav.exchange_key"),
		)?;
		let nav = self.nav()?;
		let (unreported, fault, hold) = tokio::try_join!(
			nav.backlog(seller.id),
			nav.oldest_open_fault(seller.id),
			nav.seller_hold(seller.id),
		)?;
		Ok(NavCredentialsStatus {
			connected: seller.nav_login.is_some()
				&& tech_password.set
				&& sign_key.set
				&& exchange_key.set,
			login: seller.nav_login.clone(),
			unreported,
			credentials_refused: fault.as_deref() == Some("E-NAV-CREDENTIALS"),
			held: hold.map(|h| NavHeld { code: h.code, since: h.held_at }),
			tech_password,
			sign_key,
			exchange_key,
		})
	}

	/// Connect the acting org's own seller to NAV: verify the credentials with one
	/// `tokenExchange`, and only then store them and release the deferred filings.
	///
	/// Secrets first, `nav_login` second: repeating the call repairs a write that stopped
	/// between them. The deployment's own seller reads the global secrets and is refused here.
	pub async fn set_credentials(
		&self,
		ctx: &Ctx,
		c: &NavCredentials,
	) -> ClResult<NavCredentialsStatus> {
		let seller = self.admin_seller(ctx).await?;
		auth_mw::require_stepup(&self.app, ctx).await?;
		let org = crate::auth::credential_org(&self.app, &seller).await?;
		if org == 0 {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				"E-NAV-CREDENTIALS-GLOBAL",
				"the deployment's own seller takes its NAV credentials from the operator's \
				 configuration",
			));
		}

		// Nothing is dialled before the shapes pass: a malformed key is the typist's, not NAV's.
		let (login, password, sign_key, exchange_key) =
			(c.login.trim(), c.tech_password.trim(), c.sign_key.trim(), c.exchange_key.trim());
		let mut fields = FieldErrors::new();
		if !crate::auth::valid_login(login) {
			fields.insert("login".into(), E_FORMAT);
		}
		for (name, value) in [("techPassword", password), ("signKey", sign_key)] {
			if value.is_empty() || value.len() > 256 {
				fields.insert(name.into(), E_RANGE);
			}
		}
		if exchange_key.len() != 16 {
			fields.insert("exchangeKey".into(), E_RANGE);
		}
		if !fields.is_empty() {
			return Err(Error::ValidationFields("malformed NAV credentials".into(), fields));
		}

		let invoices = self.invoices()?;
		let current = invoices.current_seller_version(seller.id).await?.ok_or(Error::NotFound)?;
		let mut connected = seller.clone();
		connected.nav_login = Some(login.to_owned());
		let creds = crate::auth::Credentials {
			tech_password: password.to_owned(),
			sign_key: sign_key.to_owned(),
			exchange_key: exchange_key.as_bytes().to_vec(),
		};
		NavAuth::with_credentials(&self.app, &connected, &current, creds)
			.await?
			.token_exchange()
			.await
			.map_err(|e| match e.parts().1 {
				// `auth::rejected` formats NAV's `errorCode` into the message.
				"E-NAV-CREDENTIALS" if e.to_string().contains("NOT_REGISTERED_CUSTOMER") => {
					let tax8: String =
						current.tax_number.chars().filter(char::is_ascii_digit).take(8).collect();
					Error::coded(
						StatusCode::BAD_REQUEST,
						"E-NAV-TAXPAYER-UNKNOWN",
						format!(
							"NAV knows no taxpayer {tax8}: the company tax number must be the one \
							 the technical user belongs to ({e})"
						),
					)
				}
				"E-NAV-CREDENTIALS" => Error::coded(
					StatusCode::BAD_REQUEST,
					"E-NAV-CREDENTIALS-INVALID",
					e.to_string(),
				),
				_ => e,
			})?;

		let by = ctx.actor.account_id();
		for (key, value) in [
			("nav.tech_password", password.as_bytes()),
			("nav.sign_key", sign_key.as_bytes()),
			("nav.exchange_key", exchange_key.as_bytes()),
		] {
			self.app.secrets.set_at(org, key, value, by).await?;
		}
		invoices.put_seller(&connected).await?;

		self.release_and_wake(seller.id).await?;
		audit::try_log(
			&self.app.store,
			ctx,
			"nav_credentials",
			Some(seller.uid.as_str()),
			"SET",
			Some(serde_json::json!({ "login": login })),
		)
		.await?;
		// NAV just took these credentials; the open batch's fault is stale until its job runs.
		let status = self.status_of(&connected).await?;
		Ok(NavCredentialsStatus { credentials_refused: false, ..status })
	}

	/// Ends the acting org's seller's test mode: its next invoices number in the live series
	/// and are reported to NAV. Owner on the seller org, step-up, and only once NAV is
	/// connected (`E-NAV-NOT-CONNECTED`). Idempotent, and there is no way back; invoices
	/// already issued in test mode stay test.
	pub async fn go_live(&self, ctx: &Ctx) -> ClResult<()> {
		let invoices = self.invoices()?;
		let seller = invoices.seller_for_org(ctx.org()?).await?.ok_or(Error::NotFound)?;
		auth_mw::require_role_on(&self.app, ctx, seller.org_id, Role::Owner).await?;
		auth_mw::require_stepup(&self.app, ctx).await?;
		if !seller.test_mode {
			return Ok(());
		}
		if !crate::auth::connected(&self.app, &seller).await? {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				"E-NAV-NOT-CONNECTED",
				"connect the NAV credentials before going live",
			));
		}
		invoices.set_seller_test_mode(seller.id, false).await?;
		audit::try_log(&self.app.store, ctx, "seller", Some(seller.uid.as_str()), "GO_LIVE", None)
			.await?;
		Ok(())
	}

	/// Lift a seller's hold once a person has dealt with why NAV refused it, for a seller whose
	/// credentials did not change; [`Nav::set_credentials`] releases on its own. Operator, step-up,
	/// audited. The next POST is real and spends another `requestId` if NAV still refuses, which
	/// is why nothing probes automatically. Releasing a seller with no hold only wakes its jobs.
	///
	/// # Errors
	/// `E-CORE-NOTFOUND` when no seller has this uid.
	pub async fn release_seller_hold(&self, ctx: &Ctx, seller_uid: &str) -> ClResult<()> {
		auth_mw::require_operator(&self.app, ctx).await?;
		auth_mw::require_stepup(&self.app, ctx).await?;
		let seller = self.invoices()?.seller_by_uid(seller_uid).await?.ok_or(Error::NotFound)?;
		self.release_and_wake(seller.id).await?;
		audit::try_log(
			&self.app.store,
			ctx,
			"nav_seller_hold",
			Some(seller.uid.as_str()),
			"RELEASE",
			None,
		)
		.await
	}

	/// Lifts the seller's hold and pulls its deferred work forward: the `NAV_SEND` of each `CLAIMED`
	/// batch, and the `NAV_REPORT` of each invoice with no row.
	async fn release_and_wake(&self, seller_id: i64) -> ClResult<()> {
		let nav = self.nav()?;
		nav.release_seller(seller_id).await?;
		let mut keys = nav.open_batch_keys(seller_id).await?;
		let unfiled = nav.unfiled_invoices(seller_id, i64::MAX).await?;
		keys.extend(unfiled.into_iter().map(mintworks_invoice::invoice_job_key));
		self.app.store.job_wake(&keys, Timestamp::now()).await?;
		Ok(())
	}

	// `report`, `send`, `poll`, `adopt`, `sweep` and `reconcile` stay free functions in
	// `crate::job`: they take no `&Ctx` (a job has no actor).

	pub(crate) async fn run_report(&self, invoice_id: i64) -> ClResult<Next> {
		crate::job::report(&self.app, self.invoices()?.as_ref(), self.nav()?.as_ref(), invoice_id)
			.await
	}

	pub(crate) async fn run_send(&self, batch_uid: &str) -> ClResult<Next> {
		crate::job::send(&self.app, self.invoices()?.as_ref(), self.nav()?.as_ref(), batch_uid)
			.await
	}

	pub(crate) async fn run_poll(&self, job: &Job, batch_uid: &str) -> ClResult<Next> {
		crate::job::poll(&self.app, self.invoices()?.as_ref(), self.nav()?.as_ref(), job, batch_uid)
			.await
	}

	pub(crate) async fn run_adopt(&self, batch_uid: &str) -> ClResult<Next> {
		crate::job::adopt(&self.app, self.invoices()?.as_ref(), self.nav()?.as_ref(), batch_uid)
			.await
	}

	pub(crate) async fn run_reconcile(&self, batch_uid: &str) -> ClResult<()> {
		crate::job::reconcile(&self.app, self.invoices()?.as_ref(), self.nav()?.as_ref(), batch_uid)
			.await
	}

	pub(crate) async fn run_sweep(&self) -> ClResult<()> {
		crate::job::sweep(&self.app, self.nav()?.as_ref()).await
	}

	async fn audit(&self, ctx: &Ctx, invoice_uid: &str, action: &str, detail: serde_json::Value) {
		audit::log(&self.app.store, ctx, "nav_submission", Some(invoice_uid), action, Some(detail))
			.await;
	}
}

/// This crate's condition alerts, registered by the application with
/// `AppBuilder::alerts(mintworks_nav::service_api::alerts)`.
///
/// `A-NAV-REJECTED` covers every filing nothing automatic will move: NAV refused it, its status
/// with NAV is unproved (`HELD`), or a fault outlasted `nav.fault_alert_after`. The remediations
/// are opposite, so the message must not name only one: correcting and re-issuing an invoice NAV
/// already holds files it twice. It is deliberately **not** `A-JOB-FAILED`: the job that recorded
/// any of them did its work correctly and completes, so no job alert will ever surface the
/// invoice. Counts the whole deployment: this product's sellers are tenants, so the root org need
/// not own a seller at all.
///
/// `A-NAV-SELLER-HELD` counts sellers whose filing stopped because NAV spent a `requestId`.
///
/// # Errors
/// Propagates the store read; `Error::Internal` when no `NavStore` was registered.
pub async fn alerts(app: App) -> ClResult<Vec<Alert>> {
	let nav = nav_store(&app)?;
	let grace = app.settings.int("nav.fault_alert_after").await?;
	let count = nav.awaiting_operator(None, Timestamp(Timestamp::now().0 - grace)).await?;
	let held = nav.seller_holds_count().await?;
	let mut alerts = Vec::new();
	if count > 0 {
		alerts.push(Alert {
			code: "A-NAV-REJECTED",
			severity: Severity::Error,
			count,
			message: format!(
				"{count} invoice(s) need a person: NAV rejected them (correct and re-issue, or \
				 retry), their status with NAV is unproved (find the transaction and attach it, or \
				 confirm it was not filed — do not storno on the strength of the error), or a \
				 fault has outlasted its grace"
			),
			since: None,
			// `state=open` is the one filter that spans every class `awaiting_operator` counts.
			link: Some("/api/admin/nav-submissions?state=open".into()),
		});
	}
	if held > 0 {
		alerts.push(Alert {
			code: "A-NAV-SELLER-HELD",
			severity: Severity::Error,
			count: held,
			message: format!(
				"{held} seller(s) are held: NAV spent a requestId on an unauthorised request, so \
				 nothing of theirs reaches NAV until they reconnect or an operator releases the \
				 hold"
			),
			since: None,
			link: None,
		});
	}
	Ok(alerts)
}

/// The `invoiceAppearance` the invoice's latest filing went out with, if one was recorded: the
/// export must say what NAV holds, not what the setting says today.
// One lookup per exported invoice; add a bulk `NavStore` read if exports get slow.
async fn filed_appearance(nav: &dyn NavStore, invoice_id: i64) -> ClResult<Option<bool>> {
	Ok(nav.filing_by_invoice(invoice_id).await?.and_then(|f| f.row.electronic))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_state_filter_reads_absent_open_and_a_tag() {
		assert!(filing_states(None).unwrap().is_empty());
		let open = filing_states(Some("open")).unwrap();
		for counted in [FilingState::Held, FilingState::Rejected, FilingState::Failed] {
			assert!(open.contains(&counted), "`awaiting_operator` counts {counted:?}");
		}
		assert!(!open.contains(&FilingState::Accepted));
		assert!(!open.contains(&FilingState::Cancelled));
		assert_eq!(filing_states(Some("HELD")).unwrap(), [FilingState::Held]);
		assert!(filing_states(Some("held")).is_err(), "a typo must not list everything");
	}
}

// vim: ts=4
