// SPDX-License-Identifier: MPL-2.0
//! The persistence contract for `mintworks-invoice`, and the row types it moves.
//!
//! Implemented for `SqliteStore` in `adapters/store-sqlite/src/invoice.rs`. Public
//! identifiers (`InvoiceId`, `PartyId`, `ServiceId`) are the `uid` columns; the `i64`
//! arguments below are internal primary keys and never appear in a URL.
//!
//! **Issued invoices are immutable.** Every mutating method here either scopes its statement
//! `WHERE id = ? AND status = 'DRAFT'` or is one of the three permitted post-issue writes
//! ([`InvoiceStore::mark_paid`], [`InvoiceStore::mark_stornoed`], [`InvoiceStore::set_paid`]).
//! No trigger backs that up — the baseline migrations create none — so the predicate on each
//! write *is* the guarantee.

use async_trait::async_trait;
use std::fmt::Write as _;

use mintworks_core::error::StatusCode;
use mintworks_core::ids::SellerId;
use mintworks_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::currency::Currency;
use crate::draft::Priced;
use crate::vat::VatCode;
use crate::vies::ViesResult;

// ---------------------------------------------------------------- validation

/// NAV caps every text it accepts: `lineDescription` is `SimpleText512NotBlankType`,
/// `unitOfMeasureOwn` `SimpleText50`, `discountDescription` `SimpleText255`, `customerName`
/// `SimpleText512`. An over-long value is accepted here, issued onto a legally binding
/// invoice, and then rejected by the schema on every filing attempt — the same permanent
/// retry loop the blank-description guard exists to prevent, so bound it before the number
/// is allocated.
///
/// Characters, not bytes: the XSD counts characters, and a Hungarian address is not ASCII.
///
/// C0 control bytes are refused for the same reason the length is: `quick-xml`'s escaping
/// covers `< > & ' "` only, so a `\x01` pasted into a description passes straight into
/// `invoiceData`, which is then not well-formed XML. NAV answers a schema error, the
/// submission faults — and the invoice is already ISSUED and immutable, so it can never be
/// corrected.
pub fn bounded_text(field: &str, value: &str, max: usize) -> ClResult<()> {
	// `\n` is legal XML but not legal NAV: `SimpleText*NotBlankType`'s pattern is `.*[^\s].*`,
	// and XSD's `.` excludes #x0A/#x0D, so a multi-line description is rejected on an invoice
	// that already has a number. Tab is #x09, which `.` does match, so it stays legal.
	if value.chars().any(|c| c.is_control() && c != '\t') {
		return Err(Error::coded(
			StatusCode::BAD_REQUEST,
			"E-INV-BAD-TEXT",
			format!("{field} contains a control character or a line break"),
		));
	}
	if value.chars().count() > max {
		return Err(Error::coded(
			StatusCode::BAD_REQUEST,
			"E-INV-TOO-LONG",
			format!("{field} is longer than {max} characters"),
		));
	}
	Ok(())
}

/// [`bounded_text`] minus the line-break rule, for free text that never reaches NAV.
///
/// `invoices.notes` is the only such field: it is rendered into the PDF (`pdf::document`
/// decodes it as a string — there is no typst injection) and appears in no `invoiceData`
/// element, so a line break in it is a formatting choice rather than a filing failure. Every
/// other control character is refused exactly as above, and the ceiling is the point: a
/// ~2 MB `notes` — axum's default body limit — made `RENDER_PDF` exhaust its eight attempts,
/// after which `Invoices::document` answers `E-INV-PDF-PENDING` forever for an invoice that
/// is already numbered and immutable and so can never be corrected.
pub fn bounded_multiline_text(field: &str, value: &str, max: usize) -> ClResult<()> {
	let one_line: String =
		value.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
	bounded_text(field, &one_line, max)
}

/// Anything that is not `[A-Za-z0-9._-]` becomes `-`, and the result is capped at 64.
///
/// For the one place a DB-sourced string becomes a `Content-Disposition: attachment;
/// filename="…"`: `invoices.number` is built from `doc_series.format` and
/// `sellers.series_code`, so a `"` corrupts the header and a CR/LF fails `HeaderValue`
/// conversion outright, turning a download into a 500.
pub fn safe_filename_part(s: &str) -> String {
	let mut out: String = s
		.chars()
		.map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
		.collect();
	out.truncate(64);
	out
}

/// `A2026-000123-<sha256>.pdf`. The hash is in the name because it is the value filed with NAV
/// as `electronicInvoiceHash`: a recipient can check the file against what NAV holds without
/// opening it. Full 64 hex characters — a truncated hash proves nothing against NAV's copy.
pub fn pdf_filename(number: Option<&str>, uid: &str, sha256: &str) -> String {
	format!("{}-{}.pdf", safe_filename_part(number.unwrap_or(uid)), safe_filename_part(sha256))
}

/// The NAV caps, named where they are enforced.
pub const MAX_DESCRIPTION: usize = 512;
pub const MAX_UNIT: usize = 50;
pub const MAX_DISCOUNT_DESCRIPTION: usize = 255;
pub const MAX_PARTY_NAME: usize = 512;
/// NAV `SimpleText255NotBlankType`, which `city`, `streetName` and `additionalAddressDetail`
/// all are (`crates/nav/xsd/invoiceBase.xsd`). Narrower than [`MAX_PARTY_NAME`], which
/// stays 512 because the name field is not that type.
pub const MAX_ADDRESS_TEXT: usize = 255;
/// `invoices.notes` and the storno `reason`, which is stored in the same column. Not a NAV
/// cap — the column reaches no `invoiceData` element — so it takes the widest one this module
/// already enforces rather than a number invented for it. See [`bounded_multiline_text`].
pub const MAX_NOTES: usize = MAX_DESCRIPTION;
/// Not a NAV cap — `note` reaches no `invoiceData` element. The ceiling is what keeps an
/// unrenderable PDF out of an immutable invoice: `mintworks_nav::job::report` refuses to file one
/// with no `invoice_documents` row, so a note that breaks `RENDER_PDF` also blocks the filing,
/// on a row that can never be corrected.
pub const MAX_LINE_NOTE: usize = 512;

/// NAV `common:SimpleText50NotBlankType`, the `thirdStateTaxId` element a non-HU tax number
/// is filed as.
pub const MAX_THIRD_STATE_TAX_ID: usize = 50;
/// `sellers.series_code`, which `numbering::render_number` puts in front of the year and
/// counter to form `invoiceNumber` — NAV's `common:SimpleText50NotBlankType`. Capped well
/// under 50 so the rest of `doc_series.format` (year, separator, a padded counter) cannot push
/// the rendered number past the type.
pub const MAX_SERIES_CODE: usize = 38;

/// The series named at issue is not one of the seller's `seller_series` rows.
pub fn unknown_series() -> Error {
	Error::coded(
		StatusCode::BAD_REQUEST,
		"E-INV-SERIES-UNKNOWN",
		"the seller has no numbering series with this code",
	)
}

/// The default series can be neither deleted nor unset: a seller always has one.
pub fn default_series_locked() -> Error {
	Error::coded(
		StatusCode::CONFLICT,
		"E-INV-SERIES-DEFAULT",
		"the default series cannot be deleted or unset; make another series the default first",
	)
}

/// A series some invoice, draft included, carries cannot be deleted.
pub fn series_used_error() -> Error {
	Error::coded(StatusCode::CONFLICT, "E-INV-SERIES-IN-USE", "an invoice uses this series")
}
/// `base:TaxNumberType` splits `base:taxpayerId` off the first 8 digits, so anything shorter
/// cannot be filed at all — `mintworks_nav::xml::Xml::tax_number` raises it, which is after the
/// number has been allocated and the invoice is immutable.
pub const MIN_TAX_NUMBER_DIGITS: usize = 8;

/// NAV `PostalCodeType` (`crates/nav/xsd/common.xsd`): `[A-Z0-9][A-Z0-9\s\-]{1,8}[A-Z0-9]`,
/// so 3 to 10 characters. Returns the uppercased form rather than rejecting `"sw1a 1aa"` — a
/// postcode is not case-bearing data. A length bound alone let a one-character or lowercase
/// postcode onto a numbered invoice that NAV then refuses on every attempt.
pub fn checked_postcode(value: &str) -> ClResult<String> {
	let up = value.trim().to_uppercase();
	let chars: Vec<char> = up.chars().collect();
	let edge = |c: &char| c.is_ascii_uppercase() || c.is_ascii_digit();
	let ok = (3..=10).contains(&chars.len())
		&& chars.first().is_some_and(edge)
		&& chars.last().is_some_and(edge)
		&& chars[1..chars.len() - 1].iter().all(|c| edge(c) || *c == ' ' || *c == '-');
	if !ok {
		return Err(Error::coded(
			StatusCode::BAD_REQUEST,
			"E-INV-BAD-TEXT",
			"postcode must be 3-10 characters of A-Z, 0-9, space or hyphen, \
			 starting and ending alphanumeric",
		));
	}
	Ok(up)
}

// ---------------------------------------------------------------- enums

/// `billing_parties.kind` and the frozen `invoices.buyer_kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartyKind {
	#[serde(rename = "P")]
	Person,
	#[serde(rename = "C")]
	Company,
}

/// `invoices.kind`. `MODIFY` is not in v1; `modification_index` is the column that awaits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum InvoiceKind {
	Normal,
	Storno,
}

/// `invoices.status`.
///
/// `Pending` is a draft a gateway payment has locked: still unnumbered and never filed at NAV,
/// but immutable, because the gateway is charging the total the draft had when the payment
/// opened. It settles to `Issued` or, when the payment fails, unwinds to `Draft`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum InvoiceStatus {
	Draft,
	Pending,
	Issued,
	Paid,
	Stornoed,
}

/// `invoices.payment_method`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PaymentMethod {
	Transfer,
	Card,
	Cash,
	Other,
}

/// `currency_rates.source` and the frozen `invoices.rate_source`. Kept in the rate primary
/// key rather than overwritten: the legally usable source depends on an election filed with
/// NAV, and that election can change while historical invoices must keep resolving.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum RateSource {
	Mnb,
	Ecb,
	Bank,
	Manual,
}

/// `invoice_lines.discount_kind`. The resolved figure is `discount_amount`; these two columns
/// keep what the caller asked for, which the PDF prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DiscountKind {
	Amount,
	Percent,
}

mintworks_core::str_enum!(PartyKind {
	Person => "P",
	Company => "C",
});

mintworks_core::str_enum!(InvoiceKind {
	Normal => "NORMAL",
	Storno => "STORNO",
});

mintworks_core::str_enum!(InvoiceStatus {
	Draft => "DRAFT",
	Pending => "PENDING",
	Issued => "ISSUED",
	Paid => "PAID",
	Stornoed => "STORNOED",
});

mintworks_core::str_enum!(PaymentMethod {
	Transfer => "TRANSFER",
	Card => "CARD",
	Cash => "CASH",
	Other => "OTHER",
});

mintworks_core::str_enum!(RateSource {
	Mnb => "MNB",
	Ecb => "ECB",
	Bank => "BANK",
	Manual => "MANUAL",
});

mintworks_core::str_enum!(DiscountKind {
	Amount => "AMOUNT",
	Percent => "PERCENT",
});

/// `seller_versions.status`. `Draft` is the only mutable row, and at most one of `Draft` and
/// one of `Current` exists per seller — both partial unique indexes in the schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SellerVersionStatus {
	Draft,
	Current,
	Archived,
}

