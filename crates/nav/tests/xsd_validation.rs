// SPDX-License-Identifier: MPL-2.0
//! Offline validation of generated XML against the vendored NAV XSDs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{fs, path::PathBuf};

use aes::{
	Aes128,
	cipher::{Block, BlockCipherEncrypt, KeyInit},
};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use libxml::{
	error::StructuredError,
	parser::Parser,
	schemas::{SchemaParserContext, SchemaValidationContext},
};
use mintworks_core::{
	ids::SellerId,
	prelude::{ClResult, CurrencyCode, InvoiceId, Money, Qty, Timestamp},
};
use mintworks_invoice::{
	store::{
		DiscountKind, Invoice, InvoiceKind, InvoiceLine, InvoiceStatus, InvoiceVatGroup, PartyKind,
		PaymentMethod, Seller, SellerVersion, SellerVersionStatus,
	},
	vat::VatCode,
};
use mintworks_nav::xml::invoice_data;
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{method, path},
};

const XSD_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xsd");

/// NAV's schemas import one another by namespace only, with no `schemaLocation`, so
/// libxml2 cannot resolve them as published. Patch a throwaway copy rather than the
/// vendored files, which must stay byte-identical for `drift.rs` to be meaningful.
const IMPORTS: &[(&str, &str)] = &[
	("http://schemas.nav.gov.hu/NTCA/1.0/common", "common.xsd"),
	("http://schemas.nav.gov.hu/OSA/3.0/base", "invoiceBase.xsd"),
	("http://schemas.nav.gov.hu/OSA/3.0/data", "invoiceData.xsd"),
];

/// `invoiceApi.xsd` references `common:RequestPageType` and `common:ResponsePageType`, and the
/// `common.xsd` NAV's own `catalog.xml` pins OSA 3.0 to (`Common-1.0.RC3`) does not define
/// either — so `invoiceApi.xsd` does not compile as published. That upstream gap is why the
/// request envelope was never validated against it.
///
/// Both types are used only by the `query*` paging elements this framework never emits, so
/// stubbing them into the throwaway copy cannot weaken validation of anything it does emit.
/// The vendored files stay byte-identical for `drift.rs`, exactly as with the import rewrite.
const MISSING_PAGE_TYPES: &str = r#"
	<xs:simpleType name="RequestPageType">
		<xs:restriction base="xs:int"><xs:minInclusive value="1"/></xs:restriction>
	</xs:simpleType>
	<xs:simpleType name="ResponsePageType">
		<xs:restriction base="xs:int"><xs:minInclusive value="0"/></xs:restriction>
	</xs:simpleType>
"#;

/// Every test needs the rewritten schemas, and they all share one directory — so the copy
/// happens exactly once, or a test reads a file another thread is still writing.
fn resolvable_schema_dir() -> PathBuf {
	static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
	DIR.get_or_init(build_schema_dir).clone()
}

fn build_schema_dir() -> PathBuf {
	let dir = std::env::temp_dir().join(format!("mintworks-nav-xsd-{}", std::process::id()));
	fs::create_dir_all(&dir).unwrap();
	for entry in fs::read_dir(XSD_DIR).unwrap() {
		let path = entry.unwrap().path();
		if path.extension().is_none_or(|ext| ext != "xsd") {
			continue;
		}
		let mut xsd = fs::read_to_string(&path).unwrap();
		for (ns, file) in IMPORTS {
			xsd = xsd.replace(
				&format!("<xs:import namespace=\"{ns}\"/>"),
				&format!("<xs:import namespace=\"{ns}\" schemaLocation=\"{file}\"/>"),
			);
		}
		if path.file_name().is_some_and(|n| n == "common.xsd") {
			xsd = xsd.replace("</xs:schema>", &format!("{MISSING_PAGE_TYPES}</xs:schema>"));
		}
		fs::write(dir.join(path.file_name().unwrap()), xsd).unwrap();
	}
	dir
}

fn render(errs: &[StructuredError]) -> String {
	errs.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>().join("\n")
}

