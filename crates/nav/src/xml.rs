//! The `invoiceData` document, hand-built with `quick-xml`.
//!
//! Where the vendored `xsd/invoiceData.xsd` contradicts the field mapping, the schema wins:
//! `exchangeRate` and every `…HUF` element are mandatory, so a HUF invoice emits rate
//! `1.000000` and HUF figures equal to the base ones.
//!
//! Nothing here invents a value. A mandatory element whose source column is NULL is an
//! error, never an empty string or a zero.

use mintworks_core::money::format_scaled;
use mintworks_core::prelude::{ClResult, Error, Money};
use mintworks_invoice::{
	DiscountKind, Invoice, InvoiceKind, InvoiceLine, InvoiceVatGroup, PartyKind, PaymentMethod,
	SellerVersion, VatClass, apportion, date_of, to_base,
};
use quick_xml::{
	Writer,
	events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event},
};

pub const DATA_NS: &str = "http://schemas.nav.gov.hu/OSA/3.0/data";
pub const BASE_NS: &str = "http://schemas.nav.gov.hu/OSA/3.0/base";

fn missing(field: &str) -> Error {
	Error::Validation(format!("NAV mapping: `{field}` is required but has no value"))
}

fn xml_err<E: std::fmt::Display>(e: E) -> Error {
	Error::Internal(format!("NAV XML writer: {e}"))
}

struct Xml(Writer<Vec<u8>>);

impl Xml {
	fn open(&mut self, name: &str) -> ClResult<()> {
		self.0.write_event(Event::Start(BytesStart::new(name))).map_err(xml_err)
	}

	fn close(&mut self, name: &str) -> ClResult<()> {
		self.0.write_event(Event::End(BytesEnd::new(name))).map_err(xml_err)
	}

	fn text(&mut self, name: &str, value: &str) -> ClResult<()> {
		self.open(name)?;
		self.0.write_event(Event::Text(BytesText::new(value))).map_err(xml_err)?;
		self.close(name)
	}

	fn opt(&mut self, name: &str, value: Option<&str>) -> ClResult<()> {
		match value {
			Some(v) => self.text(name, v),
			None => Ok(()),
		}
	}

	/// `base:taxpayerId` + `base:vatCode` + `base:countyCode`, from an 11-digit tax number
	/// however it is punctuated. The last two are optional in `base:TaxNumberType`.
	fn tax_number(&mut self, wrapper: &str, raw: &str) -> ClResult<()> {
		let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
		if digits.len() < 8 {
			return Err(Error::Validation(format!(
				"NAV mapping: tax number `{raw}` has fewer than 8 digits"
			)));
		}
		self.open(wrapper)?;
		self.text("base:taxpayerId", &digits[..8])?;
		if digits.len() >= 9 {
			self.text("base:vatCode", &digits[8..9])?;
		}
		if digits.len() >= 11 {
			self.text("base:countyCode", &digits[9..11])?;
		}
		self.close(wrapper)
	}

	fn address(
		&mut self,
		wrapper: &str,
		country: &str,
		postcode: &str,
		city: &str,
		street: &str,
	) -> ClResult<()> {
		self.open(wrapper)?;
		self.open("base:simpleAddress")?;
		// `CountryCodeType` and `PostalCodeType` are both uppercase-only. Writes are normalised
		// at the service handle, but older rows and the unvalidated `sellers` row still hold
		// lowercase, which fails the schema on an invoice that already has a number.
		self.text("base:countryCode", &country.to_ascii_uppercase())?;
		self.text("base:postalCode", &postcode.to_ascii_uppercase())?;
		self.text("base:city", city)?;
		self.text("base:additionalAddressDetail", street)?;
		self.close("base:simpleAddress")?;
		self.close(wrapper)
	}
}

/// `invoices.issued_at` as a calendar date, in Europe/Budapest — the same conversion the
/// invoice's own number and dates went through (`mintworks_invoice::numbering::date_of`), because
/// NAV must be told the date printed on the document (Áfa tv. 169. §).
fn issue_date(invoice: &Invoice) -> ClResult<String> {
	date_of(invoice.issued_at.ok_or_else(|| missing("issued_at"))?)
}

