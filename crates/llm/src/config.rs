//! Parsers for the `llm.profile.<role>` and `llm.price.<p>:<model>` setting values. Each is
//! also the setting's `check`, so a malformed value is refused when the row is written.

use mintworks_core::error::{ClResult, Error};

/// One `provider:model` entry of a profile. The model is in the provider's own spelling and may
/// itself contain `/` or `:`, so only the first `:` separates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
	pub provider: String,
	pub model: String,
}

/// `"stackit:google/gemma-4-31b, regolo:gemma-4-31b"` → the ordered fallback list.
///
/// # Errors
/// `E-CORE-SETTING` on an empty list or an entry without a provider or a model.
pub fn parse_profile(raw: &str) -> ClResult<Vec<Target>> {
	let targets = raw
		.split(',')
		.map(|entry| match entry.trim().split_once(':') {
			Some((p, m)) if !p.trim().is_empty() && !m.trim().is_empty() => {
				Ok(Target { provider: p.trim().to_owned(), model: m.trim().to_owned() })
			}
			_ => Err(Error::Setting(format!("'{}' is not provider:model", entry.trim()))),
		})
		.collect::<ClResult<Vec<_>>>()?;
	Ok(targets)
}

/// Input, output and cached-input price, integer micro-EUR per 1M tokens. Without `cached`, a
/// cache hit is billed at the full `input` rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Price {
	pub input: i64,
	pub output: i64,
	pub cached: Option<i64>,
}

/// `"150000,600000"` or `"150000,600000,3000"` → micro-EUR per 1M input, output and cached-input
/// tokens.
///
/// # Errors
/// `E-CORE-SETTING` unless the value is two or three non-negative integers separated by commas.
pub fn parse_price(raw: &str) -> ClResult<Price> {
	let ints = raw
		.split(',')
		.map(|s| s.trim().parse::<i64>().ok().filter(|n| *n >= 0))
		.collect::<Option<Vec<_>>>();
	match ints.as_deref() {
		Some(&[input, output]) => Ok(Price { input, output, cached: None }),
		Some(&[input, output, cached]) => Ok(Price { input, output, cached: Some(cached) }),
		_ => Err(Error::Setting(format!("'{raw}' is not input,output[,cached] micro-EUR"))),
	}
}

/// Blank passes: the registry treats blank as absent.
pub(crate) fn check_profile(raw: &str) -> ClResult<()> {
	if raw.trim().is_empty() { Ok(()) } else { parse_profile(raw).map(drop) }
}

pub(crate) fn check_price(raw: &str) -> ClResult<()> {
	if raw.trim().is_empty() { Ok(()) } else { parse_price(raw).map(drop) }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn profile_splits_on_the_first_colon_only() {
		let t = parse_profile("stackit:google/gemma-4-31b, regolo:gemma:4").unwrap();
		assert_eq!(t[0], Target { provider: "stackit".into(), model: "google/gemma-4-31b".into() });
		assert_eq!(t[1], Target { provider: "regolo".into(), model: "gemma:4".into() });
	}

	#[test]
	fn profile_refuses_malformed_entries() {
		for bad in ["", "stackit", "stackit:", ":model", "a:b,,c:d"] {
			assert!(parse_profile(bad).is_err(), "{bad}");
		}
		assert!(check_profile("  ").is_ok());
	}

	#[test]
	fn price_is_two_or_three_non_negative_integers() {
		assert_eq!(
			parse_price(" 150000 , 600000").unwrap(),
			Price { input: 150_000, output: 600_000, cached: None }
		);
		assert_eq!(
			parse_price("300000,1200000, 6000").unwrap(),
			Price { input: 300_000, output: 1_200_000, cached: Some(6_000) }
		);
		for bad in ["150000", "1.5,2", "-1,2", "1,2,-3", "1,2,3,4", "a,b", "1,2,"] {
			assert!(parse_price(bad).is_err(), "{bad}");
		}
		assert!(check_price("").is_ok());
	}
}

// vim: ts=4
