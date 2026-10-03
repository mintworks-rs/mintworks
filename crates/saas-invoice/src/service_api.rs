//! `Invoices` — the service handle a consumer application bills its users through.
//!
//! Every method takes `&Ctx` first and derives its permission from `ctx.actor`, never from how it
//! was reached, so a consumer route that forgets a middleware cannot leak data. A `User` is
//! confined to `ctx.org_id` and another org's row reads as [`Error::NotFound`], never `403` —
//! the API does not confirm that it exists.

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use saas_core::alert::{Alert, Severity};
use saas_core::app::App;
use saas_core::audit;
use saas_core::ctx::Ctx;
use saas_core::event::{self, Event};
use saas_core::prelude::*;
use saas_core::store::Role;

use crate::catalog;
use crate::currency::{self, Currency};
use crate::draft::{self, Line, NewDraft, Party};
use crate::issue;
use crate::money::Discount;
use crate::numbering::{
	self, check_dates, check_patch_dates, check_period, check_range, resolved_today,
};
use crate::pricing;
use crate::store::{
	BillingParty, Invoice, InvoiceDocument, InvoiceFilter, InvoiceLine, InvoicePatch,
	InvoiceStatus, InvoiceStore, InvoiceSummary, InvoiceVatGroup, NewInvoice, PartyPatch,
	PaymentMethod, RevenueMonth, Seller, SellerVersion, SellerVersionPatch, Service, ServiceDef,
	ServicePatch,
};
use crate::storno;
use crate::taxrule::determine;
use crate::vat::VatCode;

/// Reaches the application's store.
///
/// The application registers it with
/// `AppBuilder::extension(Arc::new(store) as Arc<dyn InvoiceStore>)`; this crate goes
/// through here rather than being generic over the store.
pub fn store(app: &App) -> ClResult<Arc<dyn InvoiceStore>> {
	app.extensions.get::<Arc<dyn InvoiceStore>>().cloned().ok_or_else(no_store)
}

fn no_store() -> Error {
	Error::internal("saas-invoice: no InvoiceStore was registered on the app")
}

/// The party write paths' one validation gate: an ISO-3166 alpha-2 country and a
/// NAV-filable `customerName`.
///
/// The country is *rewritten* to the normalised form rather than only checked, so the stored
/// column is always uppercase alpha-2 — `taxrule::BuyerZone::of` and `saas-nav`'s
/// `base:countryCode` both depend on that, and the store is the last place either can be
/// fixed. The store trait takes a `&PartyPatch`, so this returns an owned corrected copy.
fn checked_party(patch: &PartyPatch) -> ClResult<PartyPatch> {
	check_terms(patch.payment_days.value().copied(), patch.payment_method.value().copied())?;
	let mut patch = patch.clone();
	if let Some(country) = &patch.country {
		patch.country = Some(crate::party::normalise_country(country)?);
	}
	if let Some(name) = &patch.name {
		crate::store::bounded_text("name", name, crate::store::MAX_PARTY_NAME)?;
	}
	// The address reaches NAV through the frozen buyer snapshot. `MAX_ADDRESS_TEXT`, not
	// `MAX_PARTY_NAME`: these are NAV's `SimpleText255NotBlankType`, so an over-long street
	// failed the schema on a numbered, immutable invoice. The postcode is a pattern.
	for (field, value) in [("city", &patch.city), ("street", &patch.street)] {
		if let Patch::Value(v) = value {
			crate::store::bounded_text(field, v, crate::store::MAX_ADDRESS_TEXT)?;
		}
	}
	if let Patch::Value(v) = &patch.postcode {
		patch.postcode = Patch::Value(crate::store::checked_postcode(v)?);
	}
	// Normalised, not merely bounded: `communityVatNumber` is `[A-Z]{2}[0-9A-Z]{2,13}`, so a
	// spaced `"DE 811569869"` cleared a length check and was then rejected on every filing
	// attempt — against a frozen buyer snapshot on an invoice that already has a number.
	if let Patch::Value(v) = &patch.eu_vat_id {
		let (full, _, _) = crate::vies::normalise(v)?;
		patch.eu_vat_id = Patch::Value(full);
	}
	if let Patch::Value(v) = &patch.tax_number {
		crate::store::bounded_text("taxNumber", v, crate::store::MAX_THIRD_STATE_TAX_ID)?;
	}
	// A length bound is the wrong rule: `base:TaxNumberType` takes the first 8 *digits* as
	// `base:taxpayerId`, so filing-time's check runs here too, where the party is still
	// correctable. Filed as `groupMemberTaxNumber`, so it carries `vat_code_ok` as well.
	if let Patch::Value(v) = &patch.group_tax_no {
		crate::store::bounded_text("groupTaxNo", v, crate::store::MAX_THIRD_STATE_TAX_ID)?;
		let digits = issue::tax_digits(v);
		if digits.len() < crate::store::MIN_TAX_NUMBER_DIGITS {
			return Err(bad_text(format!(
				"groupTaxNo has fewer than {} digits",
				crate::store::MIN_TAX_NUMBER_DIGITS
			)));
		}
		if !issue::vat_code_ok(&digits) {
			return Err(bad_text("the 9th digit of groupTaxNo must be 1-5".into()));
		}
	}
	Ok(patch)
}

fn bad_text(msg: String) -> Error {
	Error::coded(StatusCode::BAD_REQUEST, "E-INV-BAD-TEXT", msg)
}

fn bad_seller(code: &'static str, msg: String) -> Error {
	Error::coded(StatusCode::BAD_REQUEST, code, msg)
}

/// The seller-version write paths' validation gate — the buyer side's own rules, run on the
/// seller's fields so the two cannot drift, plus the normalisation `checked_party` applies.
///
/// Run on **save**, not only on publish: `saas_nav::auth::check_seller` used to be the first
/// thing that ever looked at these, and by then a bad value had already booted, issued
/// numbered invoices and faulted every filing against a refusal that can never change. What is
/// still absent is allowed here — a half-filled draft is savable; [`complete_seller_version`]
/// is what refuses to make one live.
fn checked_seller_version(patch: &SellerVersionPatch) -> ClResult<SellerVersionPatch> {
	let mut patch = patch.clone();
	if let Some(country) = &patch.country {
		patch.country = Some(crate::party::normalise_country(country)?);
	}
	if let Some(name) = &patch.name {
		crate::store::bounded_text("name", name, crate::store::MAX_PARTY_NAME)?;
	}
	// `supplierAddress`'s `city` and the street are `SimpleText255NotBlankType`, the same
	// bound the buyer's address carries, and the postcode is a pattern.
	for (field, value) in [("city", &patch.city), ("street", &patch.street)] {
		if let Some(v) = value {
			crate::store::bounded_text(field, v, crate::store::MAX_ADDRESS_TEXT)
				.map_err(|e| bad_seller("E-INV-SELLER-ADDRESS", e.to_string()))?;
		}
	}
	if let Some(v) = &patch.postcode {
		patch.postcode = Some(
			crate::store::checked_postcode(v)
				.map_err(|e| bad_seller("E-INV-SELLER-ADDRESS", e.to_string()))?,
		);
	}
	// Normalised, not merely bounded: `communityVatNumber` is `[A-Z]{2}[0-9A-Z]{2,13}`, so a
	// spaced `"DE 811569869"` clears a length check and is then rejected on every filing.
	if let Patch::Value(v) = &patch.eu_vat_id {
		let (full, _, _) = crate::vies::normalise(v)?;
		patch.eu_vat_id = Patch::Value(full);
	}
	// `base:TaxNumberType` takes the first 8 *digits* as `base:taxpayerId` and digit 9 as
	// `base:vatCode`, which `common.xsd` restricts to `[1-5]` — a length bound is the wrong
	// rule. Both figures reach `saas_nav::xml::Xml::tax_number`, so both carry it.
	for (field, value) in [
		("taxNumber", patch.tax_number.as_ref()),
		("groupMemberTaxNo", patch.group_member_tax_no.value()),
	] {
		let Some(value) = value else { continue };
		crate::store::bounded_text(field, value, crate::store::MAX_THIRD_STATE_TAX_ID)
			.map_err(|e| bad_seller("E-INV-SELLER-TAXNUMBER", e.to_string()))?;
		let digits = issue::tax_digits(value);
		if digits.len() < crate::store::MIN_TAX_NUMBER_DIGITS {
			return Err(bad_seller(
				"E-INV-SELLER-TAXNUMBER",
				format!("{field} has fewer than {} digits", crate::store::MIN_TAX_NUMBER_DIGITS),
			));
		}
		if !issue::vat_code_ok(&digits) {
			return Err(bad_seller(
				"E-INV-SELLER-TAXNUMBER",
				format!("the 9th digit of {field} must be 1-5"),
			));
		}
	}
	if let Some(scheme) = &patch.vat_scheme
		&& !matches!(scheme.as_str(), "NORMAL" | "ALANYI_MENTES")
	{
		return Err(bad_text(format!("unknown vatScheme '{scheme}'")));
	}
	if let Some(regime) = &patch.income_regime
		&& !matches!(regime.as_str(), "NONE" | "KATA" | "ATALANY")
	{
		return Err(bad_text(format!("unknown incomeRegime '{regime}'")));
	}
	if let Patch::Value(pct) = patch.expense_ratio_pct
		&& !matches!(pct, 40 | 45 | 50 | 80 | 90)
	{
		return Err(bad_text(format!("expenseRatioPct must be 40, 45, 50, 80 or 90, not {pct}")));
	}
	numbering::check_dates([patch.regime_since.value().map(String::as_str)])?;
	Ok(patch)
}

/// A ratio is judged against the regime it lands on, stored or patched: `merged` would
/// otherwise silently drop one sent without `ATALANY`.
fn check_expense_ratio(patch: &SellerVersionPatch, base: Option<&SellerVersion>) -> ClResult<()> {
	if patch.expense_ratio_pct.value().is_some() && patch.merged(base).income_regime != "ATALANY" {
		return Err(bad_text("expenseRatioPct needs incomeRegime 'ATALANY'".into()));
	}
	Ok(())
}

/// The fields `saas_nav::xml::supplier_info` emits unconditionally. Checked at publish, which
/// is the last moment before an invoice can freeze the row: a version that reaches an invoice
/// with a blank `supplierName` fails the XSD on a document that is already immutable.
fn complete_seller_version(v: &SellerVersion) -> ClResult<()> {
	for (field, value) in [
		("name", &v.name),
		("taxNumber", &v.tax_number),
		("country", &v.country),
		("postcode", &v.postcode),
		("city", &v.city),
		("street", &v.street),
	] {
		if value.trim().is_empty() {
			return Err(bad_seller(
				"E-INV-SELLER-INCOMPLETE",
				format!("the seller has no {field}"),
			));
		}
	}
	Ok(())
}

/// Catalogue text becomes a line's `description` and `unit` on every invoice drawn from it,
/// so bound it where the row is written rather than only in `draft::price`.
///
/// `unit_price` is bounded here and not only in `catalog::money_in`: the handle is the trust
/// boundary, and a `Money` past [`saas_core::money::MAX_MINOR`] stored here makes `read_money`
/// reject the row, which fails every catalogue read rather than the one bad service.
fn check_service_fields(
	name: Option<&str>,
	description: Option<&str>,
	unit: Option<&str>,
	unit_price: Option<Money>,
) -> ClResult<()> {
	use crate::store::{MAX_DESCRIPTION, MAX_UNIT, bounded_text};
	if let Some(v) = name {
		bounded_text("name", v, MAX_DESCRIPTION)?;
	}
	if let Some(v) = description {
		bounded_text("description", v, MAX_DESCRIPTION)?;
	}
	if let Some(v) = unit {
		bounded_text("unit", v, MAX_UNIT)?;
	}
	if let Some(v) = unit_price {
		bounded(v.0)?;
	}
	Ok(())
}

