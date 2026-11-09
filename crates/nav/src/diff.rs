// SPDX-License-Identifier: MPL-2.0
//! A structural comparison of two `invoiceData` documents. `queryInvoiceData` returns NAV's own
//! serialisation, so bytes never match; every leaf value does. One output serves both `NAV_ADOPT`,
//! where empty is proof, and the person reading what differs.

use std::collections::{HashMap, HashSet};

use quick_xml::{Reader, events::Event};
use serde::Serialize;

use crate::auth::push_text;

/// How many differences are stored. [`NavDiff::total`] still counts them all.
const MAX_ITEMS: usize = 50;

/// An element's local name and its 1-based occurrence among same-named siblings.
pub type Segment = (String, usize);

/// One leaf of a document: its full path and trimmed text.
pub type Leaf = (Vec<Segment>, String);

/// One field the two sides disagree on. `field` is a stable code for the UI, never shown as is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldDiff {
	pub section: &'static str,
	/// The occurrence of `line` or `summaryByVatRate`, otherwise `None`.
	pub index: Option<usize>,
	/// The percentage or exemption case of a `VAT_RATE` item, which names the group to a reader.
	pub rate: Option<String>,
	pub field: String,
	pub ours: Option<String>,
	pub nav: Option<String>,
}

/// What `nav_submissions.adopt_detail` holds, as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NavDiff {
	pub total: usize,
	pub items: Vec<FieldDiff>,
}

impl NavDiff {
	#[must_use]
	pub fn new(mut items: Vec<FieldDiff>) -> Self {
		let total = items.len();
		items.truncate(MAX_ITEMS);
		Self { total, items }
	}
}

/// An element still open: its [`Segment`], its text, and how many of each name it has inside.
#[derive(Default)]
struct Open {
	segment: Segment,
	text: String,
	children: HashMap<String, usize>,
}

fn enter(stack: &mut Vec<Open>, name: String) {
	let occurrence = stack.last_mut().map_or(1, |parent| {
		let seen = parent.children.entry(name.clone()).or_insert(0);
		*seen += 1;
		*seen
	});
	stack.push(Open { segment: (name, occurrence), ..Open::default() });
}

/// Every non-empty leaf of `xml`, in document order. The declaration, comments, attributes,
/// prefixes and the whitespace between elements all vanish, and `<x/>` equals absent.
///
/// `None` unless `xml` is a whole document: two unreadable sides must never compare as equal.
#[must_use]
pub fn flatten(xml: &str) -> Option<Vec<Leaf>> {
	let mut reader = Reader::from_str(xml);
	let mut stack: Vec<Open> = Vec::new();
	let mut leaves = Vec::new();
	let mut seen_element = false;
	loop {
		let event = reader.read_event().ok()?;
		match &event {
			Event::Start(e) => {
				seen_element = true;
				let name = e.local_name().as_ref().to_owned();
				enter(&mut stack, name);
			}
			// Not `enter`: that would count `<x/>` among its siblings and shift their indexes.
			Event::Empty(_) => seen_element = true,
			Event::End(_) => {
				let open = stack.last()?;
				let text = open.text.trim();
				if open.children.is_empty() && !text.is_empty() {
					let path = stack.iter().map(|o| o.segment.clone()).collect();
					leaves.push((path, text.to_owned()));
				}
				stack.pop();
			}
			Event::Eof => return (seen_element && stack.is_empty()).then_some(leaves),
			other => {
				if let Some(open) = stack.last_mut() {
					push_text(&mut open.text, other);
				}
			}
		}
	}
}

/// `value` with what a decimal does not need removed, or `None` when it is not one. Leading zeros
/// stay: a postcode or a tax number is not a number, and `0123` is not `123`.
fn decimal(value: &str) -> Option<String> {
	let (negative, unsigned) = match value.strip_prefix('-') {
		Some(rest) => (true, rest),
		None => (false, value.strip_prefix('+').unwrap_or(value)),
	};
	let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
	let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
	if whole.is_empty() || !digits(whole) || !digits(fraction) {
		return None;
	}
	let fraction = fraction.trim_end_matches('0');
	let zero = fraction.is_empty() && whole.bytes().all(|b| b == b'0');
	let sign = if negative && !zero { "-" } else { "" };
	Some(if fraction.is_empty() {
		format!("{sign}{whole}")
	} else {
		format!("{sign}{whole}.{fraction}")
	})
}

fn same(ours: &str, theirs: &str) -> bool {
	ours == theirs || decimal(ours).is_some_and(|d| decimal(theirs).is_some_and(|e| d == e))
}

