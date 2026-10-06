//! Building and pricing a `DRAFT` invoice.
//!
//! A draft *is* the cart: mutable, unnumbered, invisible to NAV, and swept after
//! `settings['invoice.draft_ttl_days']`. Everything here is the arithmetic and the resolution
//! around it; [`crate::service_api::Invoices`] is the authorized front.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use mintworks_core::app::App;
use mintworks_core::job::{self, Runner};
use mintworks_core::prelude::*;
use mintworks_core::store::CoreStore;

use crate::currency::{self, Currency};
use crate::money::{Discount, DraftLine};
use crate::store::{
	self, InvoiceLine, InvoiceStore, InvoiceVatGroup, NewInvoiceLine, PaymentMethod, Service,
};
use crate::taxrule::Verdict;
use crate::vat::{self, VatCode};

/// The one `E-INV-LINE` message the two guards for this rule give: a catalogue line takes its
/// price and tax code from the `services` row, so carrying either is an error rather than an
/// override. [`resolve`] owns the rule for a new line — the router used to hold a copy, and
/// `Invoices::draft` bypassed it — while [`crate::service_api::Invoices::edit_line`] tests a
/// *stored* line's `service_id`, a different shape that cannot share the check.
pub(crate) const CATALOGUE_LINE_MSG: &str =
	"a catalogue line takes its price and vat code from the service";

/// Which billing party an invoice is for.
#[derive(Clone, Debug, Default)]
pub enum Party {
	/// The org's `is_default` party — what a subscription renewal bills.
	#[default]
	OrgDefault,
	Uid(PartyId),
}

/// One line as a caller names it, before the `services` row is read.
///
/// `code` set is a catalogue line: description, unit, price and default VAT code come from the
/// `services` row and the price is converted into the invoice currency. `code` unset is ad hoc —
/// the consumer owns what the line costs and why.
#[derive(Clone, Debug)]
pub struct Line {
	pub code: Option<String>,
	pub description: String,
	pub unit: String,
	pub qty: Qty,
	/// `None` means "unset", which is the only thing a catalogue line may say. They used to
	/// be `Money::ZERO`/`Std27` sentinels, indistinguishable from a caller's assertion, so
	/// [`resolve`] could only discard a catalogue line's price silently.
	pub unit_price: Option<Money>,
	pub vat_code: Option<VatCode>,
	pub discount: Option<Discount>,
	pub discount_description: Option<String>,
	/// Caller free text. [`resolve`] overwrites a catalogue line's description, never this.
	pub note: Option<String>,
}

impl Line {
	/// A catalogue line. The four unset fields are filled from the `services` row.
	pub fn code(code: impl Into<String>, qty: Qty) -> Self {
		Self {
			code: Some(code.into()),
			description: String::new(),
			unit: String::new(),
			qty,
			unit_price: None,
			vat_code: None,
			discount: None,
			discount_description: None,
			note: None,
		}
	}

	/// An ad-hoc line. `unit` is required because `invoice_lines.unit` is `NOT NULL` and
	/// NAV wants `unitOfMeasure`; `unit_price` is already in the invoice currency.
	pub fn adhoc(
		description: impl Into<String>,
		unit: impl Into<String>,
		qty: Qty,
		unit_price: Money,
		vat_code: VatCode,
	) -> Self {
		Self {
			code: None,
			description: description.into(),
			unit: unit.into(),
			qty,
			unit_price: Some(unit_price),
			vat_code: Some(vat_code),
			discount: None,
			discount_description: None,
			note: None,
		}
	}
}

/// What [`crate::service_api::Invoices::draft`] and `issue_now` take. `request_id` is the
/// consumer's idempotency key: replaying it returns the invoice already made.
#[derive(Clone, Debug, Default)]
pub struct NewDraft {
	pub request_id: Option<String>,
	pub billing_party: Party,
	pub lines: Vec<Line>,
	/// Apportioned across the lines pro rata by net, so every VAT group keeps its share.
	pub discount: Option<Discount>,
	pub payment_method: Option<PaymentMethod>,
	/// Defaults to the org's `billing_currency`, then to `settings['currency.base']`.
	pub currency: Option<CurrencyCode>,
	pub fulfilment_date: Option<String>,
	pub due_date: Option<String>,
	/// The settlement period (Áfa tv. 58. §); `fulfilment_date` is then derived at issue.
	pub period_start: Option<String>,
	pub period_end: Option<String>,
	pub notes: Option<String>,
}