/// Validate `xml` against the vendored schema named `schema`, e.g. `"invoiceData.xsd"`.
/// The error is the list of libxml2 validation messages, one per line.
///
/// One at a time: libxml2's parser and its structured error handler are process-global, and
/// two test threads compiling a schema at once hung the binary outright about one run in
/// three. The whole suite takes 0.2s serialized, so there is nothing to win by racing.
pub fn validate(xml: &str, schema: &str) -> Result<(), String> {
	static LIBXML: std::sync::Mutex<()> = std::sync::Mutex::new(());
	let _guard = LIBXML.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

	let path = resolvable_schema_dir().join(schema);
	let mut parser = SchemaParserContext::from_file(path.to_str().unwrap());
	let mut validator = SchemaValidationContext::from_parser(&mut parser)
		.map_err(|errs| format!("{schema} did not compile:\n{}", render(&errs)))?;
	let doc = Parser::default()
		.parse_string(xml)
		.map_err(|e| format!("not well-formed XML: {e}"))?;
	validator.validate_document(&doc).map_err(|errs| render(&errs))
}

fn seller() -> Seller {
	Seller {
		id: 1,
		uid: SellerId::generate(),
		org_id: 1,
		nav_base_url: "https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3".into(),
		nav_login: Some("tesztuser".into()),
		series_code: "A".into(),
		closed_at: None,
		payment_days: None,
		created_at: Timestamp(0),
	}
}

/// The statutory half — what `supplierInfo` is built from, and what `NavAuth::load` takes the
/// `user/taxNumber` out of. Built as the row rather than as a patch: nothing here goes through
/// a store.
fn seller_version() -> SellerVersion {
	SellerVersion {
		seller_ver: 1,
		seller_id: 1,
		status: SellerVersionStatus::Current,
		name: "Teszt Kft.".into(),
		country: "HU".into(),
		tax_number: "12345676-2-02".into(),
		group_member_tax_no: None,
		eu_vat_id: Some("HU12345676".into()),
		postcode: "1011".into(),
		city: "Budapest".into(),
		street: "Fő utca 1.".into(),
		bank_account: Some("12345678-12345678-12345678".into()),
		bank_name: None,
		small_business: false,
		vat_scheme: "NORMAL".into(),
		income_regime: "NONE".into(),
		expense_ratio_pct: None,
		regime_since: None,
		created_at: Timestamp(0),
		valid_from: Some(Timestamp(0)),
		superseded_at: None,
	}
}

/// One issued invoice with a single discounted line in rate group `code`. `storno` negates every
/// monetary figure, exactly as `invoices.kind = 'STORNO'` does. The group's `*_huf` columns are
/// left NULL so the writer exercises the §8 conversion.
fn parts(
	code: VatCode,
	currency: &str,
	storno: bool,
) -> (SellerVersion, Invoice, Vec<InvoiceLine>, Vec<InvoiceVatGroup>) {
	let sign = if storno { -1 } else { 1 };
	let unit_price = Money(sign * 100_000); // 1000.00
	let discount = Money(sign * 10_000); // 10% of it
	let net = Money(sign * 90_000);
	let vat = Money(net.0 * code.rate_bp() / 10_000);
	let gross = Money(net.0 + vat.0);

	let invoice = Invoice {
		id: 1,
		uid: InvoiceId::generate(),
		request_id: None,
		org_id: 1,
		seller_id: 1,
		seller_ver: Some(1),
		billing_party_id: Some(1),
		kind: if storno { InvoiceKind::Storno } else { InvoiceKind::Normal },
		status: InvoiceStatus::Issued,
		series_code: Some("A".into()),
		series_year: Some(2026),
		number: Some("A2026/000123".into()),
		issued_at: Some(Timestamp(1_768_435_200)), // 2026-01-15T00:00:00Z
		fulfilment_date: Some("2026-01-15".into()),
		due_date: Some("2026-01-29".into()),
		payment_method: PaymentMethod::Transfer,
		original_invoice_id: storno.then_some(2),
		modification_index: None,
		currency: CurrencyCode::parse(currency).unwrap(),
		rate_e6: 1_000_000,
		rate_date: Some("2026-01-15".into()),
		rate_source: None,
		huf_rate_e6: (!currency.eq_ignore_ascii_case("HUF")).then_some(400_000_000),
		net,
		vat,
		gross,
		paid_amount: Money::ZERO,
		paid_at: None,
		vat_note: Some(
			"Az Áfa tv. 37. §-a alapján a teljesítés helye a megrendelő országa.".into(),
		),
		notes: None,
		discount_kind: None,
		discount_value: None,
		buyer_kind: Some(PartyKind::Company),
		buyer_name: Some("Vevő Zrt.".into()),
		buyer_country: Some("HU".into()),
		buyer_tax_number: Some("10770001-2-41".into()),
		buyer_eu_vat_id: None,
		buyer_group_tax_no: None,
		buyer_postcode: Some("1052".into()),
		buyer_city: Some("Budapest".into()),
		buyer_street: Some("Váci utca 2.".into()),
		buyer_vies_request_id: None,
		buyer_vies_checked_at: None,
		created_at: Timestamp(0),
		updated_at: Timestamp(0),
		version: 1,
		period_start: None,
		period_end: None,
	};

	let line = InvoiceLine {
		id: 1,
		invoice_id: 1,
		line_no: 1,
		service_id: None,
		description: "Havi előfizetés".into(),
		unit: "hó".into(),
		qty: Qty(1_000_000),
		unit_price,
		discount_kind: Some(DiscountKind::Percent),
		discount_value: Some(1000), // 0.1000 as NAV's RateType
		discount_amount: discount,
		discount_description: Some("Éves fizetés kedvezménye".into()),
		net,
		vat_code: code,
		vat_rate_bp: code.rate_bp(),
		vat,
		gross,
		note: None,
	};

	let group = InvoiceVatGroup {
		invoice_id: 1,
		vat_code: code,
		vat_rate_bp: code.rate_bp(),
		net,
		vat,
		gross,
		net_huf: None,
		vat_huf: None,
		gross_huf: None,
	};

	(seller_version(), invoice, vec![line], vec![group])
}

