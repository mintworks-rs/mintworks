//! `EntitleStore`: the `grants` rows and the `usage` ledger drawn from them.

use async_trait::async_trait;
use saas_core::error::ClResult;
use saas_core::ids::{GrantId, OrgId};
use saas_core::prelude::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Source {
	Subscription,
	Purchase,
	Reward,
	Manual,
	Trial,
}

saas_core::str_enum!(Source {
	Subscription => "SUBSCRIPTION",
	Purchase => "PURCHASE",
	Reward => "REWARD",
	Manual => "MANUAL",
	Trial => "TRIAL",
});

/// One `grants` row plus what has been drawn from it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
	#[serde(skip)]
	pub id: i64,
	pub uid: GrantId,
	#[serde(skip)]
	pub org_id: i64,
	pub key: String,
	pub amount: i64,
	pub valid_from: Timestamp,
	/// `None` = forever.
	pub valid_until: Option<Timestamp>,
	pub source: Source,
	pub source_ref: Option<String>,
	pub created_at: Timestamp,
	/// `SUM(usage.amount)` over the rows drawn from this grant.
	pub used: i64,
}

#[derive(Clone, Debug)]
pub struct NewGrant {
	pub uid: GrantId,
	pub org_id: i64,
	pub key: String,
	pub amount: i64,
	pub valid_from: Timestamp,
	pub valid_until: Option<Timestamp>,
	pub source: Source,
	/// A `NULL` ref is never deduplicated: SQL `NULL`s are distinct in a unique index.
	pub source_ref: Option<String>,
}

/// One debit of `amount` (> 0) from `key`'s active grants at `at`.
#[derive(Clone, Debug)]
pub struct Debit {
	pub org_id: i64,
	pub key: String,
	pub amount: i64,
	pub idem_key: String,
	pub account_id: Option<i64>,
	pub at: Timestamp,
	/// `charge`: debit even past the balance. `consume` leaves it `false`.
	pub overdraw: bool,
}

#[async_trait]
pub trait EntitleStore: Send + Sync {
	/// Idempotent on `(org_id, key, source, source_ref)`: a repeat returns the existing row
	/// unchanged.
	async fn grant_insert(&self, new: &NewGrant) -> ClResult<Grant>;

	/// Grants active at `now` (`valid_from <= now < valid_until`), all keys when `key` is
	/// `None`, in drain order: soonest `valid_until` first, forever last, then `id`.
	async fn grants_active(
		&self,
		org_id: i64,
		key: Option<&str>,
		now: Timestamp,
	) -> ClResult<Vec<Grant>>;

	/// Every grant of the org, expired included, newest first.
	async fn grants_of_org(&self, org_id: i64) -> ClResult<Vec<Grant>>;

	/// Moves `valid_until` on the org's grants from `(source, source_ref)` later, never earlier:
	/// only a finite `valid_until` below the new one (`None` = forever) changes, so a replayed
	/// settlement cannot shorten a grant. Shortening is [`Self::grants_cut`]'s. Rows touched.
	async fn grants_extend(
		&self,
		org_id: i64,
		source: Source,
		source_ref: &str,
		valid_until: Option<Timestamp>,
	) -> ClResult<u64>;

	/// `valid_until = min(valid_until, at)` on the org's grants from `(source, source_ref)`.
	/// Usage stays: no clawback. Rows touched.
	async fn grants_cut(
		&self,
		org_id: i64,
		source: Source,
		source_ref: &str,
		at: Timestamp,
	) -> ClResult<u64>;

	/// Check-and-debit in one transaction. `false` only when `!overdraw` and the balance is
	/// short; a repeated `(org_id, idem_key)` with the same key and amount debits nothing and
	/// answers `true`, with any other debit it is `E-ENT-IDEM` (409). Must be atomic
	/// against a concurrent debit: two consumes never both pass the balance check.
	async fn usage_debit(&self, debit: &Debit) -> ClResult<bool>;

	/// `orgs.id` for a public org uid.
	async fn entitle_org_id(&self, uid: &OrgId) -> ClResult<Option<i64>>;
}

// vim: ts=4
