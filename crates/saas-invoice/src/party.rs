//! Handlers for org-owned billing parties.
//!
//! Org scoping lives in [`Invoices`], not here: another org's `pty_` uid reads as
//! [`Error::NotFound`] and never as a 403, so the API does not confirm that it exists.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::prelude::*;

use crate::routes::Page;
use crate::service_api::Invoices;
use crate::store::{BillingParty, PartyPatch};

/// Every assigned ISO-3166-1 alpha-2 code, concatenated. A `&str` scanned in 2-byte chunks
/// rather than a 249-entry array: same check, a fraction of the source, no dependency.
///
/// `EL` is deliberately absent. It is the VIES VAT-id prefix for Greece — [`crate::vies`] is
/// right to accept it there — but the *country* column is `GR`.
const ISO_3166_ALPHA2: &str = concat!(
	"ADAEAFAGAIALAMAOAQARASATAUAWAXAZBABBBDBEBFBGBHBIBJBLBMBNBOBQBRBSBTBVBWBYBZCACCCD",
	"CFCGCHCICKCLCMCNCOCRCUCVCWCXCYCZDEDJDKDMDODZECEEEGEHERESETFIFJFKFMFOFRGAGBGDGEGF",
	"GGGHGIGLGMGNGPGQGRGSGTGUGWGYHKHMHNHRHTHUIDIEILIMINIOIQIRISITJEJMJOJPKEKGKHKIKMKN",
	"KPKRKWKYKZLALBLCLILKLRLSLTLULVLYMAMCMDMEMFMGMHMKMLMMMNMOMPMQMRMSMTMUMVMWMXMYMZNA",
	"NCNENFNGNINLNONPNRNUNZOMPAPEPFPGPHPKPLPMPNPRPSPTPWPYQARERORSRURWSASBSCSDSESGSHSI",
	"SJSKSLSMSNSOSRSSSTSVSXSYSZTCTDTFTGTHTJTKTLTMTNTOTRTTTVTWTZUAUGUMUSUYUZVAVCVEVGVI",
	"VNVUWFWSYEYTZAZMZW",
);

/// Is `code` an assigned ISO-3166-1 alpha-2 country code? Uppercase only — callers normalise
/// first.
#[must_use]
pub fn is_iso_alpha2(code: &str) -> bool {
	code.len() == 2 && ISO_3166_ALPHA2.as_bytes().chunks(2).any(|k| k == code.as_bytes())
}

/// A country as the schema and NAV both require it: ISO-3166-1 alpha-2, uppercase.
///
/// `common.xsd` `CountryCodeType` is `[A-Z]{2}`, and [`crate::taxrule::BuyerZone::of`]
/// compares against `EU_COUNTRIES`, so anything else silently misclassifies the sale — a
/// domestic buyer stored as `"Magyarország"` reads as `Third` and issues at 0% VAT.
pub fn normalise_country(raw: &str) -> ClResult<String> {
	let c = raw.trim().to_ascii_uppercase();
	if is_iso_alpha2(&c) {
		Ok(c)
	} else {
		Err(Error::coded(
			StatusCode::BAD_REQUEST,
			"E-INV-COUNTRY",
			"country must be an ISO-3166-1 alpha-2 code",
		))
	}
}

/// `GET /api/billing-parties`
pub async fn list(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Page<BillingParty>>> {
	Ok(Json(Page::all(Invoices::new(app).list_parties(&ctx).await?)))
}

/// `GET /api/billing-parties/{uid}`
pub async fn get(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<BillingParty>> {
	Ok(Json(Invoices::new(app).party(&ctx, &uid).await?))
}

/// `POST /api/billing-parties`
pub async fn create(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<PartyPatch>,
) -> ClResult<(StatusCode, Json<BillingParty>)> {
	let party = Invoices::new(app).create_party(&ctx, &body).await?;
	Ok((StatusCode::CREATED, Json(party)))
}

/// `PATCH /api/billing-parties/{uid}`
pub async fn patch(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(body): Json<PartyPatch>,
) -> ClResult<Json<BillingParty>> {
	Ok(Json(Invoices::new(app).update_party(&ctx, &uid, &body).await?))
}

/// `DELETE /api/billing-parties/{uid}` — a hard delete. Issued invoices are unaffected: they
/// carry a frozen buyer snapshot, not a reference to this row.
pub async fn delete(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<StatusCode> {
	Invoices::new(app).delete_party(&ctx, &uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn code(e: &Error) -> &'static str {
		e.parts().1
	}

	#[test]
	fn country_normalises_to_alpha2_uppercase() {
		assert_eq!(normalise_country("hu").unwrap(), "HU");
		assert_eq!(normalise_country(" Hu ").unwrap(), "HU");
		assert_eq!(normalise_country("DE").unwrap(), "DE");

		assert_eq!(normalise_country("us").unwrap(), "US");

		// `EL`/`UH`/`GE`-for-DE used to pass the `[A-Z]{2}` shape test, fall through
		// `BuyerZone::of` to `Third` and issue at 0% VAT.
		for bad in ["Magyarország", "HUN", "", "H1", "H", "hu hu", "XX", "EL", "UH", "QQ"] {
			assert_eq!(code(&normalise_country(bad).unwrap_err()), "E-INV-COUNTRY", "{bad}");
		}
	}

	/// The chunked scan in `is_iso_alpha2` silently misaligns on an odd length: one dropped
	/// character turns every later code into garbage that still compiles and still passes
	/// `normalise_country`'s happy path.
	#[test]
	fn the_country_table_is_well_formed() {
		assert_eq!(ISO_3166_ALPHA2.len() % 2, 0);
		let codes: Vec<&[u8]> = ISO_3166_ALPHA2.as_bytes().chunks(2).collect();
		assert_eq!(codes.len(), 249);
		assert!(codes.iter().all(|c| c.iter().all(u8::is_ascii_uppercase)));
		assert!(codes.windows(2).all(|w| w[0] < w[1]), "sorted and duplicate-free");
	}

	#[test]
	fn every_eu_country_is_an_assigned_alpha2() {
		for c in crate::taxrule::EU_COUNTRIES {
			assert!(is_iso_alpha2(c), "{c}");
		}
	}
}

// vim: ts=4