fn build(code: VatCode, currency: &str, storno: bool) -> ClResult<String> {
	let (seller, invoice, lines, groups) = parts(code, currency, storno);
	invoice_data(&seller, &invoice, &lines, &groups, storno.then_some("A2026/000122"))
}

fn check(label: &str, xml: &str) {
	if let Err(errs) = validate(xml, "invoiceData.xsd") {
		panic!("{label} failed to validate:\n{errs}\n---\n{xml}");
	}
}

/// One sample per `vat_code`: the three `vatPercentage` rates, both `vatExemption` cases and all
/// three `vatOutOfScope` cases — then the reverse-charge and currency shapes that ride on the same
/// fixture.
#[test]
fn every_vat_code_validates() {
	for code in VatCode::ALL {
		check(code.as_str(), &build(code, "HUF", false).expect("writer failed"));
	}

	// (label, code, currency, what the document must carry, what it must not)
	for (label, code, currency, must, must_not) in [
		// `EUFAD37` is the cross-border reverse-charge case, reported as `vatOutOfScope` and
		// never as `vatDomesticReverseCharge`.
		(
			"EUFAD37",
			VatCode::Eufad37,
			"HUF",
			&["<vatOutOfScope>", "<case>EUFAD37</case>"][..],
			&["vatDomesticReverseCharge"][..],
		),
		// A HUF invoice still emits `exchangeRate` and every `…HUF` element — both are
		// mandatory in the vendored schema — at the identity rate.
		(
			"HUF identity",
			VatCode::Std27,
			"HUF",
			&[
				"<exchangeRate>1.000000</exchangeRate>",
				"<lineNetAmountHUF>900.00</lineNetAmountHUF>",
			],
			// `unitPriceHUF` was an independent per-line conversion, so it disagreed with the
			// apportioned `lineNetAmountHUF` and broke NAV's `lineNetAmount == quantity ×
			// unitPrice` cross-check. It is `minOccurs="0"`, so the fix is to omit it.
			&["unitPriceHUF"],
		),
		// 900.00 EUR × 400 = 360 000 HUF, converted per group, never summed from lines.
		(
			"EUR",
			VatCode::Eufad37,
			"EUR",
			&[
				"<currencyCode>EUR</currencyCode>",
				"<exchangeRate>400.000000</exchangeRate>",
				"<lineNetAmountHUF>360000.00</lineNetAmountHUF>",
			],
			&["unitPriceHUF"],
		),
		// `common:CurrencyType` is `[A-Z]{3}`. The writer emits `invoice.currency` verbatim,
		// and what makes that safe is `CurrencyCode::parse` uppercasing at the door — not a
		// second `to_ascii_uppercase` here.
		("lowercase EUR", VatCode::Eufad37, "eur", &["<currencyCode>EUR</currencyCode>"], &[]),
	] {
		let xml = build(code, currency, false).expect("writer failed");
		check(label, &xml);
		for want in must {
			assert!(xml.contains(want), "{label}: missing {want}\n{xml}");
		}
		for unwanted in must_not {
			assert!(!xml.contains(unwanted), "{label}: carries {unwanted}\n{xml}");
		}
	}
}

