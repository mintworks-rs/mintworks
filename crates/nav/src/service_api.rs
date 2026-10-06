// SPDX-License-Identifier: MPL-2.0
//! `Nav` — the service handle a consumer application drives NAV reporting through.
//!
//! Every method takes `&Ctx` first and derives its permission from `ctx.actor`, matching
//! `Auth` and `Invoices`. The one route bundle, [`crate::routes::org_credentials`], covers a
//! tenant connecting its own NAV technical user; for the filing methods a consumer that wants
//! HTTP writes the handlers itself.
//!
//! The handle also carries the audit writes NAV never had. `nav_submissions` is a statutory
//! archive but it produced no `audit_logs` row at all — only [`crate::export`] audited — so
//! filing an invoice with the tax authority left no trace of *who* asked for it.

use std::collections::HashMap;
use std::sync::Arc;

use mintworks_core::alert::{Alert, Severity};
use mintworks_core::secrets::SecretStatus;
use mintworks_core::store::Role;
use mintworks_core::{
	App, audit, auth_mw, ctx::Actor, ctx::Ctx, error::StatusCode, job, prelude::*,
};
use mintworks_invoice::store::{InvoiceKind, InvoiceStatus};
use mintworks_invoice::{Invoice, InvoiceStore, KIND_NAV_REPORT, invoice_store};
use serde::{Deserialize, Serialize};

use crate::auth::NavAuth;
use crate::client::Taxpayer;
use crate::export::{Selection, original_number};
use crate::job::KIND_NAV_POLL;
use crate::store::{NavStore, store as nav_store};
use crate::submission::{NavArchive, NavSubmission, NavVerdict};
use crate::xml::invoice_data;

/// Invoices per batched read in [`Nav::audit_export`]. Well under SQLite's 32 766 variable
/// cap, which an unchunked year of invoices would pass.
const EXPORT_CHUNK: usize = 1000;

/// Rows keyed by whatever `key` reads off them, order preserved within each group.
fn group_by<T>(rows: Vec<T>, key: fn(&T) -> i64) -> HashMap<i64, Vec<T>> {
	let mut out: HashMap<i64, Vec<T>> = HashMap::new();
	for row in rows {
		out.entry(key(&row)).or_default().push(row);
	}
	out
}

/// `jobs.err_code` on a row an operator stopped, so a cancelled filing is distinguishable
/// from one that failed on its own.
pub const E_NAV_CANCELLED: &str = "E-NAV-CANCELLED";

/// [`Nav::cancel_filing`] on an invoice some other invoice's batch files: there is nothing for
/// it to stop, so it refuses rather than reporting a cancellation that did not happen.
pub const E_NAV_BATCH_MEMBER: &str = "E-NAV-BATCH-MEMBER";

/// [`Nav::cancel_filing`] on a batch leader whose fate with NAV is still being established: a
/// `NAV_RECONCILE` is outstanding, or the leader's own `NAV_REPORT` is mid-POST.
pub const E_NAV_FILING_IN_FLIGHT: &str = "E-NAV-FILING-IN-FLIGHT";

/// [`Nav::resolve_filing`] on a submission that is not waiting on a person: no recorded
/// rejection or fault, or already resolved.
pub const E_NAV_SUBMISSION_STATE: &str = "E-NAV-SUBMISSION-STATE";

/// A tenant seller's NAV technical user, as typed. No `Debug`: three of the four are secrets.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavCredentials {
	pub login: String,
	pub tech_password: String,
	pub sign_key: String,
	pub exchange_key: String,
}