/// `issue_now` takes exactly what `draft` takes — it is the same request, issued in one go.
pub type IssueNow = NewDraft;

/// The invoice currency alone. A [`Currency`] carries no rate, so unlike [`currency_for`] this
/// needs no date and does no rate lookup — which is all a caller wanting the decimal scale is
/// after.
pub async fn currency_only(
	app: &App,
	store: &dyn InvoiceStore,
	org_id: i64,
	asked: Option<&CurrencyCode>,
) -> ClResult<Currency> {
	let base = CurrencyCode::parse(&app.settings.text("currency.base").await?)?;
	let code = if let Some(code) = asked {
		code.clone()
	} else {
		store.org_billing_currency(org_id).await?.unwrap_or_else(|| base.clone())
	};
	currency::get(store, &code).await
}

/// The invoice currency and the base -> billing rate frozen onto the draft.
pub async fn currency_for(
	app: &App,
	store: &dyn InvoiceStore,
	org_id: i64,
	asked: Option<&CurrencyCode>,
	date: &str,
) -> ClResult<(Currency, i64)> {
	let base = CurrencyCode::parse(&app.settings.text("currency.base").await?)?;
	let cur = currency_only(app, store, org_id, asked).await?;
	let source = app.settings.text("currency.rate_source").await?;
	let max_age = app.settings.int("currency.max_rate_age_days").await?;
	let rate =
		currency::effective_rate_e6(store, &cur, &base, &base, &source, date, max_age).await?;
	Ok((cur, rate))
}

/// Read the `services` rows a line set names and turn the whole set into [`DraftLine`]s.
///
/// `org_id` is the *seller's* org, whose catalogue the codes resolve against — not the org the
/// invoice belongs to, which is the buyer's.
///
/// The backing `services.id` rides on each line ([`DraftLine::service_id`]) rather than in a
/// slice beside it, so a `PricingHook` that adds or removes a line cannot desynchronise the
/// two.
pub async fn resolve(
	store: &dyn InvoiceStore,
	org_id: i64,
	cur: &Currency,
	rate_e6: i64,
	lines: &[Line],
) -> ClResult<Vec<DraftLine>> {
	let bad = |msg: &'static str| Error::coded(StatusCode::BAD_REQUEST, "E-INV-LINE", msg);

	// One query for the whole line set: a 500-line draft made 500 reader round-trips.
	let codes: Vec<&str> = {
		let mut c: Vec<&str> = lines.iter().filter_map(|l| l.code.as_deref()).collect();
		c.sort_unstable();
		c.dedup();
		c
	};
	let by_code: HashMap<String, Service> = store
		.services_by_codes(org_id, &codes)
		.await?
		.into_iter()
		.filter_map(|s| s.code.clone().map(|c| (c, s)))
		.collect();

	let mut out = Vec::with_capacity(lines.len());
	for line in lines {
		let (id, description, unit, unit_price, vat_code) = if let Some(code) = &line.code {
			// The trust boundary is this service handle, not the router: `Invoices::draft`
			// and `issue_now` reach here directly, and the price a catalogue line carried
			// used to be discarded here with no diagnostic and billed at catalogue price.
			if line.unit_price.is_some() || line.vat_code.is_some() {
				return Err(bad(CATALOGUE_LINE_MSG));
			}
			let svc = by_code.get(code.as_str()).ok_or(Error::NotFound)?;
			let price = currency::price_in(svc.unit_price, cur, rate_e6)?;
			(Some(svc.id), svc.name.clone(), svc.unit.clone(), price, svc.vat_code)
		} else {
			let (Some(price), Some(vat_code)) = (line.unit_price, line.vat_code) else {
				return Err(bad("an ad-hoc line needs a unit price and a vat code"));
			};
			// The same `price_round_step` a catalogue line gets through `price_in` above,
			// but *checked* rather than applied: this price is the caller's assertion, and
			// rewriting it silently billed a different amount than the one posted.
			let price = currency::ensure_price_on_step(price, cur.price_round_step)?;
			(None, line.description.clone(), line.unit.clone(), price, vat_code)
		};
		out.push(DraftLine {
			service_id: id,
			description,
			unit,
			qty: line.qty,
			unit_price,
			vat_code,
			discount: line.discount,
			discount_description: line.discount_description.clone(),
			note: line.note.clone(),
		});
	}
	Ok(out)
}