#[test]
fn storno_validates_and_negates() {
	let xml = build(VatCode::Std27, "HUF", true).expect("writer failed");
	check("storno", &xml);
	assert!(xml.contains("<originalInvoiceNumber>A2026/000122</originalInvoiceNumber>"));
	assert!(xml.contains("<modificationIndex>1</modificationIndex>"));
	assert!(xml.contains("<lineNetAmount>-900.00</lineNetAmount>"));
	// The XSD makes this `minOccurs="0"`, so its absence validated — and NAV's *business*
	// rules refuse the supply without it (ERROR 21 `LINE_MODIFICATION_EXPECTED`), which would
	// have failed every storno eight times and then hourly, forever.
	assert!(xml.contains("<lineNumberReference>2</lineNumberReference>"), "{xml}");
	assert!(xml.contains("<lineOperation>CREATE</lineOperation>"), "{xml}");

	// A plain invoice must not carry it: that is ERROR 22, the same refusal from the other
	// side (`LINE_MODIFICATION_NOT_EXPECTED`).
	let plain = build(VatCode::Std27, "HUF", false).expect("writer failed");
	assert!(!plain.contains("lineModificationReference"), "{plain}");
}

/// The counter-invoice's lines are *new* lines continuing the original's numbering, not
/// pointers back at it (interfész specifikáció v3.0 §2.5.1). Pointing line 1 at line 1 would
/// have been `MODIFY` semantics under a `CREATE` operation, which NAV refuses.
#[test]
fn a_multi_line_storno_continues_the_originals_line_numbering() {
	let (seller, mut invoice, mut lines, mut groups) = parts(VatCode::Std27, "HUF", true);
	let mut second = lines[0].clone();
	second.id = 2;
	second.line_no = 2;
	lines.push(second);
	for field in [&mut invoice.net, &mut groups[0].net] {
		*field = lines[0].net + lines[1].net;
	}
	for field in [&mut invoice.vat, &mut groups[0].vat] {
		*field = lines[0].vat + lines[1].vat;
	}
	for field in [&mut invoice.gross, &mut groups[0].gross] {
		*field = lines[0].gross + lines[1].gross;
	}

	let xml = invoice_data(&seller, &invoice, &lines, &groups, Some("A2026/000122"))
		.expect("writer failed");
	check("two-line storno", &xml);
	// Two original lines, so the counter-invoice's own lines 1 and 2 are 3 and 4.
	assert!(xml.contains("<lineNumberReference>3</lineNumberReference>"), "{xml}");
	assert!(xml.contains("<lineNumberReference>4</lineNumberReference>"), "{xml}");
	assert!(!xml.contains("<lineNumberReference>1</lineNumberReference>"), "{xml}");
}

/// `countryCode` was uppercased on the way out and the `postalCode` beside it was not, though
/// `PostalCodeType` is `[A-Z0-9][A-Z0-9\s\-]{1,8}[A-Z0-9]` — uppercase-only too. The buyer
/// address is a frozen snapshot on an immutable invoice, so a stored `sw1a 1aa` is a numbered
/// invoice NAV refuses on every attempt, forever.
#[test]
fn a_lowercase_postcode_is_uppercased_like_the_country_beside_it() {
	let (seller, mut invoice, lines, groups) = parts(VatCode::Eufad37, "HUF", false);
	invoice.buyer_country = Some("gb".into());
	invoice.buyer_postcode = Some("sw1a 1aa".into());
	invoice.buyer_tax_number = None;
	let xml = invoice_data(&seller, &invoice, &lines, &groups, None).expect("writer failed");
	check("lowercase postcode", &xml);
	assert!(xml.contains("<base:postalCode>SW1A 1AA</base:postalCode>"), "{xml}");
}