/// The HUF equivalent of `amount`. On a HUF invoice that is `amount` itself; otherwise the
/// stored `*_huf` column when the schema has one, else the §8 conversion.
///
/// No rescaling: `Money` is two decimals for every currency, so `to_base` cannot hand back a
/// figure on a different scale than the `base:MonetaryType` element it is printed into.
///
/// The `stored` branch is `invoice_vat_groups.{net,vat,gross}_huf`, persisted in HUF minor
/// units already.
fn to_huf(invoice: &Invoice, amount: Money, stored: Option<Money>) -> ClResult<Money> {
	if invoice.currency == "HUF" {
		return Ok(amount);
	}
	if let Some(v) = stored {
		return Ok(v);
	}
	to_base(amount, invoice.huf_rate_e6.ok_or_else(|| missing("huf_rate_e6"))?)
}

fn payment_method(m: PaymentMethod) -> &'static str {
	match m {
		PaymentMethod::Transfer => "TRANSFER",
		PaymentMethod::Card => "CARD",
		PaymentMethod::Cash => "CASH",
		PaymentMethod::Other => "OTHER",
	}
}

/// The `VatRateType` choice, shared by `lineVatRate` and `summaryByVatRate/vatRate` (§5.4).
/// `reason` comes from the `VatCode` itself ([`VatCode::nav_reason`]), not from
/// `invoices.vat_note`: the element is per-code, and `vat_note` is the PDF's newline-joined
/// list of invoice-level i18n keys — the right shape for a page, not for one XML element.
fn vat_rate(
	x: &mut Xml,
	wrapper: &str,
	code: mintworks_invoice::VatCode,
	rate_bp: i64,
) -> ClResult<()> {
	x.open(wrapper)?;
	match code.nav_class() {
		// `rate_bp` from the frozen column, not from `VatCode::rate_bp()`: a statutory rate
		// change must not re-file a historical invoice at the new rate. `pdf.rs`'s `vat_label`
		// already reads the column, and the two renderers have to agree.
		VatClass::Percentage(_) => x.text("vatPercentage", &format_scaled(rate_bp, 4))?,
		VatClass::Exemption | VatClass::OutOfScope => {
			let element = if matches!(code.nav_class(), VatClass::Exemption) {
				"vatExemption"
			} else {
				"vatOutOfScope"
			};
			let reason = code.nav_reason().ok_or_else(|| missing("nav_reason"))?;
			x.open(element)?;
			x.text("case", code.as_str())?;
			x.text("reason", reason)?;
			x.close(element)?;
		}
	}
	x.close(wrapper)
}

/// Build the `invoiceData` document for one issued invoice.
///
/// `original_number` is the stornoed invoice's number and is required exactly when
/// `invoice.kind` is `Storno`. The `invoiceOperation`
/// (`CREATE`/`STORNO`) lives in the `manageInvoice` request, not here.
pub fn invoice_data(
	seller: &SellerVersion,
	invoice: &Invoice,
	lines: &[InvoiceLine],
	groups: &[InvoiceVatGroup],
	original_number: Option<&str>,
) -> ClResult<String> {
	let mut x = Xml(Writer::new_with_indent(Vec::new(), b'\t', 1));
	x.0.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
		.map_err(xml_err)?;
	x.0.write_event(Event::Start(
		BytesStart::new("InvoiceData")
			.with_attributes([("xmlns", DATA_NS), ("xmlns:base", BASE_NS)]),
	))
	.map_err(xml_err)?;

	x.text("invoiceNumber", invoice.number.as_deref().ok_or_else(|| missing("number"))?)?;
	x.text("invoiceIssueDate", &issue_date(invoice)?)?;
	// The data element is not itself the invoice — a PDF is issued (§4.1).
	x.text("completenessIndicator", "false")?;
	x.open("invoiceMain")?;
	x.open("invoice")?;

	if let Some(original) = original_number {
		x.open("invoiceReference")?;
		x.text("originalInvoiceNumber", original)?;
		x.text("modifyWithoutMaster", "false")?;
		// Constant 1: at most one storno per invoice, so the chain is never longer (§6).
		x.text("modificationIndex", "1")?;
		x.close("invoiceReference")?;
	}

	x.open("invoiceHead")?;
	supplier_info(&mut x, seller)?;
	customer_info(&mut x, invoice)?;
	invoice_detail(&mut x, seller, invoice)?;
	x.close("invoiceHead")?;

	invoice_lines(&mut x, invoice, lines, groups)?;
	invoice_summary(&mut x, invoice, groups)?;

	x.close("invoice")?;
	x.close("invoiceMain")?;
	x.close("InvoiceData")?;
	String::from_utf8(x.0.into_inner()).map_err(xml_err)
}

