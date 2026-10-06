//! `CoreStore` over PostgreSQL — settings, secrets, audit, jobs, the schema-version probe and
//! the lookups the auth middleware makes on every request. The statements are the SQLite
//! adapter's `core.rs`, translated; the semantics are `adapter-contract.md` §3.

use async_trait::async_trait;
use mintworks_core::job::Job;
use mintworks_core::prelude::*;
use mintworks_core::store::{ApiKey, AuditEntry, CoreStore, Role, TokenAccount};
use sqlx::{Row, postgres::PgRow};

use crate::{
	PgStore,
	tx::insert_audit,
	util::{DbExt, RowExt},
};

/// How long a detached row waits for a writer connection once `try_acquire` has missed one.
/// Bounded rather than `ACQUIRE_TIMEOUT`: the wait must not become the caller's error.
const DETACHED_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

#[async_trait]
impl CoreStore for PgStore {
	// ---- settings ------------------------------------------------------------------

	async fn setting_get(&self, key: &str) -> ClResult<Option<String>> {
		sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
			.bind(key)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn setting_set(&self, key: &str, raw: &str, updated_by: Option<i64>) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO settings (key, value, updated_at, updated_by) VALUES ($1, $2, $3, $4)
			ON CONFLICT (key) DO UPDATE SET value = excluded.value,
				updated_at = excluded.updated_at, updated_by = excluded.updated_by",
		)
		.bind(key)
		.bind(raw)
		.bind(Timestamp::now().0)
		.bind(updated_by)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	// ---- secrets -------------------------------------------------------------------

	async fn secret_get(&self, org_id: i64, key: &str) -> ClResult<Option<(Vec<u8>, Vec<u8>)>> {
		sqlx::query_as("SELECT nonce, ciphertext FROM secrets WHERE org_id = $1 AND key = $2")
			.bind(org_id)
			.bind(key)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn secret_set(
		&self,
		org_id: i64,
		key: &str,
		nonce: &[u8],
		ciphertext: &[u8],
		updated_by: Option<i64>,
	) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO secrets (org_id, key, nonce, ciphertext, updated_at, updated_by)
			VALUES ($1, $2, $3, $4, $5, $6)
			ON CONFLICT (org_id, key) DO UPDATE SET nonce = excluded.nonce,
				ciphertext = excluded.ciphertext, updated_at = excluded.updated_at,
				updated_by = excluded.updated_by",
		)
		.bind(org_id)
		.bind(key)
		.bind(nonce)
		.bind(ciphertext)
		.bind(Timestamp::now().0)
		.bind(updated_by)
		.execute(&mut *self.conn().await?)
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
		// `DO NOTHING`, not an upsert: the loser of a race must leave the winner's key in place.
		sqlx::query(
			"INSERT INTO secrets (org_id, key, nonce, ciphertext, updated_at, updated_by)
			VALUES (0, $1, $2, $3, $4, NULL) ON CONFLICT (org_id, key) DO NOTHING",
		)
		.bind(key)
		.bind(nonce)
		.bind(ciphertext)
		.bind(Timestamp::now().0)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn secret_updated_at(&self, org_id: i64, key: &str) -> ClResult<Option<Timestamp>> {
		let at: Option<i64> =
			sqlx::query_scalar("SELECT updated_at FROM secrets WHERE org_id = $1 AND key = $2")
				.bind(org_id)
				.bind(key)
				.fetch_optional(&mut *self.reader().await?)
				.await
				.db()?;
		Ok(at.map(Timestamp))
	}

	// ---- audit ---------------------------------------------------------------------

	async fn audit_log(&self, entry: &AuditEntry) -> ClResult<()> {
		insert_audit(&mut *self.conn().await?, entry).await
	}

	async fn audit_detached(&self, entry: &AuditEntry) -> ClResult<()> {
		// Its own pooled writer connection in autocommit, bound handle or not, so the row outlives
		// the caller's rollback. `try_acquire` then a short bound: losing the row degrades to the
		// `error!` line rather than making the evidence the caller's error.
		if let Some(mut conn) = self.writer.try_acquire() {
			return insert_audit(&mut conn, entry).await;
		}
		let mut conn = tokio::time::timeout(DETACHED_WAIT, self.writer.acquire())
			.await
			.map_err(|_| Error::Unavailable("no writer connection is free".to_owned()))?
			.db()?;
		insert_audit(&mut conn, entry).await
	}

	// ---- jobs ----------------------------------------------------------------------