mintworks_core::str_enum!(SellerVersionStatus {
	Draft => "DRAFT",
	Current => "CURRENT",
	Archived => "ARCHIVED",
});

// ---------------------------------------------------------------- rows

/// A `sellers` row — the **operational** half: what identifies the seller to the numbering
/// series and to NAV. The statutory supplier data lives in [`SellerVersion`], which is
/// versioned and frozen onto every invoice at ISSUE; this half is deliberately not, because a
/// redriven filing must reach today's endpoint under today's technical user.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Seller {
	pub id: i64,
	pub uid: SellerId,
	/// The org that owns the seller. Granted, never claimed: see [`InvoiceStore::put_seller`].
	pub org_id: i64,
	/// Authoritative when non-blank; blank falls back to `settings['nav.base_url']`, then to the
	/// system `settings['deployment.env']` names.
	pub nav_base_url: String,
	pub nav_login: Option<String>,
	pub series_code: String,
	/// Read-only since: only payments may still be recorded. Never written by
	/// [`InvoiceStore::put_seller`], only by [`InvoiceStore::set_seller_closed`].
	pub closed_at: Option<Timestamp>,
	/// The org's own payment term; `None` inherits `settings['invoice.default_payment_days']`.
	/// Written only by [`InvoiceStore::set_seller_payment_days`].
	pub payment_days: Option<i64>,
	/// Test mode: issued invoices number under [`SERIES_KIND_TEST`] and are never reported to
	/// NAV. Set on insert by [`InvoiceStore::put_seller`] / [`InvoiceStore::create_seller`], then
	/// written only by [`InvoiceStore::set_seller_test_mode`]: a boot re-put must not undo go-live.
	pub test_mode: bool,
	pub created_at: Timestamp,
}

/// The `doc_series.kind` a live invoice numbers under.
pub const SERIES_KIND_LIVE: &str = "INVOICE";
/// The `doc_series.kind` a test-mode invoice numbers under, so live numbering stays gapless.
pub const SERIES_KIND_TEST: &str = "INVOICE_TEST";

/// The `doc_series.kind` for an invoice frozen with this `test_mode`.
#[must_use]
pub const fn series_kind(test_mode: bool) -> &'static str {
	if test_mode { SERIES_KIND_TEST } else { SERIES_KIND_LIVE }
}

/// Prefix of every [`SERIES_KIND_TEST`] number: `idx_invoice_number` is unique per seller, not per
/// kind, so a test invoice must never render the same number as a live one.
pub const TEST_NUMBER_PREFIX: &str = "TESZT-";

/// [`render_number`] for a `doc_series.kind`; test-kind numbers carry [`TEST_NUMBER_PREFIX`].
#[must_use]
pub fn render_kind_number(kind: &str, format: &str, code: &str, year: i64, no: i64) -> String {
	let number = render_number(format, code, year, no);
	if kind == SERIES_KIND_TEST { format!("{TEST_NUMBER_PREFIX}{number}") } else { number }
}

/// A `seller_series` row: one numbering series of a seller. The code is the number prefix
/// (`{code}` in `doc_series.format`). Exactly one row per seller is the default, and
/// [`Seller::series_code`] always equals its code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SellerSeries {
	#[serde(skip)]
	pub seller_id: i64,
	pub code: String,
	/// A `blobs` uid: the series' own logo.
	pub logo_blob: Option<String>,
	pub is_default: bool,
	pub created_at: Timestamp,
}

/// The writable half of a `seller_series` row. `is_default: Some(true)` promotes the series,
/// demoting the old default and rewriting `sellers.series_code`; `Some(false)` on the default
/// is refused, because a seller always has one.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesPatch {
	#[serde(default)]
	pub logo_blob: Patch<String>,
	pub is_default: Option<bool>,
}

/// The seller's statutory data as of one point in time — a `seller_versions` row. An invoice
/// freezes the id of the version that was [`SellerVersionStatus::Current`] at ISSUE, so a
/// later edit never changes what an issued invoice prints or files.
///
/// The operational half (`nav_base_url`, `nav_login`, `series_code`) is on [`Seller`] and is
/// deliberately *not* versioned: a redrive must reach today's NAV endpoint, not the one that
/// was configured when the invoice was issued.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SellerVersion {
	pub seller_ver: i64,
	pub seller_id: i64,
	pub status: SellerVersionStatus,
	pub name: String,
	pub country: String,
	pub tax_number: String,
	pub group_member_tax_no: Option<String>,
	pub eu_vat_id: Option<String>,
	pub postcode: String,
	pub city: String,
	pub street: String,
	pub bank_account: Option<String>,
	pub bank_name: Option<String>,
	pub small_business: bool,
	/// Pénzforgalmi elszámolás (Áfa tv. 196/B. §). Only with `vat_scheme == "NORMAL"`.
	pub cash_accounting: bool,
	/// `NORMAL` or `ALANYI_MENTES` — the VAT side only; the income-tax regime is separate.
	pub vat_scheme: String,
	/// `NONE`, `KATA` or `ATALANY`. Never read by the VAT engine.
	pub income_regime: String,
	/// The átalányadó költséghányad; `None` means the year's general rate. Only with `ATALANY`.
	pub expense_ratio_pct: Option<i64>,
	/// `YYYY-MM-DD` the current tax status started, when that was mid-year.
	pub regime_since: Option<String>,
	/// Branding: the contact email printed on documents, the `#rrggbb` accent colour and a
	/// `blobs` uid for the logo. All optional.
	pub email: Option<String>,
	pub accent: Option<String>,
	pub logo_blob: Option<String>,
	pub created_at: Timestamp,
	/// When the version was published; `None` while it is still a draft.
	pub valid_from: Option<Timestamp>,
	/// When the next version was published over it.
	pub superseded_at: Option<Timestamp>,
}

/// The writable half of a `seller_versions` row — what a draft edit carries. `Patch` fields
/// distinguish "absent" from "explicitly cleared", the same three-state shape as
/// [`PartyPatch`].
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SellerVersionPatch {
	pub name: Option<String>,
	pub country: Option<String>,
	pub tax_number: Option<String>,
	#[serde(default)]
	pub group_member_tax_no: Patch<String>,
	#[serde(default)]
	pub eu_vat_id: Patch<String>,
	pub postcode: Option<String>,
	pub city: Option<String>,
	pub street: Option<String>,
	#[serde(default)]
	pub bank_account: Patch<String>,
	#[serde(default)]
	pub bank_name: Patch<String>,
	pub small_business: Option<bool>,
	pub cash_accounting: Option<bool>,
	pub vat_scheme: Option<String>,
	pub income_regime: Option<String>,
	#[serde(default)]
	pub expense_ratio_pct: Patch<i64>,
	#[serde(default)]
	pub regime_since: Patch<String>,
	#[serde(default)]
	pub email: Patch<String>,
	#[serde(default)]
	pub accent: Patch<String>,
	#[serde(default)]
	pub logo_blob: Patch<String>,
}

impl SellerVersionPatch {
	/// The row a draft save writes: `base` with this patch applied. An absent field keeps
	/// `base`'s value, a `Patch::Null` clears one.
	///
	/// `base` is the open draft when there is one, otherwise the `CURRENT` version — and
	/// `None` on a fresh install, where the required text fields start empty. Nothing here
	/// refuses that: `Invoices::publish_seller` is what will not make a blank row live, so a
	/// half-filled draft can be saved and finished later.
	///
	/// The identity fields (`seller_ver`, `seller_id`, `status`, and the three timestamps) are
	/// the store's to set; whatever they hold here is ignored by it.
	#[must_use]
	pub fn merged(&self, base: Option<&SellerVersion>) -> SellerVersion {
		let text = |patch: Option<&String>, base: Option<&String>| {
			patch.or(base).cloned().unwrap_or_default()
		};
		let opt = |patch: &Patch<String>, base: Option<&String>| match patch.as_option() {
			Some(v) => v.cloned(),
			None => base.cloned(),
		};
		let income_regime = self
			.income_regime
			.clone()
			.or_else(|| base.map(|b| b.income_regime.clone()))
			.unwrap_or_else(|| "NONE".to_owned());
		SellerVersion {
			seller_ver: 0,
			seller_id: base.map_or(0, |b| b.seller_id),
			status: SellerVersionStatus::Draft,
			name: text(self.name.as_ref(), base.map(|b| &b.name)),
			country: self
				.country
				.clone()
				.or_else(|| base.map(|b| b.country.clone()))
				.unwrap_or_else(|| "HU".to_owned()),
			tax_number: text(self.tax_number.as_ref(), base.map(|b| &b.tax_number)),
			group_member_tax_no: opt(
				&self.group_member_tax_no,
				base.and_then(|b| b.group_member_tax_no.as_ref()),
			),
			eu_vat_id: opt(&self.eu_vat_id, base.and_then(|b| b.eu_vat_id.as_ref())),
			postcode: text(self.postcode.as_ref(), base.map(|b| &b.postcode)),
			city: text(self.city.as_ref(), base.map(|b| &b.city)),
			street: text(self.street.as_ref(), base.map(|b| &b.street)),
			bank_account: opt(&self.bank_account, base.and_then(|b| b.bank_account.as_ref())),
			bank_name: opt(&self.bank_name, base.and_then(|b| b.bank_name.as_ref())),
			small_business: self.small_business.or(base.map(|b| b.small_business)).unwrap_or(false),
			cash_accounting: self
				.cash_accounting
				.or(base.map(|b| b.cash_accounting))
				.unwrap_or(false),
			vat_scheme: self
				.vat_scheme
				.clone()
				.or_else(|| base.map(|b| b.vat_scheme.clone()))
				.unwrap_or_else(|| "NORMAL".to_owned()),
			income_regime: income_regime.clone(),
			// Leaving átalány drops its ratio, so a later switch back starts from the general rate.
			expense_ratio_pct: if income_regime == "ATALANY" {
				match self.expense_ratio_pct.as_option() {
					Some(v) => v.copied(),
					None => base.and_then(|b| b.expense_ratio_pct),
				}
			} else {
				None
			},
			regime_since: opt(&self.regime_since, base.and_then(|b| b.regime_since.as_ref())),
			email: opt(&self.email, base.and_then(|b| b.email.as_ref())),
			accent: opt(&self.accent, base.and_then(|b| b.accent.as_ref())),
			logo_blob: opt(&self.logo_blob, base.and_then(|b| b.logo_blob.as_ref())),
			created_at: base.map_or_else(Timestamp::now, |b| b.created_at),
			valid_from: None,
			superseded_at: None,
		}
	}
}

/// A `billing_parties` row.
///
/// The VIES verdict is not here: it lives in `vies_checks` keyed by EU VAT id, not on the
/// party row, and reading it would be a join no store method offers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingParty {
	#[serde(skip)]
	pub id: i64,
	pub uid: PartyId,
	#[serde(skip)]
	pub org_id: i64,
	pub kind: PartyKind,
	pub name: String,
	pub country: String,
	pub tax_number: Option<String>,
	pub eu_vat_id: Option<String>,
	pub group_tax_no: Option<String>,
	pub postcode: Option<String>,
	pub city: Option<String>,
	pub street: Option<String>,
	/// Always `emails[0]`, `None` when the list is empty.
	pub email: Option<String>,
	/// Every address, primary first; at most 10, case-insensitively distinct.
	pub emails: Vec<String>,
	/// The currency a new draft for this party opens in; `None` falls back to the org's.
	pub currency: Option<CurrencyCode>,
	pub is_default: bool,
	/// Overrides the seller's payment term; `None` inherits it.
	pub payment_days: Option<i64>,
	/// The method a new draft for this party starts with: `TRANSFER` or `CASH` only.
	pub payment_method: Option<PaymentMethod>,
	/// The language this party's documents print in: `hu`, `en` or `hu-en`. `None` derives it
	/// from `country` (HU -> `hu`, else `en`).
	pub doc_lang: Option<String>,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
}

