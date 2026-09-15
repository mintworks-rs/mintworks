//! VAT determination: which treatment the *seller's scheme* and the *buyer* force onto an
//! invoice.
//!
//! `(buyer zone, buyer is a company, buyer has a VIES-validated EU VAT id)` either overrides
//! every line with a single code and the i18n key of the legal note the invoice must print,
//! or defers to the line's own code. That second case is the common one: outside reverse
//! charge and export, the rate of a book or a hotel night is a property of the *product*, so
//! it comes from the `services` row or from what the caller passed ad hoc.
//!
//! Deliberately not a `tax_rules` lookup table: it would hold three rows, no plausible change
//! to it is data-only, and its `id` was never frozen onto an invoice, so the audit trail such
//! a table is supposed to buy would not exist either.

use crate::store::Seller;
use crate::vat::VatCode;

/// EU member states, for [`BuyerZone::of`]. Not `saas-core`'s business: only VAT cares.
pub const EU_COUNTRIES: [&str; 27] = [
	"AT", "BE", "BG", "CY", "CZ", "DE", "DK", "EE", "ES", "FI", "FR", "GR", "HR", "HU", "IE", "IT",
	"LT", "LU", "LV", "MT", "NL", "PL", "PT", "RO", "SE", "SI", "SK",
];

/// Where the buyer sits relative to the seller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuyerZone {
	Dom,
	Eu,
	Third,
}

impl BuyerZone {
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Dom => "DOM",
			Self::Eu => "EU",
			Self::Third => "THIRD",
		}
	}

	/// Classify an ISO-3166 alpha-2 buyer country against the seller's. Case-insensitive.
	#[must_use]
	pub fn of(seller_country: &str, buyer_country: &str) -> Self {
		let buyer = buyer_country.to_ascii_uppercase();
		if buyer == seller_country.to_ascii_uppercase() {
			Self::Dom
		} else if EU_COUNTRIES.contains(&buyer.as_str()) {
			Self::Eu
		} else {
			Self::Third
		}
	}
}

/// What the determination matches on. `has_eu_vat` is true only for a VIES check that
/// succeeded inside `settings['vies.cache_days']` — see [`crate::vies`]. A failed or
/// unreachable check leaves it false, so a VIES outage can never fall through to reverse
/// charge.
#[derive(Debug, Clone)]
pub struct BuyerProfile {
	pub zone: BuyerZone,
	pub is_company: bool,
	pub has_eu_vat: bool,
}

/// The verdict for one invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
	/// Every line on the invoice takes this code, whatever its `services` row says — which
	/// is how a caller cannot talk the framework out of reverse charge or export. The
	/// `&'static str` is an i18n key resolved by the PDF template.
	Override(VatCode, Option<&'static str>),
	/// Each line keeps its own code. Domestic sales, and EU B2C — correct under the
	/// distance-selling threshold (`architecture.md` §3.8).
	Product,
}

impl Verdict {
	/// The code this verdict imposes on a line that would otherwise carry `line_code`.
	#[must_use]
	pub fn effective(&self, line_code: VatCode) -> VatCode {
		match self {
			Self::Override(code, _) => *code,
			Self::Product => line_code,
		}
	}

	/// The i18n key of the legal note. Frozen onto `invoices.vat_note` at issue — as one of
	/// possibly several newline-joined keys, because a mixed exempt invoice owes a reference
	/// per exempt supply. See `issue::vat_note`.
	#[must_use]
	pub fn note_key(&self) -> Option<&'static str> {
		match self {
			Self::Override(_, key) => *key,
			Self::Product => None,
		}
	}
}

/// Split a stored `invoices.vat_note` back into its i18n keys. Empty for `None`, which is
/// what a fully-taxed invoice stores. The join is `issue::vat_note`'s; this is its inverse and
/// the only way `pdf.rs` and `routes.rs` are guaranteed to agree on the format.
#[must_use]
pub fn vat_notes(stored: Option<&str>) -> Vec<&str> {
	stored.unwrap_or_default().split('\n').filter(|k| !k.is_empty()).collect()
}

