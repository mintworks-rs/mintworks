// SPDX-License-Identifier: MPL-2.0
//! Shared wire value types: the three-state [`Patch`] and [`Timestamp`].

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::Error as _};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Three-state PATCH field.
///
/// | JSON | Variant |
/// |---|---|
/// | key absent | `Undefined` — leave the column alone |
/// | `"key": null` | `Null` — clear it to `NULL` |
/// | `"key": <v>` | `Value(v)` — set it |
///
/// `Undefined` is only reachable through `#[serde(default)]` on the containing field: serde
/// cannot otherwise tell an absent key from a present one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Patch<T> {
	#[default]
	Undefined,
	Null,
	Value(T),
}

impl<T> Patch<T> {
	pub fn is_undefined(&self) -> bool {
		matches!(self, Self::Undefined)
	}

	pub fn is_null(&self) -> bool {
		matches!(self, Self::Null)
	}

	pub fn value(&self) -> Option<&T> {
		match self {
			Self::Value(v) => Some(v),
			_ => None,
		}
	}

	/// `Undefined -> None`, `Null -> Some(None)`, `Value(v) -> Some(Some(v))`.
	pub fn as_option(&self) -> Option<Option<&T>> {
		match self {
			Self::Undefined => None,
			Self::Null => Some(None),
			Self::Value(v) => Some(Some(v)),
		}
	}
}

impl<T: Serialize> Serialize for Patch<T> {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		match self {
			Self::Undefined | Self::Null => serializer.serialize_none(),
			Self::Value(v) => v.serialize(serializer),
		}
	}
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Patch<T> {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		Option::<T>::deserialize(deserializer).map(|opt| match opt {
			None => Self::Null,
			Some(v) => Self::Value(v),
		})
	}
}

/// Unix seconds UTC. Stored as `INTEGER`, carried on the wire as ISO-8601 UTC with a `Z` suffix and
/// second precision.
///
/// Calendar dates — where the date itself is the legal fact — are a different type and are
/// not this; they stay `TEXT 'YYYY-MM-DD'` and belong to the crate that owns them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
	pub fn now() -> Self {
		Self(OffsetDateTime::now_utc().unix_timestamp())
	}

	pub fn to_rfc3339(self) -> Option<String> {
		OffsetDateTime::from_unix_timestamp(self.0).ok()?.format(&Rfc3339).ok()
	}

	pub fn parse_rfc3339(s: &str) -> Option<Self> {
		OffsetDateTime::parse(s, &Rfc3339).ok().map(|dt| Self(dt.unix_timestamp()))
	}
}

impl Serialize for Timestamp {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		let s = self.to_rfc3339().ok_or_else(|| S::Error::custom("timestamp out of range"))?;
		serializer.serialize_str(&s)
	}
}

impl<'de> Deserialize<'de> for Timestamp {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		let s = String::deserialize(deserializer)?;
		Self::parse_rfc3339(&s).ok_or_else(|| D::Error::custom("expected ISO-8601 UTC timestamp"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// ISO-8601 UTC, `Z`-suffixed, second precision: a documented wire contract, so the serde
	/// path is asserted and not just the helpers.
	#[test]
	fn timestamp_wire_round_trip() {
		let t = Timestamp(1_772_806_951);
		assert_eq!(t.to_rfc3339().as_deref(), Some("2026-03-06T14:22:31Z"));
		assert_eq!(Timestamp::parse_rfc3339("2026-03-06T14:22:31Z"), Some(t));
		assert_eq!(Timestamp::parse_rfc3339("not a timestamp"), None);
		assert_eq!(serde_json::to_string(&t).unwrap(), "\"2026-03-06T14:22:31Z\"");
	}

	#[test]
	fn patch_distinguishes_null_from_value() {
		let null: Patch<i32> = serde_json::from_str("null").unwrap();
		let value: Patch<i32> = serde_json::from_str("7").unwrap();
		assert!(null.is_null());
		assert_eq!(value.value(), Some(&7));
		assert!(Patch::<i32>::default().is_undefined());
	}
}

// vim: ts=4
