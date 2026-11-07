// SPDX-License-Identifier: MPL-2.0
//! The invoice handlers, and the four opt-in route bundles this crate exposes.
//!
//! A bundle is the unit of exposure and its membership is a contract: nothing is served that
//! the application did not merge, and a later phase may not move a route between bundles
//! without a `## Revisions` entry. Most consumers mount [`org_read`] and [`org_parties`]
//! and leave [`org_invoices`] unmounted, billing through the [`Invoices`] service instead.
//!
//! **Handlers decide nothing.** Org scoping, authorization and the audit trail are all in
//! [`Invoices`]. Two calls appear in some handlers, and neither is a second decision:
//! [`Invoices::currency`] and [`Invoices::invoice_currency`] answer "how many decimals does
//! this amount have", which is parsing, and [`Invoices::hydrate`] turns a returned row into
//! something renderable. The mutation itself is always exactly one call.
//!
//! Each bundle layers `mintworks_core::auth_mw::require_auth` itself, so a consumer mounts it
//! as-is; all it owes is the `App` request extension `AppBuilder::run` supplies.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use mintworks_core::app::App;
use mintworks_core::auth_mw::{ClientIp, RouteGate};
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::catalog;
use crate::currency::Currency;
use crate::draft::{Line, NewDraft, Party};
use crate::issue::IssuePlan;
use crate::money::discount_of;
use crate::party;
use crate::service_api::{FullInvoice, Invoices, LinePatch};
use crate::store::{
	DiscountKind, Invoice, InvoiceDocument, InvoiceFilter, InvoiceKind, InvoiceLine, InvoicePatch,
	InvoiceStatus, InvoiceVatGroup, OrgCurrency, PartyKind, PaymentMethod, RateSource,
	SellerSeries, SeriesPatch,
};
use crate::taxrule::vat_notes;
use crate::vat::VatCode;

// ---------------------------------------------------------------- pagination

/// A collection response. Every list is shaped this way, so `nextCursor` is present and null
/// on a collection the store returns whole rather than being absent.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
	pub items: Vec<T>,
	/// Opaque; send it back as `?cursor=`. `None` when this was the last page.
	pub next_cursor: Option<String>,
	/// Rows the filter matches across every page; only the invoice list counts.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub total: Option<u64>,
}

impl<T> Page<T> {
	pub fn all(items: Vec<T>) -> Self {
		Self { items, next_cursor: None, total: None }
	}
}

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
	#[serde(default)]
	pub cursor: Option<String>,
	#[serde(default)]
	pub limit: Option<i64>,
	/// Comma-separated, e.g. `status=DRAFT,ISSUED`. An unknown name is `E-CORE-VALIDATION`,
	/// not a silently empty page.
	#[serde(default)]
	pub status: Option<String>,
	#[serde(default)]
	pub q: Option<String>,
	/// `issueDate`, `gross`, `dueDate` or `partner`, `-` first for descending. Set, the page is
	/// addressed by `offset` and `cursor` is refused.
	#[serde(default)]
	pub sort: Option<String>,
	#[serde(default)]
	pub offset: Option<i64>,
	/// Inclusive `YYYY-MM-DD` issue dates; either one excludes drafts.
	#[serde(default)]
	pub from: Option<String>,
	#[serde(default)]
	pub to: Option<String>,
	/// A `prt_` uid in the caller's org.
	#[serde(default)]
	pub party: Option<String>,
}

// ---------------------------------------------------------------- wire types

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuyerView {
	pub kind: Option<PartyKind>,
	pub name: Option<String>,
	pub country: Option<String>,
	pub tax_number: Option<String>,
	pub eu_vat_id: Option<String>,
	pub group_tax_no: Option<String>,
	pub postcode: Option<String>,
	pub city: Option<String>,
	pub street: Option<String>,
}

/// One `invoice_lines` row. `vat` and `gross` are display values apportioned back out of the group
/// figure; `vatSummary` is the authoritative VAT.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineView {
	pub line_no: i64,
	pub description: String,
	pub unit: String,
	pub qty: Qty,
	pub unit_price: MoneyWire,
	pub discount_kind: Option<DiscountKind>,
	pub discount_value: Option<i64>,
	pub discount_amount: MoneyWire,
	pub discount_description: Option<String>,
	pub net: MoneyWire,
	pub vat_code: VatCode,
	pub vat_rate_bp: i64,
	pub vat: MoneyWire,
	pub gross: MoneyWire,
	pub note: Option<String>,
}

impl LineView {
	fn of(l: InvoiceLine, cur: &CurrencyCode) -> Self {
		Self {
			line_no: l.line_no,
			description: l.description,
			unit: l.unit,
			qty: l.qty,
			unit_price: l.unit_price.to_wire(cur),
			discount_kind: l.discount_kind,
			discount_value: l.discount_value,
			discount_amount: l.discount_amount.to_wire(cur),
			discount_description: l.discount_description,
			net: l.net.to_wire(cur),
			vat_code: l.vat_code,
			vat_rate_bp: l.vat_rate_bp,
			vat: l.vat.to_wire(cur),
			gross: l.gross.to_wire(cur),
			note: l.note,
		}
	}
}