/// `individualExemption` and `smallBusinessIndicator` were emitted by no fixture, so neither
/// their content nor — the half that matters — their **position in the XSD sequence** was ever
/// validated. `xml.rs` writes ~60 elements by hand and a reorder is invisible to every
/// `contains()` assertion while NAV rejects the document outright, which is the whole reason
/// this file and its libxml2 dependency exist.
#[test]
fn an_exempt_small_business_seller_emits_both_indicators_in_sequence() {
	let (mut seller, invoice, lines, groups) = parts(VatCode::Std27, "HUF", false);
	seller.vat_scheme = "ALANYI_MENTES".to_owned();
	seller.small_business = true;

	let xml = invoice_data(&seller, &invoice, &lines, &groups, None).expect("writer failed");
	check("exempt small business", &xml);
	assert!(xml.contains("<individualExemption>true</individualExemption>"), "{xml}");
	assert!(xml.contains("<smallBusinessIndicator>true</smallBusinessIndicator>"), "{xml}");

	// And neither is emitted for a seller on the normal scheme, or NAV reads every invoice as
	// exempt supply.
	let plain = build(VatCode::Std27, "HUF", false).expect("writer failed");
	assert!(!plain.contains("individualExemption"), "{plain}");
	assert!(!plain.contains("smallBusinessIndicator"), "{plain}");
}

#[test]
fn a_periodic_settlement_emits_the_delivery_period_in_sequence() {
	let (mut seller, mut invoice, lines, groups) = parts(VatCode::Std27, "HUF", false);
	invoice.period_start = Some("2026-01-01".to_owned());
	invoice.period_end = Some("2026-01-31".to_owned());
	// So the XSD also checks the period precedes `smallBusinessIndicator`.
	seller.small_business = true;

	let xml = invoice_data(&seller, &invoice, &lines, &groups, None).expect("writer failed");
	check("periodic settlement", &xml);
	assert!(
		xml.contains("<invoiceDeliveryPeriodStart>2026-01-01</invoiceDeliveryPeriodStart>"),
		"{xml}"
	);
	assert!(
		xml.contains("<invoiceDeliveryPeriodEnd>2026-01-31</invoiceDeliveryPeriodEnd>"),
		"{xml}"
	);
	assert!(xml.contains("<periodicalSettlement>true</periodicalSettlement>"), "{xml}");

	// Half a period writes nothing: a lone start fails the XSD.
	invoice.period_end = None;
	let half = invoice_data(&seller, &invoice, &lines, &groups, None).expect("writer failed");
	check("half period", &half);
	assert!(!half.contains("invoiceDeliveryPeriod"), "{half}");
	assert!(!half.contains("periodicalSettlement"), "{half}");
}

/// A natural person's identifying data is withheld from the report (§4.3); it still prints
/// on the PDF.
#[test]
fn private_person_omits_identifying_data() {
	let (seller, mut invoice, lines, groups) = parts(VatCode::Std27, "HUF", false);
	invoice.buyer_kind = Some(PartyKind::Person);
	let xml = invoice_data(&seller, &invoice, &lines, &groups, None).expect("writer failed");
	check("private person", &xml);
	assert!(xml.contains("<customerVatStatus>PRIVATE_PERSON</customerVatStatus>"));
	assert!(!xml.contains("customerName"));
	assert!(!xml.contains("customerAddress"));
	assert!(!xml.contains("customerVatData"));
}

/// `vatExemption/reason` is mandatory and comes from the `VatCode`, not from
/// `invoices.vat_note`: that is one invoice-level i18n key, while `reason` is per-code.
/// A mixed AAM + TAM + STD27 invoice therefore emits three *different* reasons, and it
/// emits them with no `vat_note` at all — a product-level exempt line leaves the column
/// NULL, which used to fail the writer and strand the invoice in `unfiled_invoices`.
#[test]
fn a_mixed_exempt_invoice_emits_one_statutory_reason_per_code() {
	let (seller, mut invoice, mut lines, mut groups) = parts(VatCode::Aam, "HUF", false);
	invoice.vat_note = None;

	for (n, code) in [(2, VatCode::Tam), (3, VatCode::Std27)] {
		let mut line = lines[0].clone();
		line.id = n;
		line.line_no = n;
		line.vat_code = code;
		line.vat_rate_bp = code.rate_bp();
		line.vat = Money(line.net.0 * code.rate_bp() / 10_000);
		line.gross = Money(line.net.0 + line.vat.0);
		lines.push(line);

		let mut group = groups[0].clone();
		group.vat_code = code;
		group.vat_rate_bp = code.rate_bp();
		group.vat = Money(group.net.0 * code.rate_bp() / 10_000);
		group.gross = Money(group.net.0 + group.vat.0);
		groups.push(group);
	}
	invoice.net = Money(groups.iter().map(|g| g.net.0).sum());
	invoice.vat = Money(groups.iter().map(|g| g.vat.0).sum());
	invoice.gross = Money(groups.iter().map(|g| g.gross.0).sum());

	let xml = invoice_data(&seller, &invoice, &lines, &groups, None)
		.expect("a product-level exempt line has no vat_note and must still be filable");
	check("mixed exempt", &xml);

	// One reason per zero-rated code, each the statutory text — not one repeated note.
	for code in [VatCode::Aam, VatCode::Tam] {
		let reason = code.nav_reason().unwrap();
		assert!(xml.contains(reason), "{} reason missing from:\n{xml}", code.as_str());
	}
	assert_ne!(VatCode::Aam.nav_reason(), VatCode::Tam.nav_reason());
	// The percentage group carries `vatPercentage` and no reason at all.
	assert_eq!(xml.matches("<reason>").count(), 4, "two lines + two summary groups");
}

