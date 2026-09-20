//! `CoreStore` over SQLite — settings, secrets, audit, jobs, the schema-version probe and
//! the three lookups the auth middleware makes on every request.
//!
//! Every statement here moved verbatim out of `saas-core`; the parsing, caching, encryption
//! and authorization decisions stayed behind. Reads run on `reader()`, writes on `writer()`.

use async_trait::async_trait;
use saas_core::job::Job;
use saas_core::prelude::*;
use saas_core::store::{AuditEntry, CoreStore, Role, TokenAccount};

use crate::{SqliteStore, util::DbExt};

#[async_trait]
impl CoreStore for SqliteStore {
	// ---- settings ------------------------------------------------------------------

	async fn setting_get(&self, key: &str) -> ClResult<Option<String>> {
		let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
			.bind(key)
			.fetch_optional(self.reader())
			.await
			.db()?;
		Ok(row.map(|(v,)| v))
	}

	async fn setting_set(&self, key: &str, raw: &str, updated_by: Option<i64>) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO settings (key, value, updated_at, updated_by) VALUES (?, ?, ?, ?)
			ON CONFLICT(key) DO UPDATE SET value = excluded.value,
				updated_at = excluded.updated_at, updated_by = excluded.updated_by",
		)
		.bind(key)
		.bind(raw)
		.bind(Timestamp::now().0)
		.bind(updated_by)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	// ---- secrets -------------------------------------------------------------------

	async fn secret_get(&self, key: &str) -> ClResult<Option<(Vec<u8>, Vec<u8>)>> {
		let row: Option<(Vec<u8>, Vec<u8>)> =
			sqlx::query_as("SELECT nonce, ciphertext FROM secrets WHERE key = ?")
				.bind(key)
				.fetch_optional(self.reader())
				.await
				.db()?;
		Ok(row)
	}

	async fn secret_set(
		&self,
		key: &str,
		nonce: &[u8],
		ciphertext: &[u8],
		updated_by: Option<i64>,
	) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO secrets (key, nonce, ciphertext, updated_at, updated_by)
			VALUES (?, ?, ?, ?, ?)
			ON CONFLICT(key) DO UPDATE SET nonce = excluded.nonce,
				ciphertext = excluded.ciphertext, updated_at = excluded.updated_at,
				updated_by = excluded.updated_by",
		)
		.bind(key)
		.bind(nonce)
		.bind(ciphertext)
		.bind(Timestamp::now().0)
		.bind(updated_by)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	async fn secret_put_if_absent(
		&self,
		key: &str,
		nonce: &[u8],
		ciphertext: &[u8],
	) -> ClResult<()> {
		// `DO NOTHING`, not an upsert: the loser of a race must leave the winner's key in
		// place. `updated_by` is NULL — nobody set this, it was minted.
		sqlx::query(
			"INSERT INTO secrets (key, nonce, ciphertext, updated_at, updated_by)
			VALUES (?, ?, ?, ?, NULL) ON CONFLICT(key) DO NOTHING",
		)
		.bind(key)
		.bind(nonce)
		.bind(ciphertext)
		.bind(Timestamp::now().0)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	async fn secret_updated_at(&self, key: &str) -> ClResult<Option<Timestamp>> {
		let row: Option<(i64,)> = sqlx::query_as("SELECT updated_at FROM secrets WHERE key = ?")
			.bind(key)
			.fetch_optional(self.reader())
			.await
			.db()?;
		Ok(row.map(|(at,)| Timestamp(at)))
	}

	// ---- audit ---------------------------------------------------------------------

	async fn audit_log(&self, entry: &AuditEntry) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO audit_logs
			 (at, account_id, org_id, ip, entity, entity_id, action, detail, request_id)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
		)
		.bind(entry.at.0)
		.bind(entry.account_id)
		.bind(entry.org_id)
		.bind(entry.ip.as_deref())
		.bind(entry.entity.as_str())
		.bind(entry.entity_id.as_deref())
		.bind(entry.action.as_str())
		.bind(entry.detail.as_deref())
		.bind(entry.request_id.as_deref())
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	// ---- jobs ----------------------------------------------------------------------

	async fn job_enqueue(
		&self,
		kind: &str,
		payload: &str,
		dedup_key: Option<&str>,
		run_at: Timestamp,
	) -> ClResult<Option<i64>> {
		let id = sqlx::query_scalar(
			"INSERT INTO jobs (kind, payload, dedup_key, run_at, created_at) \
			 VALUES (?, ?, ?, ?, ?) ON CONFLICT (dedup_key) DO NOTHING RETURNING id",
		)
		.bind(kind)
		.bind(payload)
		.bind(dedup_key)
		.bind(run_at.0)
		.bind(Timestamp::now().0)
		.fetch_optional(self.writer())
		.await
		.db()?;
		Ok(id)
	}

	async fn job_has_live(&self, kind: &str, besides: Option<i64>) -> ClResult<bool> {
		// On `reader()`: under WAL the claim that marked the row `RUNNING` has committed
		// before this runs, so the reader's snapshot already carries it.
		let found: Option<i64> = sqlx::query_scalar(
			"SELECT 1 FROM jobs WHERE kind = ? AND status IN ('PENDING','RUNNING') AND id <> ? \
			 LIMIT 1",
		)
		.bind(kind)
		// Ids are a positive `INTEGER PRIMARY KEY`, so -1 excludes nothing.
		.bind(besides.unwrap_or(-1))
		.fetch_optional(self.reader())
		.await.db()?;
		Ok(found.is_some())
	}

	async fn job_seed_periodic(&self, kind: &str, run_at: Timestamp) -> ClResult<Option<i64>> {
		// The `NOT EXISTS` runs inside this statement's own transaction on `writer()`, and
		// SQLite serialises writers — which `job_has_live` on `reader()` plus a separate
		// insert did not, so two processes booting a rolling deploy both seeded the chain.
		let id = sqlx::query_scalar(
			"INSERT INTO jobs (kind, payload, dedup_key, run_at, created_at) \
			 SELECT ?, '{}', NULL, ?, ? \
			 WHERE NOT EXISTS ( \
			     SELECT 1 FROM jobs WHERE kind = ? AND status IN ('PENDING','RUNNING') \
			 ) RETURNING id",
		)
		.bind(kind)
		.bind(run_at.0)
		.bind(Timestamp::now().0)
		.bind(kind)
		.fetch_optional(self.writer())
		.await
		.db()?;
		Ok(id)
	}

	async fn job_status_by_key(&self, dedup_key: &str) -> ClResult<Option<String>> {
		sqlx::query_scalar("SELECT status FROM jobs WHERE dedup_key = ?")
			.bind(dedup_key)
			.fetch_optional(self.reader())
			.await
			.db()
	}

	async fn job_statuses_by_keys(&self, keys: &[String]) -> ClResult<Vec<(String, String)>> {
		let mut out = Vec::with_capacity(keys.len());
		for chunk in keys.chunks(crate::invoice::MAX_IN_LIST) {
			let sql = format!(
				"SELECT dedup_key, status FROM jobs WHERE dedup_key IN ({})",
				"?,".repeat(chunk.len() - 1) + "?"
			);
			let mut q = sqlx::query_as(sqlx::AssertSqlSafe(sql));
			for key in chunk {
				q = q.bind(key);
			}
			out.extend(q.fetch_all(self.reader()).await.db()?);
		}
		Ok(out)
	}

	async fn job_status(&self, id: i64) -> ClResult<Option<String>> {
		// On `reader()`: every caller has just written through the writer, so under WAL that
		// write has committed and the reader's snapshot carries it.
		sqlx::query_scalar("SELECT status FROM jobs WHERE id = ?")
			.bind(id)
			.fetch_optional(self.reader())
			.await
			.db()
	}

	/// `ORDER BY run_at, id`: without the tiebreaker two jobs enqueued in the same second
	/// claimed in whatever order the index scan produced, and an invoice and its storno are
	/// exactly that pair. Deterministic FIFO is not sufficient on its own — two workers still
	/// claim concurrently — so `saas_nav::job::report` also checks the original's submission.
	///
	/// `INDEXED BY` is a hard constraint on purpose: the planner preferred the equality on
	/// `idx_job_status` and sorted the whole PENDING backlog per claim, on the writer
	/// connection every other write queues behind. This query must never degrade to a sort.
	async fn job_claim(&self, now: Timestamp) -> ClResult<Option<Job>> {
		let row: Option<(i64, String, String, i64)> = sqlx::query_as(
			"UPDATE jobs SET status = 'RUNNING', attempts = attempts + 1, claimed_at = ? \
			 WHERE id = (SELECT id FROM jobs INDEXED BY idx_job_claim \
			 WHERE status = 'PENDING' AND run_at <= ? \
			 ORDER BY run_at, id LIMIT 1) \
			 RETURNING id, kind, payload, attempts",
		)
		.bind(now.0)
		.bind(now.0)
		.fetch_optional(self.writer())
		.await
		.db()?;
		Ok(row.map(|(id, kind, payload, attempts)| Job { id, kind, payload, attempts }))
	}

	async fn job_complete(&self, id: i64, now: Timestamp) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'DONE', payload = '', done_at = ?, last_error = NULL \
			 WHERE id = ? AND status = 'RUNNING'",
		)
		.bind(now.0)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	async fn job_defer(&self, id: i64, run_at: Timestamp) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'PENDING', run_at = ?, last_error = NULL, err_code = NULL \
			 WHERE id = ? AND status = 'RUNNING'",
		)
		.bind(run_at.0)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	async fn job_fail(
		&self,
		id: i64,
		run_at: Timestamp,
		err: &str,
		err_code: Option<&str>,
	) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'PENDING', run_at = ?, last_error = ?, err_code = ? \
			 WHERE id = ? AND status = 'RUNNING'",
		)
		.bind(run_at.0)
		.bind(err)
		.bind(err_code)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	/// `dedup_key` is deliberately **not** cleared: it is a permanent idempotency record, and
	/// releasing it here let a replayed enqueue file the same invoice a second time.
	///
	/// `AND status = 'RUNNING'`, like `job_complete` and `job_fail`: an operator's `job_cancel`
	/// can land while the handler is still in flight, and a bare `WHERE id = ?` overwrote the
	/// reason they recorded with the handler's own.
	async fn job_terminate(
		&self,
		id: i64,
		now: Timestamp,
		err: &str,
		err_code: Option<&str>,
	) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'FAILED', done_at = ?, last_error = ?, err_code = ? \
			 WHERE id = ? AND status = 'RUNNING'",
		)
		.bind(now.0)
		.bind(err)
		.bind(err_code)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	/// The inverse of `job_cancel`, addressed the same way and keeping `dedup_key` for the same
	/// reason. `attempts = 0` because a re-drive is a decision to start over, not to resume a
	/// spent budget — with `jobs.max_attempts.<KIND>` set, leaving the count would re-terminate
	/// on the first failure.
	async fn job_redrive(&self, kind: &str, payload: &str, now: Timestamp) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'PENDING', attempts = 0, run_at = ?, done_at = NULL, \
			 last_error = NULL, err_code = NULL \
			 WHERE kind = ? AND payload = ? AND status = 'FAILED'",
		)
		.bind(now.0)
		.bind(kind)
		.bind(payload)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	/// `payload = ?` is the point of the statement, not incidental: `job_complete` blanked it,
	/// which is also why the row is addressed by the unique `dedup_key` and not `(kind, payload)`.
	async fn job_redrive_done(
		&self,
		dedup_key: &str,
		payload: &str,
		now: Timestamp,
	) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'PENDING', attempts = 0, payload = ?, run_at = ?, \
			 done_at = NULL, last_error = NULL, err_code = NULL \
			 WHERE dedup_key = ? AND status = 'DONE'",
		)
		.bind(payload)
		.bind(now.0)
		.bind(dedup_key)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	/// Addressed by `(kind, payload)` rather than by id — `idx_job_kind_payload` covers the
	/// pair — because the caller is a service method holding an invoice, not the runner
	/// holding a claimed row. `dedup_key` is kept for the same reason `job_terminate` keeps
	/// it.
	///
	/// Cancelling a *periodic* kind's `PENDING` occurrence ends the chain until the next
	/// process start — see `CoreStore::job_cancel`. Deliberate: an operator stopping a
	/// recurring job means it, and `job::seed_periodic` is the documented revival.
	async fn job_cancel(
		&self,
		kind: &str,
		payload: &str,
		now: Timestamp,
		err: &str,
		err_code: Option<&str>,
	) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'FAILED', done_at = ?, last_error = ?, err_code = ? \
			 WHERE kind = ? AND payload = ? AND status IN ('PENDING', 'RUNNING')",
		)
		.bind(now.0)
		.bind(err)
		.bind(err_code)
		.bind(kind)
		.bind(payload)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	/// `claimed_at IS NULL` catches rows claimed before this column existed, once.
	async fn job_reclaim(&self, before: Timestamp) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'PENDING' WHERE status = 'RUNNING' \
			 AND (claimed_at IS NULL OR claimed_at < ?)",
		)
		.bind(before.0)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	async fn job_sweep(&self, cutoff: Timestamp) -> ClResult<u64> {
		// A handler-supplied `dedup_key` is retained forever — it is the idempotency record
		// `NAV_REPORT`'s "never file twice" rests on — while a `periodic:` one lasts one period,
		// bounded below `jobs.retention_days` so reclaiming it cannot hit a live successor.
		// `GLOB`, not `LIKE`: `LIKE` folds ASCII case, so the sweep reclaimed a caller's
		// `PERIODIC:…` permanent record and a replayed enqueue filed the invoice twice.
		let gone = sqlx::query(
			"DELETE FROM jobs WHERE status IN ('DONE', 'FAILED') AND done_at < ? \
			 AND (dedup_key IS NULL OR dedup_key GLOB ?)",
		)
		.bind(cutoff.0)
		.bind(format!("{}*", saas_core::job::PERIODIC_KEY_PREFIX))
		.execute(self.writer())
		.await
		.db()?;
		Ok(gone.rows_affected())
	}

	async fn job_status_counts(&self) -> ClResult<Vec<(String, i64, Timestamp, Timestamp)>> {
		// The newest figure is the newest *transition*, not the newest enqueue: it age-bounds
		// `A-JOB-FAILED`, and `MAX(created_at)` hid a filing cancelled today on a job enqueued
		// three days ago. `COALESCE` because `done_at` is NULL for `PENDING`/`RUNNING`.
		let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
			"SELECT status, COUNT(*), MIN(created_at), MAX(COALESCE(done_at, created_at)) \
			 FROM jobs GROUP BY status",
		)
		.fetch_all(self.reader())
		.await
		.db()?;
		Ok(rows
			.into_iter()
			.map(|(status, count, oldest, newest)| {
				(status, count, Timestamp(oldest), Timestamp(newest))
			})
			.collect())
	}

	async fn job_retrying_kinds(&self) -> ClResult<Vec<String>> {
		// `last_error IS NOT NULL`, not `attempts > 0`: `job_claim` increments before the handler
		// runs, so a restart made a job that never failed raise `A-JOB-STALE`. Resetting
		// `attempts` on reclaim would also un-bound a job whose handler kills the process.
		sqlx::query_scalar(
			"SELECT DISTINCT kind FROM jobs WHERE status = 'PENDING' AND last_error IS NOT NULL",
		)
		.fetch_all(self.reader())
		.await
		.db()
	}

	async fn job_stale(&self, kind: &str, before: Timestamp) -> ClResult<Option<(i64, Timestamp)>> {
		// An aggregate with no GROUP BY always yields one row; `MIN` is NULL when it matched
		// nothing, which is what distinguishes "no stale jobs" from a count of zero.
		let (count, oldest): (i64, Option<i64>) = sqlx::query_as(
			// `last_error IS NOT NULL` for the same reason as `job_retrying_kinds`, which this
			// has to agree with or a crash-reclaimed job still gets counted.
			"SELECT COUNT(*), MIN(created_at) FROM jobs \
			 WHERE kind = ? AND status = 'PENDING' AND last_error IS NOT NULL AND created_at < ?",
		)
		.bind(kind)
		.bind(before.0)
		.fetch_one(self.reader())
		.await
		.db()?;
		Ok(oldest.map(|oldest| (count, Timestamp(oldest))))
	}

	// ---- vars ----------------------------------------------------------------------

	async fn var_get(&self, name: &str) -> ClResult<Option<String>> {
		sqlx::query_scalar::<_, String>("SELECT value FROM vars WHERE name = ?")
			.bind(name)
			.fetch_optional(self.reader())
			.await
			.db()
	}

	async fn var_set(&self, name: &str, value: &str) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO vars (name, value) VALUES (?, ?) \
			 ON CONFLICT(name) DO UPDATE SET value = excluded.value",
		)
		.bind(name)
		.bind(value)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	// ---- health --------------------------------------------------------------------

	async fn db_version(&self) -> ClResult<i64> {
		// The framework module's row only — a consumer's own module has its own version and no
		// place in `/readyz`'s `dbVersion`. No row means unmigrated, which is version 0; a read
		// error stays an `Err` so `/readyz` can report `db: "fail"` instead of swallowing it.
		Ok(sqlx::query_scalar("SELECT version FROM schema_version WHERE module = ?")
			.bind(crate::schema::MODULE_NAME)
			.fetch_optional(self.reader())
			.await
			.db()?
			.unwrap_or(0))
	}

	// ---- auth middleware -----------------------------------------------------------

	async fn account_for_token(&self, uid: &str) -> ClResult<Option<TokenAccount>> {
		let row: Option<(i64, i64, i64, String)> = sqlx::query_as(
			"SELECT a.id, a.token_epoch,
			        EXISTS (SELECT 1 FROM memberships m
			                 WHERE m.account_id = a.id
			                   AND m.org_id = (SELECT id FROM orgs WHERE kind = 'ROOT')
			                   AND m.role IN ('ADMIN', 'OWNER')
			                   AND m.accepted_at IS NOT NULL),
			        a.status
			   FROM accounts a WHERE a.uid = ?",
		)
		.bind(uid)
		.fetch_optional(self.reader())
		.await
		.db()?;
		Ok(row.map(|(id, token_epoch, is_root_admin, status)| TokenAccount {
			id,
			token_epoch,
			is_root_admin: is_root_admin != 0,
			status,
		}))
	}

	async fn org_membership_role(
		&self,
		account_id: i64,
		org_uid: &str,
	) -> ClResult<Option<(i64, Role)>> {
		let row: Option<(i64, Option<i64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
			"{}
			 SELECT o.id, ({BEST_ROLE}) FROM orgs o WHERE o.uid = ? AND o.status = 'ACTIVE'",
			ancestors("uid = ?")
		)))
		// Bind order is load-bearing: the CTE's `?`, then `BEST_ROLE`'s, then the outer one.
		.bind(org_uid)
		.bind(account_id)
		.bind(org_uid)
		.fetch_optional(self.reader())
		.await
		.db()?;
		Ok(row.and_then(|(id, rank)| rank.map(|r| (id, role_of(r)))))
	}

	async fn org_role(&self, account_id: i64, org_id: i64) -> ClResult<Option<Role>> {
		let rank: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(sqlx::AssertSqlSafe(format!(
			"{}
			 SELECT ({BEST_ROLE})",
			ancestors("id = ?")
		)))
		.bind(org_id)
		.bind(account_id)
		.fetch_optional(self.reader())
		.await
		.db()?
		.flatten();
		Ok(rank.map(role_of))
	}

	async fn root_org_id(&self) -> ClResult<i64> {
		if let Some(id) = self.root_org.get() {
			return Ok(*id);
		}
		let id = sqlx::query_scalar::<_, i64>("SELECT id FROM orgs WHERE kind = 'ROOT'")
			.fetch_optional(self.reader())
			.await
			.db()?
			.ok_or_else(|| Error::internal("no root org"))?;
		// `set` losing a race is fine: both racers read the same row.
		let _ = self.root_org.set(id);
		Ok(id)
	}
}

