// SPDX-License-Identifier: MPL-2.0
//! `EntitleStore`: the `grants` rows and the `usage` ledger drawn from them.

use async_trait::async_trait;
use mintworks_core::error::ClResult;
use mintworks_core::ids::{AccountId, GrantId, OrgId};
use mintworks_core::prelude::Timestamp;
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

mintworks_core::str_enum!(Source {
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

	/// Seats the account on `(org_id, key)`, in one write transaction: two concurrent assigns
	/// never both take the last seat. Inside it, in order: an account that is not an accepted
	/// member of `org_id` or an active org below it is [`SeatOutcome::NotMember`]; one already
	/// seated is [`SeatOutcome::AlreadySeated`] whatever the count; the limit is
	/// `MAX(amount)` of the `(org_id, key)` grants active at `at`, 0 with none, and only active
	/// seats whose holder still qualifies the same way count against it — a seat whose
	/// release event was lost holds no capacity.
	async fn seat_assign(
		&self,
		org_id: i64,
		key: &str,
		account_id: i64,
		at: Timestamp,
	) -> ClResult<SeatOutcome>;

	/// Sets `released_at` on the account's active seat; `false` when it held none.
	async fn seat_release(
		&self,
		org_id: i64,
		key: &str,
		account_id: i64,
		at: Timestamp,
	) -> ClResult<bool>;

	/// Active seats of `(org_id, key)` whose holder is still an accepted member of `org_id` or
	/// an active org below it, oldest first.
	async fn seats_of_org(&self, org_id: i64, key: &str) -> ClResult<Vec<(AccountId, Timestamp)>>;

	/// Active seats of the account across orgs: `(org uid, key, assigned_at)`, oldest first.
	async fn seats_of_account(&self, account_id: i64) -> ClResult<Vec<(OrgId, String, Timestamp)>>;

	/// Releases every active seat of the account (GDPR erase); the rows stay. Rows touched.
	async fn seats_release_account(&self, account_id: i64, at: Timestamp) -> ClResult<u64>;

	/// Releases every active seat of the account whose holder no longer qualifies for it (no
	/// accepted membership in the seat's org or an active org below it), atomically, so a
	/// concurrent re-join cannot lose a valid seat. Returns `(org uid, key)` per released row.
	async fn seats_release_stale(
		&self,
		account_id: i64,
		at: Timestamp,
	) -> ClResult<Vec<(OrgId, String)>>;
}

/// What [`EntitleStore::seat_assign`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeatOutcome {
	Inserted,
	AlreadySeated,
	/// The qualifying count had reached the limit, carried here; nothing was written.
	Full(i64),
	/// The account is no member of the org or its active subtree; nothing was written.
	NotMember,
}

// vim: ts=4