// Everything above validates `invoiceData`, the invoice document; this validates the envelope
// that carries it against `invoiceApi.xsd`. `softwareId` is `[0-9A-Z\-]{18}` exactly, four more
// are `…NotBlankType`, and the two optional ones are legal absent but not blank — a deployment
// that leaves one `nav.software_*` at `""` has every `manageInvoice` rejected on a schema error.

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, so the pool must be over a file.
struct EnvelopeDb(std::path::PathBuf);

/// `inv_` plus a 26-character ULID — exactly `EntityIdType`'s 30-char maximum.
const INV_UID: &str = "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV";
/// AES-128-ECB, so exactly 16 bytes, and exactly one block so the reply needs no padding.
const EXCHANGE_KEY: &[u8; 16] = b"0123456789abcdef";
const TOKEN: &str = "TOKENTOKENTOKEN1";

/// The `tokenExchange` reply `manage_invoice_request` needs before it will build an envelope:
/// `encodedExchangeToken` is AES-128-ECB under the exchange key, base64'd.
fn token_reply() -> String {
	let mut block = Block::<Aes128>::try_from(TOKEN.as_bytes()).unwrap();
	Aes128::new(&(*EXCHANGE_KEY).into()).encrypt_block(&mut block);
	format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
		 <TokenExchangeResponse xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\" \
		 xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\">\
		 <common:result><common:funcCode>OK</common:funcCode></common:result>\
		 <encodedExchangeToken>{}</encodedExchangeToken>\
		 <tokenValidityFrom>2026-09-05T10:00:00.000Z</tokenValidityFrom>\
		 <tokenValidityTo>2026-09-05T10:05:00.000Z</tokenValidityTo>\
		 </TokenExchangeResponse>",
		B64.encode(block),
	)
}

impl EnvelopeDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-nav-envelope-test-{}-{name}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).unwrap();
		Self(dir)
	}

	fn path(&self) -> String {
		self.0.join("test.db").display().to_string()
	}
}

impl Drop for EnvelopeDb {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.0);
	}
}

/// The six `nav.software_*` keys `invoiceApi.xsd` requires, all populated.
const SOFTWARE_SETTINGS: [(&str, &str); 6] = [
	("nav.software_id", "HU12345678MINTWRKS"),
	("nav.software_name", "Mintworks"),
	("nav.software_operation", "LOCAL_SOFTWARE"),
	("nav.software_main_version", "0.1"),
	("nav.software_dev_name", "Teszt Kft."),
	("nav.software_dev_contact", "dev@e.st"),
];

