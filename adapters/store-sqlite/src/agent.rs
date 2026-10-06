//! `AgentRunStore` over SQLite: `agent_runs` and the append-only `agent_run_events`.

use async_trait::async_trait;
use mintworks_agent::store::{AgentRunStore, EventKind, NewRun, Run, RunEvent, RunStatus};
use mintworks_core::prelude::*;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::SqliteStore;
use crate::util::DbExt;

const RUN_COLS: &str = "id, uid, thread_uid, org_id, account_id, role, spec, status, error, \
	created_at, started_at, finished_at";

fn run(row: &SqliteRow) -> ClResult<Run> {
	let status: String = row.try_get("status").db()?;
	Ok(Run {
		id: row.try_get("id").db()?,
		uid: RunId::from_trusted(row.try_get("uid").db()?),
		thread: ThreadId::from_trusted(row.try_get("thread_uid").db()?),
		org_id: row.try_get("org_id").db()?,
		account_id: row.try_get("account_id").db()?,
		role: row.try_get("role").db()?,
		spec: row.try_get("spec").db()?,
		status: RunStatus::parse(&status)
			.ok_or_else(|| Error::internal(format!("agent_runs.status {status:?}")))?,
		error: row.try_get("error").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		started_at: row.try_get::<Option<i64>, _>("started_at").db()?.map(Timestamp),
		finished_at: row.try_get::<Option<i64>, _>("finished_at").db()?.map(Timestamp),
	})
}

#[async_trait]
impl AgentRunStore for SqliteStore {
	async fn run_insert(&self, new: &NewRun<'_>) -> ClResult<Option<Run>> {
		// The conflict target names the partial index `idx_agent_runs_live`; a `uid` clash still
		// errors.
		let sql = format!(
			"INSERT INTO agent_runs (uid, thread_uid, org_id, account_id, role, spec, status, \
				created_at, heartbeat_at)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7, ?7)
			 ON CONFLICT (thread_uid) WHERE status IN ('queued','running') DO NOTHING
			 RETURNING {RUN_COLS}"
		);
		let row = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(new.uid.as_str())
			.bind(new.thread.as_str())
			.bind(new.org_id)
			.bind(new.account_id)
			.bind(new.role)
			.bind(new.spec)
			.bind(Timestamp::now().0)
			.fetch_optional(&mut *self.conn().await?)
			.await
			.db()?;
		row.as_ref().map(run).transpose()
	}

	async fn run_get(&self, uid: &RunId) -> ClResult<Option<Run>> {
		let sql = format!("SELECT {RUN_COLS} FROM agent_runs WHERE uid = ?");
		let row = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()?;
		row.as_ref().map(run).transpose()
	}

	async fn run_set_status(
		&self,
		run_id: i64,
		status: RunStatus,
		error: Option<&str>,
	) -> ClResult<bool> {
		let now = Timestamp::now().0;
		let started = (status == RunStatus::Running).then_some(now);
		let finished = (!status.is_live()).then_some(now);
		let res = sqlx::query(
			"UPDATE agent_runs SET status = ?, error = ?, started_at = COALESCE(?, started_at),
				finished_at = COALESCE(?, finished_at)
			 WHERE id = ? AND status IN ('queued','running')",
		)
		.bind(status.as_str())
		.bind(error)
		.bind(started)
		.bind(finished)
		.bind(run_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() > 0)
	}

	async fn event_append(&self, run_id: i64, kind: EventKind, payload: &str) -> ClResult<i64> {
		// One statement on the single writer, so the MAX+1 cannot race another append.
		sqlx::query_scalar(
			"INSERT INTO agent_run_events (run_id, seq, kind, payload, at)
			 SELECT ?, COALESCE(MAX(seq), 0) + 1, ?, ?, ? FROM agent_run_events WHERE run_id = ?
			 RETURNING seq",
		)
		.bind(run_id)
		.bind(kind.as_str())
		.bind(payload)
		.bind(Timestamp::now().0)
		.bind(run_id)
		.fetch_one(&mut *self.conn().await?)
		.await
		.db()
	}

	async fn events_after(&self, run_id: i64, after: i64) -> ClResult<Vec<RunEvent>> {
		let rows: Vec<(i64, String, String, i64)> = sqlx::query_as(
			"SELECT seq, kind, payload, at FROM agent_run_events
			 WHERE run_id = ? AND seq > ? ORDER BY seq",
		)
		.bind(run_id)
		.bind(after)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;
		rows.into_iter()
			.map(|(seq, kind, payload, at)| {
				let kind = EventKind::parse(&kind)
					.ok_or_else(|| Error::internal(format!("agent_run_events.kind {kind:?}")))?;
				Ok(RunEvent { seq, kind, payload, at: Timestamp(at) })
			})
			.collect()
	}

	async fn runs_heartbeat(&self, run_ids: &[i64], at: Timestamp) -> ClResult<()> {
		if run_ids.is_empty() {
			return Ok(());
		}
		let ids = format!("{run_ids:?}");
		sqlx::query(
			"UPDATE agent_runs SET heartbeat_at = ?
			 WHERE id IN (SELECT value FROM json_each(?)) AND status IN ('queued','running')",
		)
		.bind(at.0)
		.bind(ids)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn runs_interrupt_stale(&self, before: Timestamp) -> ClResult<Vec<Run>> {
		let sql = format!(
			"UPDATE agent_runs SET status = 'interrupted', finished_at = ?
			 WHERE status IN ('queued','running') AND COALESCE(heartbeat_at, created_at) < ?
			 RETURNING {RUN_COLS}"
		);
		let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(Timestamp::now().0)
			.bind(before.0)
			.fetch_all(&mut *self.conn().await?)
			.await
			.db()?;
		rows.iter().map(run).collect()
	}
}

// vim: ts=4