/// A priced line set, ready for `create_draft` + `replace_draft_lines`, or for `issue`.
pub struct Priced {
	pub lines: Vec<NewInvoiceLine>,
	pub groups: Vec<InvoiceVatGroup>,
	pub net: Money,
	pub vat: Money,
	pub gross: Money,
}

/// How many lines one invoice may carry. A deliberate policy bound, not a protocol limit:
/// `pdf::run` hands the table to `typst::compile` in a `spawn_blocking` with **no** timeout
/// and eight retries behind it, `insert_lines` holds the single writer connection for the
/// whole batch, and the resulting `invoiceData` is base64 on a `manageInvoice` request.
/// Raising it means re-measuring the typst render first.
pub const MAX_LINES: usize = 500;

/// Apply the buyer's verdict to every line, run the VAT engine, and shape the result into
/// store rows. At most [`MAX_LINES`] of them.
///
/// `huf_rate_e6` fills the `*_huf` trio on each group, computed **per group** rather than
/// summed from rounded line values. It is `None` on a draft, where no rate is frozen yet,
/// and mandatory at issue on a foreign-currency invoice (Áfa tv. 172. §).
///
/// **This function is where that rule is kept, together with its only issue-path caller** —
/// no `CHECK` backs it up, because a `CHECK` cannot reach the parent invoice's `currency`. A
/// construction rather than a guard: `issue::plan` resolves `huf_rate_e6` to `Some(_)` for
/// every non-HUF currency and the `huf` closure below yields `Some(_)` for exactly those.
/// `storno::run` copies the trio instead of calling this, and checks it explicitly.
///
/// `freeze_vat` decides what goes into `invoice_lines.vat_code`. On a draft it is `false` and
/// the line keeps the **product's own** code, so the next re-price still sees what the caller
/// asked for — persisting the verdict there is a one-way door, since a buyer change back to
/// domestic could never undo an `HO` override. At issue it is `true` and the winning code is
/// frozen, which is what the PDF and the NAV filing must agree on. The money is the same
/// either way: `amounts` always comes from `computed`, over `effective`.
pub fn price(
	invoice_id: i64,
	lines: &[DraftLine],
	invoice_discount: Option<Discount>,
	verdict: &Verdict,
	huf_rate_e6: Option<i64>,
	freeze_vat: bool,
	vat_round_step: i64,
) -> ClResult<Priced> {
	// Every entry path re-prices the whole set and funnels here, so one check covers them all.
	// axum's 2 MB `Json` cap bounded a wire draft at ~20k lines and a Rust caller at nothing.
	if lines.len() > MAX_LINES {
		return Err(Error::coded(
			axum::http::StatusCode::BAD_REQUEST,
			"E-INV-LINE",
			format!("an invoice may carry at most {MAX_LINES} lines"),
		));
	}

	// The verdict overrides every line, which is how a caller cannot talk the framework out
	// of reverse charge or export. `Verdict::Product` leaves each line's own code alone.
	let effective: Vec<DraftLine> = lines
		.iter()
		.map(|l| DraftLine { vat_code: verdict.effective(l.vat_code), ..l.clone() })
		.collect();

	let computed = vat::compute(&effective, invoice_discount, vat_round_step)?;
	let mut out = Vec::with_capacity(lines.len());
	for ((line, orig), amounts) in effective.iter().zip(lines).zip(&computed.lines) {
		let stored_code = if freeze_vat { line.vat_code } else { orig.vat_code };
		// `qty`, `unit_price` and an `AMOUNT` discount's magnitude are **not** re-checked here:
		// `vat::compute` above is the guard of record. What follows is what it never looks at —
		// the text fields and the net it produced. The trust boundary is the service handle, not
		// the router: `issue_now` builds a draft with no consumer in the loop.
		let line_error = |msg: &'static str| {
			Err(Error::coded(axum::http::StatusCode::BAD_REQUEST, "E-INV-LINE", msg))
		};
		if line.description.trim().is_empty() || line.unit.trim().is_empty() {
			return line_error("a line needs a description and a unit");
		}
		// Too long fails NAV's schema exactly as blank does, and this is the one funnel
		// every draft, catalogue and issue-now line reaches. See `store::bounded_text`.
		store::bounded_text("description", &line.description, store::MAX_DESCRIPTION)?;
		store::bounded_text("unit", &line.unit, store::MAX_UNIT)?;
		if let Some(d) = &line.discount_description {
			// Blank is not absent: `discountDescription` is `SimpleText255NotBlankType`, so a
			// whitespace-only value filed, was rejected on all eight attempts and then hourly
			// forever. Same rule as `description` and `unit` above; absent stays legal.
			if d.trim().is_empty() {
				return line_error("a discount description cannot be blank");
			}
			store::bounded_text("discountDescription", d, store::MAX_DISCOUNT_DESCRIPTION)?;
		}
		// `bounded_multiline_text`, not `bounded_text`: `note` reaches no `SimpleText*NotBlankType`,
		// so a line break in it is a formatting choice. Blank stays legal for the same reason.
		if let Some(n) = &line.note {
			store::bounded_multiline_text("note", n, store::MAX_LINE_NOTE)?;
		}
		if amounts.net.0 < 0 {
			return Err(Error::coded(
				axum::http::StatusCode::BAD_REQUEST,
				"E-INV-DISCOUNT",
				"the discount exceeds the line net",
			));
		}
		let (discount_kind, discount_value) = crate::money::discount_parts(line.discount);
		out.push(NewInvoiceLine {
			service_id: line.service_id,
			description: line.description.clone(),
			unit: line.unit.clone(),
			qty: line.qty,
			unit_price: line.unit_price,
			discount_kind,
			discount_value,
			discount_amount: amounts.discount_amount,
			discount_description: line.discount_description.clone(),
			net: amounts.net,
			vat_code: stored_code,
			// The rate follows the money, not `stored_code`: `amounts` is computed over
			// `effective`, so on a draft the applying figure is the verdict's. Storing the
			// product's rate here put `2700` beside `vat: 0.00`.
			vat_rate_bp: line.vat_code.rate_bp(),
			vat: amounts.vat,
			gross: amounts.gross,
			note: line.note.clone(),
		});
	}

	let mut groups = Vec::with_capacity(computed.groups.len());
	for g in &computed.groups {
		let huf = |amount: Money| match huf_rate_e6 {
			Some(rate) => currency::to_base(amount, rate).map(Some),
			None => Ok(None),
		};
		// `gross_huf` is derived, not rounded: three independent roundings need not sum, and
		// these are the figures Áfa tv. 172. § makes mandatory and NAV cross-validates — the
		// section is about *converting* a foreign-currency invoice's áthárított adó to forint,
		// which is a reported legal figure and takes no step, unlike the invoice's own VAT in
		// `vat::compute`. `sum_bounded` because `Money`'s `Add` is unchecked.
		let (net_huf, vat_huf) = (huf(g.net)?, huf(g.vat)?);
		let gross_huf = match (net_huf, vat_huf) {
			(Some(n), Some(v)) => Some(vat::sum_bounded([n, v])?),
			_ => None,
		};
		groups.push(InvoiceVatGroup {
			invoice_id,
			vat_code: g.vat_code,
			vat_rate_bp: g.vat_rate_bp,
			net: g.net,
			vat: g.vat,
			gross: g.gross,
			net_huf,
			vat_huf,
			gross_huf,
		});
	}

	Ok(Priced { lines: out, groups, net: computed.net, vat: computed.vat, gross: computed.gross })
}