/// The `section` and `index` of a leaf: the nearest ancestor that means something to a reader.
fn section_of(path: &[Segment]) -> (&'static str, Option<usize>) {
	for (name, occurrence) in path.iter().rev().skip(1) {
		match name.as_str() {
			"supplierInfo" => return ("SELLER", None),
			"customerInfo" => return ("BUYER", None),
			"line" => return ("LINE", Some(*occurrence)),
			"summaryByVatRate" => return ("VAT_RATE", Some(*occurrence)),
			"invoiceSummary" => return ("TOTALS", None),
			_ => {}
		}
	}
	("DETAILS", None)
}

/// The path up to and including the first `summaryByVatRate`, if it sits in one.
fn rate_group(path: &[Segment]) -> Option<&[Segment]> {
	Some(&path[..=path.iter().position(|(name, _)| name == "summaryByVatRate")?])
}

/// Each `summaryByVatRate`'s rate, keyed by [`rate_group`], from whichever side has one: `ours`
/// first.
fn rates(sides: [&[Leaf]; 2]) -> HashMap<&[Segment], &str> {
	let mut rates = HashMap::new();
	for (path, value) in sides.iter().flat_map(|side| side.iter()) {
		let names_rate = path.last().is_some_and(|(n, _)| n == "vatPercentage" || n == "case");
		if let Some(group) = rate_group(path).filter(|_| names_rate) {
			rates.entry(group).or_insert(value.as_str());
		}
	}
	rates
}

/// Every leaf the sides disagree on, matched by full path: `ours` in order, then NAV's own.
#[must_use]
pub fn compare(ours: &[Leaf], theirs: &[Leaf]) -> Vec<FieldDiff> {
	let nav: HashMap<&[Segment], &str> =
		theirs.iter().map(|(path, value)| (path.as_slice(), value.as_str())).collect();
	let mine: HashSet<&[Segment]> = ours.iter().map(|(path, _)| path.as_slice()).collect();
	let rates = rates([ours, theirs]);
	let item = |path: &[Segment], our: Option<&str>, their: Option<&str>| {
		let (section, index) = section_of(path);
		FieldDiff {
			section,
			index,
			rate: rate_group(path).and_then(|g| rates.get(g)).map(|r| (*r).to_owned()),
			field: path.last().map_or_else(String::new, |(name, _)| name.clone()),
			ours: our.map(str::to_owned),
			nav: their.map(str::to_owned),
		}
	};
	let mut diffs: Vec<FieldDiff> = ours
		.iter()
		.filter_map(|(path, value)| {
			let their = nav.get(path.as_slice()).copied();
			let equal = their.is_some_and(|t| same(value, t));
			(!equal).then(|| item(path, Some(value), their))
		})
		.collect();
	diffs.extend(
		theirs
			.iter()
			.filter(|(path, _)| !mine.contains(path.as_slice()))
			.map(|(path, value)| item(path, None, Some(value))),
	);
	diffs
}

#[cfg(test)]
mod tests {
	use super::{FieldDiff, NavDiff, compare, flatten};

	const OURS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<InvoiceData xmlns="http://schemas.nav.gov.hu/OSA/3.0/data" xmlns:base="b">
	<invoiceNumber>A1</invoiceNumber>
	<!-- a comment -->
	<invoiceMain><invoice>
		<invoiceHead>
			<supplierInfo><supplierTaxNumber><base:taxpayerId>01234567</base:taxpayerId></supplierTaxNumber></supplierInfo>
			<customerInfo><customerName>Kft &amp; Tsa</customerName></customerInfo>
		</invoiceHead>
		<invoiceLines>
			<line><lineDescription>one</lineDescription><lineNetAmount>100.00</lineNetAmount></line>
			<line><lineDescription>two</lineDescription><lineNetAmount>50.50</lineNetAmount></line>
		</invoiceLines>
		<invoiceSummary>
			<summaryByVatRate><vatRate><vatPercentage>0.27</vatPercentage></vatRate><vatRateNetAmount>150.50</vatRateNetAmount></summaryByVatRate>
			<invoiceNetAmount>150.50</invoiceNetAmount>
		</invoiceSummary>
	</invoice></invoiceMain>
</InvoiceData>"#;

	fn diff(ours: &str, theirs: &str) -> Vec<FieldDiff> {
		compare(&flatten(ours).unwrap(), &flatten(theirs).unwrap())
	}

