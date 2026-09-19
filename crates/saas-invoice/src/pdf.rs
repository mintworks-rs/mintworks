//! Typst PDF rendering, and the `RENDER_PDF` job handler behind it.
//!
//! The two `.typ` files in `templates/invoice/` are `include_str!`-ed and concatenated into
//! a single in-memory [`Source`], so [`InvoiceWorld`] never resolves a path: `source()`
//! answers for exactly one [`FileId`] and `file()` always fails. That is why neither
//! template may `#import` the other, and why [`TEMPLATE_VERSION`] can be truthful — the
//! bytes that produced a PDF are the bytes that were compiled into the binary.
//!
//! Every amount is formatted to a string *here*, in integer arithmetic, and handed to the
//! template as text. Typst does no arithmetic on money, so no float can reach the page.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use saas_core::app::App;
use saas_core::error::{ClResult, Error};
use saas_core::job::{Job, Runner};
use saas_core::money::{Money, Qty, format_scaled};
use saas_core::types::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use typst::World;
use typst::diag::FileResult;
use typst::foundations::{Bytes, Datetime, Dict, Duration, Value};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{
	Library, LibraryExt,
	syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot},
};

use crate::issue::KIND_RENDER_PDF;
use crate::numbering;
use crate::store::{
	Invoice, InvoiceDocument, InvoiceLine, InvoiceStore, InvoiceVatGroup, SellerVersion,
};
use crate::taxrule::vat_notes;
use crate::vat::{VatClass, VatCode};

/// Bumped whenever a template change would produce a materially different page. Stored on
/// every `invoice_documents` row, so a reprint can be told from the layout that made it.
pub const TEMPLATE_VERSION: &str = "invoice-3";

const STRINGS_TYP: &str = include_str!("../../../templates/invoice/strings.typ");
const INVOICE_TYP: &str = include_str!("../../../templates/invoice/invoice.typ");

// ---------------------------------------------------------------- the world

/// The bundled faces are the same on every render, and parsing the ~10 MB of `typst-assets`
/// font bytes per `RENDER_PDF` job dominated the render itself.
static FONTS: LazyLock<(LazyHash<FontBook>, Vec<Font>)> = LazyLock::new(|| {
	let mut book = FontBook::new();
	let mut fonts = Vec::new();
	for bytes in typst_assets::fonts() {
		for font in Font::iter(Bytes::new(bytes)) {
			book.push(font.info().clone());
			fonts.push(font);
		}
	}
	(LazyHash::new(book), fonts)
});

/// The concatenated template, for the same reason.
static SOURCE_TEXT: LazyLock<String> = LazyLock::new(|| format!("{STRINGS_TYP}\n{INVOICE_TYP}"));

/// A [`World`] over one synthetic source file and the fonts bundled in `typst-assets`.
/// Nothing on disk is reachable from a template.
pub struct InvoiceWorld {
	library: LazyHash<Library>,
	main: FileId,
	source: Source,
}

impl InvoiceWorld {
	/// `data` is the JSON document the template reads back as `sys.inputs.invoice`.
	fn new(data: &str) -> ClResult<Self> {
		let mut inputs = Dict::new();
		inputs.insert("invoice".into(), Value::Str(data.into()));

		let vpath = VirtualPath::new("/invoice.typ")
			.map_err(|e| Error::internal(format!("saas-invoice/pdf: bad template path: {e:?}")))?;
		let main = RootedPath::new(VirtualRoot::Project, vpath).intern();
		Ok(Self {
			library: LazyHash::new(Library::builder().with_inputs(inputs).build()),
			main,
			source: Source::new(main, SOURCE_TEXT.clone()),
		})
	}
}

impl World for InvoiceWorld {
	fn library(&self) -> &LazyHash<Library> {
		&self.library
	}

	fn book(&self) -> &LazyHash<FontBook> {
		&FONTS.0
	}

	fn main(&self) -> FileId {
		self.main
	}

	fn source(&self, id: FileId) -> FileResult<Source> {
		if id == self.main {
			Ok(self.source.clone())
		} else {
			Err(typst::diag::FileError::NotFound(id.vpath().get_without_slash().into()))
		}
	}

	fn file(&self, id: FileId) -> FileResult<Bytes> {
		Err(typst::diag::FileError::NotFound(id.vpath().get_without_slash().into()))
	}

