// SPDX-License-Identifier: MPL-2.0
//! `LlmStore`: the `llm_usage` ledger and the per-subject budgets, in the core DB.

use async_trait::async_trait;
use mintworks_core::{ClResult, prelude::Timestamp};

/// What a ledger row paid for. `mintworks-search` writes `Search` and `Fetch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageKind {
	Llm,
	Search,
	Fetch,
}

impl UsageKind {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Llm => "llm",
			Self::Search => "search",
			Self::Fetch => "fetch",
		}
	}
}

/// One provider call, search or fetch. `account_id`/`org_id` carry no FK: the ledger outlives
/// the account, and an erased account's row is anonymised in place, so the ids are pseudonymous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageRow {
	pub at: Timestamp,
	pub run: Option<String>,
	pub account_id: Option<i64>,
	pub org_id: Option<i64>,
	pub subject: Option<String>,
	pub step: String,
	pub kind: UsageKind,
	pub provider: String,
	pub model: String,
	pub tokens_in: i64,
	pub tokens_out: i64,
	pub cost_micro_eur: i64,
	/// Not the call's first attempt: a fallback entry or a structured-output retry.
	pub retry: bool,
}

/// A `usage_grouped` grouping column. Each maps to a fixed SQL expression in the adapter, never
/// to interpolated text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageDim {
	Provider,
	Model,
	Step,
	/// The UTC date of `at`, `YYYY-MM-DD`.
	Day,
}

/// Rows with `since <= at < until`, grouped by `by` (order and duplicates do not matter; empty =
/// one total row, or none when nothing matches).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageQuery {
	pub since: Timestamp,
	pub until: Option<Timestamp>,
	pub by: Vec<UsageDim>,
	/// Only this org's rows; `None` is the deployment-wide ledger, rows without an org included.
	pub org_id: Option<i64>,
}

/// One group; a dimension not in `by` is `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageGroup {
	pub provider: Option<String>,
	pub model: Option<String>,
	pub step: Option<String>,
	pub day: Option<String>,
	pub calls: i64,
	pub input: i64,
	pub output: i64,
	pub micro_eur: i64,
}

#[async_trait]
pub trait LlmStore: Send + Sync + 'static {
	/// Append one row; the ledger is never updated or deleted from.
	async fn usage_insert(&self, row: &UsageRow) -> ClResult<()>;
	/// Lifetime `cost_micro_eur` of `subject`, 0 when it has no rows.
	async fn usage_cost_for_subject(&self, subject: &str) -> ClResult<i64>;
	/// `cost_micro_eur` of every row with `at >= since`, 0 when there are none.
	async fn usage_cost_since(&self, since: Timestamp) -> ClResult<i64>;
	/// Every row kind summed per group, ordered by the grouping columns.
	async fn usage_grouped(&self, q: &UsageQuery) -> ClResult<Vec<UsageGroup>>;
	/// `None` when the subject has no budget: it is uncapped.
	async fn budget_get(&self, subject: &str) -> ClResult<Option<i64>>;
	/// Insert or replace the subject's budget.
	async fn budget_set(&self, subject: &str, micro_eur: i64) -> ClResult<()>;
}

// vim: ts=4