/// A stored line read back as engine input, so editing one line re-prices the whole set
/// without losing what the caller originally asked for.
///
/// Only the line's *own* discount comes back. `discount_amount` also holds its share of the
/// invoice-level discount, which the re-price re-applies from `invoices.discount_kind` — so
/// reading the column back here as a line discount would charge that share twice.
/// # Errors
/// `E-INV-DISCOUNT` when the stored column pair is not a discount. The check belongs to
/// [`crate::money::discount_of`], the single funnel: hand-rolled here as
/// `u32::try_from(v).unwrap_or(0)`, an out-of-range stored percentage became *no discount at
/// all* and issued and filed with no diagnostic. `invoice_lines.discount_value` has no sign
/// `CHECK`, and a consumer's own SQL writes to it too.
pub fn to_draft(line: &InvoiceLine) -> ClResult<DraftLine> {
	let discount = crate::money::discount_of(line.discount_kind, line.discount_value)?;
	Ok(DraftLine {
		service_id: line.service_id,
		description: line.description.clone(),
		unit: line.unit.clone(),
		qty: line.qty,
		unit_price: line.unit_price,
		vat_code: line.vat_code,
		discount,
		discount_description: line.discount_description.clone(),
		note: line.note.clone(),
	})
}

/// The `SWEEP_DRAFTS` job kind. `mintworks-core` reserves it and registers no handler.
pub const KIND_SWEEP: &str = "SWEEP_DRAFTS";

