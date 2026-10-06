// SPDX-License-Identifier: MPL-2.0
//! Entitlements: what an org may do (`feature`), up to how much (`limit`) and how much it has
//! left to spend (`meter`). Every source — a subscription, a purchase, a reward, an operator's
//! gift, a trial — only inserts `grants` rows; consumption is the append-only `usage` ledger.
//!
//! The framework never gates a request by itself: only a call the app makes
//! ([`Entitle::require`], [`Entitle::consume`]) refuses.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use mintworks_core::app::AppBuilder;

pub mod routes;
pub mod service;
pub mod store;

pub use routes::routes;
pub use service::{
	AdminGrant, E_DENIED, E_EXHAUSTED, E_IDEM, E_UNKNOWN, Entitle, GrantReq, MeterView, Summary,
};
pub use store::{Debit, EntitleStore, Grant, NewGrant, Source};

/// How a key's active grants aggregate, fixed per key at declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
	/// Held while any active grant has `amount > 0`.
	Feature,
	/// The largest `amount` over active grants; `None` when there is none.
	Limit,
	/// Active grant amounts minus the usage drawn from those grants.
	Meter,
}

mintworks_core::str_enum!(Kind { Feature => "feature", Limit => "limit", Meter => "meter" });

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntitlementDef {
	pub key: String,
	pub kind: Kind,
}

impl EntitlementDef {
	pub fn feature(key: impl Into<String>) -> Self {
		Self { key: key.into(), kind: Kind::Feature }
	}

	pub fn limit(key: impl Into<String>) -> Self {
		Self { key: key.into(), kind: Kind::Limit }
	}

	pub fn meter(key: impl Into<String>) -> Self {
		Self { key: key.into(), kind: Kind::Meter }
	}
}

/// The declared keys, read back from `app.extensions`. Absent means nothing is declared, so
/// every key is `E-ENT-UNKNOWN`.
#[derive(Clone, Debug, Default)]
pub struct EntitlementRegistry(Arc<BTreeMap<String, Kind>>);

impl EntitlementRegistry {
	pub fn new(defs: impl IntoIterator<Item = EntitlementDef>) -> Self {
		Self(Arc::new(defs.into_iter().map(|d| (d.key, d.kind)).collect()))
	}

	pub fn kind(&self, key: &str) -> Option<Kind> {
		self.0.get(key).copied()
	}

	pub fn iter(&self) -> impl Iterator<Item = (&str, Kind)> {
		self.0.iter().map(|(k, v)| (k.as_str(), *v))
	}
}

/// Registers the declarations. The store is registered separately, as
/// `.extension(Arc::new(store) as Arc<dyn EntitleStore>)`, like every other feature crate's.
pub fn install(builder: AppBuilder, defs: impl IntoIterator<Item = EntitlementDef>) -> AppBuilder {
	builder.extension(EntitlementRegistry::new(defs))
}

// vim: ts=4