	fn font(&self, index: usize) -> Option<Font> {
		FONTS.1.get(index).cloned()
	}

	/// `None`: the template never calls `datetime`, and a PDF must not vary with the clock.
	fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
		None
	}
}

/// Compiles the template against `data` and returns PDF bytes. Pure and blocking — call it
/// from `spawn_blocking`, never straight off an async task.
pub fn render(data: &str) -> ClResult<Vec<u8>> {
	let world = InvoiceWorld::new(data)?;
	let compiled = typst::compile(&world);
	let doc = compiled.output.map_err(|errs| {
		let first = errs.first().map(|e| e.message.to_string()).unwrap_or_default();
		Error::internal(format!("saas-invoice/pdf: typst compile failed: {first}"))
	})?;
	// PDF/A-3b: the archived file is the statutory evidence copy, and A-3 is the one archival
	// level that permits embedded files, leaving the Factur-X XML attachment open later.
	let standards = typst_pdf::PdfStandards::new(&[typst_pdf::PdfStandard::A_3b])
		.map_err(|e| Error::internal(format!("saas-invoice/pdf: PDF/A-3b: {}", e.message())))?;
	typst_pdf::pdf(&doc, &typst_pdf::PdfOptions { standards, ..Default::default() }).map_err(
		|errs| {
			let first = errs.first().map(|e| e.message.to_string()).unwrap_or_default();
			Error::internal(format!("saas-invoice/pdf: typst pdf export failed: {first}"))
		},
	)
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
		out.push(if lang == "hu" { ',' } else { '.' });
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
		.ok_or_else(|| Error::internal("saas-invoice/pdf: enum did not serialise as a string"))
}

fn addr(postcode: Option<&str>, city: Option<&str>, street: Option<&str>) -> String {
	let head = [postcode, city].into_iter().flatten().collect::<Vec<_>>().join(" ");
	[head.as_str(), street.unwrap_or_default()]
		.into_iter()
		.filter(|s| !s.is_empty())
		.collect::<Vec<_>>()
		.join(", ")
}

