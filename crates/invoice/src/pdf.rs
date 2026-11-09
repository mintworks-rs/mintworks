// SPDX-License-Identifier: MPL-2.0
//! Typst PDF rendering, and the `RENDER_PDF` job handler behind it.
//!
//! The two `.typ` files in `templates/invoice/` are `include_str!`-ed and concatenated into
//! one file of a `mintworks-pdf` template set, so neither may `#import` the other, and
//! [`TEMPLATE_VERSION`] stays truthful — the bytes that produced a PDF are the bytes that
//! were compiled into the binary.
//!
//! Every amount is formatted to a string *here*, in integer arithmetic, and handed to the
//! template as text. Typst does no arithmetic on money, so no float can reach the page.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use mintworks_core::app::App;
use mintworks_core::blob::Blobs;
use mintworks_core::ctx::Ctx;
use mintworks_core::error::{ClResult, Error};
use mintworks_core::job::{Job, Runner};
use mintworks_core::money::{Money, Qty, format_scaled};
use mintworks_core::types::Timestamp;
use mintworks_pdf::Files;
pub use mintworks_pdf::doc_path;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::issue::KIND_RENDER_PDF;
use crate::numbering;
use crate::store::{
	Invoice, InvoiceDocument, InvoiceLine, InvoiceStore, InvoiceVatGroup, PaymentMethod,
	SellerVersion,
};
use crate::taxrule::vat_notes;
use crate::vat::{VatClass, VatCode};

/// Bumped whenever a template change would produce a materially different page. Stored on
/// every `invoice_documents` row, so a reprint can be told from the layout that made it.
pub const TEMPLATE_VERSION: &str = "invoice-7";

const STRINGS_TYP: &str = include_str!("../../../templates/invoice/strings.typ");
const INVOICE_TYP: &str = include_str!("../../../templates/invoice/invoice.typ");

// ---------------------------------------------------------------- the template

/// One file, the two templates concatenated, at `/invoice.typ` — the source text and path
/// every earlier invoice was compiled from, so moving onto `mintworks-pdf` changes no byte.
static FILES: LazyLock<Files> = LazyLock::new(|| {
	Files::from([("invoice.typ".to_owned(), format!("{STRINGS_TYP}\n{INVOICE_TYP}").into_bytes())])
});

/// Compiles the template against `data`, read back as `sys.inputs.invoice`, and returns PDF
/// bytes. `logo` is `(file name, bytes)`, added at the virtual root under the name `data`'s
/// `brand.logo` gives. Pure and blocking — call it from `spawn_blocking`, never straight off an
/// async task.
pub fn render(data: &str, logo: Option<(&str, Vec<u8>)>) -> ClResult<Vec<u8>> {
	mintworks_pdf::render(&files(logo), "invoice.typ", &inputs(data), true)
}

/// The laid-out pages `render` would export, for tests that check positions.
#[cfg(test)]
fn layout(data: &str, logo: Option<(&str, Vec<u8>)>) -> ClResult<mintworks_pdf::PagedDocument> {
	mintworks_pdf::compile(&files(logo), "invoice.typ", &inputs(data))
}

/// The template files, plus the logo at the virtual root when there is one.
fn files(logo: Option<(&str, Vec<u8>)>) -> std::borrow::Cow<'static, Files> {
	match logo {
		Some((name, bytes)) => {
			let mut files = FILES.clone();
			files.insert(name.to_owned(), bytes);
			std::borrow::Cow::Owned(files)
		}
		None => std::borrow::Cow::Borrowed(&*FILES),
	}
}

fn inputs(data: &str) -> BTreeMap<String, String> {
	BTreeMap::from([("invoice".to_owned(), data.to_owned())])
}

// ---------------------------------------------------------------- formatting

/// Groups the integer part in non-breaking spaces and picks the locale's decimal mark.
/// Operates on the decimal *string* — there is no arithmetic here and nothing to round.
fn group(s: &str, lang: &str) -> String {
	let (sign, rest) = s.strip_prefix('-').map_or(("", s), |r| ("-", r));
	let (int, frac) = rest.split_once('.').map_or((rest, None), |(i, f)| (i, Some(f)));
	let mut out = String::with_capacity(rest.len() + 6);
	for (i, c) in int.chars().enumerate() {
		if i > 0 && (int.len() - i) % 3 == 0 {
			out.push('\u{a0}');
		}
		out.push(c);
	}
	if let Some(f) = frac {
		out.push(if matches!(lang, "hu" | "hu-en") { ',' } else { '.' });
		out.push_str(f);
	}
	format!("{sign}{out}")
}

fn money(m: Money, lang: &str) -> String {
	group(&m.to_decimal_string(), lang)
}

