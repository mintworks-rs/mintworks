//! *Adóhatósági ellenőrzési adatszolgáltatás* — the tax-authority audit data export.
//!
//! A statutory obligation under 23/2014. (VI. 30.) NGM rendelet, not a convenience
//! feature. 11/A. § requires the function to carry that Hungarian name verbatim, so the
//! admin UI and the user documentation must not translate it.
//!
//! It **never contacts NAV**, and it is driven from `invoices` — never from
//! `nav_submissions` — so an invoice that failed to report still appears
//! (`nav-mapping.md` §9.3). 13/A. § (1) lets the file use the Online Számla `invoiceData`
//! structure, which is why one writer serves both this and reporting.
//!
//! The export itself is [`crate::Nav::audit_export`] — this module is the request shaping
//! around it: the selection form, the filename, and the storno back-reference lookup.

use saas_core::{error::StatusCode, prelude::*};
use saas_invoice::InvoiceKind;
use saas_invoice::store::safe_filename_part;

/// `Content-Type` of the produced file. UTF-8 is mandatory — non-UTF-8 output is one of
/// NAV's named recurring rejection causes.
pub const CONTENT_TYPE: &str = "application/xml; charset=utf-8";

/// Which invoices the auditor asked for. Exactly one form, never both and never neither.
#[derive(Debug, Clone, Copy)]
pub enum Selection<'a> {
	/// `invoices.issued_at` within `[from, to]`, inclusive, `YYYY-MM-DD` UTC days.
	IssueDate { from: &'a str, to: &'a str },
	/// `invoices.number` between two rendered numbers, inclusive.
	Number { from: &'a str, to: &'a str },
}

impl Selection<'_> {
	/// `adatexport_{tax_number}_{from}_{to}.xml`. Both range parts are sanitised by
	/// [`safe_filename_part`], which among other things renders the number form's
	/// `A2026/000001` as `A2026-000001` — `/` is not legal in a filename.
	///
	/// It is not decorative: [`selection`] validates nothing — the date form's `from`/`to` are
	/// only ever checked inside [`crate::Nav::audit_export`] by `numbering::utc_span` — so a
	/// quote or a CRLF straight from a query parameter would be header injection.
	///
	/// No statutory filename rule was found; this is our convention (`nav-mapping.md` §9.3).
	pub fn filename(&self, tax_number: &str) -> String {
		let core: String = tax_number.chars().filter(char::is_ascii_digit).take(8).collect();
		let (from, to) = match *self {
			Self::IssueDate { from, to } | Self::Number { from, to } => {
				(safe_filename_part(from), safe_filename_part(to))
			}
		};
		format!("adatexport_{core}_{from}_{to}.xml")
	}
}

/// Exactly one selection form must be given.
pub fn range_error() -> Error {
	Error::coded(
		StatusCode::BAD_REQUEST,
		"E-NAV-EXPORT-RANGE",
		"give either from/to or numberFrom/numberTo, not both and not neither",
	)
}

/// Build a [`Selection`] from the endpoint's four optional query parameters.
pub fn selection<'a>(
	from: Option<&'a str>,
	to: Option<&'a str>,
	number_from: Option<&'a str>,
	number_to: Option<&'a str>,
) -> ClResult<Selection<'a>> {
	match (from, to, number_from, number_to) {
		(Some(from), Some(to), None, None) => Ok(Selection::IssueDate { from, to }),
		(None, None, Some(from), Some(to)) => Ok(Selection::Number { from, to }),
		_ => Err(range_error()),
	}
}

/// A storno carries `invoiceReference/originalInvoiceNumber`; a normal invoice carries none.
pub(crate) async fn original_number(
	invoices: &dyn saas_invoice::InvoiceStore,
	invoice: &saas_invoice::Invoice,
) -> ClResult<Option<String>> {
	if invoice.kind != InvoiceKind::Storno {
		return Ok(None);
	}
	let original_id = invoice
		.original_invoice_id
		.ok_or_else(|| Error::internal(format!("storno {} has no original", invoice.id)))?;
	let original = invoices.invoice_by_id(original_id).await?.ok_or_else(|| {
		Error::internal(format!("storno {} references a missing original", invoice.id))
	})?;
	original
		.number
		.ok_or_else(|| {
			Error::internal(format!("storno {} references an unnumbered original", invoice.id))
		})
		.map(Some)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `selection()` validates nothing and `filename()` is a public entry point of its own, so
	/// a consumer building `Content-Disposition` before calling `export()` — where
	/// `numbering::utc_span` would eventually reject the date — gets the raw query parameter.
	#[test]
	fn a_filename_never_carries_a_quote_or_a_line_break() {
		let s = Selection::IssueDate { from: "2026-01-01\"\r\nX-Foo: bar", to: "2026-01-31" };
		let name = s.filename("12345678-2-02");
		assert!(!name.contains(['"', '\r', '\n']), "{name}");
		// The number form still renders the documented convention.
		assert_eq!(
			Selection::Number { from: "A2026/000001", to: "A2026/000009" }
				.filename("12345678-2-02"),
			"adatexport_12345678_A2026-000001_A2026-000009.xml"
		);
	}
}

// vim: ts=4