/// A [`BillingParty`] as the paged list returns it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedParty {
	#[serde(flatten)]
	pub party: BillingParty,
	/// [`InvoiceStore::open_receivables`], one entry per currency; empty when nothing is owed.
	pub open_receivable: Vec<MoneyWire>,
}

/// The writable half of a `billing_parties` row, and the one body both POST and PATCH take.
/// `Patch` fields distinguish "absent" from "explicitly cleared", so a PATCH body can null a
/// tax number without nulling the address. On POST the store rejects a row without `kind`,
/// `name` and `country`, so nothing validates those twice.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartyPatch {
	pub kind: Option<PartyKind>,
	pub name: Option<String>,
	pub country: Option<String>,
	#[serde(default)]
	pub tax_number: Patch<String>,
	#[serde(default)]
	pub eu_vat_id: Patch<String>,
	#[serde(default)]
	pub group_tax_no: Patch<String>,
	#[serde(default)]
	pub postcode: Patch<String>,
	#[serde(default)]
	pub city: Patch<String>,
	#[serde(default)]
	pub street: Patch<String>,
	#[serde(default)]
	pub email: Patch<String>,
	/// Replaces the whole list and sets `email` to its head. `email` alone replaces the head
	/// and keeps the tail (`Invoices::update_party`).
	#[serde(default)]
	pub emails: Patch<Vec<String>>,
	/// Must name an enabled currency.
	#[serde(default)]
	pub currency: Patch<CurrencyCode>,
	pub is_default: Option<bool>,
	#[serde(default)]
	pub payment_days: Patch<i64>,
	#[serde(default)]
	pub payment_method: Patch<PaymentMethod>,
	#[serde(default)]
	pub doc_lang: Patch<String>,
}

impl PartyPatch {
	/// `emails` as the column's JSON text; absent or null is `[]`.
	pub fn emails_json(&self) -> String {
		serde_json::to_string(self.emails.value().map_or(&[][..], Vec::as_slice))
			.unwrap_or_else(|_| "[]".into())
	}
}

/// `billing_parties.emails` read back.
pub fn parse_emails(raw: &str) -> ClResult<Vec<String>> {
	serde_json::from_str(raw).map_err(|e| Error::internal(format!("billing_parties.emails: {e}")))
}

/// A `services` row. `unit_price` is in the base currency; `vat_code` is only the default,
/// because [`crate::taxrule::determine`] can override it for the buyer's zone.
#[derive(Clone, Debug)]
pub struct Service {
	pub id: i64,
	pub uid: ServiceId,
	pub org_id: i64,
	pub code: Option<String>,
	pub name: String,
	pub description: Option<String>,
	pub unit: String,
	pub unit_price: Money,
	pub vat_code: VatCode,
	pub active: bool,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
	/// The line text on an `en` document; `None` falls back to `name`.
	pub name_en: Option<String>,
	/// Prices in non-base currencies, ordered by currency; see [`ServicePrice`].
	pub prices: Vec<ServicePrice>,
}

/// A `service_prices` row: the service's own price in `currency`, in that currency's minor
/// units. Drafting in `currency` bills it as is — no rate, no `fee_bp`. Never the base currency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServicePrice {
	pub currency: CurrencyCode,
	pub unit_price: Money,
}

/// An `org_currencies` row: whether a catalogue line with no own `code` price may convert the
/// base price into `code`, and at which fee. `fee_bp: None` = the currency's own `fee_bp`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgCurrency {
	pub code: CurrencyCode,
	pub convert: bool,
	pub fee_bp: Option<i64>,
}

/// One service as the consumer declares it in code, for [`InvoiceStore::sync_services`].
/// `code` is the identity, so renaming a plan is an update and never a second row.
#[derive(Clone, Debug)]
pub struct ServiceDef {
	pub code: String,
	pub name: String,
	pub description: Option<String>,
	pub unit: String,
	pub unit_price: Money,
	pub vat_code: VatCode,
	pub name_en: Option<String>,
	/// Replace-all when `Some`; `None` keeps the stored prices (`sync_services`) or sets none.
	pub prices: Option<Vec<ServicePrice>>,
}

/// The writable half of a `services` row.
#[derive(Clone, Debug, Default)]
pub struct ServicePatch {
	pub code: Patch<String>,
	pub name: Option<String>,
	pub description: Patch<String>,
	pub unit: Option<String>,
	pub unit_price: Option<Money>,
	pub vat_code: Option<VatCode>,
	pub active: Option<bool>,
	pub name_en: Patch<String>,
	/// Replace-all when `Some`.
	pub prices: Option<Vec<ServicePrice>>,
}

/// One row of [`InvoiceStore::list_invoices_page`]: the invoice plus the uids its internal
/// keys stand for, which is everything a listing shows.
#[derive(Clone, Debug)]
pub struct ListedInvoice {
	pub invoice: Invoice,
	pub party_uid: Option<PartyId>,
	pub original_invoice_uid: Option<InvoiceId>,
	pub storno_invoice_uid: Option<InvoiceId>,
}

/// What narrows an invoice listing. `Default` is "everything", which is what every caller that
/// does not filter passes.
#[derive(Clone, Debug, Default)]
pub struct InvoiceFilter {
	/// Empty means any status. A list rather than one value: a UI's "open" view is
	/// `ISSUED + PENDING`.
	pub statuses: Vec<InvoiceStatus>,
	/// Empty means any. Non-empty also narrows to live `ISSUED`/`PAID`/`STORNOED` invoices, and
	/// is AND'd with `statuses`.
	pub nav_states: Vec<NavState>,
	/// Free text, already trimmed and non-empty. Matched against the invoice number, the frozen
	/// `buyer_name`, and the linked party's name -- the last because a DRAFT has no buyer
	/// snapshot yet and would otherwise be unfindable by who it is for -- and both tax numbers.
	pub q: Option<String>,
	/// [`tax_stem`] of `q`: a prefix match on every tax column, the party's and the snapshot's.
	pub tax_stem: Option<String>,
	/// `issued_at >= from`; local midnight of the wire date. Set, it excludes every draft.
	pub from: Option<Timestamp>,
	/// `issued_at < to`; the local midnight *after* the wire date, so `to` is inclusive.
	pub to: Option<Timestamp>,
	/// The linked billing party. The service checks it exists in the caller's org.
	pub party: Option<PartyId>,
	/// `None` is `id DESC` under the uid cursor; `Some` pages by `offset` instead.
	pub sort: Option<InvoiceSort>,
	/// Rows skipped; `None` with a cursor, never both (the service refuses it).
	pub offset: Option<i64>,
}

/// The invoice list's NAV filter tags (`NAV_ERROR`, `NAV_PENDING`, `NAV_OK`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavState {
	Rejected,
	Pending,
	Accepted,
}

mintworks_core::str_enum!(NavState {
	Rejected => "NAV_ERROR",
	Pending => "NAV_PENDING",
	Accepted => "NAV_OK",
});

impl NavState {
	/// The verdicts of the invoice's newest `nav_submissions` row (by `id`, resolved or not --
	/// what `Nav::filing` shows) this state stands for; `""` is no row or a clean NULL verdict,
	/// `"FAULT"` a NULL verdict with an `error_code`, which both adapters' filter spell so.
	pub fn verdicts(self) -> &'static [&'static str] {
		match self {
			Self::Rejected => &["REJECTED", "FAILED", "FAULT"],
			Self::Pending => &[""],
			Self::Accepted => &["DONE", "WARN"],
		}
	}
}

/// `issueDate`, `gross`, `dueDate`, `partner` (`COALESCE(buyer_name, party name)`). NULL keys
/// sort last in both directions; ties break on `id DESC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvoiceSortKey {
	IssueDate,
	Gross,
	DueDate,
	Partner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvoiceSort {
	pub key: InvoiceSortKey,
	pub desc: bool,
}

impl InvoiceSort {
	/// A key name, `-` first for descending.
	pub fn parse(s: &str) -> ClResult<Self> {
		let (key, desc) = parse_sort(s, |name| match name {
			"issueDate" => Some(InvoiceSortKey::IssueDate),
			"gross" => Some(InvoiceSortKey::Gross),
			"dueDate" => Some(InvoiceSortKey::DueDate),
			"partner" => Some(InvoiceSortKey::Partner),
			_ => None,
		})?;
		Ok(Self { key, desc })
	}
}

fn field_error(field: &str, msg: String) -> Error {
	let mut fields = FieldErrors::new();
	fields.insert(field.to_owned(), E_FORMAT);
	Error::ValidationFields(msg, fields)
}

/// A key name, `-` first for descending.
fn parse_sort<K>(s: &str, key: impl FnOnce(&str) -> Option<K>) -> ClResult<(K, bool)> {
	let (desc, name) = s.strip_prefix('-').map_or((false, s), |n| (true, n));
	let key = key(name).ok_or_else(|| field_error("sort", format!("unknown sort key: {s}")))?;
	Ok((key, desc))
}

/// The 8-digit stem of a typed tax number: `-` and spaces dropped, then all digits and at least
/// 8 long. Tax numbers are stored bare; the stem also finds a VAT group through a member's number.
pub fn tax_stem(q: &str) -> Option<String> {
	let d: String = q.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
	(d.len() >= 8 && d.bytes().all(|b| b.is_ascii_digit())).then(|| d[..8].to_owned())
}

/// `name` (the default), `taxNumber`, `createdAt`. NULL keys sort last; ties break on `id`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PartySortKey {
	#[default]
	Name,
	TaxNumber,
	CreatedAt,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PartySort {
	pub key: PartySortKey,
	pub desc: bool,
}

impl PartySort {
	/// A key name, `-` first for descending.
	pub fn parse(s: &str) -> ClResult<Self> {
		let (key, desc) = parse_sort(s, |name| match name {
			"name" => Some(PartySortKey::Name),
			"taxNumber" => Some(PartySortKey::TaxNumber),
			"createdAt" => Some(PartySortKey::CreatedAt),
			_ => None,
		})?;
		Ok(Self { key, desc })
	}
}

/// What narrows [`InvoiceStore::list_parties_page`]. `q` matches name, tax number, EU VAT id
/// and any of `emails`; [`tax_stem`] of it also prefix-matches `tax_number` and `group_tax_no`.
#[derive(Clone, Debug)]
pub struct PartyFilter {
	pub q: Option<String>,
	pub tax_stem: Option<String>,
	pub sort: PartySort,
	/// Range-checked by the service, as is `limit`.
	pub offset: i64,
	pub limit: i64,
}