async fn envelope_app(db: &EnvelopeDb) -> mintworks_core::App {
	let config = mintworks_core::config::Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	let store = mintworks_store_sqlite::SqliteStore::open(&config).await.unwrap();
	// The whole framework module: this test needs only `mintworks-core`'s tables, but the schema is
	// one versioned unit and the rest costs a few CREATEs.
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	// Registered as defaults rather than rows: `AppBuilder::build` refuses to boot on a blank
	// `.required()` key, and every test here then overwrites them with rows of its own.
	let mut builder = mintworks_core::AppBuilder::new()
		.config(config)
		.store(std::sync::Arc::new(store) as std::sync::Arc<dyn mintworks_core::store::CoreStore>)
		.settings(mintworks_nav::SETTINGS);
	for (key, value) in SOFTWARE_SETTINGS {
		builder = builder.setting_default(key, value);
	}
	let app = builder.build().await.unwrap();

	app.settings
		.set("nav.base_url", "https://api-test.example/v3", None)
		.await
		.unwrap();
	app.secrets.set("nav.tech_password", b"tech-pw", None).await.unwrap();
	app.secrets.set("nav.sign_key", b"sign-key", None).await.unwrap();
	app.secrets.set("nav.exchange_key", b"0123456789abcdef", None).await.unwrap();
	for (key, value) in SOFTWARE_SETTINGS {
		app.settings.set(key, value, None).await.unwrap();
	}
	app
}

/// `QueryTaxpayerRequest` is the smallest root element that carries the shared envelope, so it
/// validates the `software` block without also dragging in an invoice.
#[tokio::test]
async fn a_built_envelope_validates_against_the_api_schema() {
	let db = EnvelopeDb::new("ok");
	let app = envelope_app(&db).await;
	let client = mintworks_nav::auth::NavAuth::load(&app, &seller(), &seller_version())
		.await
		.unwrap();

	let xml = client.query_taxpayer_request("12345676");
	if let Err(errs) = validate(&xml, "invoiceApi.xsd") {
		panic!("the request envelope failed to validate:\n{errs}\n---\n{xml}");
	}
}

/// `InvoiceOperationType` is an `xs:sequence` — `electronicInvoiceHash` after `invoiceData`
/// and nowhere else — and nothing but the schema catches a misplaced element.
#[tokio::test]
async fn a_manage_invoice_envelope_validates_with_the_pdf_hash() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/tokenExchange"))
		.respond_with(ResponseTemplate::new(200).set_body_string(token_reply()))
		.mount(&server)
		.await;

	let db = EnvelopeDb::new("manage");
	let app = envelope_app(&db).await;
	// The seller row wins over `settings['nav.base_url']`, and `seller()` names NAV's real
	// test system — so it is the field that has to point at the stand-in.
	let seller = Seller { nav_base_url: server.uri(), ..seller() };
	let client = mintworks_nav::auth::NavAuth::load(&app, &seller, &seller_version())
		.await
		.unwrap();

	let hash = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
	// N=2, because the schema is the only thing that checks the `xs:sequence` and the gapless
	// 1-based `index` a batch has to carry (§1.8.1); the second invoice files no hash, which is
	// where a misplaced optional `electronicInvoiceHash` would show.
	let token = client.token_exchange().await.unwrap();
	let xml = client
		.manage_invoice_request(
			mintworks_nav::NavOp::Create,
			INV_UID,
			&[
				("<InvoiceData/>".to_owned(), Some(hash.to_owned())),
				("<InvoiceData/>".to_owned(), None),
			],
			&token,
		)
		.unwrap();
	assert!(xml.contains("<index>1</index>"), "{xml}");
	assert!(xml.contains("<index>2</index>"), "{xml}");
	assert!(
		xml.contains(&format!(
			"<electronicInvoiceHash cryptoType=\"SHA-256\">{}</electronicInvoiceHash>",
			hash.to_ascii_uppercase()
		)),
		"{xml}"
	);
	if let Err(errs) = validate(&xml, "invoiceApi.xsd") {
		panic!("the manageInvoice envelope failed to validate:\n{errs}\n---\n{xml}");
	}
}

/// A blank required setting must fail where an operator sees it, not silently produce a
/// document NAV rejects. The two `minOccurs="0"` fields go the other way: omitted, not blank.
#[tokio::test]
async fn a_blank_software_setting_is_an_error_not_an_invalid_document() {
	let db = EnvelopeDb::new("blank");
	let app = envelope_app(&db).await;
	app.settings.set("nav.software_dev_name", "", None).await.unwrap();

	assert!(
		mintworks_nav::auth::NavAuth::load(&app, &seller(), &seller_version())
			.await
			.is_err(),
		"a blank softwareDevName must stop the request being built at all"
	);
	// And the boot gate says the same thing, so this never reaches filing time.
	assert!(app.settings.check_required("nav.").await.is_err());

	// The optional pair is legal absent, so blanking one changes nothing.
	app.settings.set("nav.software_dev_name", "Teszt Kft.", None).await.unwrap();
	app.settings.set("nav.software_dev_tax_number", "", None).await.unwrap();
	let client = mintworks_nav::auth::NavAuth::load(&app, &seller(), &seller_version())
		.await
		.unwrap();
	let xml = client.query_taxpayer_request("12345676");
	assert!(!xml.contains("softwareDevTaxNumber"), "a blank optional field was emitted");
	if let Err(errs) = validate(&xml, "invoiceApi.xsd") {
		panic!("omitting the optional field made the envelope invalid:\n{errs}\n---\n{xml}");
	}
}