/// One `invoice_vat_groups` row. The `*Huf` trio is present only on a foreign-currency
/// invoice, where Áfa tv. 172. § requires the HUF VAT figure.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VatGroupView {
	pub vat_code: VatCode,
	pub vat_rate_bp: i64,
	pub net: MoneyWire,
	pub vat: MoneyWire,
	pub gross: MoneyWire,
	pub net_huf: Option<MoneyWire>,
	pub vat_huf: Option<MoneyWire>,
	pub gross_huf: Option<MoneyWire>,
}

impl VatGroupView {
	fn of(g: &InvoiceVatGroup, cur: &CurrencyCode) -> Self {
		let huf_code = CurrencyCode::huf();
		let huf = |m: Option<Money>| m.map(|m| m.to_wire(&huf_code));
		Self {
			vat_code: g.vat_code,
			vat_rate_bp: g.vat_rate_bp,
			net: g.net.to_wire(cur),
			vat: g.vat.to_wire(cur),
			gross: g.gross.to_wire(cur),
			net_huf: huf(g.net_huf),
			vat_huf: huf(g.vat_huf),
			gross_huf: huf(g.gross_huf),
		}
	}
}

/// The rendered PDF's metadata. Present once `RENDER_PDF` has run; absent on a draft and on an
/// issued invoice whose render is still queued.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentView {
	pub kind: String,
	pub sha256: String,
	pub bytes: i64,
	pub template_version: String,
	pub rendered_at: Timestamp,
}

impl DocumentView {
	fn of(d: InvoiceDocument) -> Self {
		Self {
			kind: d.kind,
			sha256: d.sha256,
			bytes: d.bytes,
			template_version: d.template_version,
			rendered_at: d.rendered_at,
		}
	}
}

/// An `invoices` row. `lines` and `vatSummary` are present on a single read and absent from a
/// listing; `hufRate` is set only when `currency` is not HUF.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvoiceView {
	pub uid: InvoiceId,
	pub number: Option<String>,
	pub kind: InvoiceKind,
	pub status: InvoiceStatus,
	pub billing_party_uid: Option<PartyId>,
	/// Set on a `STORNO` (what it cancels) and on a `STORNOED` invoice (what cancelled it)
	/// respectively — `notes` carries only the free-text reason, so without these nothing pairs
	/// the two documents. `null` rather than absent on every other invoice, which is how every
	/// other optional field of this view behaves.
	pub original_invoice_uid: Option<InvoiceId>,
	pub storno_invoice_uid: Option<InvoiceId>,
	pub series_code: Option<String>,
	pub test_mode: bool,
	pub doc_lang: Option<String>,
	pub issued_at: Option<Timestamp>,
	pub fulfilment_date: Option<String>,
	pub due_date: Option<String>,
	pub period_start: Option<String>,
	pub period_end: Option<String>,
	pub payment_method: PaymentMethod,
	pub currency: CurrencyCode,
	pub huf_rate: Option<String>,
	pub rate_date: Option<String>,
	pub rate_source: Option<RateSource>,
	pub net: MoneyWire,
	pub vat: MoneyWire,
	pub gross: MoneyWire,
	pub paid_amount: MoneyWire,
	pub paid_at: Option<Timestamp>,
	/// Every applicable Áfa tv. 169. § note key, in the order `issue::prepare` froze them.
	/// A list because a mixed exempt invoice owes a reference per exempt supply — the PDF
	/// prints exactly this.
	pub vat_notes: Vec<String>,
	pub notes: Option<String>,
	/// The invoice-level discount as asked for. The resolved figure is apportioned into each
	/// line's `discountAmount`.
	pub discount_kind: Option<DiscountKind>,
	/// Basis points, on `PERCENT` only.
	pub discount_value: Option<i64>,
	/// On `AMOUNT` only: an amount crosses the wire as a string, never as bare minor units.
	pub discount_fixed: Option<MoneyWire>,
	pub buyer: Option<BuyerView>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub lines: Option<Vec<LineView>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub vat_summary: Option<Vec<VatGroupView>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub document: Option<DocumentView>,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
}