	/// What `queryInvoiceData` does to a document changes nothing that was filed.
	#[test]
	fn a_reserialised_document_is_the_same_document() {
		let theirs = OURS
			.replace(r#"<?xml version="1.0" encoding="UTF-8"?>"#, "")
			.replace("xmlns:base", "xmlns:ns2")
			.replace("base:", "ns2:")
			.replace("\n\t", "\n  ")
			.replace("<!-- a comment -->", "<empty/>")
			.replace("100.00", "100")
			.replace("50.50", "50.5");
		assert_eq!(diff(OURS, &theirs), []);
	}

	#[test]
	fn an_empty_element_is_absent_and_shifts_no_index() {
		assert_eq!(flatten("<a><l/><l><v>1</v></l></a>"), flatten("<a><l><v>1</v></l></a>"));
		let theirs = OURS.replace("</line>\n\t\t\t<line>", "</line><line/><line>");
		assert_ne!(theirs, OURS);
		assert_eq!(diff(OURS, &theirs), []);
	}

	#[test]
	fn a_changed_value_names_its_section_and_field() {
		let found = diff(OURS, &OURS.replace("Kft &amp; Tsa", "Bt"));
		assert_eq!(found.len(), 1);
		assert_eq!((found[0].section, found[0].index), ("BUYER", None));
		assert_eq!(found[0].field, "customerName");
		assert_eq!(found[0].ours.as_deref(), Some("Kft & Tsa"));
		assert_eq!(found[0].nav.as_deref(), Some("Bt"));

		let found = diff(OURS, &OURS.replace("01234567", "1234567"));
		assert_eq!((found[0].section, found[0].field.as_str()), ("SELLER", "taxpayerId"));

		let found = diff(OURS, &OURS.replace("<invoiceNumber>A1", "<invoiceNumber>A2"));
		assert_eq!((found[0].section, found[0].index), ("DETAILS", None));
	}

	#[test]
	fn a_sum_differs_in_its_group_and_in_the_totals() {
		let found = diff(OURS, &OURS.replace("150.50", "151"));
		assert_eq!(found.len(), 2);
		assert_eq!((found[0].section, found[0].index), ("VAT_RATE", Some(1)));
		assert_eq!(found[0].rate.as_deref(), Some("0.27"));
		assert_eq!((found[1].section, found[1].rate.as_deref()), ("TOTALS", None));
	}

	#[test]
	fn a_missing_line_and_a_nav_only_leaf_show_their_absent_side() {
		let line = "<line><lineDescription>two</lineDescription><lineNetAmount>50.50</lineNetAmount></line>";
		let found = diff(OURS, &OURS.replace(line, ""));
		assert_eq!(found.len(), 2);
		assert_eq!((found[0].section, found[0].index), ("LINE", Some(2)));
		assert_eq!(found[0].nav, None);

		let found = diff(&OURS.replace(line, ""), OURS);
		assert_eq!(found[0].ours, None);
		assert_eq!(found[0].nav.as_deref(), Some("two"));
	}

	/// Values stay text unless both are decimals, and even then leading zeros mean something.
	#[test]
	fn only_the_scale_of_a_decimal_is_forgiven() {
		let doc = |v: &str| format!("<a><b>{v}</b></a>");
		for (x, y) in [("100.00", "100"), ("-0.00", "0"), ("+1.50", "1.5"), ("10", "10.0")] {
			assert_eq!(diff(&doc(x), &doc(y)), [], "{x} {y}");
		}
		for (x, y) in [("100", "1000"), ("0123", "123"), ("-1", "1"), ("1.5", "1.05"), ("x", "x0")]
		{
			assert_eq!(diff(&doc(x), &doc(y)).len(), 1, "{x} {y}");
		}
	}

	/// Two unreadable sides must not compare as equal.
	#[test]
	fn what_is_not_a_whole_document_does_not_flatten() {
		for bad in ["", "A", "   ", "<a><b>1</b>", "<a><b>1</a></b>", "<?xml version=\"1.0\"?>"] {
			assert!(flatten(bad).is_none(), "{bad:?}");
		}
		assert!(flatten("<a>1</a>").is_some());
	}

	#[test]
	fn the_stored_diff_is_capped_and_its_total_is_not() {
		let item = FieldDiff {
			section: "LINE",
			index: Some(1),
			rate: None,
			field: "quantity".to_owned(),
			ours: Some("1".to_owned()),
			nav: None,
		};
		let capped = NavDiff::new(vec![item; 60]);
		assert_eq!((capped.total, capped.items.len()), (60, 50));
		let json = serde_json::to_value(&capped).unwrap();
		assert_eq!(json["items"][0]["section"], "LINE");
		assert_eq!(json["items"][0]["nav"], serde_json::Value::Null);
	}
}

// vim: ts=4