impl PartyFilter {
	/// The wire spelling, shared by the route and the Rune binding. Blank is absent; `limit`
	/// defaults to the old unpaged cap, so a caller wanting "all" still gets it.
	pub fn parse(
		q: Option<&str>,
		sort: Option<&str>,
		offset: Option<i64>,
		limit: Option<i64>,
	) -> ClResult<Self> {
		let q = q.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
		let sort = sort.map(str::trim).filter(|s| !s.is_empty()).map(PartySort::parse);
		Ok(Self {
			tax_stem: q.as_deref().and_then(tax_stem),
			q,
			sort: sort.transpose()?.unwrap_or_default(),
			offset: offset.unwrap_or(0),
			limit: limit.unwrap_or(crate::service_api::MAX_PAGE_LIMIT),
		})
	}
}

impl InvoiceFilter {
	/// Parses the wire spelling: `status` a comma-separated list of [`InvoiceStatus`] and
	/// [`NavState`] tags, `q` free text. Blank is absent, as everywhere else here.
	///
	/// On the type rather than in the route handler because the Rune binding decodes the same
	/// two strings out of a script object.
	pub fn parse(status: Option<&str>, q: Option<&str>) -> ClResult<Self> {
		let mut f = Self::default();
		for name in status.unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()) {
			if let Ok(nav) = name.parse::<NavState>() {
				if !f.nav_states.contains(&nav) {
					f.nav_states.push(nav);
				}
				continue;
			}
			// `str_enum!`'s `FromStr` reports an unknown tag as `Internal`, which is right for a
			// column value and wrong for request text.
			let st: InvoiceStatus = name
				.parse()
				.map_err(|_| field_error("status", format!("unknown invoice status: {name}")))?;
			if !f.statuses.contains(&st) {
				f.statuses.push(st);
			}
		}
		f.q = q.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
		f.tax_stem = f.q.as_deref().and_then(tax_stem);
		Ok(f)
	}

	/// The listing half of the wire spelling, after [`InvoiceFilter::parse`]: `sort` per
	/// [`InvoiceSort::parse`], `from`/`to` inclusive `YYYY-MM-DD` issue dates in local time,
	/// `party` a `prt_` uid. Blank is absent. `offset` is range-checked by the service.
	pub fn narrow(
		mut self,
		sort: Option<&str>,
		offset: Option<i64>,
		from: Option<&str>,
		to: Option<&str>,
		party: Option<&str>,
	) -> ClResult<Self> {
		fn blank(s: Option<&str>) -> Option<&str> {
			s.map(str::trim).filter(|s| !s.is_empty())
		}
		self.sort = blank(sort).map(InvoiceSort::parse).transpose()?;
		self.offset = offset;
		self.from =
			blank(from).map(|d| crate::numbering::utc_span(d, d).map(|s| s.0)).transpose()?;
		self.to = blank(to).map(|d| crate::numbering::utc_span(d, d).map(|s| s.1)).transpose()?;
		self.party = blank(party)
			.map(|p| {
				PartyId::parse(p).map_err(|_| field_error("party", format!("not a party id: {p}")))
			})
			.transpose()?;
		Ok(self)
	}
}

/// One `(status, currency)` bucket of [`InvoiceStore::invoice_summary`]. Amounts are in
/// `currency` and nothing here converts, so a caller showing one headline figure picks a
/// currency rather than adding the rows up. A `STORNO` row (stored `ISSUED`, unpaid) counts
/// under `STORNOED`, beside the invoice it cancels, so the pair nets to zero there.
#[derive(Clone, Debug)]
pub struct StatusBucket {
	pub status: InvoiceStatus,
	pub currency: CurrencyCode,
	pub count: i64,
	pub net: Money,
	pub vat: Money,
	pub gross: Money,
	pub paid: Money,
}

/// One `(fulfilment month, currency)` bucket. Unnumbered invoices — `DRAFT` and `PENDING` —
/// are not revenue and never appear here.
#[derive(Clone, Debug)]
pub struct MonthBucket {
	/// `'YYYY-MM'` taken from `fulfilment_date`: that is the statutory period (Áfa tv.
	/// 55–58. §), while bucketing `issued_at` in UTC files a 00:30 CET invoice into the
	/// month before.
	pub month: String,
	pub currency: CurrencyCode,
	pub count: i64,
	pub gross: Money,
	pub paid: Money,
}

/// One local month of a year's HUF revenue, the facts a tax-limit check is built from. Both are
/// net of VAT, numbered invoices only, STORNOs included so a cancellation nets out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevenueMonth {
	/// `'YYYY-MM'`.
	pub month: String,
	/// By fulfilment month — the alanyi adómentesség basis (Áfa tv. 188. §). `EUFAD37` and `HO`
	/// groups are left out: their place of supply is abroad.
	pub invoiced_huf: Money,
	/// Cash received — the KATA and átalányadó basis: by the local month of `paid_at`, plus
	/// every `CASH` invoice by its fulfilment month, since cash is taken on the spot.
	pub received_huf: Money,
	/// Issued, not yet paid, not `CASH`, not stornoed — by fulfilment month, since it has no
	/// payment date yet. Shown beside `received_huf`, never counted into it.
	pub outstanding_huf: Money,
}

/// `ISSUED`, past its `due_date`, not fully paid. `outstanding` is `gross - paid_amount`.
#[derive(Clone, Debug)]
pub struct OverdueBucket {
	pub currency: CurrencyCode,
	pub count: i64,
	pub outstanding: Money,
}

/// What [`InvoiceStore::invoice_summary`] answers. `STORNO` rows are counted unfiltered, so an
/// invoice and its cancellation net to zero — which is the correct revenue answer. In
/// `statuses` a storno is bucketed `STORNOED`, never `ISSUED`, so it cannot lower outstanding.
#[derive(Clone, Debug, Default)]
pub struct InvoiceSummary {
	pub statuses: Vec<StatusBucket>,
	pub months: Vec<MonthBucket>,
	pub overdue: Vec<OverdueBucket>,
	/// `SUM(paid_amount)` per currency of invoices whose `paid_at` falls in the current local
	/// month — by payment date, where `months[].paid` is by fulfilment month.
	pub paid_this_month: Vec<(CurrencyCode, Money)>,
}

/// An `invoices` row. Everything from `series_code` down to `buyer_street` is frozen at
/// ISSUE; see the module doc.
#[derive(Clone, Debug)]
pub struct Invoice {
	pub id: i64,
	pub uid: InvoiceId,
	pub request_id: Option<String>,
	pub org_id: i64,
	pub seller_id: i64,
	/// The [`SellerVersion`] frozen at ISSUE; `None` while `DRAFT`. `seller_id` above stays the
	/// identity — numbering, NAV batching and the export ranges all key on it.
	pub seller_ver: Option<i64>,
	pub billing_party_id: Option<i64>,
	pub kind: InvoiceKind,
	pub status: InvoiceStatus,

	/// The chosen series while `DRAFT` (`None` = the seller's default), frozen at ISSUE.
	pub series_code: Option<String>,
	pub series_year: Option<i64>,
	/// Frozen at ISSUE from [`Seller::test_mode`]; a test invoice is never reported to NAV.
	pub test_mode: bool,
	/// `hu`, `en` or `hu-en`: copied from the party when the draft is made or its party
	/// changes, and filled at ISSUE if still `None`. `None` = derive from `buyer_country`.
	pub doc_lang: Option<String>,
	pub number: Option<String>,
	pub issued_at: Option<Timestamp>,
	pub fulfilment_date: Option<String>,
	pub due_date: Option<String>,
	/// The settlement period (Áfa tv. 58. §), `YYYY-MM-DD`; both set or both `None`.
	pub period_start: Option<String>,
	pub period_end: Option<String>,
	pub payment_method: PaymentMethod,

	pub original_invoice_id: Option<i64>,
	pub modification_index: Option<i64>,

	pub currency: CurrencyCode,
	pub rate_e6: i64,
	pub rate_date: Option<String>,
	pub rate_source: Option<RateSource>,
	pub huf_rate_e6: Option<i64>,

	pub net: Money,
	pub vat: Money,
	pub gross: Money,
	pub paid_amount: Money,
	pub paid_at: Option<Timestamp>,

	pub vat_note: Option<String>,
	pub notes: Option<String>,

	/// The invoice-level discount, as the column pair. Read back through
	/// [`crate::money::discount_of`] whenever the invoice is re-priced: its apportioned
	/// share is indistinguishable inside `invoice_lines.discount_amount`.
	pub discount_kind: Option<DiscountKind>,
	pub discount_value: Option<i64>,

	pub buyer_kind: Option<PartyKind>,
	pub buyer_name: Option<String>,
	pub buyer_country: Option<String>,
	pub buyer_tax_number: Option<String>,
	pub buyer_eu_vat_id: Option<String>,
	pub buyer_group_tax_no: Option<String>,
	pub buyer_postcode: Option<String>,
	pub buyer_city: Option<String>,
	pub buyer_street: Option<String>,
	pub buyer_vies_request_id: Option<String>,
	pub buyer_vies_checked_at: Option<Timestamp>,

	pub created_at: Timestamp,
	pub updated_at: Timestamp,
	/// Strictly increasing on every commit that touches the row — by one or more, since a
	/// `replace_draft_lines` carrying a patch writes twice. The optimistic-concurrency token
	/// [`InvoiceStore::replace_draft_lines`] and [`InvoiceStore::issue`] take: what they need
	/// is that it *changed*, not by how much.
	pub version: i64,
}

/// What [`InvoiceStore::create_draft`] needs. Currency and rate are settled at draft time so
/// the draft already prices correctly; they are re-frozen unchanged at ISSUE.
#[derive(Clone, Debug)]
pub struct NewInvoice {
	pub org_id: i64,
	pub seller_id: i64,
	pub billing_party_id: Option<i64>,
	pub request_id: Option<String>,
	pub kind: InvoiceKind,
	pub original_invoice_id: Option<i64>,
	pub currency: CurrencyCode,
	pub rate_e6: i64,
	pub payment_method: PaymentMethod,
	pub notes: Option<String>,
	/// The invoice-level discount as the stored column pair; `(None, None)` for a storno,
	/// whose lines already carry the apportioned share negated.
	pub discount_kind: Option<DiscountKind>,
	pub discount_value: Option<i64>,
	/// The chosen series; `None` is the seller's default. The caller validates it against
	/// [`InvoiceStore::list_series`]; a storno carries its original's.
	pub series_code: Option<String>,
	/// See [`Invoice::doc_lang`].
	pub doc_lang: Option<String>,
}

/// The writable half of a `DRAFT` invoice.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct InvoicePatch {
	pub billing_party_id: Option<i64>,
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
	pub currency: Option<CurrencyCode>,
	pub rate_e6: Option<i64>,
	/// `Null` returns the draft to the seller's default series.
	#[serde(default)]
	pub series_code: Patch<String>,
	/// Refreshed by the service when `billing_party_id` changes; see [`Invoice::doc_lang`].
	#[serde(default)]
	pub doc_lang: Patch<String>,
	/// Rescaled `discount_value` for an `AMOUNT` invoice-level discount, which is denominated
	/// in the invoice currency and so has to move with it. Never read off the wire: the
	/// discount itself is set when the draft is created.
	#[serde(skip)]
	pub discount_value: Option<i64>,
}

