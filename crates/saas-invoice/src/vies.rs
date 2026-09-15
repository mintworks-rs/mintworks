//! EU VAT id validation against VIES, cached `settings['vies.cache_days']`.
//!
//! A failed or unreachable VIES call is an error the caller must handle. It never returns
//! "valid", and it never downgrades to `valid: false` — either would let a VIES outage flip
//! a domestic-VAT invoice into reverse charge, or the reverse. A stale cached row is a cache
//! miss, not an answer (`claude-docs/db-schema.md` §`vies_checks`).

use std::time::Duration;

use axum::http::StatusCode;
use saas_core::{App, http, prelude::*};
use serde::Deserialize;

use crate::taxrule::EU_COUNTRIES;

const VIES_URL: &str = "https://ec.europa.eu/taxation_customs/vies/rest-api/check-vat-number";
const TIMEOUT: Duration = Duration::from_secs(15);

/// VIES prefixes that are not ISO country codes: Greece files as `EL`, and Northern Ireland
/// stayed in the EU VAT area as `XI` after Brexit.
const EXTRA_PREFIXES: [&str; 2] = ["EL", "XI"];

/// One VIES answer, fresh or from the cache. Mirrors the `vies_checks` row plus `cached`.
#[derive(Debug, Clone)]
pub struct ViesResult {
	pub eu_vat_id: String,
	pub valid: bool,
	pub name: Option<String>,
	pub address: Option<String>,
	/// The VIES consultation number: the proof that the check happened.
	pub request_id: Option<String>,
	pub checked_at: Timestamp,
	pub cached: bool,
}

/// Uppercase, strip separators, and split into `(full, country prefix, number)`.
///
/// `full` is also the only form NAV accepts: `common:CommunityVatNumberType` is
/// `[A-Z]{2}[0-9A-Z]{2,13}`, 4-15 characters, filed as `communityVatNumber`.
///
/// The ISO spelling is accepted from callers and rewritten to the VIES one: Greece is `GR` in
/// ISO 3166 and `EL` to VIES and NAV. Posting `countryCode: "GR"` earned a `userError`
/// [`is_answer`] rejects, so every Greek buyer was `E-NAV-VIES-UNAVAILABLE` and uninvoiceable.
/// `EU_COUNTRIES` keeps `GR`, because [`crate::taxrule::BuyerZone::of`] compares real ISO codes.
///
/// # Errors
/// `E-CORE-VALIDATION` when the prefix is not an EU VAT prefix or the number is not 2-13
/// alphanumerics.
pub fn normalise(eu_vat_id: &str) -> ClResult<(String, String, String)> {
	let raw: String = eu_vat_id
		.chars()
		.filter(char::is_ascii_alphanumeric)
		.map(|c| c.to_ascii_uppercase())
		.collect();
	let bad = || Error::validation(format!("'{eu_vat_id}' is not an EU VAT id"));
	if raw.len() < 4 || raw.len() > 15 {
		return Err(bad());
	}
	let (cc, number) = raw.split_at(2);
	if !EU_COUNTRIES.contains(&cc) && !EXTRA_PREFIXES.contains(&cc) {
		return Err(bad());
	}
	let cc = if cc == "GR" { "EL" } else { cc };
	Ok((format!("{cc}{number}"), cc.to_owned(), number.to_owned()))
}

fn unavailable() -> Error {
	Error::coded(
		StatusCode::BAD_GATEWAY,
		"E-NAV-VIES-UNAVAILABLE",
		"the VIES service is unreachable, try again later",
	)
}

/// Validate `eu_vat_id`, answering from the cache while the row is fresh.
///
/// `requester` is the seller's own EU VAT id; supplying it makes VIES return a
/// `requestIdentifier`, which is the only durable proof the check was made.
///
/// # Errors
/// `E-CORE-VALIDATION` for a malformed id; `E-NAV-VIES-UNAVAILABLE` (502) when VIES cannot
/// answer — never a silent `valid: false`.
pub async fn check(app: &App, eu_vat_id: &str, requester: Option<&str>) -> ClResult<ViesResult> {
	let (full, cc, number) = normalise(eu_vat_id)?;
	let ttl = app.settings.int("vies.cache_days").await? * 86_400;
	let store = crate::service_api::store(app)?;
	if let Some(hit) = store.vies_cached(&full, ttl).await? {
		return Ok(hit);
	}
	let fresh = call(&full, &cc, &number, requester).await?;
	store.vies_store(&fresh).await?;
	Ok(fresh)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
	valid: bool,
	request_identifier: Option<String>,
	name: Option<String>,
	address: Option<String>,
	/// `"VALID"` / `"INVALID"` on an answer; a service condition otherwise.
	user_error: Option<String>,
}