	async fn job_enqueue(
		&self,
		kind: &str,
		payload: &str,
		dedup_key: Option<&str>,
		run_at: Timestamp,
	) -> ClResult<Option<i64>> {
		sqlx::query_scalar(
			"INSERT INTO jobs (kind, payload, dedup_key, run_at, created_at) \
			 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (dedup_key) DO NOTHING RETURNING id",
		)
		.bind(kind)
		.bind(payload)
		.bind(dedup_key)
		.bind(run_at.0)
		.bind(Timestamp::now().0)
		.fetch_optional(&mut *self.conn().await?)
		.await
		.db()
	}

	async fn job_has_live(&self, kind: &str, besides: Option<i64>) -> ClResult<bool> {
		sqlx::query_scalar(
			"SELECT EXISTS (SELECT 1 FROM jobs \
			 WHERE kind = $1 AND status IN ('PENDING','RUNNING') AND id IS DISTINCT FROM $2)",
		)
		.bind(kind)
		.bind(besides)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn job_seed_periodic(&self, kind: &str, run_at: Timestamp) -> ClResult<Option<i64>> {
		// Under the write lock: `NOT EXISTS` in one autocommit statement is not atomic here as it
		// is under SQLite's single writer, and two processes booting a rolling deploy both seeded.
		let tx = self.write_tx().await?;
		let id = sqlx::query_scalar(
			"INSERT INTO jobs (kind, payload, dedup_key, run_at, created_at) \
			 SELECT $1, '{}', NULL, $2, $3 \
			 WHERE NOT EXISTS ( \
			     SELECT 1 FROM jobs WHERE kind = $1 AND status IN ('PENDING','RUNNING') \
			 ) RETURNING id",
		)
		.bind(kind)
		.bind(run_at.0)
		.bind(Timestamp::now().0)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.db()?;
		tx.commit().await?;
		Ok(id)
	}

	async fn job_status_by_key(&self, dedup_key: &str) -> ClResult<Option<String>> {
		sqlx::query_scalar("SELECT status FROM jobs WHERE dedup_key = $1")
			.bind(dedup_key)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn job_statuses_by_keys(&self, keys: &[String]) -> ClResult<Vec<(String, String)>> {
		sqlx::query_as("SELECT dedup_key, status FROM jobs WHERE dedup_key = ANY($1)")
			.bind(keys)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn job_status(&self, id: i64) -> ClResult<Option<String>> {
		sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn job_set_result(&self, id: i64, result: &str) -> ClResult<()> {
		sqlx::query("UPDATE jobs SET result = $1 WHERE id = $2")
			.bind(result)
			.bind(id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(())
	}

	async fn job_result_by_key(
		&self,
		dedup_key: &str,
	) -> ClResult<Option<(String, Option<String>)>> {
		sqlx::query_as("SELECT status, result FROM jobs WHERE dedup_key = $1")
			.bind(dedup_key)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	/// `ORDER BY run_at, id`: an invoice and its storno enqueued in the same second must claim in
	/// FIFO order. `SKIP LOCKED` so concurrent workers neither double-claim nor block each other;
	/// `idx_job_claim` is partial on `PENDING` over `(run_at, id)`, so the claim never sorts.
	async fn job_claim(&self, now: Timestamp) -> ClResult<Option<Job>> {
		let row: Option<(i64, String, String, i64)> = sqlx::query_as(
			"UPDATE jobs SET status = 'RUNNING', attempts = attempts + 1, claimed_at = $1 \
			 WHERE id = (SELECT id FROM jobs \
			 WHERE status = 'PENDING' AND run_at <= $1 \
			 ORDER BY run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED) \
			 RETURNING id, kind, payload, attempts",
		)
		.bind(now.0)
		.fetch_optional(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(row.map(|(id, kind, payload, attempts)| Job { id, kind, payload, attempts }))
	}

	async fn job_complete(&self, id: i64, now: Timestamp) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'DONE', payload = '', done_at = $1, last_error = NULL \
			 WHERE id = $2 AND status = 'RUNNING'",
		)
		.bind(now.0)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	async fn job_defer(&self, id: i64, run_at: Timestamp) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'PENDING', run_at = $1, last_error = NULL, err_code = NULL \
			 WHERE id = $2 AND status = 'RUNNING'",
		)
		.bind(run_at.0)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	async fn job_wake(&self, dedup_keys: &[String], now: Timestamp) -> ClResult<u64> {
		if dedup_keys.is_empty() {
			return Ok(0);
		}
		let done = sqlx::query(
			"UPDATE jobs SET run_at = $1 WHERE status = 'PENDING' AND run_at > $1 \
			 AND dedup_key = ANY($2)",
		)
		.bind(now.0)
		.bind(dedup_keys)
		.execute(&mut *self.conn().await?)
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
			"UPDATE jobs SET status = 'PENDING', run_at = $1, last_error = $2, err_code = $3 \
			 WHERE id = $4 AND status = 'RUNNING'",
		)
		.bind(run_at.0)
		.bind(err)
		.bind(err_code)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	/// `dedup_key` is deliberately **not** cleared: it is a permanent idempotency record.
	/// `AND status = 'RUNNING'` so an operator's concurrent `job_cancel` keeps its reason.
	async fn job_terminate(
		&self,
		id: i64,
		now: Timestamp,
		err: &str,
		err_code: Option<&str>,
	) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'FAILED', done_at = $1, last_error = $2, err_code = $3 \
			 WHERE id = $4 AND status = 'RUNNING'",
		)
		.bind(now.0)
		.bind(err)
		.bind(err_code)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	/// `attempts = 0`: a re-drive starts over rather than resuming a spent budget.
	async fn job_redrive(&self, kind: &str, payload: &str, now: Timestamp) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'PENDING', attempts = 0, run_at = $1, done_at = NULL, \
			 last_error = NULL, err_code = NULL \
			 WHERE kind = $2 AND payload = $3 AND status = 'FAILED'",
		)
		.bind(now.0)
		.bind(kind)
		.bind(payload)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	/// Addressed by `dedup_key`, and restores `payload`: `job_complete` blanked it.
	async fn job_redrive_done(
		&self,
		dedup_key: &str,
		payload: &str,
		now: Timestamp,
	) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'PENDING', attempts = 0, payload = $1, run_at = $2, \
			 done_at = NULL, last_error = NULL, err_code = NULL \
			 WHERE dedup_key = $3 AND status = 'DONE'",
		)
		.bind(payload)
		.bind(now.0)
		.bind(dedup_key)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	/// Cancelling a *periodic* kind's `PENDING` occurrence ends the chain until the next process
	/// start — deliberate, see `CoreStore::job_cancel`.
	async fn job_cancel(
		&self,
		kind: &str,
		payload: &str,
		now: Timestamp,
		err: &str,
		err_code: Option<&str>,
	) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'FAILED', done_at = $1, last_error = $2, err_code = $3 \
			 WHERE kind = $4 AND payload = $5 AND status IN ('PENDING', 'RUNNING')",
		)
		.bind(now.0)
		.bind(err)
		.bind(err_code)
		.bind(kind)
		.bind(payload)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	async fn job_reclaim(&self, before: Timestamp) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE jobs SET status = 'PENDING' WHERE status = 'RUNNING' \
			 AND (claimed_at IS NULL OR claimed_at < $1)",
		)
		.bind(before.0)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	async fn job_sweep(&self, cutoff: Timestamp) -> ClResult<u64> {
		// A handler-supplied `dedup_key` is retained forever; a `periodic:` one lasts one period.
		// Case-sensitive `LIKE` with the prefix escaped, never `ILIKE`: reclaiming a caller's
		// `PERIODIC:…` permanent record let a replayed enqueue file the invoice twice.
		let prefix = mintworks_core::job::PERIODIC_KEY_PREFIX
			.replace('\\', "\\\\")
			.replace('%', "\\%")
			.replace('_', "\\_");
		let gone = sqlx::query(
			"DELETE FROM jobs WHERE status IN ('DONE', 'FAILED') AND done_at < $1 \
			 AND (dedup_key IS NULL OR dedup_key LIKE $2)",
		)
		.bind(cutoff.0)
		.bind(format!("{prefix}%"))
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(gone.rows_affected())
	}