fn check_service_def(def: &ServiceDef) -> ClResult<()> {
	check_service_fields(
		Some(&def.name),
		def.description.as_deref(),
		Some(&def.unit),
		Some(def.unit_price),
	)
}

/// What to change on one line of a draft. Absent fields are left alone; `discount` is
/// `Some(None)` to clear it.
#[derive(Clone, Debug, Default)]
pub struct LinePatch {
	pub description: Option<String>,
	pub qty: Option<Qty>,
	pub unit_price: Option<Money>,
	pub vat_code: Option<VatCode>,
	pub discount: Option<Option<Discount>>,
	pub discount_description: Option<Option<String>>,
	pub note: Option<Option<String>>,
}

/// An invoice with the three things a renderer needs that the row does not carry. `lines`
/// and `groups` are `None` on a listing, which omits them, and `Some` on a single read.
#[derive(Clone, Debug)]
pub struct FullInvoice {
	pub invoice: Invoice,
	pub party_uid: Option<PartyId>,
	/// The invoice this `STORNO` cancels, and the `STORNO` that cancelled this one. `Invoice`
	/// holds the pairing as an internal key, and the wire shape wants both uids — without them
	/// nothing on the wire links a cancellation to what it cancelled.
	pub original_invoice_uid: Option<InvoiceId>,
	pub storno_invoice_uid: Option<InvoiceId>,
	pub lines: Option<Vec<InvoiceLine>>,
	pub groups: Option<Vec<InvoiceVatGroup>>,
	/// The rendered PDF's metadata, printed on the wire as `document` — without it a client
	/// learns whether one exists only by calling `GET …/pdf` and reading `E-INV-PDF-PENDING`.
	/// Loaded with `lines`, so a listing does not pay for it.
	pub document: Option<InvoiceDocument>,
}

fn conflict(code: &'static str, msg: &'static str) -> Error {
	Error::coded(StatusCode::CONFLICT, code, msg)
}

/// The invoice-level discount off the stored column pair. Every re-price — at ISSUE and on
/// every line edit — must pass this, not `None`: `vat::compute` folds its apportioned share
/// into each `invoice_lines.discount_amount`, where it is indistinguishable from the line's
/// own discount and so cannot be recovered from the lines.
pub(crate) fn invoice_discount(invoice: &Invoice) -> ClResult<Option<Discount>> {
	crate::money::discount_of(invoice.discount_kind, invoice.discount_value)
}

/// The store reports a failed draft write as `None`/`false` without deciding why; this is where
/// that becomes an answer: the row exists, so it is no longer a draft.
fn not_draft() -> Error {
	conflict("E-INV-NOT-DRAFT", "the invoice is no longer a draft")
}

/// A `PENDING` draft: a gateway is charging the total this invoice had when the payment opened,
/// so editing it would bill one figure and invoice another. Its own code rather than
/// `not_draft`, because the caller's move is to wait or abandon the payment, not to give up.
fn locked() -> Error {
	conflict("E-INV-LOCKED", "a payment is open on this invoice")
}

/// A caller may choose `TRANSFER` or `CASH` (paid at issue). `CARD` means a gateway confirmed
/// the charge, so only [`Invoices::begin_card_payment`] sets it; `OTHER` has no defined flow.
fn check_method(method: Option<PaymentMethod>) -> ClResult<()> {
	match method {
		None | Some(PaymentMethod::Transfer | PaymentMethod::Cash) => Ok(()),
		Some(PaymentMethod::Card) => Err(Error::coded(
			StatusCode::BAD_REQUEST,
			"E-INV-METHOD-RESERVED",
			"CARD is set by the payment flow, not by a caller",
		)),
		Some(PaymentMethod::Other) => Err(Error::coded(
			StatusCode::BAD_REQUEST,
			"E-INV-METHOD-UNSUPPORTED",
			"only TRANSFER or CASH can be chosen as a payment method",
		)),
	}
}

/// A patched field over its stored value: the patch wins when present, null included.
fn merged<'a>(patch: &'a Patch<String>, stored: Option<&'a String>) -> Option<&'a str> {
	patch.as_option().unwrap_or(stored).map(String::as_str)
}

/// `mark_paid`/`set_paid` declare money received with no gateway behind it, which is only
/// true of a bank transfer; a card invoice is paid by `saas-billing`'s settle and nothing else.
fn require_transfer(invoice: &Invoice) -> ClResult<()> {
	if invoice.payment_method == PaymentMethod::Transfer {
		return Ok(());
	}
	Err(conflict("E-INV-NOT-TRANSFER", "only a TRANSFER invoice can be marked paid by hand"))
}

/// Whether an [`InvoicePatch`] asks for anything beyond `notes`.
///
/// Destructured without `..` on purpose: a field added to [`InvoicePatch`] and missed here
/// would silently become writable on an issued invoice, and the compiler is what catches that.
fn patch_touches_more_than_notes(p: &InvoicePatch) -> bool {
	let InvoicePatch {
		billing_party_id,
		payment_method,
		fulfilment_date,
		due_date,
		notes: _,
		currency,
		rate_e6,
		discount_value,
		period_start,
		period_end,
	} = p;
	billing_party_id.is_some()
		|| payment_method.is_some()
		|| currency.is_some()
		|| rate_e6.is_some()
		|| discount_value.is_some()
		|| !fulfilment_date.is_undefined()
		|| !due_date.is_undefined()
		|| !period_start.is_undefined()
		|| !period_end.is_undefined()
}

/// The draft moved under a caller that had already re-priced it. A retry succeeds, which is
/// why this is a 409 and not a 500.
fn stale() -> Error {
	conflict("E-INV-STALE", "the draft changed while this edit was being priced")
}

/// The page ceiling, enforced in the handle: SQLite reads a negative `LIMIT` as unbounded, and
/// most consumers drive the handle from Rust without mounting a route that could clamp.
pub const MAX_PAGE_LIMIT: i64 = 200;

/// The month-series ceiling on [`Invoices::summary`]: three years of buckets, past which a
/// dashboard is asking for a report.
pub const MAX_SUMMARY_MONTHS: i64 = 36;

#[derive(Clone)]
pub struct Invoices {
	app: App,
	/// Resolved once here rather than on each of the ~30 `self.store()?` calls: `App` is
	/// immutable after `build()`, so the extension map cannot answer differently later.
	store: Option<Arc<dyn InvoiceStore>>,
}

/// `409 E-INV-CARD-DATES` when a draft carries a fulfilment or due date other than today: a
/// CARD invoice is fulfilled and due on its issue date, and money taken before a later
/// fulfilment would be an advance (Áfa tv. 59. §). Must run before the gateway charge.
pub fn check_card_dates(invoice: &Invoice) -> ClResult<()> {
	check_today(
		"E-INV-CARD-DATES",
		[invoice.fulfilment_date.as_deref(), invoice.due_date.as_deref()],
	)
}

/// `409 E-INV-SELLER-CLOSED` when the draft's seller is closed. Must run before the gateway
/// charge: a charge against a draft that can never issue is money taken for nothing.
pub fn check_seller_open(seller: &Seller) -> ClResult<()> {
	issue::seller_open(seller)
}

/// `409 {code}` when any of `dates` is other than today: a CARD or CASH invoice is fulfilled,
/// due and paid on its issue date.
fn check_today(code: &'static str, dates: [Option<&str>; 2]) -> ClResult<()> {
	let today = resolved_today()?;
	if dates.into_iter().flatten().any(|d| d != today) {
		return Err(conflict(
			code,
			"a card- or cash-paid invoice is fulfilled and due on its issue date",
		));
	}
	Ok(())
}

/// A party's payment term and preferred method: 0..=36500 days, `TRANSFER` or `CASH`.
fn check_terms(days: Option<i64>, method: Option<PaymentMethod>) -> ClResult<()> {
	if days.is_some_and(|d| !(0..=36_500).contains(&d)) {
		return Err(Error::coded(
			StatusCode::BAD_REQUEST,
			"E-INV-PAYMENT-DAYS",
			"payment days must be between 0 and 36500",
		));
	}
	check_method(method)
}

impl Invoices {
	pub fn new(app: App) -> Self {
		let store = app.extensions.get::<Arc<dyn InvoiceStore>>().cloned();
		Self { app, store }
	}

	fn store(&self) -> ClResult<Arc<dyn InvoiceStore>> {
		self.store.clone().ok_or_else(no_store)
	}

	/// The invoice-currency -> HUF rate every VAT group of a non-HUF invoice has to carry
	/// (Áfa tv. 172. §). Held by construction rather than checked: `draft::price` fills each
	/// group's `vat_huf` from exactly the `Some` this returns, so a foreign-currency invoice
	/// cannot reach the store without one. `None` for HUF. Same source and dating as
	/// `issue::plan`.
	///
	/// Through [`currency::effective_rate_e6`] and not `rate_on` directly: `rate_on` demands a
	/// published `currency_rates` row, so a `mode = 'FIXED'` currency — the documented
	/// admin-set mode, and how the seeded HUF row is written — could never be invoiced, nor
	/// could anything on a deployment whose `currency.rate_source` is not `MNB`.
	///
	/// The HUF early return stays: `effective_rate_e6` would answer `1_000_000`, but a HUF
	/// invoice stores `NULL`, which is a different thing from a rate of one.
	async fn huf_rate_e6(&self, cur: &Currency, date: &str) -> ClResult<Option<i64>> {
		if cur.code == "HUF" {
			return Ok(None);
		}
		let source = self.app.settings.text("currency.rate_source").await?;
		let configured = CurrencyCode::parse(&self.app.settings.text("currency.base").await?)?;
		let max_age = self.app.settings.int("currency.max_rate_age_days").await?;
		Ok(Some(
			currency::effective_rate_e6(
				self.store()?.as_ref(),
				cur,
				&CurrencyCode::huf(),
				&configured,
				&source,
				date,
				max_age,
			)
			.await?,
		))
	}

	async fn audit(&self, ctx: &Ctx, entity: &str, id: Option<&str>, action: &str) {
		audit::log(&self.app.store, ctx, entity, id, action, None).await;
	}

	/// [`Self::audit`] for `ISSUE` and `STORNO`, whose rows are the Számv. tv. record of who
	/// numbered a document rather than a diagnostic. A lost one is unrecoverable — the write
	/// is past the commit — so the caller is told instead of only the log.
	async fn try_audit(
		&self,
		ctx: &Ctx,
		entity: &str,
		id: Option<&str>,
		action: &str,
	) -> ClResult<()> {
		audit::try_log(&self.app.store, ctx, entity, id, action, None).await
	}

	/// The one place in this crate a raw org id reaches [`saas_core::auth_mw::require_role_on`].
	/// It takes the resolved [`Seller`] rather than an `i64` because `seller.org_id` and
	/// `ctx.org()?` are both `i64`: passing the acting org would let a customer org's admin
	/// issue under the platform's taxpayer id, with no compile error and no failing test.
	async fn require_seller_role(&self, ctx: &Ctx, seller: &Seller, min: Role) -> ClResult<()> {
		saas_core::auth_mw::require_role_on(&self.app, ctx, seller.org_id, min).await
	}