/// `Qty` always renders six decimals; trailing zeros are noise on an invoice line.
fn qty(q: Qty, lang: &str) -> String {
	let s = q.to_decimal_string();
	let trimmed = match s.split_once('.') {
		Some((i, f)) => {
			let f = f.trim_end_matches('0');
			if f.is_empty() { i.to_owned() } else { format!("{i}.{f}") }
		}
		None => s,
	};
	group(&trimmed, lang)
}

/// `2700` -> `"27%"`. Rates are whole percents by construction (`VatCode::rate_bp`).
fn rate_pct(bp: i64) -> String {
	format!("{}%", bp / 100)
}

/// What the VAT column prints. `STD27` is a storage tag and has no business on a page, but
/// an exempt code does: AAM, TAM, HO and ATK are all 0% and Áfa tv. 169. § needs them told
/// apart. `rate_bp` comes from the stored column, not from the code.
fn vat_label(code: VatCode, rate_bp: i64) -> String {
	match code.nav_class() {
		VatClass::Percentage(_) => rate_pct(rate_bp),
		VatClass::Exemption | VatClass::OutOfScope => code.as_str().to_owned(),
	}
}

/// The UPPERCASE serde tag of one of the store's enums, as a plain string.
fn tag<T: Serialize>(v: &T) -> ClResult<String> {
	serde_json::to_value(v)
		.ok()
		.as_ref()
		.and_then(serde_json::Value::as_str)
		.map(str::to_owned)
		.ok_or_else(|| Error::internal("mintworks-invoice/pdf: enum did not serialise as a string"))
}

fn addr(postcode: Option<&str>, city: Option<&str>, street: Option<&str>) -> String {
	let head = [postcode, city].into_iter().flatten().collect::<Vec<_>>().join(" ");
	[head.as_str(), street.unwrap_or_default()]
		.into_iter()
		.filter(|s| !s.is_empty())
		.collect::<Vec<_>>()
		.join(", ")
}

/// The rounded payable of a CASH invoice whose currency has a `cash_round_step`, when it
/// differs from the gross.
fn payable(invoice: &Invoice, cash_round_step: Option<i64>) -> ClResult<Option<Money>> {
	let Some(step) = cash_round_step.filter(|_| invoice.payment_method == PaymentMethod::Cash)
	else {
		return Ok(None);
	};
	let rounded = mintworks_core::money::cash_round(invoice.gross.0, step)?;
	Ok((rounded != invoice.gross.0).then_some(Money(rounded)))
}

// ---------------------------------------------------------------- the data document

/// Builds the JSON the template reads. Everything is a pre-formatted string; the template
/// only lays out.
pub fn document(
	seller: &SellerVersion,
	invoice: &Invoice,
	lines: &[InvoiceLine],
	groups: &[InvoiceVatGroup],
	original_number: Option<&str>,
	cash_round_step: Option<i64>,
) -> ClResult<String> {
	to_json(&document_value(seller, invoice, lines, groups, original_number, cash_round_step)?)
}

fn to_json(doc: &serde_json::Value) -> ClResult<String> {
	serde_json::to_string(doc)
		.map_err(|e| Error::internal(format!("mintworks-invoice/pdf: data document: {e}")))
}