/// The daily housekeeping tick. Two unrelated recoveries share it because both are
/// deadline-free and neither is worth a second job kind and a second `seed_periodic`:
///
/// * abandoned carts older than `settings['invoice.draft_ttl_days']` are deleted;
/// * up to `settings['invoice.pdf_sweep_batch']` issued invoices with no document row get a
///   fresh `RENDER_PDF`. What it recovers is the render that was never enqueued at all;
///   `jobs.max_attempts.RENDER_PDF` is `0`, so a render that *was* enqueued retries by itself
///   and the sweep is only a backstop. A terminally `FAILED` one keeps `pdf:invoice:{id}`
///   forever — `dedup_key` survives termination — which now should not happen and is logged as
///   an error needing a person, exactly as a spent NAV filing key is.
pub fn register(runner: &mut Runner, app: App, store: std::sync::Arc<dyn InvoiceStore>) {
	runner.register_periodic(KIND_SWEEP, 86_400, move |_job| {
		let (app, store) = (app.clone(), store.clone());
		async move {
			let days = app.settings.int("invoice.draft_ttl_days").await?;
			// Saturating for the reason `mintworks_core::app`'s job sweep is: `overflow-checks` is
			// on in release, so an operator-set `invoice.draft_ttl_days` large enough to
			// overflow the multiply would panic inside the handler rather than sweep nothing.
			let cutoff = Timestamp(Timestamp::now().0.saturating_sub(days.saturating_mul(86_400)));
			let gone = store.sweep_drafts(cutoff).await?;
			tracing::info!(gone, "swept abandoned drafts");

			let batch = app.settings.int("invoice.pdf_sweep_batch").await?;
			for id in store.issued_without_document(batch).await? {
				let enqueued = job::enqueue(
					&app.store,
					crate::issue::KIND_RENDER_PDF,
					&crate::issue::invoice_job_payload(id),
					Some(&format!("pdf:invoice:{id}")),
					Timestamp::now(),
				)
				.await?;
				// `job_terminate` keeps the `dedup_key`, so a terminally failed render holds it
				// for good and the enqueue above is a no-op. Silent here too once, and the
				// invoice never got its PDF — `document` answers `E-INV-PDF-PENDING` forever.
				if enqueued.is_some() {
					tracing::warn!(invoice = id, "an issued invoice had no PDF; re-enqueued");
				} else {
					tracing::error!(
						invoice = id,
						"an issued invoice has no PDF and its RENDER_PDF key is already spent; \
						 it needs an operator re-drive"
					);
				}
			}
			Ok(())
		}
	});
}

/// Seeds the periodic sweep. Call once at boot.
pub async fn seed(store: &Arc<dyn CoreStore>) -> ClResult<()> {
	job::seed_periodic(store, KIND_SWEEP).await
}

// vim: ts=4
