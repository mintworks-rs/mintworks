//! Offline validation of generated XML against the vendored NAV XSDs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{fs, path::PathBuf};

use libxml::{
	error::StructuredError,
	parser::Parser,
	schemas::{SchemaParserContext, SchemaValidationContext},
};
use saas_core::prelude::{ClResult, CurrencyCode, InvoiceId, Money, Qty, Timestamp};
use saas_invoice::{
	store::{
		DiscountKind, Invoice, InvoiceKind, InvoiceLine, InvoiceStatus, InvoiceVatGroup, PartyKind,
		PaymentMethod, Seller,
	},
	vat::VatCode,
};
use saas_nav::xml::invoice_data;

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
	let dir = std::env::temp_dir().join(format!("saas-nav-xsd-{}", std::process::id()));
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
		nav_base_url: "https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3".into(),
		nav_login: Some("tesztuser".into()),
		small_business: false,
		vat_scheme: "NORMAL".into(),
		series_code: "A".into(),
		created_at: Timestamp(0),
	}
}

/// One issued invoice with a single discounted line in rate group `code`. `storno` negates
/// every monetary figure, exactly as `invoices.kind = 'STORNO'` does (`nav-mapping.md` §6).
/// The group's `*_huf` columns are left NULL so the writer exercises the §8 conversion.
fn parts(
	code: VatCode,
	currency: &str,
	storno: bool,
) -> (Seller, Invoice, Vec<InvoiceLine>, Vec<InvoiceVatGroup>) {
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
		tenant_id: 1,
		seller_id: 1,
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

	(seller(), invoice, vec![line], vec![group])
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

/// One sample per `vat_code`: the three `vatPercentage` rates, both `vatExemption` cases
/// and all three `vatOutOfScope` cases (`nav-mapping.md` §5.4).
#[test]
fn every_vat_code_validates() {
	for code in VatCode::ALL {
		let xml = build(code, "HUF", false).expect("writer failed");
		check(code.as_str(), &xml);
	}
}

/// `EUFAD37` is the cross-border reverse-charge case, reported as `vatOutOfScope` and never
/// as `vatDomesticReverseCharge`.
#[test]
fn reverse_charge_is_out_of_scope_not_domestic() {
	let xml = build(VatCode::Eufad37, "HUF", false).expect("writer failed");
	check("EUFAD37", &xml);
	assert!(xml.contains("<vatOutOfScope>"), "EUFAD37 must use vatOutOfScope");
	assert!(xml.contains("<case>EUFAD37</case>"));
	assert!(!xml.contains("vatDomesticReverseCharge"));
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

/// A HUF invoice still emits `exchangeRate` and every `…HUF` element — both are mandatory
/// in the vendored schema — at the identity rate.
#[test]
fn huf_invoice_emits_identity_rate_and_huf_amounts() {
	let xml = build(VatCode::Std27, "HUF", false).expect("writer failed");
	check("HUF identity", &xml);
	assert!(xml.contains("<exchangeRate>1.000000</exchangeRate>"));
	assert!(xml.contains("<lineNetAmountHUF>900.00</lineNetAmountHUF>"));
	assert!(!xml.contains("unitPriceHUF"), "unitPriceHUF is never emitted");
}

#[test]
fn foreign_currency_validates() {
	let xml = build(VatCode::Eufad37, "EUR", false).expect("writer failed");
	check("EUR", &xml);
	assert!(xml.contains("<currencyCode>EUR</currencyCode>"));
	assert!(xml.contains("<exchangeRate>400.000000</exchangeRate>"));
	// `common:CurrencyType` is `[A-Z]{3}`. The writer emits `invoice.currency` verbatim, and
	// what makes that safe is `CurrencyCode::parse` uppercasing at the door — not a second
	// `to_ascii_uppercase` here. A lowercase submission must reach NAV upper-cased.
	let lower = build(VatCode::Eufad37, "eur", false).expect("writer failed");
	check("lowercase EUR", &lower);
	assert!(lower.contains("<currencyCode>EUR</currencyCode>"));
	// 900.00 EUR × 400 = 360 000 HUF, converted per group, never summed from lines.
	assert!(xml.contains("<lineNetAmountHUF>360000.00</lineNetAmountHUF>"));
	// `unitPriceHUF` was an independent per-line conversion, so it disagreed with the
	// apportioned `lineNetAmountHUF` and broke NAV's `lineNetAmount == quantity × unitPrice`
	// cross-check. It is `minOccurs="0"`, so the fix is to omit it.
	assert!(!xml.contains("unitPriceHUF"), "unitPriceHUF cannot agree with the apportionment");
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

// Everything above validates `invoiceData`, the invoice document. The envelope that carries
// it was never validated against `invoiceApi.xsd`, which is how eight `nav.software_*`
// settings defaulting to `""` went unnoticed: `softwareId` is `[0-9A-Z\-]{18}` exactly, four
// more are `…NotBlankType`, and the two optional ones are legal absent but not blank. A
// deployment that forgot one had every `manageInvoice` rejected on a schema error.

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, so the pool must be over a file.
struct EnvelopeDb(std::path::PathBuf);

impl EnvelopeDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("saas-nav-envelope-test-{}-{name}", std::process::id()));
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
	("nav.software_id", "HU12345678SAASFRWK"),
	("nav.software_name", "saas-framework"),
	("nav.software_operation", "LOCAL_SOFTWARE"),
	("nav.software_main_version", "0.1"),
	("nav.software_dev_name", "Teszt Kft."),
	("nav.software_dev_contact", "dev@e.st"),
];

async fn envelope_app(db: &EnvelopeDb) -> saas_core::App {
	let config = saas_core::config::Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: String::new(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	let store = store_adapter_sqlite::SqliteStore::open(&config).await.unwrap();
	// Every `saas-core/` step, not `STEPS[..1]`: a correction appends, so the core schema is
	// no longer one leading entry — and `AppBuilder::build`'s reclaim reads `jobs.claimed_at`.
	let core: Vec<_> = store_adapter_sqlite::STEPS
		.iter()
		.filter(|s| s.name.starts_with("saas-core/"))
		.copied()
		.collect();
	store.migrate(&core).await.unwrap();
	let app = saas_core::AppBuilder::new()
		.config(config)
		.store(std::sync::Arc::new(store) as std::sync::Arc<dyn saas_core::store::CoreStore>)
		.build()
		.await
		.unwrap();

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
	let client = saas_nav::auth::NavAuth::load(&app, &seller()).await.unwrap();

	let xml = client.query_taxpayer_request("12345676");
	if let Err(errs) = validate(&xml, "invoiceApi.xsd") {
		panic!("the request envelope failed to validate:\n{errs}\n---\n{xml}");
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
		saas_nav::auth::NavAuth::load(&app, &seller()).await.is_err(),
		"a blank softwareDevName must stop the request being built at all"
	);
	// And the startup gate says the same thing, so this never reaches filing time.
	assert!(saas_nav::auth::check_software_settings(&app).await.is_err());

	// The optional pair is legal absent, so blanking one changes nothing.
	app.settings.set("nav.software_dev_name", "Teszt Kft.", None).await.unwrap();
	app.settings.set("nav.software_dev_tax_number", "", None).await.unwrap();
	let client = saas_nav::auth::NavAuth::load(&app, &seller()).await.unwrap();
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