fn document_value(
	seller: &SellerVersion,
	invoice: &Invoice,
	lines: &[InvoiceLine],
	groups: &[InvoiceVatGroup],
	original_number: Option<&str>,
	cash_round_step: Option<i64>,
) -> ClResult<serde_json::Value> {
	// `hu-en` passes through: the template prints both label sets, and numbers format as `hu`.
	let lang =
		crate::draft::doc_lang(invoice.doc_lang.as_deref(), invoice.buyer_country.as_deref());
	let m = |v: Money| money(v, lang);

	let doc = serde_json::json!({
		"lang": lang,
		"testMode": invoice.test_mode,
		// `logo` is filled in by the caller once the blob is loaded: see [`render_doc`].
		"brand": { "logo": null, "accent": seller.accent },
		"seller": {
			"name": seller.name,
			"address": addr(Some(&seller.postcode), Some(&seller.city), Some(&seller.street)),
			"taxNumber": seller.tax_number,
			"euVatId": seller.eu_vat_id,
			"groupTaxNo": seller.group_member_tax_no,
			"bankAccount": seller.bank_account.as_ref().map(|a| match &seller.bank_name {
				Some(b) => format!("{a} ({b})"),
				None => a.clone(),
			}),
			"email": seller.email,
		},
		"buyer": {
			"name": invoice.buyer_name,
			"address": addr(
				invoice.buyer_postcode.as_deref(),
				invoice.buyer_city.as_deref(),
				invoice.buyer_street.as_deref(),
			),
			"taxNumber": invoice.buyer_tax_number,
			"euVatId": invoice.buyer_eu_vat_id,
			"groupTaxNo": invoice.buyer_group_tax_no,
		},
		"invoice": {
			"kind": tag(&invoice.kind)?,
			"number": invoice.number,
			// Every amount on the page is a bare grouped decimal, so without this the
			// currency was legible only from the optional exchange-rate footnote — which a
			// HUF invoice does not carry at all.
			"currency": invoice.currency,
			// Not optional: PDF/A-3b rejects a document with no date, and the template sets
			// `document(date:)` from this key.
			"issuedAt": numbering::date_of(invoice.issued_at.ok_or_else(|| Error::internal(
				"mintworks-invoice/pdf: an issued invoice has no issued_at",
			))?)?,
			"fulfilmentDate": invoice.fulfilment_date,
			"dueDate": invoice.due_date,
			"periodStart": invoice.period_start,
			"periodEnd": invoice.period_end,
			"paymentMethod": tag(&invoice.payment_method)?,
			"original": original_number,
			// A list, not one key: `issue::vat_note` stores every applicable Áfa tv. 169. §
			// note newline-joined, and the template prints them all.
			"vatNotes": vat_notes(invoice.vat_note.as_deref()),
			"notes": invoice.notes,
		},
		// Present only on a foreign-currency invoice — that is exactly when Áfa tv. 172. §
		// makes the rate and the HUF VAT figure mandatory page content.
		"rate": if invoice.currency == "HUF" {
			serde_json::Value::Null
		} else {
			serde_json::json!({
				"quote": invoice.currency,
				// **Not** `rate_e6`: that is the base→invoice-currency rate, and falling back to
				// it printed a fabricated statutory rate. A PDF carrying the wrong Áfa tv.
				// 172. § rate is worse than no PDF, so the render fails as `mintworks_nav::xml`
				// does.
				"value": group(&format_scaled(invoice.huf_rate_e6.ok_or_else(|| Error::internal(
					"mintworks-invoice/pdf: a foreign-currency invoice has no huf_rate_e6",
				))?, 6), lang),
				"date": invoice.rate_date,
				"source": invoice.rate_source.as_ref().map(tag).transpose()?,
			})
		},
		"lines": lines.iter().map(|l| Ok(serde_json::json!({
			"no": l.line_no,
			"description": l.description,
			"unit": l.unit,
			"qty": qty(l.qty, lang),
			"unitPrice": m(l.unit_price),
			"discount": (l.discount_amount.0 != 0).then(|| m(l.discount_amount)),
			"discountDescription": l.discount_description,
			"note": l.note,
			"net": m(l.net),
			"vatRate": vat_label(l.vat_code, l.vat_rate_bp),
			"vat": m(l.vat),
			"gross": m(l.gross),
		}))).collect::<ClResult<Vec<_>>>()?,
		// The authoritative per-code figures. Never re-summed from the lines: the per-line
		// `vat` is an apportioned display value and can differ by a fillér.
		"groups": groups.iter().map(|g| Ok(serde_json::json!({
			"vatRate": vat_label(g.vat_code, g.vat_rate_bp),
			"net": m(g.net),
			"vat": m(g.vat),
			"gross": m(g.gross),
			"netHuf": g.net_huf.map(|v| money(v, lang)),
			"vatHuf": g.vat_huf.map(|v| money(v, lang)),
			"grossHuf": g.gross_huf.map(|v| money(v, lang)),
		}))).collect::<ClResult<Vec<_>>>()?,
		"totals": {
			"net": m(invoice.net),
			"vat": m(invoice.vat),
			"gross": m(invoice.gross),
			"payable": payable(invoice, cash_round_step)?.map(m),
		},
		// Áfa tv. 169. § h): the "Pénzforgalmi elszámolás" text, from the frozen version.
		"cashAccounting": seller.cash_accounting,
	});

	Ok(doc)
}

/// The footer's "invoicing software" line from the NAV identity settings, `null` when the nav
/// module did not declare them (an error) or the deployment left them blank. Not mandatory
/// page content (23/2014. NGM rendelet), so its absence never fails a render.
async fn software(app: &App) -> serde_json::Value {
	let (name, version) = tokio::join!(
		app.settings.text("nav.software_name"),
		app.settings.text("nav.software_main_version"),
	);
	match (name, version) {
		(Ok(name), Ok(version)) if !name.is_empty() => {
			serde_json::json!({ "name": name, "version": version })
		}
		_ => serde_json::Value::Null,
	}
}

// ---------------------------------------------------------------- the logo

/// A larger logo blob is skipped like an unknown format: it would only bloat every PDF.
pub(crate) const MAX_LOGO_BYTES: usize = 2 * 1024 * 1024;

