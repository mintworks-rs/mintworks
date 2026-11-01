// SPDX-License-Identifier: MPL-2.0
//! Entitlements: what an org may do (`feature`), up to how much (`limit`) and how much it has
//! left to spend (`meter`). Every source — a subscription, a purchase, a reward, an operator's
//! gift, a trial — only inserts `grants` rows; consumption is the append-only `usage` ledger.
//!
//! The framework never gates a request by itself: only a call the app makes
//! ([`Entitle::require`], [`Entitle::consume`]) refuses.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mintworks_core::account_data::AccountDataHook;
use mintworks_core::app::{App, AppBuilder};
use mintworks_core::error::{ClResult, Error};
use mintworks_core::event::Event;
use mintworks_core::ids::AccountId;
use mintworks_core::prelude::Timestamp;
use serde_json::{Value, json};

pub mod routes;
pub mod service;
pub mod store;

pub use routes::routes;
pub use service::{
	AdminGrant, E_DENIED, E_EXHAUSTED, E_IDEM, E_SEATS_FULL, E_UNKNOWN, Entitle, GrantReq,
	MeterView, Summary,
};
pub use store::{Debit, EntitleStore, Grant, NewGrant, SeatOutcome, Source};

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

/// Registers the declarations, the seat GDPR hook and the `MembershipRemoved` seat release.
/// The store is registered separately, as
/// `.extension(Arc::new(store) as Arc<dyn EntitleStore>)`, like every other feature crate's.
pub fn install(builder: AppBuilder, defs: impl IntoIterator<Item = EntitlementDef>) -> AppBuilder {
	let cell = Arc::new(OnceLock::new());
	let hook_app = Arc::clone(&cell);
	builder
		.extension(EntitlementRegistry::new(defs))
		.account_data_hook(Arc::new(SeatHook { app: cell }))
		.on_init(move |app: App| async move {
			let _ = hook_app.set(app);
			Ok(())
		})
		.on_event(seat_events)
}

/// On `MembershipRemoved`, releases the seats the account no longer qualifies for.
/// Best-effort: a lost event leaves a stale row that holds no capacity and that the next
/// `seat_assign` in its org releases.
async fn seat_events(app: App, ev: Event) -> ClResult<()> {
	let Event::MembershipRemoved { account, .. } = ev else {
		return Ok(());
	};
	if app.extensions.get::<Arc<dyn EntitleStore>>().is_none() {
		return Ok(());
	}
	Entitle::from_app(&app)?.release_stale_seats(account.as_str()).await?;
	Ok(())
}

/// GDPR: export lists the account's active seats; erase releases them and keeps the rows.
/// No `org_deleted`: seat rows go with the org through the FK cascade.
struct SeatHook {
	/// Set by `install`'s `on_init`: the hook is registered before an `App` exists.
	app: Arc<OnceLock<App>>,
}

impl SeatHook {
	/// `None` when the app registered no `EntitleStore` (it then holds no seats) or the
	/// account is unknown.
	async fn target(&self, acc: &AccountId) -> ClResult<Option<(Arc<dyn EntitleStore>, i64)>> {
		let app = self
			.app
			.get()
			.ok_or_else(|| Error::internal("entitle seat hook called before on_init"))?;
		let Some(store) = app.extensions.get::<Arc<dyn EntitleStore>>().cloned() else {
			return Ok(None);
		};
		Ok(app.store.account_for_token(acc.as_str()).await?.map(|a| (store, a.id)))
	}
}

#[async_trait]
impl AccountDataHook for SeatHook {
	fn name(&self) -> &'static str {
		"seats"
	}

	async fn export(&self, acc: &AccountId) -> ClResult<Value> {
		let Some((store, id)) = self.target(acc).await? else {
			return Ok(json!([]));
		};
		let seats = store.seats_of_account(id).await?;
		Ok(seats
			.into_iter()
			.map(|(org, key, at)| json!({"org": org.as_str(), "key": key, "assignedAt": at}))
			.collect())
	}

	async fn erase(&self, acc: &AccountId) -> ClResult<()> {
		if let Some((store, id)) = self.target(acc).await? {
			store.seats_release_account(id, Timestamp::now()).await?;
		}
		Ok(())
	}
}

// vim: ts=4