impl InvoiceView {
	/// Public because a consumer that mounts none of these bundles still serves this shape:
	/// [`Invoices::hydrate`] plus this is the whole of `GET /api/invoices/{uid}`'s body.
	pub fn of(full: FullInvoice) -> Self {
		let FullInvoice {
			invoice: i,
			party_uid,
			original_invoice_uid,
			storno_invoice_uid,
			lines,
			groups,
			document,
		} = full;
		let cur = i.currency.clone();
		let money = |m: Money| m.to_wire(&cur);
		// The snapshot is written at ISSUE; a draft has no buyer to show yet.
		let buyer = i.buyer_name.is_some().then(|| BuyerView {
			kind: i.buyer_kind,
			name: i.buyer_name.clone(),
			country: i.buyer_country.clone(),
			tax_number: i.buyer_tax_number.clone(),
			eu_vat_id: i.buyer_eu_vat_id.clone(),
			group_tax_no: i.buyer_group_tax_no.clone(),
			postcode: i.buyer_postcode.clone(),
			city: i.buyer_city.clone(),
			street: i.buyer_street.clone(),
		});
		Self {
			uid: i.uid,
			number: i.number,
			kind: i.kind,
			status: i.status,
			billing_party_uid: party_uid,
			original_invoice_uid,
			storno_invoice_uid,
			series_code: i.series_code,
			test_mode: i.test_mode,
			doc_lang: i.doc_lang,
			issued_at: i.issued_at,
			fulfilment_date: i.fulfilment_date,
			due_date: i.due_date,
			period_start: i.period_start,
			period_end: i.period_end,
			payment_method: i.payment_method,
			// A rate is scaled 1e6 and is not money.
			huf_rate: i.huf_rate_e6.map(|r| format_scaled(r, 6)),
			rate_date: i.rate_date,
			rate_source: i.rate_source,
			net: money(i.net),
			vat: money(i.vat),
			gross: money(i.gross),
			paid_amount: money(i.paid_amount),
			paid_at: i.paid_at,
			vat_notes: vat_notes(i.vat_note.as_deref()).into_iter().map(str::to_owned).collect(),
			notes: i.notes,
			discount_kind: i.discount_kind,
			discount_value: i
				.discount_value
				.filter(|_| i.discount_kind == Some(DiscountKind::Percent)),
			discount_fixed: i
				.discount_value
				.filter(|_| i.discount_kind == Some(DiscountKind::Amount))
				.map(|v| money(Money(v))),
			buyer,
			lines: lines.map(|ls| {
				let mut views: Vec<LineView> =
					ls.into_iter().map(|l| LineView::of(l, &cur)).collect();
				// `draft.rs` stores each draft line's *product* code so a buyer change can
				// re-derive the verdict — but `vatCode` is the join key to `vatSummary`.
				// `Verdict::Override` collapses every line onto one code, which is exactly the
				// one-group case; `Verdict::Product` with one group means every line already
				// carries it, so the assignment is a no-op there.
				if let Some([only]) = groups.as_deref() {
					for v in &mut views {
						v.vat_code = only.vat_code;
					}
				}
				views
			}),
			vat_summary: groups.map(|gs| gs.iter().map(|g| VatGroupView::of(g, &cur)).collect()),
			document: document.map(DocumentView::of),
			currency: cur,
			created_at: i.created_at,
			updated_at: i.updated_at,
		}
	}
}

/// One line of a draft.
///
/// `serviceCode` and not `serviceUid`: a catalogue line is resolved through `services.code`,
/// which is the identity `sync_services` upserts on, and no store method resolves a line by
/// service uid.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineBody {
	#[serde(default)]
	pub service_code: Option<String>,
	#[serde(default)]
	pub description: Option<String>,
	#[serde(default)]
	pub unit: Option<String>,
	pub qty: Qty,
	#[serde(default)]
	pub unit_price: Option<MoneyWire>,
	#[serde(default)]
	pub vat_code: Option<VatCode>,
	#[serde(default)]
	pub discount_kind: Option<DiscountKind>,
	#[serde(default)]
	pub discount_value: Option<i64>,
	#[serde(default)]
	pub discount_description: Option<String>,
	#[serde(default)]
	pub note: Option<String>,
}