/// The series logo, else the seller version's, as a `Files` entry. A blob that is gone, over
/// [`MAX_LOGO_BYTES`] or neither PNG nor JPEG (PDF/A embeds only those) renders without a logo
/// rather than failing.
async fn logo(
	app: &App,
	store: &dyn InvoiceStore,
	seller: &SellerVersion,
	seller_org: i64,
	invoice: &Invoice,
) -> ClResult<Option<(&'static str, Vec<u8>)>> {
	let series = match invoice.series_code.as_deref() {
		Some(code) => crate::store::series_by_code(store, invoice.seller_id, code)
			.await?
			.and_then(|s| s.logo_blob),
		None => None,
	};
	let Some(uid) = series.or_else(|| seller.logo_blob.clone()) else { return Ok(None) };
	let (blob, path) = match Blobs::new(app).open(&Ctx::system("invoice-pdf"), &uid).await {
		Ok(found) => found,
		Err(Error::NotFound) => return Ok(None),
		Err(e) => return Err(e),
	};
	// The seller's org, not `invoice.org_id` (the buyer's): another org's blob never prints.
	if seller_org != blob.org_id || !usize::try_from(blob.size).is_ok_and(|n| n <= MAX_LOGO_BYTES) {
		return Ok(None);
	}
	let bytes = match tokio::fs::read(&path).await {
		Ok(b) => b,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(e) => return Err(Error::Unavailable(format!("mintworks-invoice/pdf: logo: {e}"))),
	};
	if bytes.len() > MAX_LOGO_BYTES {
		return Ok(None);
	}
	Ok(sniff(&bytes).map(|name| (name, bytes)))
}

/// A decompression bomb fits under [`MAX_LOGO_BYTES`]; typst would decode it in full.
const MAX_LOGO_PIXELS: u64 = 4096 * 4096;

/// The `Files` name for a PNG or JPEG by its magic bytes, `None` for anything else or for one
/// whose header declares more than [`MAX_LOGO_PIXELS`].
fn sniff(bytes: &[u8]) -> Option<&'static str> {
	let be32 =
		|at: usize| Some(u64::from(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?)));
	let be16 =
		|at: usize| Some(u64::from(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?)));
	let (name, w, h) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
		("logo.png", be32(16)?, be32(20)?)
	} else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
		let at = jpeg_sof(bytes)?;
		("logo.jpg", be16(at + 7)?, be16(at + 5)?)
	} else {
		return None;
	};
	(w * h <= MAX_LOGO_PIXELS).then_some(name)
}

/// The offset of the JPEG's start-of-frame marker (SOF0–SOF15 bar DHT/JPG/DAC), which carries
/// the height at +5 and the width at +7.
fn jpeg_sof(bytes: &[u8]) -> Option<usize> {
	let mut i = 2;
	while *bytes.get(i)? == 0xff {
		let marker = *bytes.get(i + 1)?;
		match marker {
			0xff => i += 1,
			0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => return Some(i),
			0x01 | 0xd0..=0xd9 => i += 2,
			_ => i += 2 + usize::from(u16::from_be_bytes([*bytes.get(i + 2)?, *bytes.get(i + 3)?])),
		}
	}
	None
}