	/// The seller the acting org invoices under: its own `sellers` row, or the nearest
	/// ancestor's — a business unit inherits its parent company's. [`Error::NotFound`] means no
	/// org up to and including the root owns one.
	async fn seller_of_org(&self, ctx: &Ctx) -> ClResult<Seller> {
		self.store()?.seller_for_org(ctx.org()?).await?.ok_or(Error::NotFound)
	}

	/// [`Self::seller_of_org`] plus the gate, which is every seller and catalogue write.
	async fn seller_admin(&self, ctx: &Ctx) -> ClResult<Seller> {
		let seller = self.seller_of_org(ctx).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		Ok(seller)
	}

	/// The seller an existing invoice carries. **Not** `invoice.org_id`: that is the buyer's
	/// org, and the org allowed to mutate a document is the one whose taxpayer id is on it.
	async fn seller_of_invoice(&self, invoice: &Invoice) -> ClResult<Seller> {
		self.store()?.seller_by_id(invoice.seller_id).await?.ok_or(Error::NotFound)
	}

	// ------------------------------------------------------------ catalogue

	/// Idempotent upsert by `(org_id, services.code)` on the seller's own catalogue. Rows whose
	/// code is absent from `catalogue` are left alone — never deleted, because
	/// `invoice_lines.service_id` references them.
	pub async fn sync_services(&self, ctx: &Ctx, catalogue: &[ServiceDef]) -> ClResult<()> {
		let seller = self.seller_admin(ctx).await?;
		for def in catalogue {
			check_service_def(def)?;
		}
		self.store()?.sync_services(seller.org_id, catalogue).await?;
		self.audit(ctx, "service", None, "SYNC").await;
		Ok(())
	}

	pub async fn service_by_code(&self, ctx: &Ctx, code: &str) -> ClResult<Service> {
		let seller = self.seller_of_org(ctx).await?;
		self.store()?.service_by_code(seller.org_id, code).await?.ok_or(Error::NotFound)
	}

	pub async fn service(&self, ctx: &Ctx, uid: &str) -> ClResult<Service> {
		let seller = self.seller_of_org(ctx).await?;
		let uid = ServiceId::parse(uid)?;
		self.store()?.service_by_uid(seller.org_id, &uid).await?.ok_or(Error::NotFound)
	}

	pub async fn list_services(&self, ctx: &Ctx, active_only: bool) -> ClResult<Vec<Service>> {
		let seller = self.seller_of_org(ctx).await?;
		self.store()?.list_services(seller.org_id, active_only, MAX_PAGE_LIMIT).await
	}

	pub async fn create_service(&self, ctx: &Ctx, def: &ServiceDef) -> ClResult<Service> {
		let seller = self.seller_admin(ctx).await?;
		issue::seller_open(&seller)?;
		check_service_def(def)?;
		let svc = self.store()?.create_service(seller.org_id, def).await?;
		self.audit(ctx, "service", Some(svc.uid.as_str()), "CREATE").await;
		Ok(svc)
	}

	pub async fn update_service(
		&self,
		ctx: &Ctx,
		uid: &str,
		patch: &ServicePatch,
	) -> ClResult<Service> {
		let seller = self.seller_admin(ctx).await?;
		issue::seller_open(&seller)?;
		let uid = ServiceId::parse(uid)?;
		check_service_fields(
			patch.name.as_deref(),
			patch.description.value().map(String::as_str),
			patch.unit.as_deref(),
			patch.unit_price,
		)?;
		let svc = self
			.store()?
			.update_service(seller.org_id, &uid, patch)
			.await?
			.ok_or(Error::NotFound)?;
		self.audit(ctx, "service", Some(svc.uid.as_str()), "UPDATE").await;
		Ok(svc)
	}

	// ------------------------------------------------------------ reference data

	/// Every currency the deployment bills in. `all` also returns the disabled rows and needs
	/// `Admin` on the seller's org: a disabled currency is policy an org cannot act on.
	/// Each row carries the rate one of its units currently fetches in the base currency.
	/// A currency that has never had a rate published yields `None` rather than failing the
	/// whole listing — `E-INV-NO-RATE` is an answer about one invoice, not about a catalogue.
	pub async fn list_currencies(
		&self,
		ctx: &Ctx,
		all: bool,
	) -> ClResult<Vec<(Currency, Option<i64>)>> {
		if all {
			self.seller_admin(ctx).await?;
		}
		let base = CurrencyCode::parse(&self.app.settings.text("currency.base").await?)?;
		let source = self.app.settings.text("currency.rate_source").await?;
		let today = numbering::date_of(Timestamp::now())?;
		let mut out = Vec::new();
		let max_age = self.app.settings.int("currency.max_rate_age_days").await?;
		let store = self.store()?;
		for cur in currency::list(store.as_ref(), all).await? {
			let rate = currency::effective_rate_e6(
				store.as_ref(),
				&cur,
				&base,
				&base,
				&source,
				&today,
				max_age,
			)
			.await
			.ok();
			out.push((cur, rate));
		}
		Ok(out)
	}

	/// The currency an amount in a *new* invoice's body is expressed in, resolved exactly as
	/// [`Invoices::draft`] will resolve it. A caller needs it before the draft exists, to
	/// know how many decimals the amounts it is about to send carry.
	pub async fn currency(&self, ctx: &Ctx, asked: Option<&CurrencyCode>) -> ClResult<Currency> {
		let org = ctx.org()?;
		// Not `currency_for`: its rate is resolved on *today*, while `draft` resolves on the
		// fulfilment date and freezes that one, so the rate read here was always discarded.
		draft::currency_only(&self.app, self.store()?.as_ref(), org, asked).await
	}

	/// The currency an existing invoice's amounts are expressed in — the same question for a
	/// draft that already has one frozen on it.
	pub async fn invoice_currency(&self, ctx: &Ctx, uid: &str) -> ClResult<Currency> {
		let invoice = self.invoice(ctx, uid).await?;
		currency::get(self.store()?.as_ref(), &invoice.currency).await
	}

	/// The base currency `services.unit_price` is quoted in, so a caller can render one.
	pub async fn base_currency(&self, ctx: &Ctx) -> ClResult<Currency> {
		ctx.org()?;
		currency::base(&self.app).await
	}

	/// The seller the acting org invoices under, with the `nav_*` credentials dropped here rather
	/// than by whatever renders it — the handle is the trust boundary, and a Rust consumer
	/// rendering this directly would otherwise publish the operator's NAV technical user. Code
	/// that genuinely needs them (`saas-nav`, `issue`, `pdf`) calls `store.seller_by_id` instead.
	pub async fn seller(&self, ctx: &Ctx) -> ClResult<catalog::SellerView> {
		// The handle is the trust boundary: `SellerView` carries the tax number and bank account,
		// and a consumer route that forgets `org_read`'s layer must not reach them.
		let seller = self.seller_of_org(ctx).await?;
		let store = self.store()?;
		let version = store.current_seller_version(seller.id).await?.ok_or(Error::NotFound)?;
		let issued = store.seller_has_issued(seller.id).await?;
		Ok(catalog::SellerView::of(
			&seller,
			version,
			ctx.org()?,
			issued,
			self.default_payment_days().await?,
		))
	}

	/// The open seller edit, or `None` when there is none. `Admin` on the seller's own org, like
	/// every method below it: a draft is half-typed master data and is nobody else's business.
	pub async fn seller_draft(&self, ctx: &Ctx) -> ClResult<Option<catalog::SellerView>> {
		let seller = self.seller_admin(ctx).await?;
		let org = ctx.org()?;
		let store = self.store()?;
		let Some(draft) = store.draft_seller_version(seller.id).await? else {
			return Ok(None);
		};
		let issued = store.seller_has_issued(seller.id).await?;
		Ok(Some(catalog::SellerView::of(
			&seller,
			draft,
			org,
			issued,
			self.default_payment_days().await?,
		)))
	}

	/// Write the seller edit. Opens the draft from the live version if there is none, and
	/// rewrites it in place otherwise — **an edit does not make a version**, only
	/// [`Self::publish_seller`] does, so an invoice issued mid-edit still freezes the live one.
	pub async fn save_seller_draft(
		&self,
		ctx: &Ctx,
		patch: &SellerVersionPatch,
	) -> ClResult<catalog::SellerView> {
		let seller = self.seller_admin(ctx).await?;
		issue::seller_open(&seller)?;
		let checked = checked_seller_version(patch)?;
		self.check_tax_number_change(&seller, checked.tax_number.as_deref()).await?;
		let store = self.store()?;
		let base = match store.draft_seller_version(seller.id).await? {
			Some(v) => Some(v),
			None => store.current_seller_version(seller.id).await?,
		};
		check_expense_ratio(&checked, base.as_ref())?;
		let draft = store.save_seller_version_draft(seller.id, &checked).await?;
		self.audit(ctx, "seller", Some(&draft.seller_ver.to_string()), "DRAFT").await;
		let issued = store.seller_has_issued(seller.id).await?;
		Ok(catalog::SellerView::of(
			&seller,
			draft,
			ctx.org()?,
			issued,
			self.default_payment_days().await?,
		))
	}

	/// *Élesít*: the draft becomes the version every invoice issued from now on freezes, and
	/// the one it replaces is archived. One transaction, so there is never a moment with two
	/// live versions or none.
	///
	/// The whole merged row is re-validated here, not just what the last patch touched: a draft
	/// opened on a fresh install carries no `CURRENT` row's values behind it, and a version
	/// that reaches an invoice with a blank `supplierName` fails NAV's schema on a document
	/// that is already immutable.
	pub async fn publish_seller(&self, ctx: &Ctx) -> ClResult<catalog::SellerView> {
		let mut seller = self.seller_admin(ctx).await?;
		issue::seller_open(&seller)?;
		let store = self.store()?;
		let no_draft =
			|| bad_seller("E-INV-SELLER-NO-DRAFT", "there is no seller edit to publish".into());
		let draft = store.draft_seller_version(seller.id).await?.ok_or_else(no_draft)?;
		let new_taxpayer = self.check_tax_number_change(&seller, Some(&draft.tax_number)).await?;
		let ver = store
			.publish_seller_version(seller.id, Timestamp::now(), &complete_seller_version)
			.await?
			.ok_or_else(no_draft)?;
		self.audit(ctx, "seller", Some(&ver.to_string()), "PUBLISH").await;
		if new_taxpayer {
			self.drop_nav_login(&mut seller).await?;
		}
		let published = store.seller_version(ver).await?.ok_or(Error::NotFound)?;
		let issued = store.seller_has_issued(seller.id).await?;
		Ok(catalog::SellerView::of(
			&seller,
			published,
			ctx.org()?,
			issued,
			self.default_payment_days().await?,
		))
	}