/// What the acting org may know about its seller's NAV connection. Never a secret value.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NavCredentialsStatus {
	pub login: Option<String>,
	pub connected: bool,
	/// Issued invoices with no filing yet — the backlog connecting releases.
	pub unreported: i64,
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

	/// File this invoice with NAV, or re-drive a filing that is already on record.
	///
	/// The normal path is `mintworks-invoice`'s issue transaction, which enqueues `NAV_REPORT`
	/// under `nav:invoice:{id}` and returns. This method exists for the two cases that path
	/// leaves open: the at-most-once gap at `crates/invoice/src/issue.rs` (a crash between
	/// `COMMIT` and the enqueue leaves an issued invoice with no job at all), and an
	/// operator re-driving a filing whose job is spent.
	///
	/// The first is a plain enqueue under the standard key. The second cannot be: a `dedup_key`
	/// is never released, so replaying that key is a no-op by design. A re-drive therefore
	/// **resets the existing row** (`CoreStore::job_redrive`) and requires an operator. Minting
	/// a *second* row under a fresh key left two live `NAV_REPORT` rows for one invoice, so two
	/// workers could claim one each, both pass [`crate::filing::may_send`] and both POST
	/// `manageInvoice`. One row per invoice makes the `jobs` claim the mutual exclusion.
	pub async fn submit(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<()> {
		// Filing a statutory return is the same class of act as `Invoices::issue`, which gates on
		// step-up too; org ownership alone was the whole permission here. In the service, not
		// the handler: the guard belongs to the operation. `Actor::System` is exempt.
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let invoice = self.invoice(ctx, invoice_uid).await?;
		// A draft has no number, so the job fails terminally on attempt one — **after** spending
		// `nav:invoice:{id}`, the key `issue::enqueue_jobs` needs at real issue time. That
		// enqueue is then a silent no-op and the invoice is never filed.
		if !matches!(
			invoice.status,
			InvoiceStatus::Issued | InvoiceStatus::Paid | InvoiceStatus::Stornoed
		) {
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-NAV-NOT-ISSUED",
				"only an issued invoice can be filed; a draft has no number to file",
			));
		}
		let payload = mintworks_invoice::invoice_job_payload(invoice.id);
		let key = format!("nav:invoice:{}", invoice.id);

		let redrive =
			job::enqueue(&self.app.store, KIND_NAV_REPORT, &payload, Some(&key), Timestamp::now())
				.await?
				.is_none();
		let mut target = "report";
		if redrive {
			auth_mw::require_operator(&self.app, ctx).await?;
			if self.app.store.job_redrive(KIND_NAV_REPORT, &payload, Timestamp::now()).await? == 0 {
				// The filing is at NAV, so the report job is `DONE` and what needs reviving is
				// the poll. Once `NAV_POLL` terminates `Retry::Never` nothing else restarts it,
				// and since batching one dead poll strands a whole `nav.batch_max` of invoices.
				target = "poll";
				if self.redrive_poll(invoice.id).await? == 0 {
					// The key is spent but no `FAILED` row answers to it: still
					// `PENDING`/`RUNNING`, or finished and swept. Say which state refused rather
					// than reporting a re-drive that did not happen.
					return Err(Error::coded(
						StatusCode::CONFLICT,
						"E-NAV-NOT-REDRIVABLE",
						"this invoice has no failed NAV filing to re-drive",
					));
				}
			}
		}

		// `try_log`, not `log`: this row is the record of who ordered a statutory filing, and
		// the write is past the enqueue's commit, so a lost one can never be recovered.
		audit::try_log(
			&self.app.store,
			ctx,
			"nav_submission",
			Some(invoice_uid),
			"SUBMIT",
			Some(serde_json::json!({ "redrive": redrive, "target": target })),
		)
		.await?;
		Ok(())
	}

	/// Revive the `NAV_POLL` chain of an invoice whose filing NAV already holds, for
	/// [`Nav::submit`]'s re-drive. `0` when there is no such filing or no poll row to revive.
	async fn redrive_poll(&self, invoice_id: i64) -> ClResult<u64> {
		let Some(prev) = self.nav()?.submission_by_invoice(invoice_id).await? else {
			return Ok(0);
		};
		let (Some(transaction_id), None) = (&prev.transaction_id, prev.verdict) else {
			return Ok(0);
		};
		// `FAILED` first, then the `DONE` row a poll that ran out of retry classes leaves, the
		// two-step `job::refile_released` uses.
		let payload = crate::job::poll_payload(prev.id);
		let now = Timestamp::now();
		let moved = self.app.store.job_redrive(KIND_NAV_POLL, &payload, now).await?;
		if moved > 0 {
			return Ok(moved);
		}
		let key = format!("nav:poll:{transaction_id}");
		self.app.store.job_redrive_done(&key, &payload, now).await
	}

	/// Stop an invoice's NAV jobs. Operator only.
	///
	/// NAV retry is unbounded — giving up on a statutory filing is not a recovery — so an
	/// invoice whose XML NAV will never accept would otherwise retry forever at six requests
	/// an hour with no way to stop it short of raw SQL. Both kinds go: the `NAV_REPORT` row
	/// and any `NAV_POLL` row for the submission it opened.
	///
	/// Returns how many job rows were stopped; `0` means nothing was live.
	pub async fn cancel_filing(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<u64> {
		auth_mw::require_operator(&self.app, ctx).await?;
		let invoice = self.invoice(ctx, invoice_uid).await?;
		let prev = self.nav()?.submission_by_invoice(invoice.id).await?;
		// Stopping a member's own jobs stops nothing that files it — the leader POSTs the whole
		// batch under its own `requestId` — while reporting `stopped = 1` as if it had. Refuse
		// and name the leader, which is the invoice whose filing an operator can actually stop.
		if let Some(leader) = prev.as_ref().and_then(|s| s.batch_uid.as_deref())
			&& leader != invoice.uid.as_str()
		{
			return Err(Error::coded(
				StatusCode::CONFLICT,
				E_NAV_BATCH_MEMBER,
				format!(
					"this invoice is filed in a NAV batch led by {leader}; \
					 cancel that invoice's filing instead"
				),
			));
		}
		// A leader's cancellation releases its members first: only the leader POSTs, so cancelling
		// it alone leaves them unfilable and invisible to `unfiled_invoices`. Released before the
		// leader is settled, for the reason `job::report` releases in that order.
		if let Some(prev) = prev.as_ref()
			&& prev.batch_uid.as_deref() == Some(invoice.uid.as_str())
		{
			// A reconciliation outstanding means NAV may hold this batch: releasing the members
			// would refile them under their own requestIds, which NAV does not dedupe. A
			// `RUNNING` NAV_REPORT is the narrower race — that worker is already past `may_send`.
			let reconcile = self
				.app
				.store
				.job_status_by_key(&format!("nav:reconcile:{}", invoice.uid.as_str()))
				.await?;
			let report =
				self.app.store.job_status_by_key(&format!("nav:invoice:{}", invoice.id)).await?;
			if matches!(reconcile.as_deref(), Some("PENDING" | "RUNNING"))
				|| report.as_deref() == Some("RUNNING")
			{
				return Err(Error::coded(
					StatusCode::CONFLICT,
					E_NAV_FILING_IN_FLIGHT,
					"this batch's filing is still in flight with NAV: a reconciliation or a \
					 running filing job has yet to settle whether NAV holds it. Wait for it to \
					 finish — releasing the members now would file each of them again",
				));
			}
			// `abandon` re-drives each released member too: its own `NAV_REPORT` may already
			// have stood down through `filing::may_send` and completed `DONE`, which spends
			// `nav:invoice:{id}` — so cancelling one leader would otherwise strand up to
			// `nav.batch_max - 1` invoices nothing can ever file.
			let released = crate::filing::abandon(
				&self.app,
				self.nav()?.as_ref(),
				invoice.uid.as_str(),
				prev.id,
			)
			.await?;
			if !released.is_empty() {
				tracing::info!(
					leader = %invoice.uid.as_str(),
					released = released.len(),
					"released the cancelled batch's members; each files on its own job"
				);
			}
		}
		let now = Timestamp::now();
		let err = "cancelled by an operator";

		let mut stopped = self
			.app
			.store
			.job_cancel(
				KIND_NAV_REPORT,
				&mintworks_invoice::invoice_job_payload(invoice.id),
				now,
				err,
				Some(E_NAV_CANCELLED),
			)
			.await?;
		// For a leader this cancels the poll for the whole batch: one `NAV_POLL` row covers
		// every invoice the transaction carries.
		if let Some(prev) = prev {
			stopped += self
				.app
				.store
				.job_cancel(
					KIND_NAV_POLL,
					&crate::job::poll_payload(prev.id),
					now,
					err,
					Some(E_NAV_CANCELLED),
				)
				.await?;
		}

		self.audit(ctx, invoice_uid, "CANCEL", serde_json::json!({ "jobs": stopped }))
			.await;
		Ok(stopped)
	}

	/// Record that an operator has dealt with a filing NAV refused or left without a verdict,
	/// so `A-NAV-REJECTED` and the hourly sweep stop counting this invoice. Operator only.
	///
	/// This is the missing half of the alert: `awaiting_operator` counts a `REJECTED`/`FAILED`
	/// row forever, and the remedy the alert recommends — correct and re-issue — produces a
	/// *new* invoice, so the old row went on alarming and the alert became permanent noise.
	///
	/// It settles the alarm, not the invoice: the verdict and both archives stand, and the
	/// invoice does not become filable again — `unfiled_invoices` skips an invoice with any
	/// row. Re-filing is [`Nav::submit`]'s re-drive, which is a separate deliberate act.
	///
	/// `note` is free text for the audit row: what the person actually did.
	///
	/// # Errors
	/// `E-CORE-NOTFOUND` when the invoice or its filing record is absent,
	/// `E-NAV-SUBMISSION-STATE` (409) when the filing is not one that needs a person.
	pub async fn resolve_filing(&self, ctx: &Ctx, invoice_uid: &str, note: &str) -> ClResult<()> {
		auth_mw::require_operator(&self.app, ctx).await?;
		let invoice = self.invoice(ctx, invoice_uid).await?;
		let row = self.nav()?.submission_by_invoice(invoice.id).await?.ok_or(Error::NotFound)?;
		if !self.nav()?.resolve(row.id, Timestamp::now()).await? {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				E_NAV_SUBMISSION_STATE,
				"this filing is not waiting on a person: it has no recorded rejection or \
				 fault, or it was already resolved",
			));
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

	/// This invoice's filing record, or `None` when nothing has been filed. Org-scoped through
	/// [`Nav::invoice`], so another org's uid reads as `E-CORE-NOTFOUND` and never as a 403.
	///
	/// Read-only, so no step-up and no audit row: `mintworks-nav` mounts no routes, and without
	/// this a consumer serving a filing's state has to query `NavStore` from a handler. The
	/// archived XML is not on the row at all — it is [`Self::filing_archive`], which is
	/// operator-only.
	///
	/// `batch_uid` and `transaction_id` are blanked for anyone but an operator: a batch spans
	/// orgs, so the leader's uid is another org's invoice id — time-sortable, so it dates
	/// that invoice too — and the `transactionId` is shared, which lets two orgs correlate
	/// their filings.
	///
	/// `error_msg` goes with them because it is free text the batch path writes and can name
	/// another org's invoice; `error_code` is NAV's own generic code and is what an org
	/// actually needs, so it stays.
	pub async fn filing(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<Option<NavSubmission>> {
		let invoice = self.invoice(ctx, invoice_uid).await?;
		let mut row = self.nav()?.submission_by_invoice(invoice.id).await?;
		if auth_mw::require_operator(&self.app, ctx).await.is_err()
			&& let Some(row) = &mut row
		{
			// A batch spans orgs — `batch_candidates` selects on `seller_id`, and the seller
			// is the operator — so the leader's uid and the shared `transactionId` are another
			// org's identifiers.
			row.batch_uid = None;
			row.transaction_id = None;
			row.error_msg = None;
		}
		Ok(row)
	}

	/// The archived NAV exchange for this invoice's filing, or `None` when nothing is archived.
	///
	/// Operator-only, and the permission is propagated rather than blanked: a batch leader's
	/// envelope carries every other org's `invoiceData` as decodable base64, and even at
	/// `nav.batch_max = 1` it carries `softwareData` and the seller's `login`. Not fixed by
	/// archiving less — `NavStore::release_batch` depends on the leader keeping the whole
	/// envelope.
	///
	/// The operator gate comes first, as in [`Self::resolve_filing`]: a non-operator gets
	/// `E-AUTH-FORBIDDEN` whatever uid they pass, which leaks nothing because that error is
	/// about the actor's role and not about the resource. For an operator the uid is still
	/// org-scoped through [`Nav::invoice`], so another org's reads as `E-CORE-NOTFOUND`.
	pub async fn filing_archive(
		&self,
		ctx: &Ctx,
		invoice_uid: &str,
	) -> ClResult<Option<NavArchive>> {
		auth_mw::require_operator(&self.app, ctx).await?;
		let invoice = self.invoice(ctx, invoice_uid).await?;
		let nav = self.nav()?;
		let Some(row) = nav.submission_by_invoice(invoice.id).await? else {
			return Ok(None);
		};
		nav.submission_archive(row.id).await
	}

	/// `queryTaxpayer` — validate a Hungarian tax number and read back the registered name.
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
				let doc = invoice_data(version, &invoice, &lines, &groups, original.as_deref())?;
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
		let unreported = self.nav()?.unfiled_invoices(seller.id, i64::MAX).await?.len();
		Ok(NavCredentialsStatus {
			connected: seller.nav_login.is_some()
				&& tech_password.set
				&& sign_key.set
				&& exchange_key.set,
			login: seller.nav_login.clone(),
			unreported: i64::try_from(unreported).unwrap_or(i64::MAX),
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

		let nav = self.nav()?;
		let keys: Vec<String> = nav
			.unfiled_invoices(seller.id, i64::MAX)
			.await?
			.into_iter()
			.map(|id| format!("nav:invoice:{id}"))
			.collect();
		self.app.store.job_wake(&keys, Timestamp::now()).await?;
		audit::try_log(
			&self.app.store,
			ctx,
			"nav_credentials",
			Some(seller.uid.as_str()),
			"SET",
			Some(serde_json::json!({ "login": login })),
		)
		.await?;
		self.status_of(&connected).await
	}

	// `report`, `poll`, `sweep` and `reconcile` stay free functions in `crate::job`: they take
	// no `&Ctx` (a job has no actor). What the handle contributes is the pair of stores.

	pub(crate) async fn run_report(&self, invoice_id: i64) -> ClResult<mintworks_core::job::Next> {
		let invoices = self.invoices()?;
		if let Some(at) = crate::job::deferral(&self.app, invoices.as_ref(), invoice_id).await? {
			return Ok(mintworks_core::job::Next::Again { at });
		}
		crate::job::report(&self.app, invoices.as_ref(), self.nav()?.as_ref(), invoice_id).await?;
		Ok(mintworks_core::job::Next::Done)
	}

	pub(crate) async fn run_poll(
		&self,
		job: &mintworks_core::job::Job,
		submission_id: i64,
	) -> ClResult<mintworks_core::job::Next> {
		crate::job::poll(
			&self.app,
			self.invoices()?.as_ref(),
			self.nav()?.as_ref(),
			job,
			submission_id,
		)
		.await
	}

	pub(crate) async fn run_reconcile(&self, batch_uid: &str) -> ClResult<()> {
		crate::job::reconcile(&self.app, self.invoices()?.as_ref(), self.nav()?.as_ref(), batch_uid)
			.await
	}

	pub(crate) async fn run_sweep(&self) -> ClResult<()> {
		crate::job::sweep(&self.app, self.invoices()?.as_ref(), self.nav()?.as_ref()).await
	}

	async fn audit(&self, ctx: &Ctx, invoice_uid: &str, action: &str, detail: serde_json::Value) {
		audit::log(&self.app.store, ctx, "nav_submission", Some(invoice_uid), action, Some(detail))
			.await;
	}
}

/// This crate's condition alerts, registered by the application with
/// `AppBuilder::alerts(mintworks_nav::service_api::alerts)`.
///
/// One code, `A-NAV-REJECTED`, over three states nothing retries: `REJECTED` (NAV answered
/// `ABORTED`), `FAILED` (the filing ended with no verdict on the invoice), and the open row
/// `job::report` leaves on `REQUEST_ID_NOT_UNIQUE` — no verdict, because NAV may hold the
/// filing. The remediations are opposite, so the message must not name only one: correcting
/// and re-issuing an invoice NAV already holds files it twice. It is deliberately **not**
/// `A-JOB-FAILED`: the job that recorded any of them did its work correctly and completes, so
/// no job alert will ever surface the invoice.
///
/// # Errors
/// Propagates the store read; `Error::Internal` when no `NavStore` was registered.
pub async fn alerts(app: App) -> ClResult<Vec<Alert>> {
	// A deployment whose root org owns no seller must still answer its other alerts, so a
	// missing seller is an empty feed rather than a 500 — `job::sweep` does the same.
	let seller = match crate::auth::deployment_seller(&app).await {
		Ok(Some(s)) => s,
		Ok(None) => return Ok(Vec::new()),
		Err(e) => {
			tracing::error!(error = %e, "could not resolve the deployment seller");
			return Ok(Vec::new());
		}
	};
	let count = nav_store(&app)?.awaiting_operator(seller.id).await?;
	if count == 0 {
		return Ok(Vec::new());
	}
	Ok(vec![Alert {
		code: "A-NAV-REJECTED",
		severity: Severity::Error,
		count,
		message: format!(
			"{count} invoice(s) ended without a NAV acceptance and need a person: NAV rejected \
			 them (correct and re-issue), or their status with NAV is unknown (query the \
			 transaction first — do not storno on the strength of the error)"
		),
		since: None,
		// `state=open` is the one filter that spans all three classes `awaiting_operator`
		// counts; a `verdict=` list cannot express the verdict-NULL-with-an-error-code one.
		link: Some("/api/admin/nav-submissions?state=open".into()),
	}])
}

// vim: ts=4
