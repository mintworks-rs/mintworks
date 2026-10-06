// SPDX-License-Identifier: MPL-2.0
//! `LlmStore` over PostgreSQL: the `llm_usage` ledger (insert-only) and `llm_budgets`.

use async_trait::async_trait;
use mintworks_core::prelude::*;
use mintworks_llm::store::{LlmStore, UsageRow};

use crate::PgStore;
use crate::util::DbExt;

#[async_trait]
impl LlmStore for PgStore {
	async fn usage_insert(&self, row: &UsageRow) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO llm_usage (at, run_uid, account_id, org_id, subject, step, kind, provider,
				model, tokens_in, tokens_out, cost_micro_eur, retry)
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
		)
		.bind(row.at.0)
		.bind(&row.run)
		.bind(row.account_id)
		.bind(row.org_id)
		.bind(&row.subject)
		.bind(&row.step)
		.bind(row.kind.as_str())
		.bind(&row.provider)
		.bind(&row.model)
		.bind(row.tokens_in)
		.bind(row.tokens_out)
		.bind(row.cost_micro_eur)
		.bind(i64::from(row.retry))
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn usage_cost_for_subject(&self, subject: &str) -> ClResult<i64> {
		sqlx::query_scalar(
			"SELECT COALESCE(SUM(cost_micro_eur), 0)::BIGINT FROM llm_usage WHERE subject = $1",
		)
		.bind(subject)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn usage_cost_since(&self, since: Timestamp) -> ClResult<i64> {
		sqlx::query_scalar(
			"SELECT COALESCE(SUM(cost_micro_eur), 0)::BIGINT FROM llm_usage WHERE at >= $1",
		)
		.bind(since.0)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn budget_get(&self, subject: &str) -> ClResult<Option<i64>> {
		sqlx::query_scalar("SELECT budget_micro_eur FROM llm_budgets WHERE subject = $1")
			.bind(subject)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn budget_set(&self, subject: &str, micro_eur: i64) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO llm_budgets (subject, budget_micro_eur, updated_at) VALUES ($1, $2, $3)
			 ON CONFLICT (subject) DO UPDATE
			 SET budget_micro_eur = excluded.budget_micro_eur, updated_at = excluded.updated_at",
		)
		.bind(subject)
		.bind(micro_eur)
		.bind(Timestamp::now().0)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}
}

// vim: ts=4
