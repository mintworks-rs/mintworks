// SPDX-License-Identifier: MPL-2.0
//! `LlmStore` over SQLite: the `llm_usage` ledger (insert-only) and `llm_budgets`.

use async_trait::async_trait;
use mintworks_core::prelude::*;
use mintworks_llm::store::{LlmStore, UsageDim, UsageGroup, UsageQuery, UsageRow};

use crate::SqliteStore;
use crate::util::DbExt;

#[async_trait]
impl LlmStore for SqliteStore {
	async fn usage_insert(&self, row: &UsageRow) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO llm_usage (at, run_uid, account_id, org_id, subject, step, kind, provider,
				model, tokens_in, tokens_out, cost_micro_eur, retry)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
		.bind(row.retry)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn usage_cost_for_subject(&self, subject: &str) -> ClResult<i64> {
		sqlx::query_scalar(
			"SELECT COALESCE(SUM(cost_micro_eur), 0) FROM llm_usage WHERE subject = ?",
		)
		.bind(subject)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn usage_cost_since(&self, since: Timestamp) -> ClResult<i64> {
		sqlx::query_scalar("SELECT COALESCE(SUM(cost_micro_eur), 0) FROM llm_usage WHERE at >= ?")
			.bind(since.0)
			.fetch_one(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn usage_grouped(&self, q: &UsageQuery) -> ClResult<Vec<UsageGroup>> {
		let dims = [
			(UsageDim::Provider, "provider"),
			(UsageDim::Model, "model"),
			(UsageDim::Step, "step"),
			(UsageDim::Day, "strftime('%Y-%m-%d', at, 'unixepoch')"),
		];
		let cols: Vec<&str> =
			dims.iter().map(|(d, e)| if q.by.contains(d) { *e } else { "NULL" }).collect();
		let group: Vec<&str> =
			dims.iter().filter(|(d, _)| q.by.contains(d)).map(|(_, e)| *e).collect();
		let tail = if group.is_empty() {
			"HAVING COUNT(*) > 0".to_owned()
		} else {
			format!("GROUP BY {0} ORDER BY {0}", group.join(", "))
		};
		// Only the fixed fragments above reach the SQL text; the bounds are bound.
		let sql = format!(
			"SELECT {}, COUNT(*), COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0),
				COALESCE(SUM(cost_micro_eur), 0)
			 FROM llm_usage WHERE at >= ? AND at < ? AND (? IS NULL OR org_id = ?) {tail}",
			cols.join(", ")
		);
		let rows: Vec<(
			Option<String>,
			Option<String>,
			Option<String>,
			Option<String>,
			i64,
			i64,
			i64,
			i64,
		)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
			.bind(q.since.0)
			.bind(q.until.map_or(i64::MAX, |t| t.0))
			.bind(q.org_id)
			.bind(q.org_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()?;
		Ok(rows
			.into_iter()
			.map(|(provider, model, step, day, calls, input, output, micro_eur)| UsageGroup {
				provider,
				model,
				step,
				day,
				calls,
				input,
				output,
				micro_eur,
			})
			.collect())
	}

	async fn budget_get(&self, subject: &str) -> ClResult<Option<i64>> {
		sqlx::query_scalar("SELECT budget_micro_eur FROM llm_budgets WHERE subject = ?")
			.bind(subject)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn budget_set(&self, subject: &str, micro_eur: i64) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO llm_budgets (subject, budget_micro_eur, updated_at) VALUES (?, ?, ?)
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