/// Always from the [`SellerVersion`] the invoice froze at ISSUE, never from the live
/// `sellers`/`seller_versions` rows (§4.2) — the supplier's half of the rule
/// [`customer_info`] states for the buyer.
fn supplier_info(x: &mut Xml, seller: &SellerVersion) -> ClResult<()> {
	x.open("supplierInfo")?;
	x.tax_number("supplierTaxNumber", &seller.tax_number)?;
	if let Some(group) = &seller.group_member_tax_no {
		x.tax_number("groupMemberTaxNumber", group)?;
	}
	x.opt("communityVatNumber", seller.eu_vat_id.as_deref())?;
	x.text("supplierName", &seller.name)?;
	x.address("supplierAddress", &seller.country, &seller.postcode, &seller.city, &seller.street)?;
	x.opt("supplierBankAccountNumber", seller.bank_account.as_deref())?;
	if seller.vat_scheme == "ALANYI_MENTES" {
		x.text("individualExemption", "true")?;
	}
	x.close("supplierInfo")
}

/// Always from the frozen `buyer_*` snapshot, never from `billing_parties` (§4.3). A private
/// person's identifying data is withheld from the report — it appears on the PDF only.
fn customer_info(x: &mut Xml, invoice: &Invoice) -> ClResult<()> {
	let kind = invoice.buyer_kind.ok_or_else(|| missing("buyer_kind"))?;
	x.open("customerInfo")?;
	if kind == PartyKind::Person {
		x.text("customerVatStatus", "PRIVATE_PERSON")?;
		return x.close("customerInfo");
	}

	// Case-insensitive: a row written before country normalisation can hold `"hu"`, and
	// filing that buyer as `OTHER` misreports a domestic sale.
	let domestic =
		invoice.buyer_country.as_ref().map(|c| c.to_ascii_uppercase()).as_deref() == Some("HU");
	x.text("customerVatStatus", if domestic { "DOMESTIC" } else { "OTHER" })?;
	if domestic {
		let tn = invoice.buyer_tax_number.as_deref().ok_or_else(|| missing("buyer_tax_number"))?;
		x.open("customerVatData")?;
		x.open("customerTaxNumber")?;
		let digits: String = tn.chars().filter(char::is_ascii_digit).collect();
		if digits.len() < 8 {
			return Err(missing("buyer_tax_number"));
		}
		x.text("base:taxpayerId", &digits[..8])?;
		if digits.len() >= 9 {
			x.text("base:vatCode", &digits[8..9])?;
		}
		if digits.len() >= 11 {
			x.text("base:countyCode", &digits[9..11])?;
		}
		if let Some(group) = &invoice.buyer_group_tax_no {
			x.tax_number("groupMemberTaxNumber", group)?;
		}
		x.close("customerTaxNumber")?;
		x.close("customerVatData")?;
	} else if let Some(eu) = &invoice.buyer_eu_vat_id {
		x.open("customerVatData")?;
		x.text("communityVatNumber", eu)?;
		x.close("customerVatData")?;
	} else if let Some(tn) = &invoice.buyer_tax_number {
		x.open("customerVatData")?;
		x.text("thirdStateTaxId", tn)?;
		x.close("customerVatData")?;
	}

	x.text("customerName", invoice.buyer_name.as_deref().ok_or_else(|| missing("buyer_name"))?)?;
	x.address(
		"customerAddress",
		invoice.buyer_country.as_deref().ok_or_else(|| missing("buyer_country"))?,
		invoice.buyer_postcode.as_deref().ok_or_else(|| missing("buyer_postcode"))?,
		invoice.buyer_city.as_deref().ok_or_else(|| missing("buyer_city"))?,
		invoice.buyer_street.as_deref().ok_or_else(|| missing("buyer_street"))?,
	)?;
	x.close("customerInfo")
}

fn invoice_detail(x: &mut Xml, seller: &SellerVersion, invoice: &Invoice) -> ClResult<()> {
	x.open("invoiceDetail")?;
	x.text("invoiceCategory", "NORMAL")?;
	x.text(
		"invoiceDeliveryDate",
		invoice.fulfilment_date.as_deref().ok_or_else(|| missing("fulfilment_date"))?,
	)?;
	// Both or neither; XSD order puts them before `smallBusinessIndicator`.
	if let (Some(start), Some(end)) = (&invoice.period_start, &invoice.period_end) {
		x.text("invoiceDeliveryPeriodStart", start)?;
		x.text("invoiceDeliveryPeriodEnd", end)?;
		x.text("periodicalSettlement", "true")?;
	}
	if seller.small_business {
		x.text("smallBusinessIndicator", "true")?;
	}
	x.text("currencyCode", invoice.currency.as_str())?;
	// Mandatory in the XSD, so a HUF invoice reports the identity rate. An `_e6` rate is not
	// money — `format_scaled`, not `Money`.
	let rate = if invoice.currency == "HUF" {
		1_000_000
	} else {
		invoice.huf_rate_e6.ok_or_else(|| missing("huf_rate_e6"))?
	};
	x.text("exchangeRate", &format_scaled(rate, 6))?;
	x.text("paymentMethod", payment_method(invoice.payment_method))?;
	x.opt("paymentDate", invoice.due_date.as_deref())?;
	// The only output is a PDF delivered electronically (§4.4).
	x.text("invoiceAppearance", "ELECTRONIC")?;
	x.close("invoiceDetail")
}