	async fn job_status_counts(&self) -> ClResult<Vec<(String, i64, Timestamp, Timestamp)>> {
		// The newest *transition*, not the newest enqueue: it age-bounds `A-JOB-FAILED`.
		let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
			"SELECT status, COUNT(*), MIN(created_at), MAX(COALESCE(done_at, created_at)) \
			 FROM jobs GROUP BY status",
		)
		.fetch_all(&mut *self.reader().await?)
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
		// runs, so a restart made a job that never failed raise `A-JOB-STALE`.
		sqlx::query_scalar(
			"SELECT DISTINCT kind FROM jobs WHERE status = 'PENDING' AND last_error IS NOT NULL",
		)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn job_stale(&self, kind: &str, before: Timestamp) -> ClResult<Option<(i64, Timestamp)>> {
		// `MIN` is NULL when nothing matched — "no stale jobs" rather than a count of zero.
		// `last_error IS NOT NULL` must agree with `job_retrying_kinds`.
		let (count, oldest): (i64, Option<i64>) = sqlx::query_as(
			"SELECT COUNT(*), MIN(created_at) FROM jobs \
			 WHERE kind = $1 AND status = 'PENDING' AND last_error IS NOT NULL AND created_at < $2",
		)
		.bind(kind)
		.bind(before.0)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(oldest.map(|oldest| (count, Timestamp(oldest))))
	}