/// `sellers.vat_scheme` for a seller under the *alanyi adómentesség* (subjective exemption,
/// Áfa tv. 187–196. §). Such a seller charges no VAT on any supply of its own.
pub const SCHEME_EXEMPT: &str = "ALANYI_MENTES";

/// The whole of VAT determination. Pure and sync: no pool, no `await`, no table.
///
/// Reverse charge (Áfa tv. 37. §) needs a company buyer in another member state *and* a
/// VIES-validated id; an EU company that failed validation falls through to [`Verdict::Product`]
/// and is charged domestic VAT, which is the safe direction.
///
/// The seller matters too, and used not to be looked at: `saas_nav::xml` emits
/// `<individualExemption>true</individualExemption>` from `sellers.vat_scheme`, so an exempt
/// seller was issuing `STD27` lines — 27% charged to the customer and printed on the PDF —
/// while the NAV filing for the same invoice declared it exempt.
#[must_use]
pub fn determine(seller: &Seller, buyer: &BuyerProfile) -> Verdict {
	let verdict = match buyer.zone {
		BuyerZone::Eu if buyer.is_company && buyer.has_eu_vat => {
			Verdict::Override(VatCode::Eufad37, Some("vat.eufad37"))
		}
		// Third country, **company only** — decided 2026-09-14. Áfa tv. 37. § (1) puts a supply
		// to a taxable person at the buyer's seat, so it is outside the territorial scope
		// (`HO`); 37. § (2) puts a non-taxable one at the supplier's seat unless 46. § lists the
		// service, so a third-country individual is charged domestic VAT below. That
		// over-collects where 46. § applies — the safe direction on an immutable document.
		// `claude-docs/legal-research.md` records the answer.
		BuyerZone::Third if buyer.is_company => Verdict::Override(VatCode::Ho, Some("vat.ho")),
		BuyerZone::Dom | BuyerZone::Eu | BuyerZone::Third => Verdict::Product,
	};

	// Only the domestic-VAT outcome is overridden: intra-EU reverse charge and third-country
	// export already produce zero VAT under notes that state the correct reason for the buyer.
	//
	// OPEN TAX QUESTION — if an alanyi mentes seller must issue AAM *unconditionally*, this
	// becomes an unconditional override (delete the `matches!` guard). A question for a tax
	// adviser; the narrow version cannot be wrong about the two zero-VAT zones.
	if seller.vat_scheme == SCHEME_EXEMPT && matches!(verdict, Verdict::Product) {
		return Verdict::Override(VatCode::Aam, Some("vat.aam"));
	}
	verdict
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn zones() {
		assert_eq!(BuyerZone::of("HU", "hu"), BuyerZone::Dom);
		assert_eq!(BuyerZone::of("HU", "DE"), BuyerZone::Eu);
		assert_eq!(BuyerZone::of("HU", "US"), BuyerZone::Third);
		assert_eq!(BuyerZone::of("HU", "GB"), BuyerZone::Third);
	}

	fn at(zone: BuyerZone, is_company: bool, has_eu_vat: bool) -> BuyerProfile {
		BuyerProfile { zone, is_company, has_eu_vat }
	}

	/// Only `country` and `vat_scheme` are read here; the rest is filler the struct demands.
	fn seller_on(scheme: &str) -> Seller {
		Seller {
			id: 1,
			name: "Teszt Kft.".into(),
			country: "HU".into(),
			tax_number: "12345676-2-02".into(),
			group_member_tax_no: None,
			eu_vat_id: None,
			postcode: "1011".into(),
			city: "Budapest".into(),
			street: "Fő utca 1.".into(),
			bank_account: None,
			bank_name: None,
			nav_base_url: String::new(),
			nav_login: None,
			small_business: false,
			vat_scheme: scheme.to_owned(),
			series_code: "A".into(),
			created_at: saas_core::prelude::Timestamp(0),
		}
	}

	fn normal() -> Seller {
		seller_on("NORMAL")
	}

	/// The whole buyer-side decision, one row per `(zone, is_company, has_eu_vat)`. `None` is
	/// [`Verdict::Product`]: the product's own rate stands, so a mixed-rate invoice keeps its
	/// groups.
	///
	/// `HO` is a company-only treatment: Áfa tv. 37. § (1) puts a service to a taxable person
	/// at the buyer's seat, while 37. § (2) puts one to a non-taxable person at the supplier's
	/// seat unless 46. § lists it. A third-country private individual is therefore charged
	/// domestic VAT — over-collecting where 46. § applies, the safe direction on a document
	/// that is immutable once issued.
	#[test]
	fn the_buyer_drives_only_the_two_overrides() {
		for (zone, is_company, has_eu_vat, over) in [
			(BuyerZone::Dom, true, false, None),
			(BuyerZone::Dom, false, false, None),
			(BuyerZone::Eu, false, false, None),
			// An EU company whose VAT id did not validate is charged domestic VAT.
			(BuyerZone::Eu, true, false, None),
			(BuyerZone::Eu, true, true, Some((VatCode::Eufad37, "vat.eufad37"))),
			(BuyerZone::Third, false, false, None),
			// A third-country company is unchanged by `has_eu_vat`: no VIES id exists
			// outside the EU to validate.
			(BuyerZone::Third, true, false, Some((VatCode::Ho, "vat.ho"))),
			(BuyerZone::Third, true, true, Some((VatCode::Ho, "vat.ho"))),
		] {
			let case = format!("{zone:?}/company={is_company}/eu_vat={has_eu_vat}");
			let v = determine(&normal(), &at(zone, is_company, has_eu_vat));
			let (red, std, note) = match over {
				None => (VatCode::Red05, VatCode::Std27, None),
				Some((code, key)) => {
					assert_eq!(v, Verdict::Override(code, Some(key)), "{case}");
					(code, code, Some(key))
				}
			};
			assert_eq!(v.effective(VatCode::Red05), red, "{case}");
			assert_eq!(v.effective(VatCode::Std27), std, "{case}");
			assert_eq!(v.note_key(), note, "{case}");
		}
	}

	/// `saas_nav::xml` emits `<individualExemption>true</individualExemption>` from
	/// `sellers.vat_scheme`, and nothing in `saas-invoice` read that column. So an
	/// `ALANYI_MENTES` seller issued `STD27` lines — 27% charged to the customer and printed
	/// on the PDF — while the NAV filing for the same invoice declared it exempt.
	#[test]
	fn an_exempt_seller_charges_no_domestic_vat() {
		let exempt = seller_on(SCHEME_EXEMPT);

		// Domestic and EU B2C: what would have been the product's own rate is now AAM.
		for p in [
			at(BuyerZone::Dom, true, false),
			at(BuyerZone::Dom, false, false),
			at(BuyerZone::Eu, false, false),
			at(BuyerZone::Eu, true, false),
		] {
			let v = determine(&exempt, &p);
			assert_eq!(v.effective(VatCode::Std27), VatCode::Aam);
			assert_eq!(v.effective(VatCode::Red05), VatCode::Aam);
			assert_eq!(v.note_key(), Some("vat.aam"));
		}

		// The two zero-VAT zones keep their own statutory notes: they already charge no VAT,
		// and their notes state the correct reason for the buyer.
		let v = determine(&exempt, &at(BuyerZone::Eu, true, true));
		assert_eq!(v.effective(VatCode::Std27), VatCode::Eufad37);
		assert_eq!(v.note_key(), Some("vat.eufad37"));

		let v = determine(&exempt, &at(BuyerZone::Third, true, false));
		assert_eq!(v.effective(VatCode::Std27), VatCode::Ho);
		assert_eq!(v.note_key(), Some("vat.ho"));

		// And a normal seller is untouched by any of it.
		assert_eq!(determine(&normal(), &at(BuyerZone::Dom, true, false)), Verdict::Product);
	}
}

// vim: ts=4