/// Each line's net in HUF, apportioned out of its **group's** stored HUF net rather than
/// converted independently.
///
/// A per-line conversion rounds on its own, so at a rate like `350.555555` two lines of net
/// `0.01` give `351 + 351 = 702` against a `vatRateNetAmountHUF` of `701` — exactly the
/// per-line rounding drift `mintworks_invoice::vat` exists to avoid, reintroduced in the HUF
/// projection, and what NAV's cross-field validation rejects. `apportion` makes each part
/// the difference of two cumulative roundings, so the parts always sum back to the group
/// total; negative weights work, which a storno needs.
///
/// A line whose frozen `vat_code` matches no group cannot happen — the groups are computed
/// from the same lines — but it falls back to a direct conversion rather than silently
/// reporting zero.
fn line_nets_huf(
	invoice: &Invoice,
	lines: &[InvoiceLine],
	groups: &[InvoiceVatGroup],
) -> ClResult<Vec<Money>> {
	let mut out: Vec<Option<Money>> = vec![None; lines.len()];
	for group in groups {
		let members: Vec<usize> = lines
			.iter()
			.enumerate()
			.filter(|(_, l)| l.vat_code == group.vat_code)
			.map(|(i, _)| i)
			.collect();
		if members.is_empty() {
			continue;
		}
		let nets: Vec<Money> = members.iter().map(|&i| lines[i].net).collect();
		let total = to_huf(invoice, group.net, group.net_huf)?;
		for (&i, part) in members.iter().zip(apportion(total, &nets)?) {
			out[i] = Some(part);
		}
	}
	lines
		.iter()
		.zip(out)
		.map(|(line, part)| match part {
			Some(v) => Ok(v),
			None => to_huf(invoice, line.net, None),
		})
		.collect()
}

fn invoice_lines(
	x: &mut Xml,
	invoice: &Invoice,
	lines: &[InvoiceLine],
	groups: &[InvoiceVatGroup],
) -> ClResult<()> {
	let nets_huf = line_nets_huf(invoice, lines, groups)?;
	x.open("invoiceLines")?;
	x.text("mergedItemIndicator", "false")?;
	for (line, net_huf) in lines.iter().zip(&nets_huf) {
		x.open("line")?;
		x.text("lineNumber", &line.line_no.to_string())?;
		// Mandatory on every line of a STORNO data supply, not optional as the XSD's
		// `minOccurs="0"` suggests: NAV's async ERROR 21 `LINE_MODIFICATION_EXPECTED` refuses
		// the whole supply without it (interfész specifikáció v3.0 §2.5.1, §2.2.3.1.1).
		if invoice.kind == InvoiceKind::Storno {
			x.open("lineModificationReference")?;
			// The counter-invoice's lines continue the original's numbering and `storno::run`
			// copies every line, so the original's count is this document's own.
			// `modificationIndex` is constant 1 — at most one storno per invoice.
			let original_lines = i64::try_from(lines.len())
				.map_err(|_| Error::internal("NAV mapping: implausible line count"))?;
			x.text("lineNumberReference", &(original_lines + line.line_no).to_string())?;
			// `CREATE`, never `MODIFY`: the `INVALID_LINE_OPERATION` validation rejects
			// `MODIFY` outright, and a storno adds negated lines rather than editing any.
			x.text("lineOperation", "CREATE")?;
			x.close("lineModificationReference")?;
		}
		x.text("lineExpressionIndicator", "true")?;
		// No column distinguishes goods from services; v1 sells only services (§10.1).
		x.text("lineNatureIndicator", "SERVICE")?;
		x.text("lineDescription", &line.description)?;
		// `line.note` is deliberately never filed: it is our metadata, not a statutory
		// particular, and NAV's only per-line free text is `lineDescription`/
		// `discountDescription`, both cross-validated after the invoice is immutable.
		x.text("quantity", &line.qty.to_decimal_string())?;
		x.text("unitOfMeasure", "OWN")?;
		x.text("unitOfMeasureOwn", &line.unit)?;
		x.text("unitPrice", &line.unit_price.to_decimal_string())?;
		// No `unitPriceHUF`: it is optional and can only be an independent per-line conversion,
		// which cannot agree with the *apportioned* `lineNetAmountHUF` under NAV's
		// `lineNetAmount == quantity × unitPrice` check. Same reason `lineVatData` is omitted.
		if line.discount_amount != Money::ZERO {
			x.open("lineDiscountData")?;
			x.opt("discountDescription", line.discount_description.as_deref())?;
			// The resolved amount, whatever `discount_kind` was; `net` is already net of it.
			x.text("discountValue", &line.discount_amount.to_decimal_string())?;
			// `discountRate` only when it describes `discountValue` alone: with an invoice-level
			// discount folded into `discount_amount`, no rate is the line's. Dropped rather than
			// corrected — NAV cross-validates it after the invoice is immutable.
			if let (Some(DiscountKind::Percent), Some(bp)) =
				(line.discount_kind, line.discount_value)
				&& line.qty.times(line.unit_price)?.mul_bp(bp)? == line.discount_amount
			{
				x.text("discountRate", &format_scaled(bp, 4))?;
			}
			x.close("lineDiscountData")?;
		}
		x.open("lineAmountsNormal")?;
		x.open("lineNetAmountData")?;
		x.text("lineNetAmount", &line.net.to_decimal_string())?;
		x.text("lineNetAmountHUF", &net_huf.to_decimal_string())?;
		x.close("lineNetAmountData")?;
		vat_rate(x, "lineVatRate", line.vat_code, line.vat_rate_bp)?;
		// `lineVatData` and `lineGrossAmountData` are deliberately omitted: line VAT is the
		// group total apportioned back, and NAV cross-validates line sums (§5.3).
		x.close("lineAmountsNormal")?;
		x.close("line")?;
	}
	x.close("invoiceLines")
}