	/// Publish `patch` as a new version **only when it changes the `CURRENT` row**, so a
	/// boot-time or `on_init` seed may run on every start without stacking an identical version
	/// per restart. `Ok(None)` is "nothing changed, no version was made".
	///
	/// The comparison runs on the *normalised* patch: [`checked_seller_version`] upper-cases the
	/// country and rewrites the EU VAT id, so comparing the raw input would see a change on every
	/// boot and publish one anyway.
	pub async fn sync_seller(
		&self,
		ctx: &Ctx,
		patch: &SellerVersionPatch,
	) -> ClResult<Option<catalog::SellerView>> {
		let mut seller = self.seller_admin(ctx).await?;
		issue::seller_open(&seller)?;
		let store = self.store()?;
		let draft_open = || {
			bad_seller(
				"E-INV-SELLER-DRAFT-OPEN",
				"a seller edit is open; publish or discard it before syncing".into(),
			)
		};
		// `save_seller_version_draft` rewrites an open draft in place, so syncing over one would
		// publish an operator's half-typed edit. Config gives way to a human, loudly. The cheap
		// refusal only: `sync_seller_version` re-probes inside the transaction, where the race
		// actually loses.
		if store.draft_seller_version(seller.id).await?.is_some() {
			return Err(draft_open());
		}
		let checked = checked_seller_version(patch)?;
		let new_taxpayer =
			self.check_tax_number_change(&seller, checked.tax_number.as_deref()).await?;
		let cur = store.current_seller_version(seller.id).await?;
		check_expense_ratio(&checked, cur.as_ref())?;
		if let Some(cur) = cur {
			// `merged` always zeroes `seller_ver` and forces `Draft`, so the store's own
			// bookkeeping is restored before the compare; everything left is statutory.
			let mut want = checked.merged(Some(&cur));
			want.seller_ver = cur.seller_ver;
			want.status = cur.status;
			want.valid_from = cur.valid_from;
			want.superseded_at = cur.superseded_at;
			if want == cur {
				return Ok(None);
			}
		}
		let ver = store
			.sync_seller_version(seller.id, Timestamp::now(), &checked, &complete_seller_version)
			.await?
			.ok_or_else(draft_open)?;
		self.audit(ctx, "seller", Some(&ver.to_string()), "SYNC").await;
		if new_taxpayer {
			self.drop_nav_login(&mut seller).await?;
		}
		let published = store.seller_version(ver).await?.ok_or(Error::NotFound)?;
		let issued = store.seller_has_issued(seller.id).await?;
		Ok(Some(catalog::SellerView::of(
			&seller,
			published,
			ctx.org()?,
			issued,
			self.default_payment_days().await?,
		)))
	}

	/// Mint the acting org's own seller, once: the self-service counterpart of the operator's
	/// boot-time `put_seller`. Admin on the **acting** org, which must be `SHARED` — a ROOT
	/// seller is the deployment's, minted at boot, and a PERSONAL org has no members, cannot be
	/// transferred, and its GDPR erasure must never touch an 8-year statutory record.
	///
	/// Two writes with no transaction between them, so it is resumable instead: a row left
	/// without a `CURRENT` version is completed by the next call rather than refused.
	pub async fn create_seller(
		&self,
		ctx: &Ctx,
		series_code: Option<&str>,
		patch: &SellerVersionPatch,
	) -> ClResult<catalog::SellerView> {
		let org = ctx.org()?;
		saas_core::auth_mw::require_role_on(&self.app, ctx, org, Role::Admin).await?;

		// Everything validated before the first write, so a malformed request writes no row.
		let series = series_code.map_or("A", str::trim);
		crate::store::bounded_text("seriesCode", series, crate::store::MAX_SERIES_CODE)?;
		if series.is_empty() {
			return Err(bad_text("seriesCode must not be blank".into()));
		}
		let checked = checked_seller_version(patch)?;
		check_expense_ratio(&checked, None)?;
		complete_seller_version(&checked.merged(None))?;

		let store = self.store()?;
		let exists = || conflict("E-INV-SELLER-EXISTS", "this org already owns a seller");
		let own = store.seller_for_org(org).await?.filter(|s| s.org_id == org);
		let seller = match own {
			Some(s) if store.current_seller_version(s.id).await?.is_some() => {
				return Err(exists());
			}
			Some(mut s) => {
				s.series_code = series.to_owned();
				store.put_seller(&s).await?;
				s
			}
			None => {
				let s = Seller {
					id: org,
					uid: saas_core::ids::SellerId::generate(),
					org_id: org,
					nav_base_url: String::new(),
					nav_login: None,
					series_code: series.to_owned(),
					closed_at: None,
					payment_days: None,
					created_at: Timestamp::now(),
				};
				match store.create_seller(&s).await {
					Ok(true) => s,
					Ok(false) => {
						return Err(conflict(
							"E-INV-SELLER-ORG-KIND",
							"only an active shared org can own a seller",
						));
					}
					Err(Error::Conflict(_)) => return Err(exists()),
					Err(e) => return Err(e),
				}
			}
		};
		let ver = store
			.sync_seller_version(seller.id, Timestamp::now(), &checked, &complete_seller_version)
			.await?
			.ok_or_else(|| bad_seller("E-INV-SELLER-DRAFT-OPEN", "a seller edit is open".into()))?;
		self.audit(ctx, "seller", Some(seller.uid.as_str()), "CREATE").await;
		let published = store.seller_version(ver).await?.ok_or(Error::NotFound)?;
		Ok(catalog::SellerView::of(
			&seller,
			published,
			org,
			false,
			self.default_payment_days().await?,
		))
	}

	/// Throw the open edit away. The live version is untouched.
	pub async fn discard_seller_draft(&self, ctx: &Ctx) -> ClResult<()> {
		let seller = self.seller_admin(ctx).await?;
		if self.store()?.discard_seller_version_draft(seller.id).await? {
			self.audit(ctx, "seller", None, "DISCARD_DRAFT").await;
		}
		Ok(())
	}

	/// Every published version, newest first — which version an invoice was issued under is
	/// `invoices.seller_ver`, and this is what names it.
	pub async fn seller_history(&self, ctx: &Ctx) -> ClResult<Vec<catalog::SellerView>> {
		let seller = self.seller_admin(ctx).await?;
		let org = ctx.org()?;
		let store = self.store()?;
		let history = store.seller_version_history(seller.id).await?;
		let days = self.default_payment_days().await?;
		let issued = store.seller_has_issued(seller.id).await?;
		Ok(history
			.into_iter()
			.map(|v| catalog::SellerView::of(&seller, v, org, issued, days))
			.collect())
	}

	/// Make the seller's company read-only (`closed`) or reopen it: only payments may be
	/// recorded while closed. `Owner` on the seller's own org, behind step-up.
	pub async fn set_seller_closed(
		&self,
		ctx: &Ctx,
		closed: bool,
	) -> ClResult<catalog::SellerView> {
		saas_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let mut seller = self.seller_of_org(ctx).await?;
		self.require_seller_role(ctx, &seller, Role::Owner).await?;
		let store = self.store()?;
		if seller.org_id == self.app.store.root_org_id().await? {
			return Err(conflict(
				"E-INV-SELLER-DEPLOYMENT",
				"the deployment's own seller cannot be made read-only",
			));
		}
		let at = closed.then(Timestamp::now);
		if !store.set_seller_closed(seller.id, at).await? {
			return Err(conflict("E-INV-SELLER-PENDING", "a card payment is in progress"));
		}
		seller.closed_at = at;
		self.audit(
			ctx,
			"seller",
			Some(seller.uid.as_str()),
			if closed { "CLOSE" } else { "REOPEN" },
		)
		.await;
		let version = store.current_seller_version(seller.id).await?.ok_or(Error::NotFound)?;
		let issued = store.seller_has_issued(seller.id).await?;
		Ok(catalog::SellerView::of(
			&seller,
			version,
			ctx.org()?,
			issued,
			self.default_payment_days().await?,
		))
	}

	/// Set or clear (`None`) the org's own payment term, which a party's overrides and
	/// `settings['invoice.default_payment_days']` backs. `Admin` on the seller's own org.
	pub async fn set_seller_payment_days(
		&self,
		ctx: &Ctx,
		days: Option<i64>,
	) -> ClResult<catalog::SellerView> {
		check_terms(days, None)?;
		let mut seller = self.seller_admin(ctx).await?;
		let store = self.store()?;
		if !store.set_seller_payment_days(seller.id, days).await? {
			return Err(Error::NotFound);
		}
		seller.payment_days = days;
		self.audit(ctx, "seller", Some(seller.uid.as_str()), "PAYMENT-DAYS").await;
		let version = store.current_seller_version(seller.id).await?.ok_or(Error::NotFound)?;
		let issued = store.seller_has_issued(seller.id).await?;
		Ok(catalog::SellerView::of(
			&seller,
			version,
			ctx.org()?,
			issued,
			self.default_payment_days().await?,
		))
	}

	/// The deployment's fallback term, which [`catalog::SellerView`] carries so a client can
	/// name what an unset one inherits.
	async fn default_payment_days(&self) -> ClResult<i64> {
		self.app.settings.int("invoice.default_payment_days").await
	}

	/// `true` when `new` names another taxpayer than the `CURRENT` version and that is still
	/// allowed; `E-INV-SELLER-TAXNUMBER-LOCKED` once an invoice has a number — NAV
	/// authenticates with the current version's digits, so a later storno would file under
	/// the wrong taxpayer.
	async fn check_tax_number_change(&self, seller: &Seller, new: Option<&str>) -> ClResult<bool> {
		let Some(new) = new else { return Ok(false) };
		let store = self.store()?;
		let Some(cur) = store.current_seller_version(seller.id).await? else {
			return Ok(false);
		};
		if issue::tax_digits(new) == issue::tax_digits(&cur.tax_number) {
			return Ok(false);
		}
		// Checked outside the publish tx; an invoice issued in the same instant as the
		// edit slips through. Move into sync/publish_seller_version if that ever matters.
		if store.seller_has_issued(seller.id).await? {
			return Err(conflict(
				"E-INV-SELLER-TAXNUMBER-LOCKED",
				"the tax number is fixed once an invoice has a number; a new tax number is a new company",
			));
		}
		Ok(true)
	}

	/// The stored NAV technical user belongs to the previous taxpayer: NAV would answer
	/// `NOT_REGISTERED_CUSTOMER`, so a new tax number starts disconnected.
	async fn drop_nav_login(&self, seller: &mut Seller) -> ClResult<()> {
		seller.nav_login = None;
		self.store()?.put_seller(seller).await
	}

	// ------------------------------------------------------------ billing parties

	pub async fn create_party(&self, ctx: &Ctx, patch: &PartyPatch) -> ClResult<BillingParty> {
		let org = ctx.org()?;
		// `PartyPatch` makes every field optional so one type can drive PATCH, but `kind`, `name`
		// and `country` are `NOT NULL`: without this a missing field is a 500, not a 400.
		for (field, present) in [
			("kind", patch.kind.is_some()),
			("name", patch.name.is_some()),
			("country", patch.country.is_some()),
		] {
			if !present {
				return Err(bad_text(format!("{field} is required to create a billing party")));
			}
		}
		let patch = checked_party(patch)?;
		let party = self.store()?.create_party(org, &patch).await?;
		self.audit(ctx, "billing_party", Some(party.uid.as_str()), "CREATE").await;
		Ok(party)
	}

	pub async fn party(&self, ctx: &Ctx, uid: &str) -> ClResult<BillingParty> {
		let org = ctx.org()?;
		let uid = PartyId::parse(uid)?;
		self.store()?.party_by_uid(org, &uid).await?.ok_or(Error::NotFound)
	}

	pub async fn list_parties(&self, ctx: &Ctx) -> ClResult<Vec<BillingParty>> {
		self.store()?.list_parties(ctx.org()?, MAX_PAGE_LIMIT).await
	}

	pub async fn update_party(
		&self,
		ctx: &Ctx,
		uid: &str,
		patch: &PartyPatch,
	) -> ClResult<BillingParty> {
		let org = ctx.org()?;
		let uid = PartyId::parse(uid)?;
		let patch = checked_party(patch)?;
		let party = self.store()?.update_party(org, &uid, &patch).await?.ok_or(Error::NotFound)?;
		self.audit(ctx, "billing_party", Some(party.uid.as_str()), "UPDATE").await;
		Ok(party)
	}