/// The cycle guard on the ancestor walk: `orgs.parent_id` permits a cycle and a recursive CTE
/// over one would spin. `create_org` can only parent onto an existing ancestor, so this `LIMIT`
/// is the standing guard against a row written by other means.
pub(crate) const MAX_ORG_DEPTH: i64 = 16;

/// The org `anchor` selects and every ancestor of it, as `anc(id, parent_id, depth)`.
/// `anchor` is a literal from this crate, never caller input.
pub(crate) fn ancestors(anchor: &str) -> String {
	format!(
		"WITH RECURSIVE anc(id, parent_id, depth) AS (
		         SELECT id, parent_id, 0 FROM orgs WHERE {anchor} AND status = 'ACTIVE'
		   UNION ALL
		         SELECT o.id, o.parent_id, anc.depth + 1 FROM orgs o JOIN anc ON o.id = anc.parent_id
		          WHERE o.status = 'ACTIVE'
		   LIMIT {MAX_ORG_DEPTH}
		 )"
	)
}

/// The highest role `?` holds anywhere on `anc`, as a rank, or `NULL` for none. `memberships`
/// stores the role as text with no ordering, so the rank is built here rather than compared in
/// SQL; [`role_of`] maps it back.
const BEST_ROLE: &str = "SELECT MAX(CASE m.role WHEN 'OWNER' THEN 3 WHEN 'ADMIN' THEN 2 ELSE 1 END)
	   FROM memberships m JOIN anc ON m.org_id = anc.id
	  WHERE m.account_id = ? AND m.accepted_at IS NOT NULL";

fn role_of(rank: i64) -> Role {
	match rank {
		3 => Role::Owner,
		2 => Role::Admin,
		_ => Role::Member,
	}
}

// vim: ts=4