/// An `invoice_lines` row. `vat` and `gross` are display values apportioned back out of the
/// group figure and are never a source of truth — see `invoice_vat_groups`.
#[derive(Clone, Debug)]
pub struct InvoiceLine {
	pub id: i64,
	pub invoice_id: i64,
	pub line_no: i64,
	pub service_id: Option<i64>,
	pub description: String,
	pub unit: String,
	pub qty: Qty,
	pub unit_price: Money,
	pub discount_kind: Option<DiscountKind>,
	pub discount_value: Option<i64>,
	pub discount_amount: Money,
	pub discount_description: Option<String>,
	pub net: Money,
	pub vat_code: VatCode,
	pub vat_rate_bp: i64,
	pub vat: Money,
	pub gross: Money,
	/// Caller free text, kept out of `description` so a catalogue line's note survives the
	/// overwrite in `crate::draft::resolve`.
	pub note: Option<String>,
}

/// One line as the caller supplies it, already priced. `line_no` is assigned by the store from
/// the slice order, so the caller never numbers lines itself.
#[derive(Clone, Debug)]
pub struct NewInvoiceLine {
	pub service_id: Option<i64>,
	pub description: String,
	pub unit: String,
	pub qty: Qty,
	pub unit_price: Money,
	pub discount_kind: Option<DiscountKind>,
	pub discount_value: Option<i64>,
	pub discount_amount: Money,
	pub discount_description: Option<String>,
	pub net: Money,
	pub vat_code: VatCode,
	pub vat_rate_bp: i64,
	pub vat: Money,
	pub gross: Money,
	/// Caller free text; see [`InvoiceLine::note`].
	pub note: Option<String>,
}

/// An `invoice_vat_groups` row: the authoritative per-code figures. The `*_huf` trio is HUF
/// equivalents, and `vat_huf` is mandatory on a foreign-currency invoice (Áfa tv. 172. §).
#[derive(Clone, Debug)]
pub struct InvoiceVatGroup {
	pub invoice_id: i64,
	pub vat_code: VatCode,
	pub vat_rate_bp: i64,
	pub net: Money,
	pub vat: Money,
	pub gross: Money,
	pub net_huf: Option<Money>,
	pub vat_huf: Option<Money>,
	pub gross_huf: Option<Money>,
}

/// The buyer as frozen onto the invoice at ISSUE. Copied from `billing_parties`, then never
/// updated and never erased: editing or deleting the party leaves this untouched.
#[derive(Clone, Debug)]
pub struct BuyerSnapshot {
	pub kind: PartyKind,
	pub name: String,
	pub country: String,
	pub tax_number: Option<String>,
	pub eu_vat_id: Option<String>,
	pub group_tax_no: Option<String>,
	pub postcode: Option<String>,
	pub city: Option<String>,
	pub street: Option<String>,
	/// The VIES consultation number that justified reverse charge, and when it was obtained.
	/// `vies_checks` is keyed by EU VAT id and upserted on every refresh, so it cannot be the
	/// evidence for a five-year-old EUFAD37 invoice. `None` off the reverse-charge path.
	pub vies_request_id: Option<String>,
	pub vies_checked_at: Option<Timestamp>,
}

/// Everything the single issue transaction writes. The invoice number is *not* here: it is
/// allocated from `doc_series` inside that transaction and never before it.
#[derive(Clone, Debug)]
pub struct IssueInvoice {
	pub series_code: String,
	pub series_year: i64,
	/// Frozen onto the row, and picks the `doc_series` kind through [`series_kind`].
	pub test_mode: bool,
	/// Written only where the row still has none; see [`Invoice::doc_lang`].
	pub doc_lang: Option<String>,
	pub issued_at: Timestamp,
	pub fulfilment_date: String,
	pub due_date: Option<String>,
	/// Written at ISSUE as well, so a storno carries the original's period.
	pub period_start: Option<String>,
	pub period_end: Option<String>,
	pub rate_date: Option<String>,
	pub rate_source: Option<RateSource>,
	pub huf_rate_e6: Option<i64>,
	/// Base units per 1 of invoice currency, resolved on `rate_date`. `None` leaves the draft's
	/// value in place — which is right only for the base currency, where it is `1_000_000`.
	pub rate_e6: Option<i64>,
	pub net: Money,
	pub vat: Money,
	pub gross: Money,
	pub vat_note: Option<String>,
	pub buyer: BuyerSnapshot,
	/// The `CURRENT` [`SellerVersion`] at issue time, frozen onto the row — the seller's half
	/// of the same rule `buyer` is the buyer's half of.
	pub seller_ver: i64,
	/// Re-priced at issue with the buyer's verdict frozen onto each line, so the lines say
	/// what the groups and the NAV filing say. Ignored by `storno`, which brings its own.
	pub lines: Vec<NewInvoiceLine>,
	pub groups: Vec<InvoiceVatGroup>,
	/// Paid in full at issue (`CASH`): the row lands `PAID` with `paid_amount = gross` and
	/// `paid_at = issued_at`. The debt is the exact gross; cash rounding is only how it is paid.
	pub paid: bool,
	/// When `NAV_REPORT` runs; the store enqueues it **inside** the issue transaction, so a
	/// committed invoice cannot lack its filing. `None` enqueues nothing: a test invoice.
	pub nav_report_at: Option<Timestamp>,
}

impl Invoice {
	/// The row [`InvoiceStore::issue`] would return for this draft, built in memory. The SQL twin
	/// is `freeze` in both store adapters; a column it writes is written here too.
	#[must_use]
	pub fn frozen_with(&self, number: String, issue: &IssueInvoice) -> Invoice {
		let b = &issue.buyer;
		Invoice {
			status: if issue.paid { InvoiceStatus::Paid } else { InvoiceStatus::Issued },
			paid_amount: if issue.paid { issue.gross } else { self.paid_amount },
			paid_at: if issue.paid { Some(issue.issued_at) } else { self.paid_at },
			number: Some(number),
			series_code: Some(issue.series_code.clone()),
			series_year: Some(issue.series_year),
			issued_at: Some(issue.issued_at),
			fulfilment_date: Some(issue.fulfilment_date.clone()),
			due_date: issue.due_date.clone(),
			period_start: issue.period_start.clone(),
			period_end: issue.period_end.clone(),
			rate_date: issue.rate_date.clone(),
			rate_source: issue.rate_source,
			huf_rate_e6: issue.huf_rate_e6,
			rate_e6: issue.rate_e6.unwrap_or(self.rate_e6),
			net: issue.net,
			vat: issue.vat,
			gross: issue.gross,
			vat_note: issue.vat_note.clone(),
			seller_ver: Some(issue.seller_ver),
			test_mode: issue.test_mode,
			doc_lang: self.doc_lang.clone().or_else(|| issue.doc_lang.clone()),
			buyer_kind: Some(b.kind),
			buyer_name: Some(b.name.clone()),
			buyer_country: Some(b.country.clone()),
			buyer_tax_number: b.tax_number.clone(),
			buyer_eu_vat_id: b.eu_vat_id.clone(),
			buyer_group_tax_no: b.group_tax_no.clone(),
			buyer_postcode: b.postcode.clone(),
			buyer_city: b.city.clone(),
			buyer_street: b.street.clone(),
			buyer_vies_request_id: b.vies_request_id.clone(),
			buyer_vies_checked_at: b.vies_checked_at,
			version: self.version + 1,
			updated_at: issue.issued_at,
			..self.clone()
		}
	}
}

/// One of the seller's series by code.
pub(crate) async fn series_by_code(
	store: &dyn InvoiceStore,
	seller_id: i64,
	code: &str,
) -> ClResult<Option<SellerSeries>> {
	// List scan; add a store method if series counts grow
	Ok(store.list_series(seller_id).await?.into_iter().find(|s| s.code == code))
}

impl NewInvoice {
	/// The row [`InvoiceStore::create_draft_full`] inserts, before its totals, lines and dates.
	/// No `..`: a new [`Invoice`] field must be decided here.
	#[must_use]
	pub fn draft_row(&self, now: Timestamp) -> Invoice {
		let n = self.clone();
		let zero = Money(0);
		Invoice {
			id: 0,
			uid: InvoiceId::generate(),
			request_id: n.request_id,
			org_id: n.org_id,
			seller_id: n.seller_id,
			seller_ver: None,
			billing_party_id: n.billing_party_id,
			kind: n.kind,
			status: InvoiceStatus::Draft,
			series_code: n.series_code,
			series_year: None,
			test_mode: false,
			doc_lang: n.doc_lang,
			number: None,
			issued_at: None,
			fulfilment_date: None,
			due_date: None,
			period_start: None,
			period_end: None,
			payment_method: n.payment_method,
			original_invoice_id: n.original_invoice_id,
			modification_index: None,
			currency: n.currency,
			rate_e6: n.rate_e6,
			rate_date: None,
			rate_source: None,
			huf_rate_e6: None,
			net: zero,
			vat: zero,
			gross: zero,
			paid_amount: zero,
			paid_at: None,
			vat_note: None,
			notes: n.notes,
			discount_kind: n.discount_kind,
			discount_value: n.discount_value,
			buyer_kind: None,
			buyer_name: None,
			buyer_country: None,
			buyer_tax_number: None,
			buyer_eu_vat_id: None,
			buyer_group_tax_no: None,
			buyer_postcode: None,
			buyer_city: None,
			buyer_street: None,
			buyer_vies_request_id: None,
			buyer_vies_checked_at: None,
			created_at: now,
			updated_at: now,
			version: 0,
		}
	}
}

impl NewInvoiceLine {
	/// The `invoice_lines` row this becomes as line `line_no` of `invoice_id`; `id` is 0.
	#[must_use]
	pub fn to_line(&self, invoice_id: i64, line_no: i64) -> InvoiceLine {
		let l = self.clone();
		InvoiceLine {
			id: 0,
			invoice_id,
			line_no,
			service_id: l.service_id,
			description: l.description,
			unit: l.unit,
			qty: l.qty,
			unit_price: l.unit_price,
			discount_kind: l.discount_kind,
			discount_value: l.discount_value,
			discount_amount: l.discount_amount,
			discount_description: l.discount_description,
			net: l.net,
			vat_code: l.vat_code,
			vat_rate_bp: l.vat_rate_bp,
			vat: l.vat,
			gross: l.gross,
			note: l.note,
		}
	}
}

/// An `invoice_documents` row. There is no path column: the file lives at
/// `{DATA_DIR}/documents/{sha256[0..2]}/{sha256[2..4]}/{sha256}.pdf`, so the location is
/// derived from the hash and the stored hash is itself the integrity check.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvoiceDocument {
	#[serde(skip)]
	pub invoice_id: i64,
	pub kind: String,
	pub sha256: String,
	pub bytes: i64,
	pub template_version: String,
	pub rendered_at: Timestamp,
}

// ---------------------------------------------------------------- numbering