/// VIES reports a member state being down, and its own input and throttling conditions,
/// inside a 200 response with `valid: false`. Only `VALID` and `INVALID` (and an absent
/// field) are answers; **everything else is an outage**, because a denylist silently
/// downgrades to `valid: false` every condition VIES adds after this was written —
/// `IP_BLOCKED` and `INVALID_REQUESTER_INFO` were already among them — and `store` then
/// caches that non-answer for the whole `vies.cache_days` window.
fn is_answer(user_error: Option<&str>) -> bool {
	matches!(user_error, None | Some("VALID" | "INVALID"))
}

/// VIES blanks undisclosed fields with `---`.
fn field(v: Option<String>) -> Option<String> {
	v.filter(|s| !s.trim().is_empty() && s.trim() != "---")
}

async fn call(full: &str, cc: &str, number: &str, requester: Option<&str>) -> ClResult<ViesResult> {
	let mut body = serde_json::json!({ "countryCode": cc, "vatNumber": number });
	if let Some(req) = requester {
		let (_, rcc, rnum) = normalise(req)?;
		body["requesterMemberStateCode"] = rcc.into();
		body["requesterNumber"] = rnum.into();
	}

	let (status, bytes) = http::post(
		VIES_URL,
		&[("content-type", "application/json")],
		body.to_string().into_bytes(),
		TIMEOUT,
	)
	.await
	.map_err(|_| unavailable())?;
	if !status.is_success() {
		tracing::warn!(%status, "VIES returned an error status");
		return Err(unavailable());
	}

	let reply: Reply = serde_json::from_slice(&bytes).map_err(|e| {
		tracing::warn!(error = %e, "unparseable VIES reply");
		unavailable()
	})?;
	if !is_answer(reply.user_error.as_deref()) {
		tracing::warn!(user_error = ?reply.user_error, "VIES returned a service condition, not an answer");
		return Err(unavailable());
	}
	Ok(ViesResult {
		eu_vat_id: full.to_owned(),
		valid: reply.valid,
		name: field(reply.name),
		address: field(reply.address),
		request_id: reply.request_identifier,
		checked_at: Timestamp::now(),
		cached: false,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The ISO spelling was accepted, stored on the party and posted to VIES verbatim,
	/// which answers a `userError` — so every Greek buyer was permanently un-invoiceable.
	#[test]
	fn the_iso_spelling_of_greece_normalises_to_the_vies_one() {
		assert_eq!(
			normalise("hu-12345678").expect("hu"),
			("HU12345678".into(), "HU".into(), "12345678".into())
		);
		// Northern Ireland stayed in the VAT area as XI; a non-member is not an id at all.
		assert!(normalise("XI123456789").is_ok());
		assert!(normalise("US123456789").is_err());
		assert!(normalise("HU1").is_err());

		assert_eq!(
			normalise("GR123456789").expect("gr"),
			("EL123456789".into(), "EL".into(), "123456789".into())
		);
		// Idempotent: the form written back onto the party normalises to itself.
		assert_eq!(normalise("EL123456789").expect("el"), normalise("gr 123456789").expect("gr"));
	}

	/// Only the two verdicts are answers. Anything else — including a condition VIES has not
	/// invented yet — is an outage, or it caches as `valid: false` and turns a reverse-charge
	/// invoice into a domestic-VAT one.
	#[test]
	fn only_a_verdict_is_an_answer() {
		assert!(is_answer(None));
		assert!(is_answer(Some("VALID")));
		assert!(is_answer(Some("INVALID")));
		assert!(!is_answer(Some("MS_UNAVAILABLE")));
		assert!(!is_answer(Some("IP_BLOCKED")));
		assert!(!is_answer(Some("SOMETHING_NEW")));
	}

	#[test]
	fn undisclosed_fields_are_none() {
		assert_eq!(field(Some("---".into())), None);
		assert_eq!(field(Some("  ".into())), None);
		assert_eq!(field(Some("Acme Kft.".into())), Some("Acme Kft.".into()));
	}
}

// vim: ts=4