/// Compiles `doc` with `logo` off the async runtime; see [`render_doc`].
async fn compile(
	doc: serde_json::Value,
	logo: Option<(&'static str, Vec<u8>)>,
) -> ClResult<Vec<u8>> {
	tokio::task::spawn_blocking(move || render_doc(doc, logo))
		.await
		.map_err(|e| Error::internal(format!("mintworks-invoice/pdf: render task: {e}")))?
}

/// Names `logo` in `doc`'s `brand.logo` and renders. A logo typst cannot decode is dropped and
/// the page rendered again: magic bytes prove nothing, and a failed render retried forever
/// would wedge every `RENDER_PDF` of the seller.
fn render_doc(mut doc: serde_json::Value, logo: Option<(&str, Vec<u8>)>) -> ClResult<Vec<u8>> {
	if let Some((name, bytes)) = logo {
		doc["brand"]["logo"] = name.into();
		match render(&to_json(&doc)?, Some((name, bytes))) {
			Ok(pdf) => return Ok(pdf),
			Err(e) => tracing::warn!(error = %e, "invoice logo does not render; dropping it"),
		}
	}
	doc["brand"]["logo"] = serde_json::Value::Null;
	render(&to_json(&doc)?, None)
}

// ---------------------------------------------------------------- the preview

/// What issuing `invoice` now would print, watermarked (`preview: true`), rendered on the
/// spot and stored nowhere. `unsaved` carries the lines of a draft body that was never
/// persisted; `None` reads the draft's own. The number is peeked from `doc_series`, not
/// allocated. The caller gates on role and status.
pub async fn preview(
	app: &App,
	store: &dyn InvoiceStore,
	invoice: &Invoice,
	unsaved: Option<Vec<crate::money::DraftLine>>,
) -> ClResult<Vec<u8>> {
	let issued_at = Timestamp::now();
	let plan = crate::issue::prepare(app, store, invoice, unsaved, issued_at).await?;
	let (number, seller, currency, seller_org) = tokio::try_join!(
		store.peek_number(
			invoice.seller_id,
			crate::store::series_kind(plan.test_mode),
			&plan.series_code,
			plan.series_year,
		),
		store.seller_version(plan.seller_ver),
		store.currency_get(invoice.currency.as_str()),
		store.seller_by_id(invoice.seller_id),
	)?;
	let seller = seller.ok_or(Error::NotFound)?;
	let seller_org = seller_org.ok_or(Error::NotFound)?;

	let inv = invoice.frozen_with(number, &plan);
	let lines: Vec<InvoiceLine> = plan
		.lines
		.iter()
		.zip(1..)
		.map(|(l, line_no)| l.to_line(inv.id, line_no))
		.collect();
	let mut doc = document_value(
		&seller,
		&inv,
		&lines,
		&plan.groups,
		None,
		currency.and_then(|c| c.cash_round_step),
	)?;
	doc["preview"] = serde_json::Value::Bool(true);
	doc["software"] = software(app).await;
	compile(doc, logo(app, store, &seller, seller_org.org_id, &inv).await?).await
}

// ---------------------------------------------------------------- the job

#[derive(Deserialize)]
struct Payload {
	#[serde(rename = "invoiceId")]
	invoice_id: i64,
}

/// Renders and stores the PDF for one invoice. Idempotent: an existing `invoice_documents`
/// row short-circuits, so a retried job never re-renders and a reader never sees the file
/// change under it.
pub async fn run(app: &App, store: &dyn InvoiceStore, invoice_id: i64) -> ClResult<()> {
	if store.invoice_document(invoice_id).await?.is_some() {
		return Ok(());
	}

	let invoice = store.invoice_by_id(invoice_id).await?.ok_or(Error::NotFound)?;
	if invoice.number.is_none() {
		// A draft has no number and no frozen buyer: there is nothing lawful to print.
		return Err(Error::internal("mintworks-invoice/pdf: invoice is not issued"));
	}
	// The **frozen** version, not `seller_by_id`: this job runs after the issue transaction
	// commits and is retried with backoff, so a seller edit landing in that window used to
	// print a supplier block the invoice was never issued under.
	let seller_ver = invoice.seller_ver.ok_or_else(|| {
		Error::internal("mintworks-invoice/pdf: an issued invoice has no seller_ver")
	})?;
	let (seller, lines, groups) = tokio::try_join!(
		store.seller_version(seller_ver),
		store.invoice_lines(invoice_id),
		store.invoice_vat_groups(invoice_id),
	)?;
	let seller = seller.ok_or(Error::NotFound)?;
	let original = match invoice.original_invoice_id {
		Some(id) => store.invoice_by_id(id).await?.and_then(|i| i.number),
		None => None,
	};

	// Not `currency::get`: a currency disabled since the issue still prints.
	let cash_step = store
		.currency_get(invoice.currency.as_str())
		.await?
		.and_then(|c| c.cash_round_step);
	let mut doc =
		document_value(&seller, &invoice, &lines, &groups, original.as_deref(), cash_step)?;
	doc["software"] = software(app).await;
	// The seller's org, not `invoice.org_id` (the buyer's): erasing a PERSONAL buyer org
	// deleted its blobs, statutory copy included.
	let seller_org = store
		.seller_by_id(invoice.seller_id)
		.await?
		.ok_or_else(|| Error::internal("mintworks-invoice/pdf: the invoice's seller is gone"))?
		.org_id;
	let pdf = compile(doc, logo(app, store, &seller, seller_org, &invoice).await?).await?;
	let sha256 = hex::encode(Sha256::digest(&pdf));
	let bytes = i64::try_from(pdf.len()).unwrap_or(i64::MAX);

	// Through the blob store, claimed before the row lands: a row whose blob `BLOB_GC` could
	// still collect would lose the statutory copy, and a retry short-circuits on the row.
	// A priority put: the quota was checked before issue (`issue::run`), never here.
	let (ctx, blobs, holder) =
		(Ctx::system("invoice-pdf"), Blobs::new(app), format!("invoice:{invoice_id}"));
	let uid = blobs
		.put_priority(&ctx, seller_org, axum::body::Bytes::from(pdf), "application/pdf", None)
		.await?;
	blobs.claim(&ctx, uid.as_str(), &holder).await?;

	// Gated on the version read above: a note edit committing during the compile passed the
	// reader-pool guard in `Invoices::patch_notes`, and the PDF — served `immutable` — kept the
	// old note forever. Retryable, so the job re-renders against the new note.
	let put = store
		.put_invoice_document(
			&InvoiceDocument {
				invoice_id,
				kind: "PDF".to_owned(),
				sha256,
				bytes,
				template_version: TEMPLATE_VERSION.to_owned(),
				rendered_at: Timestamp::now(),
			},
			invoice.version,
		)
		.await;
	let stored = match put {
		Ok(stored) => stored,
		Err(e) => {
			if let Err(re) = blobs.release(&ctx, uid.as_str(), &holder).await {
				tracing::warn!(error = %re, "mintworks-invoice/pdf: releasing the unstored PDF blob");
			}
			return Err(e);
		}
	};
	if !stored {
		// Not ours to keep: either the note moved (re-rendered on retry) or another run won.
		blobs.release(&ctx, uid.as_str(), &holder).await?;
	}
	if !stored && store.invoice_document(invoice_id).await?.is_none() {
		return Err(Error::Unavailable(
			"mintworks-invoice/pdf: the note changed mid-render".to_owned(),
		));
	}
	Ok(())
}

/// Registers the `RENDER_PDF` handler. `issue::run` enqueues the job after the issue
/// transaction commits, keyed `pdf:invoice:{id}`.
pub fn register(runner: &mut Runner, app: App, store: Arc<dyn InvoiceStore>) {
	runner.register(KIND_RENDER_PDF, move |job: Job| {
		let (app, store) = (app.clone(), store.clone());
		async move {
			let p: Payload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!(
					"mintworks-invoice/pdf: bad {KIND_RENDER_PDF} payload: {e}"
				))
			})?;
			run(&app, store.as_ref(), p.invoice_id).await
		}
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The one thing worth pinning: grouping and the locale decimal mark, on the string
	/// side of the money path. If this breaks, every amount on every invoice is wrong.
	#[test]
	fn groups_and_localises() {
		assert_eq!(group("12500.00", "hu"), "12\u{a0}500,00");
		assert_eq!(group("12500.00", "en"), "12\u{a0}500.00");
		assert_eq!(group("12500.00", "hu-en"), "12\u{a0}500,00");
		assert_eq!(group("-1234567.89", "hu"), "-1\u{a0}234\u{a0}567,89");
		assert_eq!(group("7", "hu"), "7");
		assert_eq!(group("999", "en"), "999");
		assert_eq!(group("1000", "en"), "1\u{a0}000");
		assert_eq!(qty(Qty(2_500_000), "hu"), "2,5");
		assert_eq!(qty(Qty(3_000_000), "en"), "3");
		assert_eq!(money(Money(-5), "hu"), "-0,05");
		assert_eq!(rate_pct(2700), "27%");
		assert_eq!(rate_pct(0), "0%");
	}

	/// `STD27` is a storage tag; an exempt code is printed content.
	#[test]
	fn vat_column_shows_the_rate_but_keeps_exempt_codes() {
		assert_eq!(vat_label(VatCode::Std27, 2700), "27%");
		assert_eq!(vat_label(VatCode::Red05, 500), "5%");
		assert_eq!(vat_label(VatCode::Aam, 0), "AAM");
		assert_eq!(vat_label(VatCode::Eufad37, 0), "EUFAD37");
		assert_eq!(vat_label(VatCode::Ho, 0), "HO");
	}

	/// The data document the template tests compile. `buyer` is a parameter because a glyph no
	/// bundled font has is a PDF/A export failure, which `a_glyph_no_font_has_fails_the_export`
	/// pins.
	fn doc(
		currency: &str,
		rate: &serde_json::Value,
		huf: &serde_json::Value,
		buyer: &str,
	) -> String {
		serde_json::json!({
			"lang": "hu",
			"seller": { "name": "S", "address": "A", "taxNumber": "1", "bankAccount": "2" },
			"buyer": { "name": buyer, "address": "C" },
			"invoice": {
				"kind": "NORMAL", "number": "X/1", "currency": currency,
				"issuedAt": "2026-09-16", "fulfilmentDate": "2026-09-16",
				"dueDate": "2026-09-24", "paymentMethod": "TRANSFER",
				"vatNotes": ["vat.aam"], "notes": "n",
			},
			"rate": rate,
			"lines": [{
				"no": 1, "description": "d", "unit": "db", "qty": "2",
				"unitPrice": "1,00", "net": "2,00", "vatRate": "27%",
				"vat": "0,54", "gross": "2,54",
				// The two optional line branches, which nothing else compiles.
				"discountDescription": "kedvezmény", "note": "2026-10-03, ablak melletti",
			}],
			"groups": [{
				"vatRate": "27%", "net": "2,00", "vat": "0,54", "gross": "2,54",
				"vatHuf": huf,
			}],
			"totals": { "net": "2,00", "vat": "0,54", "gross": "2,54" },
		})
		.to_string()
	}

	fn huf_doc() -> String {
		doc("HUF", &serde_json::Value::Null, &serde_json::Value::Null, "B")
	}

	/// The template is only compiled inside a job, so a syntax error would otherwise surface
	/// as a failed `RENDER_PDF` in production. Both shapes: HUF (no rate, no HUF column) and
	/// a foreign currency (both present).
	#[test]
	fn template_compiles_for_both_currency_shapes() {
		let doc = |currency: &str, rate: &serde_json::Value, huf: &serde_json::Value| {
			doc(currency, rate, huf, "B")
		};
		assert!(render(&huf_doc(), None).is_ok());
		let rate = serde_json::json!({
			"quote": "EUR", "value": "400,000000", "date": "2026-09-16", "source": "MNB",
		});
		let huf = serde_json::json!("216,00");
		assert!(render(&doc("EUR", &rate, &huf), None).is_ok());

		// A foreign-currency invoice can carry a group with no `vat_huf` beside one that has it
		// — `storno::…` guards for exactly that state. `any-huf` switches the column on for the
		// whole table, so the missing cell has to fall back to blank rather than fail the render.
		let mut mixed: serde_json::Value = serde_json::from_str(&doc("EUR", &rate, &huf)).unwrap();
		mixed["groups"].as_array_mut().unwrap().push(serde_json::json!({
			"vatRate": "5%", "net": "1,00", "vat": "0,05", "gross": "1,05",
		}));
		assert!(render(&mixed.to_string(), None).is_ok());
	}

	/// The archived PDF is the statutory evidence copy, so it is PDF/A. Without the validator
	/// the export still succeeds — only the XMP claim disappears, which nothing else notices.
	#[test]
	fn exports_pdf_a() {
		let pdf = render(&huf_doc(), None).unwrap();
		// PDF/A keeps the XMP metadata stream uncompressed, so a byte search is reliable.
		let xmp = String::from_utf8_lossy(&pdf);
		assert!(xmp.contains("pdfaid"), "no PDF/A identification in the XMP metadata");
		assert!(xmp.contains("<pdfaid:part>3</pdfaid:part>"), "not PDF/A part 3");
		assert!(xmp.contains("<pdfaid:conformance>B</pdfaid:conformance>"), "not conformance B");
	}

	/// The footer and cash-accounting note must compile and keep PDF/A. Text in the content stream
	/// is font-encoded, so the check is that both render and differ from the plain page.
	#[test]
	fn renders_software_footer_and_cash_accounting() {
		let mut d: serde_json::Value = serde_json::from_str(&huf_doc()).unwrap();
		d["software"] = serde_json::json!({ "name": "Symbion Invoice", "version": "0.5.0" });
		d["cashAccounting"] = true.into();
		let pdf = render(&d.to_string(), None).unwrap();
		assert!(String::from_utf8_lossy(&pdf).contains("<pdfaid:part>3</pdfaid:part>"));
		assert_ne!(pdf, render(&huf_doc(), None).unwrap());
		d["lang"] = "hu-en".into();
		assert!(render(&d.to_string(), None).is_ok());
	}

	/// A 1x1 opaque RGB PNG.
	const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0\x90wS\xde\0\0\0\x0cIDAT\x08\xd7c\xf8\xcf\xc0\0\0\x03\x01\x01\0\x18\xdd\x8d\xb0\0\0\0\0IEND\xaeB`\x82";

	/// Every branch the `invoice-5` data adds: bilingual labels, a period, the seller email, the
	/// accent, both watermarks and an embedded logo, which is an image XObject in the PDF.
	#[test]
	fn renders_branded_bilingual_watermarked() {
		assert_eq!(sniff(PNG), Some("logo.png"));
		assert_eq!(sniff(b"<svg"), None);
		let mut d: serde_json::Value = serde_json::from_str(&huf_doc()).unwrap();
		d["lang"] = "hu-en".into();
		d["testMode"] = true.into();
		d["preview"] = true.into();
		d["brand"] = serde_json::json!({ "logo": "logo.png", "accent": "#1a7f5a" });
		d["seller"]["email"] = "s@example.com".into();
		d["invoice"]["periodStart"] = "2026-09-01".into();
		d["invoice"]["periodEnd"] = "2026-09-30".into();
		d["invoice"]["kind"] = "STORNO".into();
		d["invoice"]["original"] = "X/0".into();
		let pdf = render(&d.to_string(), Some(("logo.png", PNG.to_vec()))).unwrap();
		assert!(pdf.windows(14).any(|w| w == b"/Subtype/Image"), "logo not embedded");
		let plain = render(&huf_doc(), None).unwrap();
		assert!(!plain.windows(14).any(|w| w == b"/Subtype/Image"), "image without a logo");
	}

	/// Typst does not error on overflow: a logo too wide for the masthead is silently clipped or
	/// drawn under the info box. Lay out extreme aspect ratios beside the widest info box (hu-en,
	/// STORNO, period) and check where the logo actually lands on page 1.
	#[test]
	fn the_logo_fits_at_any_aspect_ratio() {
		use typst::layout::{Abs, Frame, FrameItem, Point, Size};

		fn walk(frame: &Frame, at: Point, image: &mut Vec<(Point, Size)>, label: &mut Option<Abs>) {
			for (pos, item) in frame.items() {
				let p = at + *pos;
				match item {
					FrameItem::Group(g) => {
						walk(
							&g.frame,
							p + Point::new(g.transform.tx, g.transform.ty),
							image,
							label,
						);
					}
					FrameItem::Image(_, size, _) => image.push((p, *size)),
					FrameItem::Text(t) if t.text.starts_with("Számla sorszáma") => {
						label.get_or_insert(p.x);
					}
					_ => {}
				}
			}
		}

		let eps = Abs::pt(0.1);
		for (w, h) in [(10, 400), (100, 100), (3438, 625), (4000, 100), (4000, 10)] {
			let mut d: serde_json::Value = serde_json::from_str(&huf_doc()).unwrap();
			d["lang"] = "hu-en".into();
			d["brand"] = serde_json::json!({ "logo": "logo.svg", "accent": "#1a7f5a" });
			d["invoice"]["periodStart"] = "2026-09-01".into();
			d["invoice"]["periodEnd"] = "2026-09-30".into();
			d["invoice"]["kind"] = "STORNO".into();
			d["invoice"]["original"] = "X/0".into();
			let svg = format!(
				r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}"><rect width="{w}" height="{h}"/></svg>"#
			);
			let doc = layout(&d.to_string(), Some(("logo.svg", svg.into_bytes()))).unwrap();
			let (mut images, mut label) = (Vec::new(), None);
			walk(&doc.pages()[0].frame, Point::zero(), &mut images, &mut label);
			let [(pos, size)] = images[..] else {
				panic!("{w}x{h}: expected one image, found {}", images.len())
			};
			let label = label.unwrap_or_else(|| panic!("{w}x{h}: no number label"));
			assert!(size.y <= Abs::mm(14.0) + eps, "{w}x{h}: logo {:.1} mm tall", size.y.to_mm());
			assert!(
				pos.x >= Abs::mm(18.0) - eps,
				"{w}x{h}: logo starts at {:.1} mm",
				pos.x.to_mm()
			);
			assert!(
				pos.x + size.x < label - Abs::mm(6.0) + eps,
				"{w}x{h}: logo ends at {:.1} mm, info box label at {:.1} mm",
				(pos.x + size.x).to_mm(),
				label.to_mm()
			);
		}
	}

	/// PNG magic bytes over garbage pass `sniff`; the render drops the logo instead of failing.
	#[test]
	fn a_corrupt_logo_renders_without_it() {
		let mut bad = PNG[..8].to_vec();
		bad.extend_from_slice(b"definitely not an image");
		let d: serde_json::Value = serde_json::from_str(&huf_doc()).unwrap();
		let pdf = render_doc(d, Some(("logo.png", bad))).unwrap();
		assert!(pdf.starts_with(b"%PDF"));
		assert!(!pdf.windows(14).any(|w| w == b"/Subtype/Image"), "the bad logo was embedded");
	}

	/// A glyph no bundled font has is a hard export failure under PDF/A, where plain PDF drew
	/// tofu. A non-Latin buyer name therefore fails `RENDER_PDF` rather than printing a broken
	/// page — deliberate, but it must be discovered here and not in a stalled NAV filing.
	#[test]
	fn a_glyph_no_font_has_fails_the_export() {
		let d = doc("HUF", &serde_json::Value::Null, &serde_json::Value::Null, "株式会社");
		assert!(render(&d, None).is_err());
	}

	#[test]
	fn sniff_skips_a_logo_over_the_pixel_cap() {
		let png = |w: u32, h: u32| {
			let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
			b.extend(w.to_be_bytes());
			b.extend(h.to_be_bytes());
			b
		};
		assert_eq!(sniff(&png(65535, 65535)), None);
		assert_eq!(sniff(&png(100, 100)), Some("logo.png"));
		assert_eq!(sniff(&png(100, 100)[..20]), None);

		// SOI, an APP0 segment to skip, then SOF0 with height then width.
		let jpeg = |w: u16, h: u16| {
			let mut b = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 11, 8];
			b.extend(h.to_be_bytes());
			b.extend(w.to_be_bytes());
			b
		};
		assert_eq!(sniff(&jpeg(65535, 65535)), None);
		assert_eq!(sniff(&jpeg(640, 480)), Some("logo.jpg"));
	}
}

// vim: ts=4