/// Renders a `doc_series.format` string. Three placeholders, no others, substituted
/// literally: `{code}`, `{year}` (four digits) and `{no:0N}` (left-padded to width `N`,
/// overflowing past `N` digits rather than truncating). Anything else is copied verbatim.
///
/// The default `'{code}{year}/{no:06}'` with `A`, `2026`, `123` renders `A2026/000123`.
/// `kind` never appears: series are kept apart by their `code`.
///
/// The overflow is deliberate — a number is never silently wrong — so **`invoices.number` is
/// not fixed-width**. A consumer ordering or ranging on it must compare `(length(number),
/// number)`, not the text alone: past 999999 under the default format `A2026/1000000` sorts
/// *below* `A2026/999999`. `mintworks-store-sqlite`'s `BY_NUMBER` audit export does exactly that.
pub fn render_number(format: &str, code: &str, year: i64, no: i64) -> String {
	let mut out = String::with_capacity(format.len() + 8);
	let mut rest = format;
	while let Some(open) = rest.find('{') {
		out.push_str(&rest[..open]);
		let Some(close) = rest[open..].find('}').map(|i| open + i) else {
			rest = &rest[open..]; // unterminated brace: copy the tail verbatim below
			break;
		};
		let token = &rest[open + 1..close];
		match token {
			"code" => out.push_str(code),
			"year" => {
				let _ = write!(out, "{year:04}");
			}
			// Width capped: an `i64` counter never needs more than 19 digits, and `{no:0999999999}`
			// from `doc_series.format` allocated ~1 GB while holding the one writer connection.
			_ => match token
				.strip_prefix("no:0")
				.and_then(|w| w.parse::<usize>().ok())
				.filter(|&w| w <= 19)
			{
				Some(width) => {
					let _ = write!(out, "{no:0width$}");
				}
				None => out.push_str(&rest[open..=close]),
			},
		}
		rest = &rest[close + 1..];
	}
	out.push_str(rest);
	out
}

// ---------------------------------------------------------------- the trait

