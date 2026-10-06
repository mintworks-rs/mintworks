// SPDX-License-Identifier: MPL-2.0
//! Public identifiers: a short type prefix followed by a ULID.
//!
//! Rows carry `INTEGER PRIMARY KEY id` internally and `uid TEXT UNIQUE` publicly. Only the
//! `uid` ever appears in a URL or a response body — a sequential integer leaks row volume and
//! invites enumeration.
//!
//! Sixteen prefixes exist, one per table that has a `uid`. Tables without one are addressed by their
//! natural key instead: `settings` by `key`, `jobs` and `nav_submissions` by their integer `id`
//! (operator-only).

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::error::{ClResult, Error};

macro_rules! prefixed_id {
	($(#[$meta:meta])* $name:ident, $prefix:literal) => {
		$(#[$meta])*
		#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
		pub struct $name(String);

		impl $name {
			/// Mint a fresh id.
			pub fn generate() -> Self {
				Self(format!(concat!($prefix, "{}"), ulid::Ulid::new()))
			}

			/// Parse untrusted input, checking both the prefix and the ULID body, and
			/// rebuilding the value from the canonical rendering of what it parsed.
			///
			/// Crockford base32 is case-insensitive and aliases `I`/`L`/`O`, so keeping the
			/// input verbatim let two unequal values denote one row — and every `WHERE uid = ?`
			/// on the non-canonical one answered `E-CORE-NOTFOUND` with no signal.
			pub fn parse(s: &str) -> ClResult<Self> {
				let body = s.strip_prefix($prefix).ok_or_else(|| {
					Error::Validation(format!("expected an id starting with '{}'", $prefix))
				})?;
				let ulid = ulid::Ulid::from_string(body).map_err(|_| {
					Error::Validation(format!("malformed ULID in '{}' id", $prefix))
				})?;
				Ok(Self(format!(concat!($prefix, "{}"), ulid)))
			}

			pub fn as_str(&self) -> &str {
				&self.0
			}

			pub fn into_string(self) -> String {
				self.0
			}

			/// Wrap a value already read from the database, skipping validation. The DB is a
			/// trusted source; use [`Self::parse`] for anything that came off the wire.
			pub fn from_trusted(s: String) -> Self {
				Self(s)
			}
		}

		impl fmt::Display for $name {
			fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
				f.write_str(&self.0)
			}
		}

		impl FromStr for $name {
			type Err = Error;

			fn from_str(s: &str) -> ClResult<Self> {
				Self::parse(s)
			}
		}

		impl Serialize for $name {
			fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
				serializer.serialize_str(&self.0)
			}
		}

		impl<'de> Deserialize<'de> for $name {
			fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
				let s = String::deserialize(deserializer)?;
				Self::parse(&s).map_err(D::Error::custom)
			}
		}
	};
}

prefixed_id!(
	/// `accounts.uid`
	AccountId, "acc_"
);
prefixed_id!(
	/// `orgs.uid`
	OrgId, "org_"
);
prefixed_id!(
	/// `billing_parties.uid`
	PartyId, "prt_"
);
prefixed_id!(
	/// `sellers.uid`
	SellerId, "sel_"
);
prefixed_id!(
	/// `services.uid`
	ServiceId, "svc_"
);
prefixed_id!(
	/// `invoices.uid`
	InvoiceId, "inv_"
);
prefixed_id!(
	/// `api_keys.uid`. Not to be confused with `api_keys.prefix`, which is the first eight
	/// characters of the key material and serves as a lookup handle.
	ApiKeyId, "key_"
);
prefixed_id!(
	/// `payments.uid`
	PaymentId, "pay_"
);
prefixed_id!(
	/// `agent_runs.uid`
	RunId, "run_"
);
prefixed_id!(
	/// `agent_threads.uid`, in the app DB.
	ThreadId, "thr_"
);
prefixed_id!(
	/// `sources.uid`: one fetched web page, shared across orgs.
	SourceId, "src_"
);
prefixed_id!(
	/// `documents.uid`: one rendered app document.
	DocId, "doc_"
);
prefixed_id!(
	/// `refs.uid`: one redeemable code.
	RefId, "ref_"
);
prefixed_id!(
	/// `grants.uid`: one entitlement grant.
	GrantId, "grt_"
);
prefixed_id!(
	/// `offers.uid`: one sellable offer.
	OfferId, "ofr_"
);
prefixed_id!(
	/// `subscriptions.uid`: one org's subscription to a recurring offer.
	SubscriptionId, "sub_"
);

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_checks_prefix_and_body() {
		let id = AccountId::generate();
		assert!(id.as_str().starts_with("acc_"));
		assert_eq!(AccountId::parse(id.as_str()).unwrap(), id);

		// A well-formed ULID under the wrong prefix must not parse as an AccountId.
		assert!(AccountId::parse(OrgId::generate().as_str()).is_err());
		assert!(AccountId::parse("acc_not-a-ulid").is_err());
		assert!(AccountId::parse("01JQ9F0000000000000000000A").is_err());
	}

	/// Crockford base32 is case-insensitive and aliases `I`/`L`/`O`, and `parse` kept the raw
	/// input: the lowercase spelling never `==`'d the stored uid, so every `WHERE uid = ?`
	/// answered `E-CORE-NOTFOUND` and two unequal values denoted one row.
	#[test]
	fn parse_normalizes_a_lowercase_ulid_to_the_form_generate_writes() {
		let id = OrgId::generate();
		let lowered = id.as_str().to_lowercase();
		assert_ne!(lowered, id.as_str());
		assert_eq!(OrgId::parse(&lowered).unwrap(), id);
	}
}

// vim: ts=4