	// ---- vars ----------------------------------------------------------------------

	async fn var_get(&self, name: &str) -> ClResult<Option<String>> {
		// `value` is nullable; a NULL reads as absent.
		let value: Option<Option<String>> =
			sqlx::query_scalar("SELECT value FROM vars WHERE name = $1")
				.bind(name)
				.fetch_optional(&mut *self.reader().await?)
				.await
				.db()?;
		Ok(value.flatten())
	}

	async fn var_set(&self, name: &str, value: &str) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO vars (name, value) VALUES ($1, $2) \
			 ON CONFLICT (name) DO UPDATE SET value = excluded.value",
		)
		.bind(name)
		.bind(value)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	// ---- health --------------------------------------------------------------------

	async fn db_version(&self) -> ClResult<i64> {
		// The framework module's row only; no row means unmigrated, version 0. A read error stays
		// an `Err` so `/readyz` can report `db: "fail"`.
		Ok(sqlx::query_scalar("SELECT version FROM schema_version WHERE module = $1")
			.bind(crate::schema::MODULE_NAME)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()?
			.unwrap_or(0))
	}

	// ---- auth middleware -----------------------------------------------------------

	async fn account_for_token(&self, uid: &str) -> ClResult<Option<TokenAccount>> {
		let row: Option<(i64, i64, bool, String)> = sqlx::query_as(
			"SELECT a.id, a.token_epoch,
			        EXISTS (SELECT 1 FROM memberships m
			                 WHERE m.account_id = a.id
			                   AND m.org_id = (SELECT id FROM orgs WHERE kind = 'ROOT')
			                   AND m.role IN ('ADMIN', 'OWNER')
			                   AND m.accepted_at IS NOT NULL),
			        a.status
			   FROM accounts a WHERE a.uid = $1",
		)
		.bind(uid)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(row.map(|(id, token_epoch, is_root_admin, status)| TokenAccount {
			id,
			token_epoch,
			is_root_admin,
			status,
		}))
	}

	async fn api_key_by_prefix(&self, prefix: &str) -> ClResult<Option<ApiKey>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{} WHERE k.prefix = $1", api_key_select())))
			.bind(prefix)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(api_key_row)
	}

	async fn touch_api_key(&self, id: i64, at: Timestamp) -> ClResult<()> {
		sqlx::query("UPDATE api_keys SET last_used_at = $1 WHERE id = $2")
			.bind(at.0)
			.bind(id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(())
	}

	async fn org_membership_role(
		&self,
		account_id: i64,
		org_uid: &str,
	) -> ClResult<Option<(i64, Role)>> {
		let row: Option<(i64, Option<i64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
			"{}
			 SELECT o.id, ({}) FROM orgs o WHERE o.uid = $1 AND o.status = 'ACTIVE'",
			ancestors("uid = $1", true, true),
			best_role("$2"),
		)))
		.bind(org_uid)
		.bind(account_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(row.and_then(|(id, rank)| rank.map(|r| (id, role_of(r)))))
	}

	async fn org_role(&self, account_id: i64, org_id: i64) -> ClResult<Option<Role>> {
		let rank: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(sqlx::AssertSqlSafe(format!(
			"{}
			 SELECT ({})",
			ancestors("id = $1", true, true),
			best_role("$2"),
		)))
		.bind(org_id)
		.bind(account_id)
		.fetch_optional(&mut *self.reader().await?)
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
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()?
			.ok_or_else(|| Error::internal("no root org"))?;
		// `set` losing a race is fine: both racers read the same row.
		let _ = self.root_org.set(id);
		Ok(id)
	}
}