/// The HUF projection must not reintroduce per-line rounding.
///
/// At a deliberately non-round rate, two 0.01 lines in one VAT group each convert to
/// 3.51 HUF on their own but to 7.01 together — so a per-line conversion sums to 7.02
/// against a `vatRateNetAmountHUF` of 7.01, which is exactly what NAV's cross-field
/// validation rejects. The existing fixtures use 400.000000, which cannot see this.
#[test]
fn line_huf_nets_sum_to_the_group_and_invoice_figures() {
	const RATE_E6: i64 = 350_555_555;

	let (seller, mut invoice, lines, mut groups) = parts(VatCode::Std27, "EUR", false);
	invoice.huf_rate_e6 = Some(RATE_E6);
	// Two lines of 0.01 net in one group; the group carries one rounding of the summed net.
	let cent = Money(1);
	let group_net = Money(2);
	let mk = |line_no: i64| InvoiceLine {
		line_no,
		unit_price: cent,
		discount_kind: None,
		discount_value: None,
		discount_amount: Money::ZERO,
		discount_description: None,
		net: cent,
		vat: Money::ZERO,
		gross: cent,
		..lines[0].clone()
	};
	let lines = vec![mk(1), mk(2)];
	groups[0].net = group_net;
	groups[0].vat = Money::ZERO;
	groups[0].gross = group_net;
	invoice.net = group_net;
	invoice.vat = Money::ZERO;
	invoice.gross = group_net;

	let xml = invoice_data(&seller, &invoice, &lines, &groups, None).expect("writer failed");
	check("non-round rate", &xml);

	let sum: i64 = amounts(&xml, "lineNetAmountHUF").iter().sum();
	let group_huf = amounts(&xml, "vatRateNetAmountHUF");
	let invoice_huf = amounts(&xml, "invoiceNetAmountHUF");
	assert_eq!(group_huf, vec![701], "one rounding of the summed net, not two of the parts");
	assert_eq!(sum, group_huf.iter().sum::<i64>(), "lines must sum to their group");
	assert_eq!(sum, invoice_huf[0], "lines must sum to the invoice total");
	// This is the rate that exposed `unitPriceHUF`: it would have printed 3.51 on both lines
	// against `lineNetAmountHUF` of 3.51 and 3.50.
	assert!(!xml.contains("unitPriceHUF"), "an independent per-line conversion is back");

	// The same figures through the **stored** branch of `to_huf`: `invoice_vat_groups.*_huf`
	// is filed verbatim, and `Money` carried no fixed scale, so the stored column could
	// disagree about decimals with the invoice currency's figures beside it and the HUF total
	// filed with the tax authority was off by a power of ten.
	groups[0].net_huf = Some(Money(701));
	let xml = invoice_data(&seller, &invoice, &lines, &groups, None).expect("writer failed");
	check("non-round rate, stored group HUF", &xml);
	assert_eq!(amounts(&xml, "vatRateNetAmountHUF"), vec![701]);
	assert_eq!(
		amounts(&xml, "lineNetAmountHUF").iter().sum::<i64>(),
		701,
		"a stored group HUF and the per-line conversion must be the same scale"
	);
}

/// Every occurrence of `<name>` in `xml`, as minor units.
fn amounts(xml: &str, name: &str) -> Vec<i64> {
	let (open, close) = (format!("<{name}>"), format!("</{name}>"));
	xml.split(&open)
		.skip(1)
		.filter_map(|rest| rest.split(&close).next())
		.map(|v| {
			let (whole, frac) = v.split_once('.').unwrap();
			let sign = if whole.starts_with('-') { -1 } else { 1 };
			whole.parse::<i64>().unwrap() * 100 + sign * frac.parse::<i64>().unwrap()
		})
		.collect()
}

// vim: ts=4