/// Hungarian for a Hungarian buyer, English for everyone else: there is no language column
/// on any table and no `invoice.lang` setting.
fn lang_for(invoice: &Invoice) -> &'static str {
	match invoice.buyer_country.as_deref() {
		Some(c) if c.eq_ignore_ascii_case("HU") => "hu",
		_ => "en",
	}
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
) -> ClResult<String> {
	let lang = lang_for(invoice);
	let m = |v: Money| money(v, lang);

	let doc = serde_json::json!({
		"lang": lang,
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
				"saas-invoice/pdf: an issued invoice has no issued_at",
			))?)?,
			"fulfilmentDate": invoice.fulfilment_date,
			"dueDate": invoice.due_date,
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
				// 172. § rate is worse than no PDF, so the render fails as `saas_nav::xml` does.
				"value": group(&format_scaled(invoice.huf_rate_e6.ok_or_else(|| Error::internal(
					"saas-invoice/pdf: a foreign-currency invoice has no huf_rate_e6",
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
		"totals": { "net": m(invoice.net), "vat": m(invoice.vat), "gross": m(invoice.gross) },
	});

	serde_json::to_string(&doc)
		.map_err(|e| Error::internal(format!("saas-invoice/pdf: data document: {e}")))
}

// ---------------------------------------------------------------- storage

/// `{data_dir}/documents/{sha[0..2]}/{sha[2..4]}/{sha}.pdf`. Content-addressed, so the
/// stored hash is both the location and the integrity check and there is no path column.
pub fn doc_path(data_dir: &str, sha256: &str) -> ClResult<PathBuf> {
	let (a, b) = sha256
		.get(0..2)
		.zip(sha256.get(2..4))
		.ok_or_else(|| Error::internal("saas-invoice/pdf: short sha256"))?;
	Ok(PathBuf::from(data_dir)
		.join("documents")
		.join(a)
		.join(b)
		.join(format!("{sha256}.pdf")))
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
		return Err(Error::internal("saas-invoice/pdf: invoice is not issued"));
	}
	// The **frozen** version, not `seller_by_id`: this job runs after the issue transaction
	// commits and is retried with backoff, so a seller edit landing in that window used to
	// print a supplier block the invoice was never issued under.
	let seller_ver = invoice
		.seller_ver
		.ok_or_else(|| Error::internal("saas-invoice/pdf: an issued invoice has no seller_ver"))?;
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

	let data = document(&seller, &invoice, &lines, &groups, original.as_deref())?;
	let data_dir = app.config.data_dir.clone();

	// Typst compilation is CPU-bound and the write is blocking; neither belongs on the
	// async runtime.
	let (sha256, bytes) = tokio::task::spawn_blocking(move || -> ClResult<(String, i64)> {
		let pdf = render(&data)?;
		let sha256 = hex::encode(Sha256::digest(&pdf));
		let path = doc_path(&data_dir, &sha256)?;
		if let Some(dir) = path.parent() {
			std::fs::create_dir_all(dir)
				.map_err(|e| Error::Unavailable(format!("saas-invoice/pdf: mkdir: {e}")))?;
		}
		// Content-addressed: an identical file already there is the same bytes.
		std::fs::write(&path, &pdf)
			.map_err(|e| Error::Unavailable(format!("saas-invoice/pdf: write: {e}")))?;
		Ok((sha256, i64::try_from(pdf.len()).unwrap_or(i64::MAX)))
	})
	.await
	.map_err(|e| Error::internal(format!("saas-invoice/pdf: render task: {e}")))??;

	// Gated on the version read above: a note edit committing during the compile passed the
	// reader-pool guard in `Invoices::patch_notes`, and the PDF — served `immutable` — kept the
	// old note forever. Retryable, so the job re-renders against the new note.
	let stored = store
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
		.await?;
	if !stored && store.invoice_document(invoice_id).await?.is_none() {
		return Err(Error::Unavailable("saas-invoice/pdf: the note changed mid-render".to_owned()));
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
				Error::internal(format!("saas-invoice/pdf: bad {KIND_RENDER_PDF} payload: {e}"))
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
		assert!(render(&huf_doc()).is_ok());
		let rate = serde_json::json!({
			"quote": "EUR", "value": "400,000000", "date": "2026-09-16", "source": "MNB",
		});
		let huf = serde_json::json!("216,00");
		assert!(render(&doc("EUR", &rate, &huf)).is_ok());

		// A foreign-currency invoice can carry a group with no `vat_huf` beside one that has it
		// — `storno::…` guards for exactly that state. `any-huf` switches the column on for the
		// whole table, so the missing cell has to fall back to blank rather than fail the render.
		let mut mixed: serde_json::Value = serde_json::from_str(&doc("EUR", &rate, &huf)).unwrap();
		mixed["groups"].as_array_mut().unwrap().push(serde_json::json!({
			"vatRate": "5%", "net": "1,00", "vat": "0,05", "gross": "1,05",
		}));
		assert!(render(&mixed.to_string()).is_ok());
	}

	/// The archived PDF is the statutory evidence copy, so it is PDF/A. Without the validator
	/// the export still succeeds — only the XMP claim disappears, which nothing else notices.
	#[test]
	fn exports_pdf_a() {
		let pdf = render(&huf_doc()).unwrap();
		// PDF/A keeps the XMP metadata stream uncompressed, so a byte search is reliable.
		let xmp = String::from_utf8_lossy(&pdf);
		assert!(xmp.contains("pdfaid"), "no PDF/A identification in the XMP metadata");
		assert!(xmp.contains("<pdfaid:part>3</pdfaid:part>"), "not PDF/A part 3");
		assert!(xmp.contains("<pdfaid:conformance>B</pdfaid:conformance>"), "not conformance B");
	}

	/// A glyph no bundled font has is a hard export failure under PDF/A, where plain PDF drew
	/// tofu. A non-Latin buyer name therefore fails `RENDER_PDF` rather than printing a broken
	/// page — deliberate, but it must be discovered here and not in a stalled NAV filing.
	#[test]
	fn a_glyph_no_font_has_fails_the_export() {
		let d = doc("HUF", &serde_json::Value::Null, &serde_json::Value::Null, "株式会社");
		assert!(render(&d).is_err());
	}

	#[test]
	fn doc_path_fans_out_on_the_hash() {
		let p = doc_path("data", &"ab".repeat(32)).unwrap();
		assert!(p.ends_with(format!("documents/ab/ab/{}.pdf", "ab".repeat(32))), "{p:?}");
		assert!(doc_path("data", "abc").is_err());
	}
}

// vim: ts=4