/// The cycle guard on the ancestor walk: `orgs.parent_id` permits a cycle and a recursive CTE
/// over one would spin. At most this many rows, the anchor included.
pub(crate) const MAX_ORG_DEPTH: i64 = 16;

/// The one `api_keys` read: the key row plus the account, org and membership columns every
/// caller needs. It binds no parameter itself, so a caller's `WHERE` starts at `$1`.
///
/// `a.status`/`o.status` are aliased because a named mapper cannot tell two `status` columns
/// apart; `o.status` is selected rather than filtered so a suspended org answers
/// `E-AUTH-KEY-REVOKED`, never "unknown key". `member` walks ancestors as `org_role` does: the
/// **anchor** ignores `ACTIVE`, the **step** requires it.
pub(crate) fn api_key_select() -> String {
	format!(
		"SELECT k.id, k.uid, k.org_id, k.account_id, k.name, \
		 k.prefix, k.key_hash, k.scopes, k.created_at, k.last_used_at, k.expires_at, \
		 k.revoked_at, a.status AS account_status, o.status AS org_status, \
		 EXISTS ({} SELECT 1 FROM anc JOIN memberships m ON m.org_id = anc.id \
		          WHERE m.account_id = k.account_id AND m.accepted_at IS NOT NULL) AS member \
		 FROM api_keys k \
		 JOIN accounts a ON a.id = k.account_id \
		 JOIN orgs o ON o.id = k.org_id",
		ancestors("id = k.org_id", false, true)
	)
}

/// Maps [`api_key_select`]'s row.
pub(crate) fn api_key_row(row: &PgRow) -> ClResult<ApiKey> {
	Ok(ApiKey {
		id: row.try_get("id").db()?,
		uid: ApiKeyId::from_trusted(row.try_get::<String, _>("uid").db()?),
		org_id: row.try_get("org_id").db()?,
		account_id: row.try_get("account_id").db()?,
		name: row.try_get("name").db()?,
		prefix: row.try_get("prefix").db()?,
		key_hash: row.try_get("key_hash").db()?,
		scopes: row.try_get("scopes").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		last_used_at: row.try_get::<Option<i64>, _>("last_used_at").db()?.map(Timestamp),
		expires_at: row.try_get::<Option<i64>, _>("expires_at").db()?.map(Timestamp),
		revoked_at: row.try_get::<Option<i64>, _>("revoked_at").db()?.map(Timestamp),
		account_status: row.try_get("account_status").db()?,
		org_status: row.try_get("org_status").db()?,
		member: row.try_get("member").db()?,
	})
}

/// The org `anchor` selects and every ancestor of it, as `anc(id, parent_id, depth)`. `anchor`
/// is a literal from this crate carrying its own numbered placeholder, never caller input.
/// `anchor_active`/`step_active` add `status = 'ACTIVE'` independently, for [`api_key_select`].
///
/// The depth bound sits in the recursive step: PostgreSQL rejects SQLite's `LIMIT` inside a
/// recursive CTE ("LIMIT in a recursive query is not implemented").
pub(crate) fn ancestors(anchor: &str, anchor_active: bool, step_active: bool) -> String {
	let (anchor_active, step_active) = (
		if anchor_active { " AND status = 'ACTIVE'" } else { "" },
		if step_active { " AND o.status = 'ACTIVE'" } else { "" },
	);
	format!(
		"WITH RECURSIVE anc(id, parent_id, depth) AS (
		         SELECT id, parent_id, 0 FROM orgs WHERE {anchor}{anchor_active}
		   UNION ALL
		         SELECT o.id, o.parent_id, anc.depth + 1 FROM orgs o JOIN anc ON o.id = anc.parent_id
		          WHERE anc.depth < {}{step_active}
		 )",
		MAX_ORG_DEPTH - 1
	)
}

/// The highest role `param` holds anywhere on `anc`, as a `BIGINT` rank, or `NULL` for none.
/// `memberships` stores the role as text with no ordering; [`role_of`] maps the rank back.
fn best_role(param: &str) -> String {
	format!(
		"SELECT MAX(CASE m.role WHEN 'OWNER' THEN 3 WHEN 'ADMIN' THEN 2 ELSE 1 END)::BIGINT
		   FROM memberships m JOIN anc ON m.org_id = anc.id
		  WHERE m.account_id = {param} AND m.accepted_at IS NOT NULL"
	)
}

fn role_of(rank: i64) -> Role {
	match rank {
		3 => Role::Owner,
		2 => Role::Admin,
		_ => Role::Member,
	}
}

// vim: ts=4