/// Everything `mintworks-invoice` needs from a database.
///
/// Errors are `mintworks-core`'s: a unique-constraint violation surfaces as [`Error::Conflict`],
/// a rejected write against a frozen invoice as `E-INV-IMMUTABLE` (409), anything else as
/// `Error::Internal`. Methods that mutate a draft report whether they matched, so a caller can
/// tell "no such invoice" from "that invoice is no longer a draft" without a second query.
#[async_trait]
pub trait InvoiceStore: Send + Sync + 'static {
	// -- seller

	async fn seller_by_id(&self, id: i64) -> ClResult<Option<Seller>>;

	async fn seller_by_uid(&self, uid: &str) -> ClResult<Option<Seller>>;

	/// The seller `org_id` invoices under: its own `sellers` row, or the nearest ancestor's.
	/// An ERP business unit issues under its parent's taxpayer id and numbering series, so the
	/// walk climbs `orgs.parent_id`; the authorization gate then belongs on the *resolved*
	/// seller's org, never on the acting one.
	async fn seller_for_org(&self, org_id: i64) -> ClResult<Option<Seller>>;

	/// Seeds or refreshes an org's seller row. Called at boot from config; `seller.id` is the
	/// key, so it is an upsert and not a second seller.
	///
	/// `org_id` is matched by the upsert: an upsert that moved it would hand another org this
	/// taxpayer id, its NAV credentials and its `doc_series` counter, so a call naming an
	/// `org_id` the stored row does not carry is a conflict rather than a rewrite. Self-service
	/// minting goes through [`Self::create_seller`] instead, which can never overwrite.
	///
	/// Immediate and unversioned, unlike [`Self::save_seller_version_draft`]: everything on
	/// [`Seller`] is operational, and an invoice redriven five years later must use today's
	/// value. Do not give it a draft for symmetry.
	///
	/// For a config-seeded seller config owns the default series: every boot makes
	/// `seller.series_code` the default again, undoing an API promotion. An app either seeds
	/// from config or sets sellers up through [`Self::create_seller`], never both.
	async fn put_seller(&self, seller: &Seller) -> ClResult<()>;

	/// Insert-only mint of an org's own seller, and only when `seller.org_id` is an `ACTIVE`
	/// `SHARED` org: `false` when it is not. A taken `id` or `uid` is `Error::Conflict`, never an
	/// overwrite. The kind guard is contract, not convenience — a ROOT seller is the deployment's,
	/// and a PERSONAL org's GDPR erasure must never reach an 8-year statutory record.
	async fn create_seller(&self, seller: &Seller) -> ClResult<bool>;

	// -- seller versions
	//
	// Every write is one of the three below; a `CURRENT` or `ARCHIVED` row has no update path
	// at all, which is what makes the immutability rule checkable by grep.

	/// The version a new invoice freezes.
	async fn current_seller_version(&self, seller_id: i64) -> ClResult<Option<SellerVersion>>;

	/// The open, still-editable draft, if the operator has one.
	async fn draft_seller_version(&self, seller_id: i64) -> ClResult<Option<SellerVersion>>;

	/// Any status: what a frozen invoice resolves through `invoices.seller_ver`.
	async fn seller_version(&self, seller_ver: i64) -> ClResult<Option<SellerVersion>>;

	/// The rows for `vers`, in any order and with no row for an id that has none.
	/// `Nav::audit_export` needs a whole chunk's worth and must not go N+1.
	async fn seller_versions(&self, vers: &[i64]) -> ClResult<Vec<SellerVersion>>;

	/// `CURRENT` + `ARCHIVED`, newest first. The draft is excluded: it is not history yet.
	async fn seller_version_history(&self, seller_id: i64) -> ClResult<Vec<SellerVersion>>;

	/// Rewrite the open draft, or open one seeded from the `CURRENT` row (or from `patch`
	/// alone when there is no `CURRENT` yet — the fresh-install case).
	///
	/// Repeated edits rewrite the one draft in place: an edit does not make a version, only
	/// [`Self::publish_seller_version`] does.
	async fn save_seller_version_draft(
		&self,
		seller_id: i64,
		patch: &SellerVersionPatch,
	) -> ClResult<SellerVersion>;

	/// *Élesít*: archive the `CURRENT` row and promote the draft, in one transaction.
	/// `check` runs on the draft **inside** that transaction — validating a row read beforehand
	/// let a concurrent [`Self::save_seller_version_draft`] make a blank version live.
	/// Returns the promoted `seller_ver`, or `None` when there was no draft to promote.
	async fn publish_seller_version(
		&self,
		seller_id: i64,
		now: Timestamp,
		check: &(dyn for<'a> Fn(&'a SellerVersion) -> ClResult<()> + Send + Sync),
	) -> ClResult<Option<i64>>;

	/// Save `patch` as the draft and publish it, in **one transaction**, only when no draft is
	/// open. `None` means one was, and nothing was written.
	///
	/// Three statements, not three calls: a [`Self::save_seller_version_draft`] landing between
	/// a check and a save publishes an operator's half-typed edit as a statutory seller version.
	/// `check` is [`Self::publish_seller_version`]'s, run on the draft inside the same
	/// transaction.
	async fn sync_seller_version(
		&self,
		seller_id: i64,
		now: Timestamp,
		patch: &SellerVersionPatch,
		check: &(dyn for<'a> Fn(&'a SellerVersion) -> ClResult<()> + Send + Sync),
	) -> ClResult<Option<i64>>;

	/// Throw the open draft away. `false` when there was none; the `CURRENT` row is untouched.
	async fn discard_seller_version_draft(&self, seller_id: i64) -> ClResult<bool>;

	/// Whether any invoice of this seller carries a number (`number` is NULL while `DRAFT` and
	/// `PENDING`). Once one does, the tax number is the seller's NAV identity and is fixed.
	async fn seller_has_issued(&self, seller_id: i64) -> ClResult<bool>;

	/// Close (`Some`) or reopen (`None`) the seller. Reopen always applies. Close is **one
	/// conditional statement** that refuses — returns `false` — while the seller has a
	/// `PENDING` invoice: that is a card payment in flight whose completion issues it, and
	/// closing under it would capture money with no invoice. A missing seller is `false` too.
	async fn set_seller_closed(
		&self,
		seller_id: i64,
		closed_at: Option<Timestamp>,
	) -> ClResult<bool>;

	/// Set (`Some`) or clear (`None`) the seller's payment term. `false` when there is no such
	/// seller. [`InvoiceStore::put_seller`] never writes it, for the same reason as `closed_at`.
	async fn set_seller_payment_days(&self, seller_id: i64, days: Option<i64>) -> ClResult<bool>;

	/// Set the seller's test mode; `false` when there is no such seller. Invoices already issued
	/// keep the `test_mode` they froze.
	async fn set_seller_test_mode(&self, seller_id: i64, test_mode: bool) -> ClResult<bool>;

	// -- seller series
	//
	// `sellers.series_code` equals the one `is_default` row after every write below, and
	// `put_seller` / `create_seller` create that row.

	/// The seller's series, default first, then by code.
	async fn list_series(&self, seller_id: i64) -> ClResult<Vec<SellerSeries>>;

	/// Add a series. A taken `code` is `Error::Conflict`. `patch.is_default == Some(true)` also
	/// makes it the default, in the same transaction.
	async fn create_series(
		&self,
		seller_id: i64,
		code: &str,
		patch: &SeriesPatch,
	) -> ClResult<SellerSeries>;

	/// Update a series; `None` when the seller has no such series. `is_default: Some(false)` is
	/// not refused here: `Invoices::update_series` answers it `E-INV-SERIES-DEFAULT`.
	async fn update_series(
		&self,
		seller_id: i64,
		code: &str,
		patch: &SeriesPatch,
	) -> ClResult<Option<SellerSeries>>;

	/// Delete a series; `false` when there is none. Refused inside the deleting transaction
	/// with `E-INV-SERIES-DEFAULT` (409) for the default and `E-INV-SERIES-IN-USE` (409) for one
	/// any invoice of the seller, draft included, carries as its series. Pairs with the draft
	/// writes, which refuse an absent series in their own transaction (`E-INV-SERIES-UNKNOWN`).
	async fn delete_series(&self, seller_id: i64, code: &str) -> ClResult<bool>;

	/// The number the next [`Self::issue`] in `(seller_id, kind, code, year)` would allocate,
	/// rendered, without allocating it. Expected, not reserved: a concurrent issue may take it.
	async fn peek_number(
		&self,
		seller_id: i64,
		kind: &str,
		code: &str,
		year: i64,
	) -> ClResult<String>;

	// -- services

	/// Idempotent upsert of the consumer's declared catalogue, keyed on `(org_id, code)`.
	/// **Never deletes**: a service withdrawn from the code keeps its row, because issued
	/// invoice lines reference it. Deactivate it instead.
	async fn sync_services(&self, org_id: i64, defs: &[ServiceDef]) -> ClResult<()>;

	async fn service_by_code(&self, org_id: i64, code: &str) -> ClResult<Option<Service>>;

	/// The **active** rows for `codes`, in any order and with no row for a code that has none.
	/// [`crate::draft::resolve`] priced a 500-line draft one `service_by_code` at a time.
	async fn services_by_codes(&self, org_id: i64, codes: &[&str]) -> ClResult<Vec<Service>>;

	/// The org's rows for `ids`, prices attached, in any order. **Not** filtered on `active`: an
	/// inactive service still backs the draft lines that name it.
	async fn services_by_ids(&self, org_id: i64, ids: &[i64]) -> ClResult<Vec<Service>>;

	/// Scoped to the org, so an `svc_` id from another org reads as absent.
	async fn service_by_uid(&self, org_id: i64, uid: &ServiceId) -> ClResult<Option<Service>>;

	/// [`Error::Conflict`] if the org already holds this `code`.
	async fn create_service(&self, org_id: i64, def: &ServiceDef) -> ClResult<Service>;

	/// `None` if there is no such service.
	async fn update_service(
		&self,
		org_id: i64,
		uid: &ServiceId,
		patch: &ServicePatch,
	) -> ClResult<Option<Service>>;

	/// Capped at `limit`, which the handle sets. `q` is a case-insensitive name or code substring.
	async fn list_services(
		&self,
		org_id: i64,
		active_only: bool,
		q: Option<&str>,
		limit: i64,
	) -> ClResult<Vec<Service>>;

	// -- billing parties

	/// [`Error::Conflict`] if the org already holds this `(country, tax_number)`.
	async fn create_party(&self, org_id: i64, patch: &PartyPatch) -> ClResult<BillingParty>;

	/// Scoped to the org, so a `prt_` id from another org reads as absent.
	async fn party_by_uid(&self, org_id: i64, uid: &PartyId) -> ClResult<Option<BillingParty>>;

	async fn party_by_id(&self, id: i64) -> ClResult<Option<BillingParty>>;

	/// Setting `is_default` clears the previous default in the same transaction, because
	/// `idx_billing_party_default` allows only one per org. `emails_were` is the list an
	/// `email`-only patch's tail came from: a stored list differing from it is `E-CORE-CONFLICT`,
	/// checked inside the write transaction, before any write.
	async fn update_party(
		&self,
		org_id: i64,
		uid: &PartyId,
		patch: &PartyPatch,
		emails_were: Option<&[String]>,
	) -> ClResult<Option<BillingParty>>;

	/// `false` if there was no such party. Issued invoices survive: the FK is
	/// `ON DELETE SET NULL` and their buyer snapshot is self-sufficient.
	async fn delete_party(&self, org_id: i64, uid: &PartyId) -> ClResult<bool>;

	/// Capped at `limit`, which the handle sets: the statement had no `LIMIT` at all, and a
	/// org grows the table itself.
	async fn list_parties(&self, org_id: i64, limit: i64) -> ClResult<Vec<BillingParty>>;

	/// One page under `filter` (ordered by its sort, then `id`; `LIMIT`/`OFFSET` from it) and
	/// the number of parties the filter matches across every page.
	async fn list_parties_page(
		&self,
		org_id: i64,
		filter: &PartyFilter,
	) -> ClResult<(Vec<BillingParty>, u64)>;

	/// `(party id, currency, SUM(gross - paid_amount))` over each party's unpaid non-test
	/// `ISSUED` invoices, `kind = 'STORNO'` rows excluded; ordered by party then currency, and no
	/// row for a party owing nothing.
	async fn open_receivables(
		&self,
		org_id: i64,
		party_ids: &[i64],
	) -> ClResult<Vec<(i64, CurrencyCode, Money)>>;

	async fn default_party(&self, org_id: i64) -> ClResult<Option<BillingParty>>;

	// -- invoices: draft

	async fn create_draft(&self, new: &NewInvoice) -> ClResult<Invoice>;

	/// The draft in one transaction: the `invoices` row, its lines, its VAT groups, its
	/// totals and an optional patch, all under one `BEGIN IMMEDIATE`.
	///
	/// One method rather than `create_draft` + `replace_draft_lines` + `update_draft`,
	/// because as three unrelated writes any failure after the insert left an `invoices` row
	/// with the `request_id` consumed and **zero lines**. The retry then got that empty
	/// invoice back off the idempotency branch, and `issue::run` refused it with
	/// `E-INV-EMPTY` forever — the consumer's checkout wedged until `SWEEP_DRAFTS` cleared
	/// it, `invoice.draft_ttl_days` later.
	///
	/// A `UNIQUE (org_id, request_id)` collision is not surfaced as a conflict either: it
	/// means a concurrent caller won the same idempotency key, so *its* invoice is read back
	/// and returned. That closes the read-then-insert race in `Invoices::draft`, which the
	/// idempotency guarantee ("a checkout handler is retried by machines") depends on.
	///
	/// `priced` may be computed before the row exists: the lines and groups are written
	/// against the id this method allocates, not against any id inside `priced`.
	async fn create_draft_full(
		&self,
		new: &NewInvoice,
		priced: &Priced,
		patch: Option<&InvoicePatch>,
	) -> ClResult<Invoice>;

	/// The `request_id` idempotency lookup: a consumer replaying "invoice order-9182" gets
	/// the invoice it already made instead of a second one.
	///
	/// Scoped to the org, because `request_id` is unique per org: the key space belongs
	/// to the caller, and two orgs numbering their own subscriptions must not collide.
	async fn invoice_by_request_id(
		&self,
		org_id: i64,
		request_id: &str,
	) -> ClResult<Option<Invoice>>;

	/// Scoped to the org. Pass `None` for the framework's own unscoped reads.
	async fn invoice_by_uid(
		&self,
		org_id: Option<i64>,
		uid: &InvoiceId,
	) -> ClResult<Option<Invoice>>;

	async fn invoice_by_id(&self, id: i64) -> ClResult<Option<Invoice>>;

	/// The `STORNO` counter-invoice that cancelled `original_id`, so a view can name the
	/// document that voided this one. At most one exists — the partial unique index
	/// `idx_invoice_storno_once` is what makes that true, and it serves this predicate too.
	async fn storno_of(&self, original_id: i64) -> ClResult<Option<Invoice>>;

	/// `WHERE id = ? AND status = 'DRAFT'`: `None` means absent *or* already issued.
	async fn update_draft(&self, id: i64, patch: &InvoicePatch) -> ClResult<Option<Invoice>>;

	/// Sets `notes` on an invoice in any status. The one column an `ISSUED` invoice may still
	/// change: it is not an amount, a line or the buyer snapshot. `None` means no such row.
	async fn update_notes(&self, id: i64, notes: Option<&str>) -> ClResult<Option<Invoice>>;

	/// Replaces the whole line set of a draft and rewrites its `invoice_vat_groups` and
	/// totals in one transaction, so the three never disagree. `line_no` is assigned from
	/// the slice order. `Ok(false)` if the invoice is not a draft **or** has moved on since
	/// `expected_version` — the caller re-priced against a snapshot it read on the reader
	/// pool, so committing over a concurrent edit would silently discard it.
	///
	/// `invoices.version` strictly increases on every commit touching the row, so the guard is
	/// exact. It used
	/// to be `AND updated_at = ?`, and `Timestamp` is unix seconds: two `POST /lines` inside
	/// one second both matched and the second silently dropped the first caller's line.
	///
	/// `patch` is applied to the `invoices` row in the same transaction. It exists for the
	/// currency change, which rewrites the row *and* every line: applied separately, a
	/// failure between the two would leave the lines in the old currency under the new code.
	async fn replace_draft_lines(
		&self,
		id: i64,
		patch: Option<&InvoicePatch>,
		priced: &Priced,
		expected_version: i64,
	) -> ClResult<bool>;

	async fn invoice_lines(&self, invoice_id: i64) -> ClResult<Vec<InvoiceLine>>;

	async fn invoice_vat_groups(&self, invoice_id: i64) -> ClResult<Vec<InvoiceVatGroup>>;

	/// The batched forms of the three reads above, for `mintworks_nav`'s audit export: a year of
	/// invoices cost four queries each. Each row carries its `invoice_id`, so the caller
	/// groups. `ids` is unbounded: the store chunks the `IN (…)` list itself, so the ordering
	/// holds within a chunk only — which is why every row carries its `invoice_id`.
	async fn invoices_by_ids(&self, ids: &[i64]) -> ClResult<Vec<Invoice>>;

	async fn invoice_lines_for(&self, ids: &[i64]) -> ClResult<Vec<InvoiceLine>>;

	async fn invoice_vat_groups_for(&self, ids: &[i64]) -> ClResult<Vec<InvoiceVatGroup>>;

	/// `false` if the invoice was not a draft. Lines and groups cascade.
	///
	/// A `payment_allocations` row against a draft goes with it, in the same transaction:
	/// those rows do not cascade — they are a money trail — but a draft's are all the
	/// zero-amount links an abandoned gateway attempt left, because settling a draft is
	/// refused outright. Without that the draft could never be deleted at all.
	///
	/// Refusing a draft whose gateway payment is still live is the **caller's** check, so it
	/// can raise a code of its own; [`Self::sweep_drafts`] has no caller to raise one and
	/// makes the check itself.
	async fn delete_draft(&self, id: i64) -> ClResult<bool>;

	/// `SWEEP_DRAFTS`: abandoned carts older than `cutoff`, `DRAFT` and `PENDING` alike — a lock
	/// whose payment no gateway answer will ever move is an abandoned cart again. Returns how
	/// many went. Takes the link rows with them, as [`Self::delete_draft`] does and for the same
	/// reason, and skips any invoice whose payment is still live for the reason that gives.
	/// A draft linked as a live (not `CANCELED`) subscription's renewal is kept too: it is
	/// what a suspended subscriber still has to pay.
	async fn sweep_drafts(&self, cutoff: Timestamp) -> ClResult<u64>;

	/// The gateway lock, `DRAFT -> PENDING` and back: a compare-and-set, `Ok(false)` when the
	/// row was not in `from`, which is how a lost race is reported rather than a panic.
	///
	/// **The implementation rejects every other pair with `Error::Internal`**, rather than
	/// trusting its callers: this trait is public and a consumer holds it directly, so
	/// `set_status(id, Issued, Draft)` would walk a numbered, NAV-filed invoice back to where
	/// [`Self::replace_draft_lines`] and [`Self::issue`] accept it and renumber it.
	/// `DRAFT -> ISSUED` is [`Self::issue`]'s alone, and stays there — the number is allocated
	/// in that one transaction and nowhere else.
	async fn set_status(&self, id: i64, from: InvoiceStatus, to: InvoiceStatus) -> ClResult<bool>;

	/// Issued invoice ids with no `invoice_documents` row, oldest first, at most `limit`.
	///
	/// The PDF side's recovery sweep, the counterpart of `mintworks-nav`'s `unfiled_invoices`: a
	/// `RENDER_PDF` that exhausts its attempts becomes `FAILED` and is never retried, and
	/// `Invoices::document` then answers `E-INV-PDF-PENDING` forever for a render that will
	/// never happen again.
	async fn issued_without_document(&self, limit: i64) -> ClResult<Vec<i64>>;

	// -- invoices: issue

	/// The whole issue transaction, and the only place an invoice number is allocated.
	///
	/// In one write transaction: `UPDATE doc_series … RETURNING` the next number (inserting
	/// the `(seller, kind, code, year)` row on first use of the year), render it
	/// through [`render_number`], flip `DRAFT` → `ISSUED`, freeze the buyer snapshot, dates,
	/// rate and totals, and write `invoice_vat_groups`. A rollback consumes no number, which
	/// is exactly what gaplessness requires. `kind` is [`series_kind`]`(issue.test_mode)`, so
	/// a test invoice never consumes a live number. `E-INV-SERIES-UNKNOWN` (400) when
	/// `issue.series_code` is not one of the seller's [`SellerSeries`].
	///
	/// [`Error::Conflict`] if the invoice is no longer a draft, and `E-INV-CHANGED` if it
	/// changed since `expected_version` — the same optimistic guard
	/// [`InvoiceStore::replace_draft_lines`] carries, and for the same reason. The caller
	/// reads the lines off the reader pool and then may spend up to 15 s in a VIES lookup
	/// before getting here; without the guard a `POST /lines` that commits in that window
	/// was silently erased from an invoice that then got a number and was filed to NAV.
	async fn issue(
		&self,
		id: i64,
		issue: &IssueInvoice,
		expected_version: i64,
	) -> ClResult<Invoice>;

	/// Creates the STORNO counter-invoice and flips the original to `STORNOED` in one
	/// transaction, with its own number from the same series and kind. `issue.lines` and
	/// `issue.groups` are the original's, negated by the caller. [`Error::Conflict`] if the
	/// original is not `ISSUED`/`PAID`, or already cancelled — `idx_invoice_storno_once`
	/// guarantees the second attempt cannot land.
	async fn storno(
		&self,
		original_id: i64,
		new: &NewInvoice,
		issue: &IssueInvoice,
	) -> ClResult<Invoice>;

	// -- invoices: read and the three permitted post-issue writes

	/// Cursor-paginated over `idx_invoice_org`, newest first: pass the last `id` seen as
	/// `before_id`.
	async fn list_invoices(
		&self,
		org_id: i64,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Invoice>>;

	/// [`InvoiceStore::list_invoices`] with the three uids a listing shows resolved by join.
	/// Reading them per row cost up to 400 extra queries on a 200-row page.
	async fn list_invoices_page(
		&self,
		org_id: i64,
		filter: &InvoiceFilter,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<ListedInvoice>>;

	/// How many rows [`InvoiceStore::list_invoices_page`] would return under `filter` with no
	/// cursor, offset or limit.
	async fn count_invoices(&self, org_id: i64, filter: &InvoiceFilter) -> ClResult<i64>;

	/// The twelve [`RevenueMonth`]s of `year`, January first, zero-filled. `[start, end)` is
	/// the local year as UTC instants, which is what `paid_at` is compared against. Test-mode
	/// invoices are left out: they are no turnover.
	async fn invoice_revenue(
		&self,
		org_id: i64,
		year: i32,
		start: Timestamp,
		end: Timestamp,
	) -> ClResult<Vec<RevenueMonth>>;

	/// Three `GROUP BY`s over `invoices`, org-scoped: per `(status, currency)`, per
	/// `(fulfilment month, currency)` from `from_month` on, and the unpaid overdue total per
	/// currency. Nothing is ever converted between currencies. Test-mode invoices are left out.
	///
	/// `from_month` is an inclusive `'YYYY-MM'`, `today` a `'YYYY-MM-DD'` and `this_month` the
	/// current local month as a half-open UTC span. All are parameters rather than read from a
	/// clock here: the deployment's local calendar is [`crate::numbering::local`]'s, and that
	/// lives in the feature crate.
	async fn invoice_summary(
		&self,
		org_id: i64,
		from_month: &str,
		today: &str,
		this_month: (Timestamp, Timestamp),
	) -> ClResult<InvoiceSummary>;

	/// `ISSUED -> PAID`, one of the two status transitions an issued invoice permits.
	///
	/// One method per legal transition rather than a general `set_status(id, status)`, so that
	/// **an illegal transition cannot be written down** and no implementation or runtime guard
	/// has to refuse one — in particular `ISSUED -> DRAFT`, which would reopen a numbered, filed
	/// invoice to [`InvoiceStore::update_draft`], [`InvoiceStore::replace_draft_lines`] and
	/// [`InvoiceStore::delete_draft`]. `DRAFT -> ISSUED` belongs to [`InvoiceStore::issue`] alone.
	///
	/// `Ok(false)` means no row matched — an unknown id, or an invoice that is not `ISSUED`.
	///
	/// Not the payment path: `BillingStore::settle` writes `PAID` from the allocation sum, in
	/// the transaction that wrote the allocation.
	async fn mark_paid(&self, id: i64) -> ClResult<bool>;

	/// `ISSUED -> STORNOED`, the other. Same contract as [`InvoiceStore::mark_paid`].
	async fn mark_stornoed(&self, id: i64) -> ClResult<bool>;

	/// The paid-amount write. `paid_at` is stamped when `paid_amount` first reaches `gross`; the
	/// caller recomputes the amount from `payment_allocations` in the same transaction that
	/// wrote the allocation. `mintworks-billing` owns those tables.
	///
	/// `Ok(false)` means no row matched: a payment allocated against a wrong id must not
	/// report success while the amount lands nowhere. Only `ISSUED`/`PAID` rows match, and
	/// that predicate is the only thing stopping an allocation landing on a draft or a
	/// cancelled invoice. Callers treat `false` as a conflict, never as success.
	async fn set_paid(
		&self,
		id: i64,
		paid_amount: Money,
		paid_at: Option<Timestamp>,
	) -> ClResult<bool>;

	// -- documents

	/// Content-addressed, so re-rendering an identical PDF is a no-op rather than a second
	/// file. **First write wins** on `(invoice_id, 'PDF')`: a second call is a no-op, not an
	/// overwrite.
	///
	/// `false` when nothing was written — either the row was already there, or the invoice
	/// moved under the render: `update_notes` bumps `version`, and a PDF served `immutable`
	/// must not be built from a note that has since changed. The caller re-runs, and the
	/// already-rendered case then short-circuits on [`InvoiceStore::invoice_document`].
	async fn put_invoice_document(&self, doc: &InvoiceDocument, version: i64) -> ClResult<bool>;

	/// The stored PDF, or `None` while the invoice is still a draft.
	async fn invoice_document(&self, invoice_id: i64) -> ClResult<Option<InvoiceDocument>>;

	/// The batched form, for `mintworks_nav`'s batch filing: one read instead of one per member.
	/// Each row carries its `invoice_id`, so the caller indexes, and an invoice with no PDF is
	/// simply absent. `ids` is unbounded: the store chunks the `IN (…)` list itself.
	async fn invoice_documents_for(&self, ids: &[i64]) -> ClResult<Vec<InvoiceDocument>>;

	/// True when an `invoice_documents` row with this sha belongs to an invoice of `org_id`: the
	/// PDF blob itself lives in the seller's org, so the blob's own org cannot answer this.
	async fn invoice_document_in_org(&self, sha256: &str, org_id: i64) -> ClResult<bool>;

	// -- currency

	/// The `currencies` row, disabled ones included: [`crate::currency::get`] owns the
	/// `E-INV-CURRENCY-DISABLED` mapping, so it has to see `enabled = 0`.
	async fn currency_get(&self, code: &str) -> ClResult<Option<Currency>>;

	/// Insert `code` with the seed defaults (`OFFICIAL`, steps 1/1, fee 0, enabled), or set
	/// `enabled` on an existing row, leaving its other columns alone. Never disables.
	async fn currency_enable(&self, code: &str) -> ClResult<()>;

	/// Every currency in `code` order. `all` includes the disabled rows.
	async fn currency_list(&self, all: bool) -> ClResult<Vec<Currency>>;

	/// The org's `org_currencies` rows in `code` order.
	async fn org_currency_list(&self, org_id: i64) -> ClResult<Vec<OrgCurrency>>;

	async fn org_currency_get(&self, org_id: i64, code: &str) -> ClResult<Option<OrgCurrency>>;

	/// Upsert. There is no delete: `convert = false` disables the row and keeps its fee.
	async fn org_currency_set(&self, org_id: i64, row: &OrgCurrency) -> ClResult<()>;

	/// `(rate_e6, date)` published for `pair` from `source` on `on`, or the last one before
	/// it. The staleness bound is [`crate::currency::rate_on`]'s, not this method's.
	async fn currency_rate(
		&self,
		pair: &str,
		source: &str,
		on: &str,
	) -> ClResult<Option<(i64, String)>>;

	// -- MNB rate fetch

	/// The enabled currency codes other than `base` — what [`crate::mnb::run`] iterates.
	async fn mnb_currencies(&self, base: &str) -> ClResult<Vec<String>>;

	/// The newest date stored for `pair` under [`crate::mnb::SOURCE`], `None` when there is
	/// none yet.
	async fn mnb_max_date(&self, pair: &str) -> ClResult<Option<String>>;

	/// Upsert `(date, rate_e6)` rows for `pair` under [`crate::mnb::SOURCE`], in one
	/// transaction — a round trip through this trait per published day is what it avoids.
	async fn mnb_upsert_rates(&self, pair: &str, rows: &[(String, i64)]) -> ClResult<()>;

	// -- VIES

	/// The cached answer while `checked_at` is within `ttl` seconds. A stale row is `None`:
	/// a cache miss, not an answer.
	async fn vies_cached(&self, full: &str, ttl: i64) -> ClResult<Option<ViesResult>>;

	/// Upsert on `vies_checks.eu_vat_id`.
	async fn vies_store(&self, r: &ViesResult) -> ClResult<()>;

	// -- orgs

	/// `orgs.billing_currency`, which is nullable — `None` means "the base currency".
	async fn org_billing_currency(&self, org_id: i64) -> ClResult<Option<CurrencyCode>>;
}

#[cfg(test)]
mod tests {
	use super::{MAX_DESCRIPTION, bounded_text, pdf_filename, render_number, safe_filename_part};

	/// A control byte pasted into a description escapes nothing in `quick-xml` and makes
	/// `invoiceData` non-well-formed, which NAV rejects on every retry of an invoice that is
	/// already issued and immutable. A line break is refused for the neighbouring reason: XSD's
	/// `.` excludes #x0A/#x0D, so `SimpleText*NotBlankType`'s `.*[^\s].*` cannot match it. Tab
	/// is #x09, which `.` does match, so it survives.
	#[test]
	fn control_characters_are_refused_at_the_trust_boundary() {
		for bad in ["a\u{1}b", "line one\nline two", "line one\rline two"] {
			assert_eq!(
				bounded_text("description", bad, MAX_DESCRIPTION).unwrap_err().parts().1,
				"E-INV-BAD-TEXT",
				"{bad:?}"
			);
		}
		bounded_text("description", "one\ttwo", MAX_DESCRIPTION).unwrap();
	}

	/// An over-wide width used to `write!` ~1 GB inside the issue transaction, holding the one
	/// writer connection; it now falls through to the unknown-token arm, like every other
	/// placeholder the formatter does not know.
	#[test]
	fn the_number_format_pads_overflows_and_copies_the_rest_verbatim() {
		for (fmt, code, year, no, want) in [
			("{code}{year}/{no:06}", "A", 2026, 123, "A2026/000123"),
			("{no:03}", "A", 2026, 7, "007"),
			// Overflow widens rather than truncating: a number must never be misprinted.
			("{no:03}", "A", 2026, 12345, "12345"),
			("INV-{code}-{no:01}", "B", 2026, 9, "INV-B-9"),
			("{kind}/{no:02}", "A", 2026, 4, "{kind}/04"),
			("no braces", "A", 2026, 1, "no braces"),
			("{code}{unterminated", "A", 2026, 1, "A{unterminated"),
			("xx{unterminated", "A", 2026, 1, "xx{unterminated"),
			("{no:0999999999}", "A", 2026, 1, "{no:0999999999}"),
			("{no:06}", "A", 2026, 1, "000001"),
		] {
			assert_eq!(render_number(fmt, code, year, no), want, "{fmt}");
		}
	}

	#[test]
	fn a_filename_part_loses_quotes_and_slashes_and_is_capped() {
		let s = safe_filename_part("A\"2026/000001");
		assert_eq!(s, "A-2026-000001");
		assert_eq!(safe_filename_part(&"x".repeat(200)).len(), 64);
	}

	#[test]
	fn a_pdf_filename_carries_the_whole_hash() {
		let hash = "a".repeat(64);
		assert_eq!(
			pdf_filename(Some("A2026/000123"), "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV", &hash),
			format!("A2026-000123-{hash}.pdf")
		);
		assert_eq!(
			pdf_filename(None, "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV", &hash),
			format!("inv_01ARZ3NDEKTSV4RRFFQ69G5FAV-{hash}.pdf")
		);
	}
}

// vim: ts=4