	pub async fn delete_party(&self, ctx: &Ctx, uid: &str) -> ClResult<()> {
		let org = ctx.org()?;
		let uid = PartyId::parse(uid)?;
		if !self.store()?.delete_party(org, &uid).await? {
			return Err(Error::NotFound);
		}
		self.audit(ctx, "billing_party", Some(uid.as_str()), "DELETE").await;
		Ok(())
	}

	// ------------------------------------------------------------ reads

	/// Org-scoped, so an `inv_` id from another org reads as absent.
	pub async fn invoice(&self, ctx: &Ctx, uid: &str) -> ClResult<Invoice> {
		let org = ctx.org()?;
		let uid = InvoiceId::parse(uid)?;
		self.store()?.invoice_by_uid(Some(org), &uid).await?.ok_or(Error::NotFound)
	}

	pub async fn list_invoices(
		&self,
		ctx: &Ctx,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Invoice>> {
		self.store()?
			.list_invoices(ctx.org()?, before_id, limit.clamp(1, MAX_PAGE_LIMIT))
			.await
	}

	/// The stored PDF of an issued invoice: the `invoice_documents` row, the invoice it
	/// belongs to (for the filename) and the file the content hash puts it at.
	///
	/// Nothing here reads the file — a route streams it — but deciding *why* there is no PDF
	/// is a service question: a draft has none by definition, while an issued invoice whose
	/// `RENDER_PDF` job has not finished will have one shortly.
	pub async fn document(
		&self,
		ctx: &Ctx,
		uid: &str,
	) -> ClResult<(Invoice, InvoiceDocument, PathBuf)> {
		let invoice = self.invoice(ctx, uid).await?;
		if invoice.status == InvoiceStatus::Draft {
			return Err(conflict("E-INV-NOT-ISSUED", "a draft has no invoice document"));
		}
		let doc =
			self.store()?.invoice_document(invoice.id).await?.ok_or_else(|| {
				conflict("E-INV-PDF-PENDING", "the document is still being rendered")
			})?;
		let path = crate::pdf::doc_path(&self.app.config.data_dir, &doc.sha256)?;
		Ok((invoice, doc, path))
	}

	/// An invoice plus what a renderer needs that the row itself does not carry.
	///
	/// `Money` has no currency of its own, `Invoice` holds the billing party as an internal
	/// key, and the wire shape wants the party's public uid — so a view of an invoice needs
	/// three things beyond the row. Collecting them is a projection, not a decision, which is
	/// why it is one call and not three from a handler.
	pub async fn hydrate(
		&self,
		ctx: &Ctx,
		invoice: Invoice,
		with_lines: bool,
	) -> ClResult<FullInvoice> {
		let store = self.store()?;
		let org_id = ctx.org()?;
		// `party_by_id` and `invoice_by_id` are not org-scoped (unlike `party_by_uid`), and
		// this is `pub`, so the confinement check has to happen here. Another org's row
		// reads as absent, never 403.
		let party_uid = match invoice.billing_party_id {
			Some(id) => store.party_by_id(id).await?.filter(|p| p.org_id == org_id).map(|p| p.uid),
			None => None,
		};
		// Both are one read on the rows that have one and none on the rest:
		// `original_invoice_id` is set only on a `STORNO`, and a counter-invoice exists only
		// once the original is `STORNOED`.
		let original_invoice_uid = match invoice.original_invoice_id {
			Some(id) => {
				store.invoice_by_id(id).await?.filter(|i| i.org_id == org_id).map(|i| i.uid)
			}
			None => None,
		};
		// `storno_of` is keyed on an invoice the caller already passed the check for.
		let storno_invoice_uid = match invoice.status {
			InvoiceStatus::Stornoed => store.storno_of(invoice.id).await?.map(|i| i.uid),
			_ => None,
		};
		let (lines, groups, document) = if with_lines {
			(
				Some(store.invoice_lines(invoice.id).await?),
				Some(store.invoice_vat_groups(invoice.id).await?),
				// On `with_lines` for the same reason they are: a listing must not pay a
				// per-row read. A draft has none and `invoice_document` already answers `None`.
				store.invoice_document(invoice.id).await?,
			)
		} else {
			(None, None, None)
		};
		Ok(FullInvoice {
			invoice,
			party_uid,
			original_invoice_uid,
			storno_invoice_uid,
			lines,
			groups,
			document,
		})
	}

	/// One invoice with its lines and VAT groups.
	pub async fn full(&self, ctx: &Ctx, uid: &str) -> ClResult<FullInvoice> {
		let invoice = self.invoice(ctx, uid).await?;
		self.hydrate(ctx, invoice, true).await
	}

	/// A page of invoices without lines or VAT groups.
	///
	/// `cursor` is the previous page's last `inv_` uid. The store still pages on `(id DESC)`,
	/// but the *token* is public: a decimal row id on the wire told a client with three
	/// invoices what the deployment's global id sequence was doing, and only a `uid` belongs
	/// in a response. Resolved under the caller's org, so a cursor from another org is
	/// simply not a cursor rather than an oracle.
	pub async fn list_full(
		&self,
		ctx: &Ctx,
		filter: &InvoiceFilter,
		cursor: Option<&str>,
		limit: i64,
	) -> ClResult<Vec<FullInvoice>> {
		let before_id = match cursor {
			Some(c) => {
				let bad = || Error::validation("cursor is not a page cursor");
				let uid = InvoiceId::parse(c).map_err(|_| bad())?;
				let row = self.store()?.invoice_by_uid(Some(ctx.org()?), &uid).await?;
				Some(row.ok_or_else(bad)?.id)
			}
			None => None,
		};
		Ok(self
			.store()?
			.list_invoices_page(ctx.org()?, filter, before_id, limit.clamp(1, MAX_PAGE_LIMIT))
			.await?
			.into_iter()
			.map(|r| FullInvoice {
				invoice: r.invoice,
				party_uid: r.party_uid,
				original_invoice_uid: r.original_invoice_uid,
				storno_invoice_uid: r.storno_invoice_uid,
				lines: None,
				groups: None,
				document: None,
			})
			.collect())
	}

	/// Dashboard aggregates for the acting org: `months` calendar months of series counting the
	/// current one, clamped to `1..=MAX_SUMMARY_MONTHS`.
	///
	/// A read — it aggregates exactly the rows [`Invoices::list_full`] already returns to the
	/// same actor — so there is no audit row and no check beyond `ctx.org()?`.
	pub async fn summary(&self, ctx: &Ctx, months: i64) -> ClResult<InvoiceSummary> {
		let now = Timestamp::now();
		// The local calendar day, never `now()` directly: a UTC day boundary marks a Budapest
		// invoice overdue up to an hour early.
		let today = numbering::date_of(now)?;
		let date = numbering::local(now)?.date();
		let months = months.clamp(1, MAX_SUMMARY_MONTHS);
		let first =
			i64::from(date.year()) * 12 + i64::from(u8::from(date.month())) - 1 - (months - 1);
		let from_month = format!("{:04}-{:02}", first.div_euclid(12), first.rem_euclid(12) + 1);
		let (y, m) = (date.year(), date.month());
		let this_month = numbering::utc_span(
			&format!("{y:04}-{:02}-01", u8::from(m)),
			&format!("{y:04}-{:02}-{:02}", u8::from(m), m.length(y)),
		)?;
		self.store()?.invoice_summary(ctx.org()?, &from_month, &today, this_month).await
	}

	/// The acting org's HUF revenue for the local `year`, month by month, on both the invoiced
	/// and the received basis. Facts only: which limit applies is the caller's. A read, like
	/// [`Self::summary`].
	pub async fn revenue(&self, ctx: &Ctx, year: i32) -> ClResult<Vec<RevenueMonth>> {
		if !(2000..=2100).contains(&year) {
			return Err(Error::validation(format!("year {year} is out of range")));
		}
		let (start, end) = numbering::utc_span(&format!("{year}-01-01"), &format!("{year}-12-31"))?;
		self.store()?.invoice_revenue(ctx.org()?, year, start, end).await
	}

	// ------------------------------------------------------------ drafts

	/// Create a draft, priced and grouped. A `request_id` that already exists returns the
	/// invoice it made rather than a second one — this matters far more in Rust than over
	/// HTTP, because a checkout handler is retried by machines.
	pub async fn draft(&self, ctx: &Ctx, req: &NewDraft) -> ClResult<Invoice> {
		check_dates([req.fulfilment_date.as_deref(), req.due_date.as_deref()])?;
		check_range(req.fulfilment_date.as_deref())?;
		check_period(
			req.period_start.as_deref(),
			req.period_end.as_deref(),
			req.fulfilment_date.as_deref(),
		)?;
		check_notes(req.notes.as_deref())?;
		check_method(req.payment_method)?;
		let org = ctx.org()?;
		let store = self.store()?;
		// The gate is the org that owns the *seller*, never the drafting org: `invoices.org_id`
		// is the buyer, and the taxpayer id the document carries is the seller's.
		let seller_row = self.seller_admin(ctx).await?;
		issue::seller_open(&seller_row)?;

		// Scoped to the org: `request_id` is unique per org, so another org holding
		// the same key is simply a different invoice, not a reason to refuse this one. The
		// read-then-insert race that remains is closed inside `create_draft_full`.
		if let Some(request_id) = &req.request_id
			&& let Some(existing) = store.invoice_by_request_id(org, request_id).await?
		{
			return Ok(existing);
		}

		let party = match &req.billing_party {
			Party::OrgDefault => store.default_party(org).await?,
			Party::Uid(uid) => store.party_by_uid(org, uid).await?,
		}
		.ok_or_else(|| conflict("E-INV-NO-BUYER", "the org has no billing party"))?;

		// **The fulfilment date prices the draft, not today** — `issue::plan` resolves on it, so
		// resolving here on today stored `rate_e6` and every `*_huf` figure at the wrong day's
		// rate and the draft silently changed at issue. An absent date falls through to today.
		let today = numbering::date_of(Timestamp::now())?;
		let priced_on = req.fulfilment_date.clone().unwrap_or_else(|| today.clone());
		let (cur, rate_e6) = draft::currency_for(
			&self.app,
			self.store()?.as_ref(),
			org,
			req.currency.as_ref(),
			&priced_on,
		)
		.await?;
		let (kind, value) = crate::money::discount_parts(req.discount);

		// Everything fallible resolves *before* anything is written. Done after the insert, any
		// of these left a row with the `request_id` consumed and no lines, which the retry read
		// back and could never issue.
		// The **live** version: a draft prices against what would be frozen if it were issued
		// now, and `issue::run` decides again from the version current at that moment.
		let seller = store.current_seller_version(seller_row.id).await?.ok_or_else(|| {
			conflict("E-INV-SELLER-INCOMPLETE", "the seller has no published version")
		})?;
		// `.0`: the verdict only. `issue::run` calls `profile` again for the consultation number
		// to freeze, and `vies::check` caches before returning — so that second call is a
		// `vies_checks` read, not a second 15 s lookup.
		let verdict = determine(&seller, &issue::profile(&self.app, &seller, &party).await?.0);
		let mut lines =
			draft::resolve(store.as_ref(), seller_row.org_id, &cur, rate_e6, &req.lines).await?;
		pricing::apply(&self.app, ctx, &mut lines).await?;
		let huf_rate_e6 = self.huf_rate_e6(&cur, &priced_on).await?;
		// The invoice id is a placeholder: `create_draft_full` writes the lines and groups
		// against the id it allocates, so nothing reads this one.
		let priced = draft::price(
			0,
			&lines,
			req.discount,
			&verdict,
			huf_rate_e6,
			false,
			cur.price_round_step,
		)?;

		let method = req.payment_method.or(party.payment_method).unwrap_or(PaymentMethod::Transfer);
		if method == PaymentMethod::Cash {
			check_today(
				"E-INV-CASH-DATES",
				[req.fulfilment_date.as_deref(), req.due_date.as_deref()],
			)?;
		}
		let dated = req.fulfilment_date.is_some()
			|| req.due_date.is_some()
			|| req.period_start.is_some()
			|| req.period_end.is_some();
		let patch = dated.then(|| InvoicePatch {
			fulfilment_date: to_patch(req.fulfilment_date.clone()),
			due_date: to_patch(req.due_date.clone()),
			period_start: to_patch(req.period_start.clone()),
			period_end: to_patch(req.period_end.clone()),
			..Default::default()
		});

		let invoice = store
			.create_draft_full(
				&NewInvoice {
					org_id: org,
					seller_id: seller_row.id,
					billing_party_id: Some(party.id),
					request_id: req.request_id.clone(),
					kind: crate::store::InvoiceKind::Normal,
					original_invoice_id: None,
					currency: cur.code.clone(),
					rate_e6,
					payment_method: method,
					notes: req.notes.clone(),
					// Persisted so ISSUE and every later re-price apply the same discount the
					// draft was priced with; `req.discount` is `Copy`.
					discount_kind: kind,
					discount_value: value,
				},
				&priced,
				patch.as_ref(),
			)
			.await?;

		self.audit(ctx, "invoice", Some(invoice.uid.as_str()), "DRAFT").await;
		Ok(invoice)
	}

	/// Re-price the whole line set of a draft from what is stored, after `edit` has changed
	/// it. Every mutation returns the recomputed invoice, so no caller re-fetches for totals.
	///
	/// The seller gate is the caller's: every path in here has just resolved the seller, so
	/// gating again paid for the ancestor walk twice on each edit.
	///
	/// **Takes the caller's own `invoice`, and never re-reads it.** `invoice.version` is the
	/// optimistic guard on `replace_draft_lines`, and the row it guards has to be the row every
	/// captured value was derived from: `change_currency` reads the invoice to capture
	/// `old_cur` and `old_rate_e6`, so a second re-read here left a concurrent currency change
	/// landing between the two reads invisible to the guard, and its lines were converted a
	/// second time as if still in the old currency. `add_line` has the same shape — it prices
	/// against the currency and rate it read.
	async fn rewrite(
		&self,
		ctx: &Ctx,
		invoice: Invoice,
		patch: Option<&InvoicePatch>,
		edit: impl FnOnce(&mut Vec<crate::money::DraftLine>) -> ClResult<()>,
	) -> ClResult<Invoice> {
		let store = self.store()?;
		let stored = store.invoice_lines(invoice.id).await?;
		let mut lines: Vec<crate::money::DraftLine> =
			stored.iter().map(draft::to_draft).collect::<ClResult<_>>()?;
		edit(&mut lines)?;

		// The patch is not applied yet, so the buyer comes from it when it carries one: the
		// buyer is what decides the VAT verdict, and re-pricing against the *previous* party
		// is how a draft ends up with groups that describe someone else.
		let party_id = patch
			.and_then(|p| p.billing_party_id)
			.or(invoice.billing_party_id)
			.ok_or_else(|| conflict("E-INV-NO-BUYER", "the draft has no billing party"))?;
		// `party_by_id` is not org-scoped (unlike `party_by_uid`), and `InvoicePatch` is
		// `Deserialize` over this raw id — so the confinement check has to happen here.
		// Another org's party reads as absent, never 403.
		let org_id = ctx.org()?;
		let party = store
			.party_by_id(party_id)
			.await?
			.filter(|p| p.org_id == org_id)
			.ok_or(Error::NotFound)?;
		// `seller_id` is read off the invoice row the caller already passed the org check
		// for, so it needs no scoping of its own. The live version, not `invoice.seller_ver`:
		// this path only ever re-prices a DRAFT, which has frozen nothing yet.
		let seller = store.current_seller_version(invoice.seller_id).await?.ok_or_else(|| {
			conflict("E-INV-SELLER-INCOMPLETE", "the seller has no published version")
		})?;
		let verdict = determine(&seller, &issue::profile(&self.app, &seller, &party).await?.0);

		// The patch is not applied yet, so a rescaled invoice-level discount has to be taken
		// from it rather than from the row.
		let discount = match patch.and_then(|p| p.discount_value) {
			Some(v) => crate::money::discount_of(invoice.discount_kind, Some(v))?,
			None => invoice_discount(&invoice)?,
		};
		let currency = patch.and_then(|p| p.currency.as_ref()).unwrap_or(&invoice.currency);
		let cur = currency::get(self.store()?.as_ref(), currency).await?;
		// `edit_line` writes a caller-supplied `unit_price` straight into the set, which is
		// the one path that does not pass through `draft::resolve`'s step check. Vacuous for
		// every other caller of `rewrite`, whose prices are already stepped.
		for line in &lines {
			currency::ensure_price_on_step(line.unit_price, cur.price_round_step)?;
		}
		// The patch is not applied yet, so the date the HUF figures resolve on comes from it when
		// it carries one: `replace_draft_lines` writes the new `fulfilment_date` and the groups
		// in one statement, and the stale date stored `*_huf` at the wrong day's rate.
		let dated = match patch.map(|p| p.fulfilment_date.as_option()) {
			// The patch sets the date, or clears it back to "today".
			Some(Some(d)) => d.cloned(),
			// Absent from the patch, or no patch at all: the row's own date stands.
			_ => invoice.fulfilment_date.clone(),
		};
		let date = match dated {
			Some(d) => d,
			None => numbering::date_of(Timestamp::now())?,
		};
		// `rate_e6` is deliberately *not* touched: a rate move must convert every line, not
		// re-stamp the row, so `Invoices::patch` routes that to `change_currency`. Re-stamping
		// here as well priced the lines at the old rate and labelled them with the new one.
		let huf_rate_e6 = self.huf_rate_e6(&cur, &date).await?;
		let priced = draft::price(
			invoice.id,
			&lines,
			discount,
			&verdict,
			huf_rate_e6,
			false,
			cur.price_round_step,
		)?;
		// `invoice.version` is the snapshot every line above was priced against, read on
		// the reader pool. Without it two concurrent `POST /lines` both read N lines and both
		// write N+1, and the second commit drops the first caller's line.
		if !store.replace_draft_lines(invoice.id, patch, &priced, invoice.version).await? {
			// The store cannot say which of the two guards refused. Re-read: still a draft
			// means someone else edited it and a retry will work; anything else means it is
			// no longer a draft.
			return Err(match store.invoice_by_id(invoice.id).await?.map(|r| r.status) {
				Some(InvoiceStatus::Draft) => stale(),
				Some(InvoiceStatus::Pending) => locked(),
				_ => not_draft(),
			});
		}
		self.audit(ctx, "invoice", Some(invoice.uid.as_str()), "EDIT").await;
		store.invoice_by_id(invoice.id).await?.ok_or(Error::NotFound)
	}

	pub async fn add_line(&self, ctx: &Ctx, uid: &str, line: Line) -> ClResult<Invoice> {
		let invoice = self.invoice(ctx, uid).await?;
		let store = self.store()?;
		// The line resolves against the *seller's* catalogue — the org the price and the VAT
		// code belong to — not `invoice.org_id`, which is the buyer's.
		let seller = self.seller_of_invoice(&invoice).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		issue::seller_open(&seller)?;
		let cur = crate::currency::get(store.as_ref(), &invoice.currency).await?;
		let mut new_lines = draft::resolve(
			store.as_ref(),
			seller.org_id,
			&cur,
			invoice.rate_e6,
			std::slice::from_ref(&line),
		)
		.await?;
		self.rewrite(ctx, invoice, None, move |lines| {
			lines.append(&mut new_lines);
			Ok(())
		})
		.await
	}

	pub async fn edit_line(
		&self,
		ctx: &Ctx,
		uid: &str,
		line_no: u32,
		patch: LinePatch,
	) -> ClResult<Invoice> {
		let invoice = self.invoice(ctx, uid).await?;
		let seller = self.seller_of_invoice(&invoice).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		issue::seller_open(&seller)?;
		self.rewrite(ctx, invoice, None, move |lines| {
			let index = usize::try_from(line_no)
				.ok()
				.and_then(|n| n.checked_sub(1))
				.filter(|n| *n < lines.len())
				.ok_or(Error::NotFound)?;
			let line = &mut lines[index];
			// `LineBody::into_line` refuses this pair on create, so PATCH was the way around it:
			// an operator's catalogue item at your own price and tax code. Same refusal, same
			// wording; the checks differ because a stored line carries `service_id`, a body
			// `serviceCode`.
			if line.service_id.is_some() && (patch.unit_price.is_some() || patch.vat_code.is_some())
			{
				return Err(Error::coded(
					StatusCode::BAD_REQUEST,
					"E-INV-LINE",
					draft::CATALOGUE_LINE_MSG,
				));
			}
			if let Some(v) = patch.description {
				line.description = v;
			}
			if let Some(v) = patch.qty {
				line.qty = v;
			}
			if let Some(v) = patch.unit_price {
				line.unit_price = v;
			}
			if let Some(v) = patch.vat_code {
				line.vat_code = v;
			}
			if let Some(v) = patch.discount {
				line.discount = v;
			}
			if let Some(v) = patch.discount_description {
				line.discount_description = v;
			}
			if let Some(v) = patch.note {
				line.note = v;
			}
			Ok(())
		})
		.await
	}

	/// Renumbers what follows, because `line_no` is NAV's `lineNumber` and NAV requires
	/// 1..n contiguous — which the store does for free, assigning numbers from slice order.
	pub async fn remove_line(&self, ctx: &Ctx, uid: &str, line_no: u32) -> ClResult<Invoice> {
		let invoice = self.invoice(ctx, uid).await?;
		let seller = self.seller_of_invoice(&invoice).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		issue::seller_open(&seller)?;
		self.rewrite(ctx, invoice, None, move |lines| {
			let index = usize::try_from(line_no)
				.ok()
				.and_then(|n| n.checked_sub(1))
				.filter(|n| *n < lines.len())
				.ok_or(Error::NotFound)?;
			lines.remove(index);
			Ok(())
		})
		.await
	}

	pub async fn patch(&self, ctx: &Ctx, uid: &str, patch: &InvoicePatch) -> ClResult<Invoice> {
		check_notes(patch.notes.value().map(String::as_str))?;
		// Read here, not per branch: a non-owning org must get `NotFound` before any of the
		// answers below say anything about the invoice.
		let stored = self.invoice(ctx, uid).await?;
		let seller = self.seller_of_invoice(&stored).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		issue::seller_open(&seller)?;
		check_patch_dates(patch)?;
		if stored.status != InvoiceStatus::Draft {
			// §5.5: an issued invoice still takes a note; everything else is frozen, and
			// `E-INV-IMMUTABLE` says so. `E-INV-NOT-DRAFT` is the draft-only-route answer.
			// A `PENDING` draft is frozen for a different reason and says which: the edit is
			// legal again once the payment settles or dies.
			if patch_touches_more_than_notes(patch) {
				return Err(if stored.status == InvoiceStatus::Pending {
					locked()
				} else {
					conflict("E-INV-IMMUTABLE", "an issued invoice takes only a note")
				});
			}
			// `as_option`, not `value()`: `update_notes` is guard-free, so an all-absent body
			// collapsed to `None` and cleared a STORNO row's cancellation reason.
			let Some(notes) = patch.notes.as_option() else { return Ok(stored) };
			// A STORNO's `notes` is the statutory justification `storno::run` wrote there, not a
			// caller's memo. Clearing it is the one note edit an issued invoice does not take.
			if notes.is_none() && stored.kind == crate::store::InvoiceKind::Storno {
				return Err(conflict(
					"E-INV-IMMUTABLE",
					"a storno's cancellation reason cannot be cleared",
				));
			}
			// `pdf::run` short-circuits on an existing `invoice_documents` row and the PDF is
			// served `immutable, max-age=31536000`, so a note edit after rendering leaves the
			// statutory document disagreeing with the API forever.
			if self.store()?.invoice_document(stored.id).await?.is_some() {
				return Err(conflict(
					"E-INV-IMMUTABLE",
					"this invoice's document is rendered; its note is frozen",
				));
			}
			let updated = self
				.store()?
				.update_notes(stored.id, notes.map(String::as_str))
				.await?
				.ok_or(Error::NotFound)?;
			self.audit(ctx, "invoice", Some(updated.uid.as_str()), "PATCH").await;
			return Ok(updated);
		}
		// After the frozen-status answers: a non-draft says LOCKED/IMMUTABLE whatever the field.
		check_method(patch.payment_method)?;
		if patch.payment_method.unwrap_or(stored.payment_method) == PaymentMethod::Cash {
			check_today(
				"E-INV-CASH-DATES",
				[
					merged(&patch.fulfilment_date, stored.fulfilment_date.as_ref()),
					merged(&patch.due_date, stored.due_date.as_ref()),
				],
			)?;
		}
		// Over the resolved state: a period patched onto a draft that already carries a
		// fulfilment date conflicts just as one sent together with it.
		check_period(
			merged(&patch.period_start, stored.period_start.as_ref()),
			merged(&patch.period_end, stored.period_end.as_ref()),
			merged(&patch.fulfilment_date, stored.fulfilment_date.as_ref()),
		)?;
		// `rate_e6` is the draft's monetary basis and nothing downstream re-derives it:
		// `issue::freeze` copies whatever is on the row. Changing the rate means changing the
		// currency, which re-prices. (Unreachable over HTTP — the patch body has no such field.)
		if patch.rate_e6.is_some() && patch.currency.is_none() {
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-INV-RATE",
				"rate_e6 cannot be patched on its own; change the currency to re-price the draft",
			));
		}
		// Same shape, same reason: a discount is apportioned into each line's `discount_amount`
		// at pricing time, so moving it alone leaves `{net,vat,gross}` contradicting the row and
		// a value with no `discount_kind` makes the draft permanently unissuable.
		if patch.discount_value.is_some() && patch.currency.is_none() {
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-INV-DISCOUNT",
				"a discount cannot be patched on its own; it is re-priced with the draft",
			));
		}
		// A currency change moves every stored magnitude, so it goes through the conversion
		// rather than the plain update — `update_draft`'s bare `currency = COALESCE(...)`
		// would relabel an HUF draft as EUR with its lines still in fillér.
		if let Some(code) = patch.currency.clone() {
			return self.change_currency(ctx, uid, &code, patch).await;
		}
		// A moved fulfilment date is a new `rate_e6`, and a new rate re-prices every line, so
		// this routes through the conversion with the invoice's own code: re-stamping without
		// converting leaves the row claiming a rate its lines were never priced at.
		// Compared by code, not by `rate_e6 == 1_000_000`: a non-base currency published at
		// exactly 1.000000 took the `rewrite` branch, which leaves `rate_e6` stale by design.
		let base = CurrencyCode::parse(&self.app.settings.text("currency.base").await?)?;
		if !patch.fulfilment_date.is_undefined() && stored.currency != base {
			let code = stored.currency.clone();
			return self.change_currency(ctx, uid, &code, patch).await;
		}
		// A new billing party is a new VAT verdict and a new fulfilment date is a new HUF rate,
		// so both re-price the whole set. `update_draft` would leave the HUF trio on the old
		// day's rate, and a date with no rate at all would only fail at issue.
		if patch.billing_party_id.is_some() || !patch.fulfilment_date.is_undefined() {
			return self.rewrite(ctx, stored, Some(patch), |_| Ok(())).await;
		}
		let updated = self.store()?.update_draft(stored.id, patch).await?.ok_or_else(not_draft)?;
		self.audit(ctx, "invoice", Some(updated.uid.as_str()), "PATCH").await;
		Ok(updated)
	}

	/// The HTTP form of [`Invoices::patch`]: the wire sends a `billingPartyUid`, while
	/// [`InvoicePatch`] carries the internal `billing_party_id`. Resolving that is a
	/// org-scoped lookup, so it belongs here and not in a handler.
	///
	/// Translation only — it folds both arguments into the patch and hands it to
	/// [`Invoices::patch`], so there is one guard order and not two.
	pub async fn patch_by_uid(
		&self,
		ctx: &Ctx,
		uid: &str,
		party_uid: Option<&str>,
		currency: Option<&CurrencyCode>,
		patch: &InvoicePatch,
	) -> ClResult<Invoice> {
		let mut patch = patch.clone();
		if let Some(party_uid) = party_uid {
			let org = ctx.org()?;
			let party_uid = PartyId::parse(party_uid)?;
			let party =
				self.store()?.party_by_uid(org, &party_uid).await?.ok_or(Error::NotFound)?;
			patch.billing_party_id = Some(party.id);
		}
		if let Some(code) = currency {
			patch.currency = Some(code.clone());
		}
		self.patch(ctx, uid, &patch).await
	}

	/// Re-denominate a draft into `code`. Shared by [`Invoices::patch`] and
	/// [`Invoices::patch_by_uid`]: reaching it through either must give the same draft.
	async fn change_currency(
		&self,
		ctx: &Ctx,
		uid: &str,
		code: &CurrencyCode,
		patch: &InvoicePatch,
	) -> ClResult<Invoice> {
		let mut patch = patch.clone();
		// `invoice_lines.unit_price` and an `AMOUNT` discount are denominated in the *invoice*
		// currency, so a currency change is not a relabelling: every stored magnitude moves with
		// it, or the draft claims fillér figures are cents. Patch and re-price go together.
		let org = ctx.org()?;
		let invoice = self.invoice(ctx, uid).await?;
		let seller = self.seller_of_invoice(&invoice).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		issue::seller_open(&seller)?;
		// **The fulfilment date prices the change, not today** — as in `draft` and `rewrite`.
		// Resolving on today wrote a `rate_e6` that `invoices.rate_date` does not describe, and
		// left the lines priced at one rate with `exchangeRate` filed at another.
		let dated = match patch.fulfilment_date.as_option() {
			Some(d) => d.cloned(),
			None => invoice.fulfilment_date.clone(),
		};
		let priced_on = match dated {
			Some(d) => d,
			None => numbering::date_of(Timestamp::now())?,
		};
		let (cur, rate_e6) =
			draft::currency_for(&self.app, self.store()?.as_ref(), org, Some(code), &priced_on)
				.await?;
		let old_rate_e6 = invoice.rate_e6;
		patch.currency = Some(cur.code.clone());
		patch.rate_e6 = Some(rate_e6);

		// A **catalogue** line's stored magnitude carries the old currency's `fee_bp`, so
		// re-pricing without `to_base_unfeed` stacked a second markup and compounded on every
		// change. An **ad-hoc** line is the mirror image — stored verbatim with no markup, so
		// unfeeding it lost `fee_bp` each time. Ad-hoc lines and both discounts take the
		// fee-free pair: money in the invoice currency, never catalogue-priced.
		let old_cur = currency::get(self.store()?.as_ref(), &invoice.currency).await?;
		// When the currency does not change — `patch` calls in here on a fulfilment-date move,
		// purely to re-stamp `rate_e6` — the fee-free arm has nothing to do: those magnitudes
		// are already in the invoice currency, so a rate move must not touch them.
		let same_currency = cur.code == invoice.currency;
		let convert = move |m: Money, from_catalogue: bool| {
			if from_catalogue {
				// The catalogue arm still re-prices: its master price does live in the base
				// currency, which is why the date move calls in here.
				currency::price_in(
					currency::to_base_unfeed(m, &old_cur, old_rate_e6)?,
					&cur,
					rate_e6,
				)
			} else if same_currency {
				Ok(m)
			} else {
				currency::price_in_nofee(currency::to_base(m, old_rate_e6)?, &cur, rate_e6)
			}
		};
		if let Some(Discount::Amount(m)) = invoice_discount(&invoice)? {
			patch.discount_value = Some(convert(m, false)?.0);
		}
		self.rewrite(ctx, invoice, Some(&patch), move |lines| {
			for line in lines.iter_mut() {
				line.unit_price = convert(line.unit_price, line.service_id.is_some())?;
				if let Some(Discount::Amount(m)) = line.discount {
					line.discount = Some(Discount::Amount(convert(m, false)?));
				}
			}
			Ok(())
		})
		.await
	}

	/// Throw an unissued draft away. A `PENDING` one is refused with `E-INV-LOCKED`: the delete
	/// takes the zero `payment_allocations` link row with it, so a payment that then succeeded
	/// would land in `settle_full`'s "no invoice to settle" branch — charged and unallocated.
	pub async fn delete_draft(&self, ctx: &Ctx, uid: &str) -> ClResult<()> {
		let invoice = self.invoice(ctx, uid).await?;
		let seller = self.seller_of_invoice(&invoice).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		issue::seller_open(&seller)?;
		if invoice.status == InvoiceStatus::Pending {
			return Err(locked());
		}
		if !self.store()?.delete_draft(invoice.id).await? {
			return Err(not_draft());
		}
		self.audit(ctx, "invoice", Some(invoice.uid.as_str()), "DELETE").await;
		Ok(())
	}

	// ------------------------------------------------------------ lifecycle

	/// `DRAFT -> PENDING`: freeze a draft while a gateway holds a charge against its total.
	///
	/// `Ok(false)` when the invoice was not a `DRAFT` — an `ISSUED` invoice paid by card is
	/// already frozen and must keep its status, so that is a no-op and not an error. Called by
	/// the billing crate once the gateway has accepted the payment; locking before that would
	/// freeze a draft for a gateway that then refused.
	pub async fn lock(&self, ctx: &Ctx, uid: &str) -> ClResult<bool> {
		self.set_status(ctx, uid, InvoiceStatus::Draft, InvoiceStatus::Pending).await
	}

	/// [`Invoices::lock`] plus the `CARD` stamp: the gateway has accepted a charge for this
	/// draft's total. The only way `payment_method = CARD` is written — `draft`/`patch` refuse
	/// it — and deliberately not in the script bridge. `Ok(false)` exactly as `lock`: not a
	/// `DRAFT`, so nothing is stamped.
	///
	/// `409 E-INV-CARD-DATES` when the draft carries a fulfilment or due date other than
	/// today ([`check_card_dates`]). `saas_billing::allocate::start` refuses it before the
	/// gateway is called; this repeats it for a PATCH that raced the charge.
	pub async fn begin_card_payment(&self, ctx: &Ctx, uid: &str) -> ClResult<bool> {
		let invoice = self.invoice(ctx, uid).await?;
		let seller = self.seller_of_invoice(&invoice).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		issue::seller_open(&seller)?;
		if invoice.status != InvoiceStatus::Draft {
			return Ok(false);
		}
		check_card_dates(&invoice)?;
		let store = self.store()?;
		let card = InvoicePatch { payment_method: Some(PaymentMethod::Card), ..Default::default() };
		// Two writes, not one transaction — a crash between them leaves an unlocked CARD draft,
		// which `issue` refuses until re-paid or patched to TRANSFER.
		if store.update_draft(invoice.id, &card).await?.is_none() {
			return Ok(false);
		}
		let locked = store
			.set_status(invoice.id, InvoiceStatus::Draft, InvoiceStatus::Pending)
			.await?;
		self.audit(ctx, "invoice", Some(uid), "CARD-PAYMENT").await;
		Ok(locked)
	}

	/// `PENDING -> DRAFT`: the payment failed, was cancelled or expired, so the cart is editable
	/// again. This is what bounds the lock — Áfa tv. 163. §'s eight-day deadline runs from
	/// teljesítés, not from payment, so a lock with no way back is not an option.
	pub async fn unlock(&self, ctx: &Ctx, uid: &str) -> ClResult<bool> {
		self.set_status(ctx, uid, InvoiceStatus::Pending, InvoiceStatus::Draft).await
	}

	async fn set_status(
		&self,
		ctx: &Ctx,
		uid: &str,
		from: InvoiceStatus,
		to: InvoiceStatus,
	) -> ClResult<bool> {
		let invoice = self.invoice(ctx, uid).await?;
		// The lock is the seller's, as every other mutation here is: `ctx.org()` above scopes the
		// invoice to the buyer, which is not the org whose taxpayer id the document carries.
		let seller = self.seller_of_invoice(&invoice).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		// `unlock` stays open on a closed seller: releasing a dead payment is not a new document.
		if to == InvoiceStatus::Pending {
			issue::seller_open(&seller)?;
		}
		self.store()?.set_status(invoice.id, from, to).await
	}

	/// Issue a draft, locked or not. Idempotent on status: an already-`ISSUED` invoice returns
	/// unchanged. No step-up: issuing a draft is routine work, unlike [`Invoices::storno`].
	pub async fn issue(&self, ctx: &Ctx, uid: &str) -> ClResult<Invoice> {
		let invoice = self.invoice(ctx, uid).await?;
		self.require_seller_role(ctx, &self.seller_of_invoice(&invoice).await?, Role::Admin)
			.await?;
		// A CARD invoice is issued from its payment's own lock; unlocked, nothing has paid it.
		if invoice.payment_method == PaymentMethod::Card && invoice.status == InvoiceStatus::Draft {
			return Err(conflict(
				"E-INV-CARD-UNPAID",
				"a card invoice issues only once its payment opens",
			));
		}
		let was_open = matches!(invoice.status, InvoiceStatus::Draft | InvoiceStatus::Pending);
		let store = self.store()?;
		let issued = issue::run(&self.app, store.as_ref(), invoice).await?;
		self.try_audit(ctx, "invoice", Some(issued.uid.as_str()), "ISSUE").await?;
		if was_open {
			event::emit(&self.app, Event::InvoiceIssued { invoice: issued.uid.clone() });
		}
		Ok(issued)
	}

	/// Cancel an issued invoice, returning the STORNO counter-invoice.
	///
	/// **Step-up.** The gate lives here rather than in the handler: `routes::org_invoices`
	/// is the bundle most consumers leave unmounted, so a router-level gate would let the
	/// primary integration path file a legally binding NAV modification with no re-presented
	/// credential. `require_stepup` exempts `Actor::System`.
	pub async fn storno(&self, ctx: &Ctx, uid: &str, reason: &str) -> ClResult<Invoice> {
		saas_core::auth_mw::require_stepup(&self.app, ctx).await?;
		// `storno::run` stores this verbatim in the counter-invoice's `notes` column, so it
		// takes the same gate the other two `notes` paths do — and this one cannot be
		// corrected afterwards: the counter-invoice is numbered and immutable at creation.
		check_notes(Some(reason))?;
		let original = self.invoice(ctx, uid).await?;
		let seller = self.seller_of_invoice(&original).await?;
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		issue::seller_open(&seller)?;
		let store = self.store()?;
		let cancelled = storno::run(&self.app, store.as_ref(), &original, reason).await?;
		self.try_audit(ctx, "invoice", Some(cancelled.uid.as_str()), "STORNO").await?;
		Ok(cancelled)
	}

	/// `ISSUED -> PAID`, with the `audit_logs` row the bare store write never had.
	///
	/// Unrouted, and no longer the payment path: `BillingStore::settle` writes `PAID` from the
	/// allocation sum, in the transaction that wrote the allocation. What is left for this is
	/// an operator declaring an invoice paid with no `payments` row behind it at all.
	///
	/// There is deliberately no counterpart for `STORNOED`. [`Invoices::storno`] is the only
	/// way to reach it, because the status flip on its own leaves a cancelled invoice with no
	/// counter-invoice — which is not a state NAV accepts.
	pub async fn mark_paid(&self, ctx: &Ctx, uid: &str) -> ClResult<Invoice> {
		let invoice = self.invoice(ctx, uid).await?;
		self.require_seller_role(ctx, &self.seller_of_invoice(&invoice).await?, Role::Admin)
			.await?;
		require_transfer(&invoice)?;
		if !self.store()?.mark_paid(invoice.id).await? {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				"E-INV-IMMUTABLE",
				"only an ISSUED invoice can be marked paid",
			));
		}
		self.audit(ctx, "invoice", Some(uid), "PAID").await;
		self.invoice(ctx, uid).await
	}

	/// Record how much of an issued invoice has been paid, with the `audit_logs` row the bare
	/// store write never had. `paid_at` is stamped by the caller when `paid_amount` first
	/// reaches `gross`; `saas-billing` owns `payment_allocations` and recomputes the total
	/// from it, which is why this method takes the amount rather than deriving one.
	///
	/// Distinct from [`Invoices::mark_paid`]: this writes the amount, that flips the status.
	/// A partial payment is the case where one happens without the other.
	pub async fn set_paid(
		&self,
		ctx: &Ctx,
		uid: &str,
		paid_amount: Money,
		paid_at: Option<Timestamp>,
	) -> ClResult<Invoice> {
		// A `Money` past the envelope stored here is unreadable afterwards — `read_money`
		// rejects it, and this method reads the invoice first, so it cannot repair the row.
		let paid_amount = Money(bounded(paid_amount.0)?);
		let invoice = self.invoice(ctx, uid).await?;
		self.require_seller_role(ctx, &self.seller_of_invoice(&invoice).await?, Role::Admin)
			.await?;
		require_transfer(&invoice)?;
		if !self.store()?.set_paid(invoice.id, paid_amount, paid_at).await? {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				"E-INV-IMMUTABLE",
				"only an ISSUED or PAID invoice can take a payment",
			));
		}
		self.audit(ctx, "invoice", Some(uid), "PAY").await;
		self.invoice(ctx, uid).await
	}

	/// Draft, lines, VAT determination and issue in one call — what subscription renewal and
	/// the payment-succeeded job use, with no consumer in the loop. Deduped on `request_id`
	/// exactly as [`Invoices::draft`] is, so a retried checkout yields one invoice.
	pub async fn issue_now(&self, ctx: &Ctx, req: &NewDraft) -> ClResult<Invoice> {
		let invoice = self.draft(ctx, req).await?;
		// A retried `request_id` hands back the already-issued row; that must not emit twice.
		let was_open = matches!(invoice.status, InvoiceStatus::Draft | InvoiceStatus::Pending);
		let store = self.store()?;
		let issued = issue::run(&self.app, store.as_ref(), invoice).await?;
		self.try_audit(ctx, "invoice", Some(issued.uid.as_str()), "ISSUE").await?;
		if was_open {
			event::emit(&self.app, Event::InvoiceIssued { invoice: issued.uid.clone() });
		}
		Ok(issued)
	}
}