impl LineBody {
	/// The four unset fields of a catalogue line are filled from the `services` row later,
	/// by `draft::resolve`; an ad-hoc line must carry its own.
	fn into_line(self, cur: &Currency) -> ClResult<Line> {
		let bad = |msg: &'static str| Error::coded(StatusCode::BAD_REQUEST, "E-INV-LINE", msg);
		// `serviceCode` together with `unitPrice`/`vatCode` is refused by `draft::resolve`,
		// not here: the service handle is the trust boundary and a second copy of the rule
		// only let `Invoices::draft` keep bypassing it.
		let unit_price = self.unit_price.as_ref().map(|w| catalog::money_in(w, cur)).transpose()?;
		let discount = discount_of(self.discount_kind, self.discount_value)?;
		// Blank is not absent: NAV's `lineDescription` is `SimpleText512NotBlankType`, so an
		// empty string is filed, rejected, and retried eight times, then hourly by
		// `NAV_SWEEP` forever.
		let blank = |s: &Option<String>| s.as_ref().is_none_or(|v| v.trim().is_empty());
		if self.service_code.is_none() {
			if blank(&self.description) || blank(&self.unit) {
				return Err(bad("an ad-hoc line needs a description and a unit"));
			}
			// No `services` row to fall back on, and `Money::ZERO` passes every check below
			// it — the step check, the sign check and `draft::price` — so an omitted price
			// was a `201` and a free line. An explicit `"0.00"` stays legal.
			if self.unit_price.is_none() {
				return Err(bad("an ad-hoc line needs a unit price"));
			}
		}
		// `vatCode` is optional on the wire for an ad-hoc line and defaults to `STD27`; on a
		// catalogue line it stays `None`, which is what `resolve` refuses.
		let vat_code = match &self.service_code {
			Some(_) => self.vat_code,
			None => Some(self.vat_code.unwrap_or(VatCode::Std27)),
		};
		Ok(Line {
			unit_price,
			discount,
			vat_code,
			code: self.service_code,
			description: self.description.unwrap_or_default(),
			unit: self.unit.unwrap_or_default(),
			qty: self.qty,
			discount_description: self.discount_description,
			note: self.note,
		})
	}
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewDraftBody {
	#[serde(default)]
	pub request_id: Option<String>,
	#[serde(default)]
	pub billing_party_uid: Option<String>,
	#[serde(default)]
	pub currency: Option<CurrencyCode>,
	#[serde(default)]
	pub payment_method: Option<PaymentMethod>,
	#[serde(default)]
	pub fulfilment_date: Option<String>,
	#[serde(default)]
	pub due_date: Option<String>,
	#[serde(default)]
	pub period_start: Option<String>,
	#[serde(default)]
	pub period_end: Option<String>,
	#[serde(default)]
	pub notes: Option<String>,
	#[serde(default)]
	pub discount_kind: Option<DiscountKind>,
	#[serde(default)]
	pub discount_value: Option<i64>,
	#[serde(default)]
	pub lines: Vec<LineBody>,
	/// One of `GET /api/seller/series`; absent takes the default series.
	#[serde(default)]
	pub series_code: Option<String>,
}

impl NewDraftBody {
	fn into_draft(self, cur: &Currency) -> ClResult<NewDraft> {
		let billing_party = match &self.billing_party_uid {
			Some(uid) => Party::Uid(PartyId::parse(uid)?),
			None => Party::OrgDefault,
		};
		let mut lines = Vec::with_capacity(self.lines.len());
		for line in self.lines {
			lines.push(line.into_line(cur)?);
		}
		Ok(NewDraft {
			billing_party,
			lines,
			discount: discount_of(self.discount_kind, self.discount_value)?,
			request_id: self.request_id,
			payment_method: self.payment_method,
			currency: self.currency,
			fulfilment_date: self.fulfilment_date,
			due_date: self.due_date,
			period_start: self.period_start,
			period_end: self.period_end,
			notes: self.notes,
			series_code: self.series_code,
		})
	}
}

/// Three-state on the nullable columns; `billingPartyUid` and `currency` are resolved
/// into the internal `billing_party_id` and `rate_e6` by [`Invoices::patch_by_uid`].
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvoicePatchBody {
	#[serde(default)]
	pub billing_party_uid: Option<String>,
	#[serde(default)]
	pub currency: Option<CurrencyCode>,
	#[serde(default)]
	pub payment_method: Option<PaymentMethod>,
	#[serde(default)]
	pub fulfilment_date: Patch<String>,
	#[serde(default)]
	pub due_date: Patch<String>,
	#[serde(default)]
	pub period_start: Patch<String>,
	#[serde(default)]
	pub period_end: Patch<String>,
	#[serde(default)]
	pub notes: Patch<String>,
	/// `null` returns the draft to the seller's default series.
	#[serde(default)]
	pub series_code: Patch<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinePatchBody {
	#[serde(default)]
	pub description: Option<String>,
	#[serde(default)]
	pub qty: Option<Qty>,
	#[serde(default)]
	pub unit_price: Option<MoneyWire>,
	#[serde(default)]
	pub vat_code: Option<VatCode>,
	#[serde(default)]
	pub discount_kind: Patch<DiscountKind>,
	#[serde(default)]
	pub discount_value: Patch<i64>,
	#[serde(default)]
	pub discount_description: Patch<String>,
	#[serde(default)]
	pub note: Patch<String>,
}

impl LinePatchBody {
	fn into_patch(self, cur: &Currency) -> ClResult<LinePatch> {
		// `LinePatch::discount` is `Option<Option<_>>`: outer absent leaves it alone, inner
		// `None` clears it. An explicit `null` on either half is the clear.
		let discount = match (self.discount_kind, self.discount_value) {
			(Patch::Undefined, Patch::Undefined) => None,
			(Patch::Null, _) | (_, Patch::Null) => Some(None),
			(kind, value) => Some(discount_of(
				match kind {
					Patch::Value(k) => Some(k),
					_ => None,
				},
				match value {
					Patch::Value(v) => Some(v),
					_ => None,
				},
			)?),
		};
		let unit_price = match &self.unit_price {
			Some(w) => Some(catalog::money_in(w, cur)?),
			None => None,
		};
		Ok(LinePatch {
			unit_price,
			discount,
			description: self.description,
			qty: self.qty,
			vat_code: self.vat_code,
			discount_description: match self.discount_description {
				Patch::Undefined => None,
				Patch::Null => Some(None),
				Patch::Value(v) => Some(Some(v)),
			},
			note: match self.note {
				Patch::Undefined => None,
				Patch::Null => Some(None),
				Patch::Value(v) => Some(Some(v)),
			},
		})
	}
}

#[derive(Debug, Default, Deserialize)]
pub struct StornoBody {
	#[serde(default)]
	pub reason: Option<String>,
}

// ---------------------------------------------------------------- handlers

/// `GET /api/invoices`
pub async fn list(
	State(app): State<App>,
	ctx: Ctx,
	Query(q): Query<ListQuery>,
) -> ClResult<Json<Page<InvoiceView>>> {
	let filter = InvoiceFilter::parse(q.status.as_deref(), q.q.as_deref())?.narrow(
		q.sort.as_deref(),
		q.offset,
		q.from.as_deref(),
		q.to.as_deref(),
		q.party.as_deref(),
	)?;
	let limit = q.limit.unwrap_or(50).clamp(1, crate::service_api::MAX_PAGE_LIMIT);
	let (rows, total, next_cursor) =
		Invoices::new(app).list_page(&ctx, &filter, q.cursor.as_deref(), limit).await?;
	Ok(Json(Page {
		items: rows.into_iter().map(InvoiceView::of).collect(),
		next_cursor,
		total: Some(total),
	}))
}

/// `GET /api/invoices/{uid}`
pub async fn get_invoice(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<InvoiceView>> {
	Ok(Json(InvoiceView::of(Invoices::new(app).full(&ctx, &uid).await?)))
}

/// `GET /api/invoices/{uid}/pdf` — the stored evidence, streamed from the content-addressed
/// path its sha256 puts it at. Immutable once issued, hence the year-long cache directive.
pub async fn pdf(State(app): State<App>, ctx: Ctx, Path(uid): Path<String>) -> ClResult<Response> {
	let found = Invoices::new(app).download(&ctx, &uid).await?;
	Ok(pdf_response(found, "private, max-age=31536000, immutable"))
}

/// `GET /api/public/invoices/{code}/pdf` — the link an invoice email carries
/// ([`crate::download`]). Same bytes as [`pdf`]; every hit is audited with its IP. `no-store`,
/// or a browser keeps serving the file after the link is revoked and no hit reaches the audit.
pub async fn public_pdf(
	State(app): State<App>,
	ip: Option<axum::Extension<ClientIp>>,
	Path(code): Path<String>,
) -> ClResult<Response> {
	let mut ctx = Ctx::public("invoice-download");
	if let Some(axum::Extension(ClientIp(ip))) = ip {
		ctx = ctx.with_ip(ip);
	}
	let found = Invoices::new(app).download_by_link(&ctx, &code).await?;
	Ok(pdf_response(found, "private, no-store"))
}

fn pdf_response(
	(invoice, doc, bytes): (Invoice, InvoiceDocument, Vec<u8>),
	cache: &'static str,
) -> Response {
	let name =
		crate::store::pdf_filename(invoice.number.as_deref(), invoice.uid.as_str(), &doc.sha256);
	(
		[
			(header::CONTENT_TYPE, "application/pdf".to_owned()),
			(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
			(header::ETAG, format!("\"{}\"", doc.sha256)),
			(header::CACHE_CONTROL, cache.to_owned()),
		],
		bytes,
	)
		.into_response()
}

/// `POST /api/invoices`
pub async fn create(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<NewDraftBody>,
) -> ClResult<(StatusCode, Json<InvoiceView>)> {
	let inv = Invoices::new(app);
	let cur = inv.currency(&ctx, body.currency.as_ref()).await?;
	let draft = inv.draft(&ctx, &body.into_draft(&cur)?).await?;
	let full = inv.hydrate(&ctx, draft, true).await?;
	Ok((StatusCode::CREATED, Json(InvoiceView::of(full))))
}

/// `PATCH /api/invoices/{uid}`
pub async fn patch_invoice(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(body): Json<InvoicePatchBody>,
) -> ClResult<Json<InvoiceView>> {
	let inv = Invoices::new(app);
	let patch = InvoicePatch {
		payment_method: body.payment_method,
		fulfilment_date: body.fulfilment_date,
		due_date: body.due_date,
		period_start: body.period_start,
		period_end: body.period_end,
		notes: body.notes,
		series_code: body.series_code,
		..Default::default()
	};
	let updated = inv
		.patch_by_uid(&ctx, &uid, body.billing_party_uid.as_deref(), body.currency.as_ref(), &patch)
		.await?;
	Ok(Json(InvoiceView::of(inv.hydrate(&ctx, updated, true).await?)))
}

/// `POST /api/invoices/preview` body: a stored draft by uid, or a whole draft body that is
/// never persisted. `Uid` first: every `NewDraftBody` field defaults, so `{"uid": ..}` would
/// otherwise parse as an empty draft.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum PreviewBody {
	Uid { uid: String },
	Draft(Box<NewDraftBody>),
}

/// `POST /api/invoices/preview` — what issuing would print now, watermarked `preview`,
/// rendered synchronously and stored nowhere. Never cached.
pub async fn preview(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<PreviewBody>,
) -> ClResult<Response> {
	let inv = Invoices::new(app);
	let pdf = match body {
		PreviewBody::Uid { uid } => inv.preview_pdf(&ctx, &uid).await?,
		PreviewBody::Draft(body) => {
			let cur = inv.currency(&ctx, body.currency.as_ref()).await?;
			inv.preview_draft_pdf(&ctx, &(*body).into_draft(&cur)?).await?
		}
	};
	Ok((
		[
			(header::CONTENT_TYPE, "application/pdf"),
			(header::CONTENT_DISPOSITION, "inline; filename=\"preview.pdf\""),
			(header::CACHE_CONTROL, "no-store"),
		],
		pdf,
	)
		.into_response())
}

/// `GET /api/invoices/{uid}/plan` — what issuing now would produce; the number is expected,
/// not reserved.
pub async fn issue_plan(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<IssuePlan>> {
	Ok(Json(Invoices::new(app).issue_plan(&ctx, &uid).await?))
}

/// `GET /api/seller/series`
pub async fn list_series(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Page<SellerSeries>>> {
	Ok(Json(Page::all(Invoices::new(app).list_series(&ctx).await?)))
}

/// `GET /api/seller/currencies` — the seller org's conversion opt-ins.
pub async fn list_org_currencies(
	State(app): State<App>,
	ctx: Ctx,
) -> ClResult<Json<Page<OrgCurrency>>> {
	Ok(Json(Page::all(Invoices::new(app).org_currencies(&ctx).await?)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgCurrencyBody {
	pub convert: bool,
	#[serde(default)]
	pub fee_bp: Option<i64>,
}

/// `PUT /api/seller/currencies/{code}` — `{convert, feeBp?}`; `feeBp` absent = the currency's own.
pub async fn set_org_currency(
	State(app): State<App>,
	ctx: Ctx,
	Path(code): Path<String>,
	Json(body): Json<OrgCurrencyBody>,
) -> ClResult<Json<OrgCurrency>> {
	Ok(Json(
		Invoices::new(app)
			.set_org_currency(&ctx, &code, body.convert, body.fee_bp)
			.await?,
	))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSeriesBody {
	pub code: String,
	#[serde(default)]
	pub logo_blob: Patch<String>,
	#[serde(default)]
	pub is_default: Option<bool>,
}

/// `POST /api/seller/series` — `{code, logoBlob?, isDefault?}`
pub async fn create_series(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<NewSeriesBody>,
) -> ClResult<(StatusCode, Json<SellerSeries>)> {
	let patch = SeriesPatch { logo_blob: body.logo_blob, is_default: body.is_default };
	let series = Invoices::new(app).create_series(&ctx, &body.code, &patch).await?;
	Ok((StatusCode::CREATED, Json(series)))
}

/// `PATCH /api/seller/series/{code}` — `{logoBlob?, isDefault?}`
pub async fn update_series(
	State(app): State<App>,
	ctx: Ctx,
	Path(code): Path<String>,
	Json(patch): Json<SeriesPatch>,
) -> ClResult<Json<SellerSeries>> {
	Ok(Json(Invoices::new(app).update_series(&ctx, &code, &patch).await?))
}

/// `DELETE /api/seller/series/{code}` — refused for the default and for a series in use.
pub async fn delete_series(
	State(app): State<App>,
	ctx: Ctx,
	Path(code): Path<String>,
) -> ClResult<StatusCode> {
	Invoices::new(app).delete_series(&ctx, &code).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/invoices/{uid}` — drafts only; an issued invoice is `E-INV-NOT-DRAFT`.
pub async fn delete_invoice(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<StatusCode> {
	Invoices::new(app).delete_draft(&ctx, &uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/invoices/{uid}/lines`
pub async fn add_line(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(body): Json<LineBody>,
) -> ClResult<Json<InvoiceView>> {
	let inv = Invoices::new(app);
	let cur = inv.invoice_currency(&ctx, &uid).await?;
	let updated = inv.add_line(&ctx, &uid, body.into_line(&cur)?).await?;
	Ok(Json(InvoiceView::of(inv.hydrate(&ctx, updated, true).await?)))
}

/// `PATCH /api/invoices/{uid}/lines/{lineNo}`
pub async fn edit_line(
	State(app): State<App>,
	ctx: Ctx,
	Path((uid, line_no)): Path<(String, u32)>,
	Json(body): Json<LinePatchBody>,
) -> ClResult<Json<InvoiceView>> {
	let inv = Invoices::new(app);
	let cur = inv.invoice_currency(&ctx, &uid).await?;
	let updated = inv.edit_line(&ctx, &uid, line_no, body.into_patch(&cur)?).await?;
	Ok(Json(InvoiceView::of(inv.hydrate(&ctx, updated, true).await?)))
}

/// `DELETE /api/invoices/{uid}/lines/{lineNo}` — what follows is renumbered, because
/// `line_no` is NAV's `lineNumber` and NAV requires 1..n contiguous.
pub async fn remove_line(
	State(app): State<App>,
	ctx: Ctx,
	Path((uid, line_no)): Path<(String, u32)>,
) -> ClResult<Json<InvoiceView>> {
	let inv = Invoices::new(app);
	let updated = inv.remove_line(&ctx, &uid, line_no).await?;
	Ok(Json(InvoiceView::of(inv.hydrate(&ctx, updated, true).await?)))
}

/// `POST /api/invoices/{uid}/issue`
///
/// `202`, because issuing enqueues the PDF render and the NAV report. Idempotent: an invoice
/// already ISSUED comes back unchanged, still as `202`.
pub async fn issue(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<(StatusCode, Json<InvoiceView>)> {
	let inv = Invoices::new(app);
	let issued = inv.issue(&ctx, &uid).await?;
	let full = inv.hydrate(&ctx, issued, true).await?;
	Ok((StatusCode::ACCEPTED, Json(InvoiceView::of(full))))
}

/// `POST /api/invoices/{uid}/storno` — **step-up**, enforced by `Invoices::storno`. Returns the STORNO counter-invoice, not
/// the cancelled one. The body is optional, so `POST` with no body is a storno with no reason.
pub async fn storno(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	body: Option<Json<StornoBody>>,
) -> ClResult<(StatusCode, Json<InvoiceView>)> {
	let reason = body.and_then(|Json(b)| b.reason).unwrap_or_default();
	let inv = Invoices::new(app);
	let cancelled = inv.storno(&ctx, &uid, &reason).await?;
	let full = inv.hydrate(&ctx, cancelled, true).await?;
	Ok((StatusCode::CREATED, Json(InvoiceView::of(full))))
}

// ---------------------------------------------------------------- bundles

/// The nine reads an org member may make: reference data, its own billing parties, and its
/// own invoices including the stored PDF. Mounting this alone gives a consumer a read-only
/// billing view with no way to create or issue anything.
///
/// Authenticated by its own `mintworks_core::auth_mw::require_auth` layer, and rate-limited inside
/// it by `mintworks_core::ratelimit::AUTHENTICATED`. Consent-gated by the `gate` it is handed —
/// `mintworks_auth::routes::consent_gate()`, or `RouteGate::none()` for a deployment that publishes
/// no legal documents. It is an argument rather than a wrap at the composition root because a
/// forgotten wrap let an account owing a new ToS issue a numbered legal invoice.
///
/// `/api/seller` and the two `/api/services` reads resolve through the acting org's seller, so
/// they are org-scoped like the rest of the bundle; only `/api/currencies` is deployment-wide.
pub fn org_read(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/currencies", get(catalog::currencies))
		.route("/api/seller", get(catalog::seller))
		.route("/api/services", get(catalog::list))
		.route("/api/services/{uid}", get(catalog::get))
		.route("/api/billing-parties", get(party::list))
		.route("/api/billing-parties/{uid}", get(party::get))
		.route("/api/invoices", get(list))
		.route("/api/invoices/{uid}", get(get_invoice))
		.route("/api/invoices/{uid}/pdf", get(pdf))
		.route("/api/invoices/{uid}/plan", get(issue_plan))
		// A synchronous typst render per call, so its own budget well under `authenticated`'s.
		.route(
			"/api/invoices/preview",
			post(preview).layer(axum::middleware::from_fn_with_state(
				"invoice.preview",
				mintworks_core::ratelimit::scoped_account_mw,
			)),
		)
		.route("/api/seller/series", get(list_series))
		.route("/api/seller/currencies", get(list_org_currencies))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

/// The invoice-PDF link ([`public_pdf`]). Unauthenticated by design: the ref code is the
/// credential, and it is IP-rate-limited.
pub fn public() -> Router<App> {
	Router::new().route(
		"/api/public/invoices/{code}/pdf",
		get(public_pdf).layer(axum::middleware::from_fn_with_state(
			"invoice.download",
			mintworks_core::ratelimit::scoped_ip_mw,
		)),
	)
}

/// Writing the org's own billing parties. Separate from [`org_read`] because a consumer
/// that manages parties from its own admin screens serves the reads and not these.
///
/// Authenticated, and consent-gated by the `gate` it is handed — `mintworks_auth::routes::consent_gate()`,
/// or `RouteGate::none()` for a deployment that publishes no legal documents. It is an argument
/// rather than a wrap at the composition root because a forgotten wrap let an account owing a
/// new ToS issue a numbered legal invoice, and nothing at boot noticed.
///
/// Every call charges the account-keyed `mintworks_core::ratelimit::AUTHENTICATED` tier; no route
/// here carries a named limit of its own.
pub fn org_parties(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/billing-parties", post(party::create))
		.route("/api/billing-parties/{uid}", patch(party::patch).delete(party::delete))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

/// The eight routes that create, edit and issue invoices over HTTP.
///
/// Most consumers leave this unmounted: the framework originates invoices itself through
/// [`Invoices::issue_now`], and a customer-facing HTTP surface that can issue a numbered legal
/// document is rarely what an application wants.
///
/// Authenticated, and consent-gated by the `gate` it is handed — `mintworks_auth::routes::consent_gate()`,
/// or `RouteGate::none()` for a deployment that publishes no legal documents. It is an argument
/// rather than a wrap at the composition root because a forgotten wrap let an account owing a
/// new ToS issue a numbered legal invoice, and nothing at boot noticed.
///
/// Every call charges the account-keyed `mintworks_core::ratelimit::AUTHENTICATED` tier. No route
/// here carries a named limit of its own — in particular not `issue` or `storno`, which the
/// framework also originates from Rust where no layer runs.
pub fn org_invoices(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/invoices", post(create))
		.route("/api/invoices/{uid}", patch(patch_invoice).delete(delete_invoice))
		.route("/api/invoices/{uid}/lines", post(add_line))
		.route("/api/invoices/{uid}/lines/{lineNo}", patch(edit_line).delete(remove_line))
		.route("/api/invoices/{uid}/issue", post(issue))
		.route("/api/invoices/{uid}/storno", post(storno))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

/// Service master data — the catalogue of the acting org's seller. The guard is `Admin` on that
/// seller's org, and it is in [`Invoices`], not here.
///
/// Authenticated, and consent-gated by the `gate` it is handed — `mintworks_auth::routes::consent_gate()`,
/// or `RouteGate::none()` for a deployment that publishes no legal documents. It is an argument
/// rather than a wrap at the composition root because a forgotten wrap let an account owing a
/// new ToS issue a numbered legal invoice, and nothing at boot noticed.
///
/// Every call charges the account-keyed `mintworks_core::ratelimit::AUTHENTICATED` tier; no route
/// here carries a named limit of its own.
pub fn org_services(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/services", post(catalog::create))
		.route("/api/services/{uid}", patch(catalog::patch).delete(catalog::deactivate))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

/// The acting org's own seller: mint it (`POST`, Admin on a SHARED org) and publish an edit
/// (`PUT`). Gated, rate-limited and authenticated like [`org_services`].
pub fn org_seller(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/seller", post(catalog::create_seller).put(catalog::sync_seller))
		.route("/api/seller/closed", put(catalog::set_seller_closed))
		.route("/api/seller/payment-days", put(catalog::set_seller_payment_days))
		.route("/api/seller/currencies/{code}", put(set_org_currency))
		.route("/api/seller/series", post(create_series))
		.route("/api/seller/series/{code}", patch(update_series).delete(delete_series))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::money::Discount;

	fn huf() -> Currency {
		Currency {
			code: CurrencyCode::huf(),
			price_round_step: 1,
			cash_round_step: None,
			mode: crate::currency::RateMode::Official,
			fixed_rate_e6: None,
			fee_bp: 0,
			enabled: true,
		}
	}

	fn body(json: serde_json::Value) -> ClResult<Line> {
		serde_json::from_value::<LineBody>(json)
			.expect("the body itself parses")
			.into_line(&huf())
	}

	/// `qty`, `unit_price` and an `AMOUNT` discount's sign and magnitude are `vat::compute`'s
	/// alone and tested there — `qty: "-1"` with `discountValue: i64::MAX` is refused one call
	/// later, as `E-INV-LINE`. What `into_line` owes is the conversion itself.
	#[test]
	fn an_ordinary_line_body_converts() {
		let ok = body(serde_json::json!({
			"qty": "2",
			"description": "Tanácsadás",
			"unit": "óra",
			"unitPrice": { "amount": "100.00", "currency": "HUF" },
			"discountKind": "AMOUNT",
			"discountValue": 500,
		}))
		.unwrap();
		assert_eq!(ok.qty, Qty(2_000_000));
		assert_eq!(ok.discount, Some(Discount::Amount(Money(500))));
	}

	/// `description` and `unit` used to fall through as `String::default()`, and
	/// `invoice_lines` only makes them `NOT NULL` — so the empty strings persisted and NAV
	/// rejected the filing against `SimpleText512NotBlankType`, forever.
	#[test]
	fn an_adhoc_line_needs_a_description_and_a_unit() {
		let money = serde_json::json!({ "amount": "100.00", "currency": "HUF" });
		for line in [
			serde_json::json!({ "qty": "1", "unitPrice": money, "unit": "óra" }),
			serde_json::json!({ "qty": "1", "unitPrice": money, "unit": "óra",
				"description": "   " }),
			serde_json::json!({ "qty": "1", "unitPrice": money, "description": "Tanácsadás" }),
			serde_json::json!({ "qty": "1", "unitPrice": money, "description": "Tanácsadás",
				"unit": "\t" }),
		] {
			assert_eq!(body(line).unwrap_err().parts().1, "E-INV-LINE");
		}

		// A catalogue line has neither, and must stay acceptable: `draft::resolve` fills
		// both from the `services` row.
		let ok = body(serde_json::json!({ "qty": "1", "serviceCode": "SVC-1" })).unwrap();
		assert_eq!(ok.code.as_deref(), Some("SVC-1"));
		assert!(ok.description.is_empty());
	}

	/// An ad-hoc line with no `unitPrice` fell through as `Money::ZERO` and every later
	/// check passed it, so the line was billed at nothing with a `201`.
	#[test]
	fn an_adhoc_line_needs_a_unit_price() {
		let line = serde_json::json!({
			"qty": "1", "description": "Tanácsadás", "unit": "óra",
		});
		assert_eq!(body(line).unwrap_err().parts().1, "E-INV-LINE");

		// A genuinely free line is a real thing — only *absence* is the error.
		let free = serde_json::json!({
			"qty": "1", "description": "Tanácsadás", "unit": "óra",
			"unitPrice": { "amount": "0.00", "currency": "HUF" },
		});
		assert_eq!(body(free).unwrap().unit_price, Some(Money::ZERO));
	}

	/// The refusal itself is `draft::resolve`'s — see `invoice_units.rs`. What this pins is
	/// that the body reaches it *unset*, so the sentinels cannot make an assertion
	/// indistinguishable from an absence again.
	#[test]
	fn a_catalogue_line_carries_its_price_and_tax_through_unset() {
		let ok = body(serde_json::json!({ "qty": "1", "serviceCode": "SVC-1" })).unwrap();
		assert_eq!(ok.unit_price, None);
		assert_eq!(ok.vat_code, None);

		let asserted = body(serde_json::json!({ "qty": "1", "serviceCode": "SVC-1",
			"unitPrice": { "amount": "1.00", "currency": "HUF" }, "vatCode": "AAM" }))
		.unwrap();
		assert_eq!(asserted.unit_price, Some(Money(100)));
		assert_eq!(asserted.vat_code, Some(VatCode::Aam));
	}
}

// vim: ts=4