fn invoice_summary(x: &mut Xml, invoice: &Invoice, groups: &[InvoiceVatGroup]) -> ClResult<()> {
	// Stable order, so two exports of the same invoice are byte-identical (§7.1).
	let mut ordered: Vec<&InvoiceVatGroup> = groups.iter().collect();
	ordered.sort_by_key(|g| g.vat_code);

	let (mut net_huf, mut vat_huf, mut gross_huf) = (0i64, 0i64, 0i64);
	x.open("invoiceSummary")?;
	x.open("summaryNormal")?;
	for group in &ordered {
		let n = to_huf(invoice, group.net, group.net_huf)?;
		let v = to_huf(invoice, group.vat, group.vat_huf)?;
		let g = to_huf(invoice, group.gross, group.gross_huf)?;
		net_huf += n.0;
		vat_huf += v.0;
		gross_huf += g.0;

		x.open("summaryByVatRate")?;
		vat_rate(x, "vatRate", group.vat_code, group.vat_rate_bp)?;
		x.open("vatRateNetData")?;
		x.text("vatRateNetAmount", &group.net.to_decimal_string())?;
		x.text("vatRateNetAmountHUF", &n.to_decimal_string())?;
		x.close("vatRateNetData")?;
		x.open("vatRateVatData")?;
		x.text("vatRateVatAmount", &group.vat.to_decimal_string())?;
		x.text("vatRateVatAmountHUF", &v.to_decimal_string())?;
		x.close("vatRateVatData")?;
		x.open("vatRateGrossData")?;
		x.text("vatRateGrossAmount", &group.gross.to_decimal_string())?;
		x.text("vatRateGrossAmountHUF", &g.to_decimal_string())?;
		x.close("vatRateGrossData")?;
		x.close("summaryByVatRate")?;
	}
	// The HUF totals are sums of the per-group figures, never a fresh conversion (§7.2).
	x.text("invoiceNetAmount", &invoice.net.to_decimal_string())?;
	x.text("invoiceNetAmountHUF", &Money(net_huf).to_decimal_string())?;
	x.text("invoiceVatAmount", &invoice.vat.to_decimal_string())?;
	x.text("invoiceVatAmountHUF", &Money(vat_huf).to_decimal_string())?;
	x.close("summaryNormal")?;
	x.open("summaryGrossData")?;
	x.text("invoiceGrossAmount", &invoice.gross.to_decimal_string())?;
	x.text("invoiceGrossAmountHUF", &Money(gross_huf).to_decimal_string())?;
	x.close("summaryGrossData")?;
	x.close("invoiceSummary")
}

// vim: ts=4
