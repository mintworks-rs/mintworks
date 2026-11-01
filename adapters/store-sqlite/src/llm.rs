// SPDX-License-Identifier: MPL-2.0
//! `LlmStore` over SQLite: the `llm_usage` ledger (insert-only), `llm_budgets` and
//! `llm_budget_windows`.

use async_trait::async_trait;
use mintworks_core::prelude::*;
use mintworks_llm::store::{LlmStore, UsageDim, UsageGroup, UsageKind, UsageQuery, UsageRow};

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
			(UsageDim::Subject, "subject"),
			(UsageDim::Account, "account_id"),
		];
		let cols: Vec<&str> =
			dims.iter().map(|(d, e)| if q.by.contains(d) { *e } else { "NULL" }).collect();
		let group: Vec<&str> =
			dims.iter().filter(|(d, _)| q.by.contains(d)).map(|(_, e)| *e).collect();
		let tail = if group.is_empty() {
			"HAVING COUNT(*) > 0".to_owned()
		} else {
			// NULLS FIRST spelled out: SQLite sorts NULL first, PostgreSQL last.
			let order: Vec<String> = group.iter().map(|g| format!("{g} NULLS FIRST")).collect();
			format!("GROUP BY {} ORDER BY {}", group.join(", "), order.join(", "))
		};
		// Only the fixed fragments above reach the SQL text; the bounds are bound.
		let sql = format!(
			"SELECT {}, COUNT(*), COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0),
				COALESCE(SUM(cost_micro_eur), 0)
			 FROM llm_usage WHERE at >= ? AND at < ? AND (? IS NULL OR org_id = ?)
				AND (? IS NULL OR subject = ?) AND (? IS NULL OR kind = ?)
				AND (? IS NULL OR account_id = ?) {tail}",
			cols.join(", ")
		);
		let rows: Vec<(
			Option<String>,
			Option<String>,
			Option<String>,
			Option<String>,
			Option<String>,
			Option<i64>,
			i64,
			i64,
			i64,
			i64,
		)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
			.bind(q.since.0)
			.bind(q.until.map_or(i64::MAX, |t| t.0))
			.bind(q.org_id)
			.bind(q.org_id)
			.bind(&q.subject)
			.bind(&q.subject)
			.bind(q.kind.map(UsageKind::as_str))
			.bind(q.kind.map(UsageKind::as_str))
			.bind(q.account_id)
			.bind(q.account_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()?;
		Ok(rows
			.into_iter()
			.map(
				|(
					provider,
					model,
					step,
					day,
					subject,
					account_id,
					calls,
					input,
					output,
					micro_eur,
				)| {
					UsageGroup {
						provider,
						model,
						step,
						day,
						subject,
						account_id,
						calls,
						input,
						output,
						micro_eur,
					}
				},
			)
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

	async fn budget_windows_set(&self, subject: &str, windows: &[(i64, i64)]) -> ClResult<()> {
		let now = Timestamp::now().0;
		let tx = self.write_tx().await?;
		sqlx::query("DELETE FROM llm_budget_windows WHERE subject = ?")
			.bind(subject)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		for &(window_secs, cap) in windows {
			sqlx::query(
				"INSERT INTO llm_budget_windows (subject, window_secs, cap_micro_eur, updated_at)
				 VALUES (?, ?, ?, ?)",
			)
			.bind(subject)
			.bind(window_secs)
			.bind(cap)
			.bind(now)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}
		tx.commit().await?;
		Ok(())
	}

	async fn subject_forget(&self, subject: &str) -> ClResult<()> {
		let tx = self.write_tx().await?;
		for sql in [
			"DELETE FROM llm_budgets WHERE subject = ?",
			"DELETE FROM llm_budget_windows WHERE subject = ?",
			"UPDATE llm_usage SET subject = NULL WHERE subject = ?",
		] {
			sqlx::query(sql).bind(subject).execute(&mut *tx.lock().await?).await.db()?;
		}
		tx.commit().await?;
		Ok(())
	}

	async fn budget_windows(
		&self,
		subject: &str,
		now: Timestamp,
	) -> ClResult<Vec<(i64, i64, i64, Option<Timestamp>)>> {
		let rows: Vec<(i64, i64, i64, Option<i64>)> = sqlx::query_as(
			"SELECT w.window_secs, w.cap_micro_eur, COALESCE(SUM(u.cost_micro_eur), 0), MIN(u.at)
			   FROM llm_budget_windows w
			   LEFT JOIN llm_usage u ON u.subject = w.subject AND u.at >= ? - w.window_secs
			  WHERE w.subject = ?
			  GROUP BY w.window_secs, w.cap_micro_eur
			  ORDER BY w.window_secs",
		)
		.bind(now.0)
		.bind(subject)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(rows
			.into_iter()
			.map(|(w, cap, spent, oldest)| (w, cap, spent, oldest.map(Timestamp)))
			.collect())
	}
}

// vim: ts=4