/// Every `invoices.notes` write, wherever it enters: `draft`, `patch`, and the storno `reason`,
/// which is stored in the same column.
fn check_notes(notes: Option<&str>) -> ClResult<()> {
	match notes {
		Some(v) => crate::store::bounded_multiline_text("notes", v, crate::store::MAX_NOTES),
		None => Ok(()),
	}
}

fn to_patch(value: Option<String>) -> Patch<String> {
	match value {
		Some(v) => Patch::Value(v),
		None => Patch::Undefined,
	}
}

/// This crate's condition alerts, registered by the application with
/// `AppBuilder::alerts(saas_invoice::service_api::alerts)`.
///
/// One code, `A-RATE-MISSING`. Nothing failed — `FETCH_RATES` succeeding while MNB publishes
/// no series for a newly enabled currency is the usual way here — so no job alert covers it;
/// the first sign would otherwise be `E-INV-NO-RATE` on somebody's issue attempt, which is
/// exactly when it is too late.
///
/// Probed per currency rather than in SQL, the way `issue` probes: the usable-rate rule spans
/// `currencies.mode` and two settings, which SQL cannot join against. Both of `issue`'s legs
/// are checked — the pricing rate against `currency.base`, and the HUF figure Áfa tv. 172. §
/// makes mandatory — so a non-HUF-base deployment flags its own base currency when the HUF
/// leg is missing.
///
/// # Errors
/// Propagates the settings and store reads; `Error::Internal` when no `InvoiceStore` was
/// registered.
pub async fn alerts(app: App) -> ClResult<Vec<Alert>> {
	let store = store(&app)?;
	let base = CurrencyCode::parse(&app.settings.text("currency.base").await?)?;
	let source = app.settings.text("currency.rate_source").await?;
	let max_age = app.settings.int("currency.max_rate_age_days").await?;
	let today = resolved_today()?;

	let mut broken = Vec::new();
	for cur in store.currency_list(false).await? {
		for target in [&CurrencyCode::huf(), &base] {
			let rate = currency::effective_rate_e6(
				store.as_ref(),
				&cur,
				target,
				&base,
				&source,
				&today,
				max_age,
			)
			.await;
			if rate.is_err() {
				broken.push(cur.code.clone());
				break;
			}
		}
	}
	if broken.is_empty() {
		return Ok(Vec::new());
	}
	Ok(vec![Alert {
		code: "A-RATE-MISSING",
		severity: Severity::Error,
		count: i64::try_from(broken.len()).unwrap_or(i64::MAX),
		message: format!(
			"no usable {source} rate for {} on {today}; issuing in {} will fail",
			broken.iter().map(CurrencyCode::as_str).collect::<Vec<_>>().join(", "),
			if broken.len() == 1 { "it" } else { "them" }
		),
		since: None,
		link: None,
	}])
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A ~2 MB `notes` — axum's default body limit — made `RENDER_PDF` exhaust its eight
	/// attempts, after which `Invoices::document` answers `E-INV-PDF-PENDING` forever for an
	/// invoice that is numbered, immutable and so beyond correction.
	#[test]
	fn notes_are_bounded_and_refuse_control_characters() {
		assert_eq!(
			check_notes(Some(&"x".repeat(crate::store::MAX_NOTES + 1)))
				.unwrap_err()
				.parts()
				.1,
			"E-INV-TOO-LONG"
		);
		assert_eq!(check_notes(Some("before\u{1}after")).unwrap_err().parts().1, "E-INV-BAD-TEXT");

		// Line breaks stay legal here, unlike every NAV-bound field: `notes` reaches no
		// `invoiceData` element, and an invoice note is naturally multi-line.
		check_notes(Some("first line\nsecond line")).unwrap();
		check_notes(Some("tab\there")).unwrap();
		check_notes(None).unwrap();
	}
}

// vim: ts=4
