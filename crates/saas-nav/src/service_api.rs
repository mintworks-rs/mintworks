//! `Nav` — the service handle a consumer application drives NAV reporting through
//! (`claude-docs/rust-api.md` §5).
//!
//! Every method takes `&Ctx` first and derives its permission from `ctx.actor`, matching
//! `Auth` and `Invoices`. Unlike those two this crate ships **no route bundle**: `saas-nav`
//! has no `axum` dependency and is not gaining one, so a consumer that wants HTTP writes the
//! three handlers itself.
//!
//! The handle also carries the audit writes NAV never had. `nav_submissions` is a statutory
//! archive but it produced no `audit_logs` row at all — only [`crate::export`] audited — so
//! filing an invoice with the tax authority left no trace of *who* asked for it.

use std::collections::HashMap;
use std::sync::Arc;

use saas_core::alert::{Alert, Severity};
use saas_core::{App, audit, auth_mw, ctx::Actor, ctx::Ctx, error::StatusCode, job, prelude::*};
use saas_invoice::store::{InvoiceKind, InvoiceStatus};
use saas_invoice::{Invoice, InvoiceStore, KIND_NAV_REPORT, SELLER_ID, invoice_store};

use crate::auth::NavAuth;
use crate::client::Taxpayer;
use crate::export::{Selection, original_number};
use crate::job::KIND_NAV_POLL;
use crate::store::{NavStore, store as nav_store};
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
			.ok_or_else(|| Error::internal("saas-nav: no NavStore was registered on the app"))
	}

	fn invoices(&self) -> ClResult<Arc<dyn InvoiceStore>> {
		self.invoices
			.clone()
			.ok_or_else(|| Error::internal("saas-nav: no InvoiceStore was registered on the app"))
	}

	/// Tenant-scoped for a `User` and for `Public`, so another tenant's `inv_` id reads as
	/// absent rather than as a `403`. Only an `Operator` or `System` caller with no tenant
	/// chosen reads unscoped — lumping `Public` in with them gave a `Ctx` carrying no tenant
	/// an unscoped read of any tenant's invoice by uid. `lookup_tax_number` below refuses
	/// `Public` outright; this is the same line drawn where a scope is what is needed.
	async fn invoice(&self, ctx: &Ctx, uid: &str) -> ClResult<Invoice> {
		let uid = InvoiceId::parse(uid)?;
		let scope = match ctx.actor {
			Actor::User { .. } | Actor::Public { .. } => Some(ctx.tenant()?),
			_ => ctx.tenant_id,
		};
		self.invoices()?.invoice_by_uid(scope, &uid).await?.ok_or(Error::NotFound)
	}

	/// File this invoice with NAV, or re-drive a filing that is already on record.
	///
	/// The normal path is `saas-invoice`'s issue transaction, which enqueues `NAV_REPORT`
	/// under `nav:invoice:{id}` and returns. This method exists for the two cases that path
	/// leaves open: the at-most-once gap at `saas-invoice/src/issue.rs` (a crash between
	/// `COMMIT` and the enqueue leaves an issued invoice with no job at all), and an
	/// operator re-driving a filing whose job is spent.
	///
	/// The first is a plain enqueue under the standard key. The second cannot be: a `dedup_key`
	/// is never released, so replaying that key is a no-op by design. A re-drive therefore
	/// **resets the existing row** (`CoreStore::job_redrive`) and requires an operator. Minting
	/// a *second* row under a fresh key left two live `NAV_REPORT` rows for one invoice, so two
	/// workers could claim one each, both pass [`crate::job::may_send`] and both POST
	/// `manageInvoice`. One row per invoice makes the `jobs` claim the mutual exclusion.
	pub async fn submit(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<()> {
		// Filing a statutory return is the same class of act as `Invoices::issue`, which gates on
		// step-up too; tenant ownership alone was the whole permission here. In the service, not
		// the handler: the guard belongs to the operation. `Actor::System` is exempt.
		saas_core::auth_mw::require_stepup(&self.app, ctx).await?;
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
		let payload = saas_invoice::invoice_job_payload(invoice.id);
		let key = format!("nav:invoice:{}", invoice.id);

		let redrive =
			job::enqueue(&self.app.store, KIND_NAV_REPORT, &payload, Some(&key), Timestamp::now())
				.await?
				.is_none();
		if redrive {
			auth_mw::require_operator(&self.app, ctx).await?;
			if self.app.store.job_redrive(KIND_NAV_REPORT, &payload, Timestamp::now()).await? == 0 {
				// The key is spent but no `FAILED` row answers to it: still `PENDING`/`RUNNING`,
				// or finished and swept. Say which state refused rather than reporting a
				// re-drive that did not happen.
				return Err(Error::coded(
					StatusCode::CONFLICT,
					"E-NAV-NOT-REDRIVABLE",
					"this invoice has no failed NAV filing to re-drive",
				));
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
			Some(serde_json::json!({ "redrive": redrive })),
		)
		.await?;
		Ok(())
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
		let now = Timestamp::now();
		let err = "cancelled by an operator";

		let mut stopped = self
			.app
			.store
			.job_cancel(
				KIND_NAV_REPORT,
				&saas_invoice::invoice_job_payload(invoice.id),
				now,
				err,
				Some(E_NAV_CANCELLED),
			)
			.await?;
		if let Some(prev) = self.nav()?.submission_by_invoice(invoice.id).await? {
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

	/// `queryTaxpayer` — validate a Hungarian tax number and read back the registered name.
	/// One round trip to NAV, nothing archived, no audit row: it touches no invoice and
	/// files nothing.
	///
	/// The credentials are the seller's, so this needs a seller row even though the tax
	/// number being looked up is a buyer's.
	pub async fn lookup_tax_number(&self, ctx: &Ctx, tax_number: &str) -> ClResult<Taxpayer> {
		// Not `require_operator`: this is the lookup an invoice form runs while a user types
		// a buyer's tax number. It is gated only on there being a tenant to act for.
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
		let seller = self
			.invoices()?
			.seller_by_id(SELLER_ID)
			.await?
			.ok_or_else(|| Error::internal("saas-nav: the seller is gone"))?;
		NavAuth::load(&self.app, &seller).await?.query_taxpayer(&core).await
	}

	/// *Adóhatósági ellenőrzési adatszolgáltatás* — write the tax-authority audit export into
	/// `out` and return how many invoices it covered. Operator only.
	///
	/// It **never contacts NAV**, and it is driven from `invoices` — never from
	/// `nav_submissions` — so an invoice that failed to report still appears
	/// (`nav-mapping.md` §9.3). Invoices are fetched and written one at a time, so a full
	/// year is never held in memory here; the sink decides what streaming means.
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
		let seller = invoices.seller_by_id(seller_id).await?.ok_or(Error::NotFound)?;

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

				let doc = invoice_data(&seller, &invoice, &lines, &groups, original.as_deref())?;
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
				"filename": selection.filename(&seller.tax_number),
				"invoices": ids.len(),
			})),
		)
		.await;

		Ok(ids.len())
	}

	// `report`, `poll` and `sweep` stay free functions in `crate::job`: they take no `&Ctx`
	// (a job has no actor). What the handle contributes is the pair of stores.

	pub(crate) async fn run_report(&self, invoice_id: i64) -> ClResult<()> {
		crate::job::report(&self.app, self.invoices()?.as_ref(), self.nav()?.as_ref(), invoice_id)
			.await
	}

	pub(crate) async fn run_poll(&self, submission_id: i64) -> ClResult<()> {
		crate::job::poll(&self.app, self.invoices()?.as_ref(), self.nav()?.as_ref(), submission_id)
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
/// `AppBuilder::alerts(saas_nav::service_api::alerts)`.
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
	let count = nav_store(&app)?.awaiting_operator(SELLER_ID).await?;
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
		link: Some("/api/admin/nav-submissions?verdict=REJECTED,FAILED".into()),
	}])
}

// vim: ts=4
