// SPDX-License-Identifier: MPL-2.0
//! `mintworks_entitle::EntitleStore` over PostgreSQL — the SQLite adapter's `entitle.rs` in PG
//! dialect.

use async_trait::async_trait;
use mintworks_core::error::StatusCode;
use mintworks_core::ids::GrantId;
use mintworks_core::prelude::*;
use mintworks_entitle::{Debit, EntitleStore, Grant, NewGrant, SeatOutcome, Source};
use sqlx::{Row, postgres::PgRow};

use crate::core::MAX_ORG_DEPTH;
use crate::{
	PgStore,
	util::{DbExt, RowExt, RowsExt},
};

/// Every read carries what has been drawn from the grant.
const SELECT: &str = "SELECT g.*,
	(SELECT COALESCE(SUM(u.amount), 0)::BIGINT FROM usage u WHERE u.grant_id = g.id) AS used
	FROM grants g";

/// Active at `$3`, in drain order: soonest expiry first, forever last. Both callers bind `now`
/// third, after the org and the key.
const ACTIVE: &str = "g.valid_from <= $3 AND (g.valid_until IS NULL OR g.valid_until > $3)
	ORDER BY g.valid_until ASC NULLS LAST, g.id";

fn grant_row(row: &PgRow) -> ClResult<Grant> {
	Ok(Grant {
		id: row.try_get("id").db()?,
		uid: GrantId::from_trusted(row.try_get::<String, _>("uid").db()?),
		org_id: row.try_get("org_id").db()?,
		key: row.try_get("key").db()?,
		amount: row.try_get("amount").db()?,
		valid_from: Timestamp(row.try_get("valid_from").db()?),
		valid_until: row.try_get::<Option<i64>, _>("valid_until").db()?.map(Timestamp),
		source: row.try_get::<String, _>("source").db()?.parse()?,
		source_ref: row.try_get("source_ref").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		used: row.try_get("used").db()?,
	})
}

fn org_seat_row(row: &PgRow) -> ClResult<(AccountId, Timestamp)> {
	let uid = AccountId::from_trusted(row.try_get::<String, _>("uid").db()?);
	Ok((uid, Timestamp(row.try_get("assigned_at").db()?)))
}

fn account_seat_row(row: &PgRow) -> ClResult<(OrgId, String, Timestamp)> {
	let uid = OrgId::from_trusted(row.try_get::<String, _>("uid").db()?);
	Ok((uid, row.try_get("key").db()?, Timestamp(row.try_get("assigned_at").db()?)))
}

fn released_seat_row(row: &PgRow) -> ClResult<(OrgId, String)> {
	Ok((OrgId::from_trusted(row.try_get::<String, _>("uid").db()?), row.try_get("key").db()?))
}

/// The org's `MANUAL`/`overdraft` grant for `d.key`, made on first use.
async fn overdraft_grant(tx: &crate::WriteTx, d: &Debit) -> ClResult<i64> {
	sqlx::query(
		"INSERT INTO grants (uid, org_id, key, amount, valid_from, valid_until, source,
		 source_ref, created_at) VALUES ($1, $2, $3, 0, $4, NULL, 'MANUAL', 'overdraft', $5)
		 ON CONFLICT (org_id, key, source, source_ref) DO NOTHING",
	)
	.bind(GrantId::generate().as_str())
	.bind(d.org_id)
	.bind(&d.key)
	.bind(d.at.0)
	.bind(Timestamp::now().0)
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;
	sqlx::query_scalar(
		"SELECT id FROM grants WHERE org_id = $1 AND key = $2 AND source = 'MANUAL'
		 AND source_ref = 'overdraft'",
	)
	.bind(d.org_id)
	.bind(&d.key)
	.fetch_one(&mut *tx.lock().await?)
	.await
	.db()
}

#[async_trait]
impl EntitleStore for PgStore {
	async fn grant_insert(&self, new: &NewGrant) -> ClResult<Grant> {
		let mut conn = self.conn().await?;
		sqlx::query(
			"INSERT INTO grants (uid, org_id, key, amount, valid_from, valid_until, source,
			 source_ref, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
			 ON CONFLICT (org_id, key, source, source_ref) DO NOTHING",
		)
		.bind(new.uid.as_str())
		.bind(new.org_id)
		.bind(&new.key)
		.bind(new.amount)
		.bind(new.valid_from.0)
		.bind(new.valid_until.map(|t| t.0))
		.bind(new.source.as_str())
		.bind(&new.source_ref)
		.bind(Timestamp::now().0)
		.execute(&mut *conn)
		.await
		.db()?;
		// The new row by uid, or on a conflict the existing one — which only a non-NULL ref has.
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{SELECT} WHERE g.uid = $1
			 OR (g.org_id = $2 AND g.key = $3 AND g.source = $4 AND g.source_ref = $5)"
		)))
		.bind(new.uid.as_str())
		.bind(new.org_id)
		.bind(&new.key)
		.bind(new.source.as_str())
		.bind(&new.source_ref)
		.fetch_optional(&mut *conn)
		.await
		.one(grant_row)?
		.ok_or_else(|| Error::internal("grant_insert: row vanished"))
	}

	async fn grants_active(
		&self,
		org_id: i64,
		key: Option<&str>,
		now: Timestamp,
	) -> ClResult<Vec<Grant>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{SELECT} WHERE g.org_id = $1 AND ($2::TEXT IS NULL OR g.key = $2) AND {ACTIVE}"
		)))
		.bind(org_id)
		.bind(key)
		.bind(now.0)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(grant_row)
	}

	async fn grants_of_org(&self, org_id: i64) -> ClResult<Vec<Grant>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{SELECT} WHERE g.org_id = $1 ORDER BY g.id DESC")))
			.bind(org_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(grant_row)
	}

	async fn grants_extend(
		&self,
		org_id: i64,
		source: Source,
		source_ref: &str,
		valid_until: Option<Timestamp>,
	) -> ClResult<u64> {
		let hit = sqlx::query(
			"UPDATE grants SET valid_until = $1
			 WHERE org_id = $2 AND source = $3 AND source_ref = $4
			   AND valid_until IS NOT NULL AND ($1::BIGINT IS NULL OR valid_until < $1)",
		)
		.bind(valid_until.map(|t| t.0))
		.bind(org_id)
		.bind(source.as_str())
		.bind(source_ref)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(hit.rows_affected())
	}

	async fn grants_cut(
		&self,
		org_id: i64,
		source: Source,
		source_ref: &str,
		at: Timestamp,
	) -> ClResult<u64> {
		let hit = sqlx::query(
			"UPDATE grants SET valid_until = $1
			 WHERE org_id = $2 AND source = $3 AND source_ref = $4
			   AND (valid_until IS NULL OR valid_until > $1)",
		)
		.bind(at.0)
		.bind(org_id)
		.bind(source.as_str())
		.bind(source_ref)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(hit.rows_affected())
	}

	async fn usage_debit(&self, d: &Debit) -> ClResult<bool> {
		let tx = self.write_tx().await?;
		let seen: Vec<(String, i64)> = sqlx::query_as(
			"SELECT key, SUM(amount)::BIGINT FROM usage WHERE org_id = $1 AND idem_key = $2
			 GROUP BY key",
		)
		.bind(d.org_id)
		.bind(&d.idem_key)
		.fetch_all(&mut *tx.lock().await?)
		.await
		.db()?;
		match seen.as_slice() {
			[] => {}
			[(key, amount)] if *key == d.key && *amount == d.amount => return Ok(true),
			_ => {
				return Err(Error::coded(
					StatusCode::CONFLICT,
					mintworks_entitle::E_IDEM,
					"idempotency key reused with a different debit",
				));
			}
		}
		let grants = sqlx::query(sqlx::AssertSqlSafe(format!(
			"{SELECT} WHERE g.org_id = $1 AND g.key = $2 AND {ACTIVE}"
		)))
		.bind(d.org_id)
		.bind(&d.key)
		.bind(d.at.0)
		.fetch_all(&mut *tx.lock().await?)
		.await
		.all(grant_row)?;
		let balance: i64 = grants.iter().map(|g| g.amount - g.used).sum();
		if !d.overdraw && balance < d.amount {
			return Ok(false);
		}
		let mut draws: Vec<(Option<i64>, i64)> = Vec::new();
		let mut left = d.amount;
		for g in &grants {
			let avail = g.amount - g.used;
			if left == 0 {
				break;
			}
			if avail > 0 {
				let take = avail.min(left);
				draws.push((Some(g.id), take));
				left -= take;
			}
		}
		// Only an overdraw is left over here: it lands on the last grant drained.
		if left > 0 {
			if let Some(last) = draws.last_mut() {
				last.1 += left;
			} else {
				// No active grant: a zero forever `overdraft` grant carries the debt into the balance.
				let id = match grants.last() {
					Some(g) => g.id,
					None => overdraft_grant(&tx, d).await?,
				};
				draws.push((Some(id), left));
			}
		}
		for (seq, (grant_id, amount)) in draws.into_iter().enumerate() {
			sqlx::query(
				"INSERT INTO usage (org_id, key, grant_id, amount, at, idem_key, seq, account_id)
				 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
			)
			.bind(d.org_id)
			.bind(&d.key)
			.bind(grant_id)
			.bind(amount)
			.bind(d.at.0)
			.bind(&d.idem_key)
			.bind(i64::try_from(seq).map_err(|_| Error::internal("usage seq overflow"))?)
			.bind(d.account_id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}
		tx.commit().await?;
		Ok(true)
	}

	async fn entitle_org_id(&self, uid: &OrgId) -> ClResult<Option<i64>> {
		sqlx::query_scalar("SELECT id FROM orgs WHERE uid = $1")
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn seat_assign(
		&self,
		org_id: i64,
		key: &str,
		account_id: i64,
		at: Timestamp,
	) -> ClResult<SeatOutcome> {
		let tx = self.write_tx().await?;
		let member: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(member_sql()))
			.bind(org_id)
			.bind(account_id)
			.fetch_one(&mut *tx.lock().await?)
			.await
			.db()?;
		if !member {
			return Ok(SeatOutcome::NotMember);
		}
		let seated: bool = sqlx::query_scalar(
			"SELECT EXISTS (SELECT 1 FROM seat_assignments
			  WHERE org_id = $1 AND key = $2 AND account_id = $3 AND released_at IS NULL)",
		)
		.bind(org_id)
		.bind(key)
		.bind(account_id)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		if seated {
			return Ok(SeatOutcome::AlreadySeated);
		}
		// A row whose release event was lost would otherwise count again once its holder rejoins.
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{} UPDATE seat_assignments AS s SET released_at = $3
			  WHERE s.org_id = $1 AND s.key = $2 AND s.released_at IS NULL AND NOT {QUALIFIES}",
			subtree_cte()
		)))
		.bind(org_id)
		.bind(key)
		.bind(at.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		let limit: i64 = sqlx::query_scalar(
			"SELECT COALESCE(MAX(amount), 0) FROM grants
			  WHERE org_id = $1 AND key = $2 AND valid_from <= $3
			    AND (valid_until IS NULL OR valid_until > $3)",
		)
		.bind(org_id)
		.bind(key)
		.bind(at.0)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		let active: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
			"{} SELECT COUNT(*) FROM seat_assignments s
			  WHERE s.org_id = $1 AND s.key = $2 AND s.released_at IS NULL AND {QUALIFIES}",
			subtree_cte()
		)))
		.bind(org_id)
		.bind(key)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		if active >= limit {
			return Ok(SeatOutcome::Full(limit));
		}
		sqlx::query(
			"INSERT INTO seat_assignments (org_id, key, account_id, assigned_at)
			 VALUES ($1, $2, $3, $4)",
		)
		.bind(org_id)
		.bind(key)
		.bind(account_id)
		.bind(at.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		tx.commit().await?;
		Ok(SeatOutcome::Inserted)
	}

	async fn seat_release(
		&self,
		org_id: i64,
		key: &str,
		account_id: i64,
		at: Timestamp,
	) -> ClResult<bool> {
		let hit = sqlx::query(
			"UPDATE seat_assignments SET released_at = $1
			 WHERE org_id = $2 AND key = $3 AND account_id = $4 AND released_at IS NULL",
		)
		.bind(at.0)
		.bind(org_id)
		.bind(key)
		.bind(account_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(hit.rows_affected() > 0)
	}

	async fn seats_of_org(&self, org_id: i64, key: &str) -> ClResult<Vec<(AccountId, Timestamp)>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{} SELECT a.uid, s.assigned_at FROM seat_assignments s
			   JOIN accounts a ON a.id = s.account_id
			  WHERE s.org_id = $1 AND s.key = $2 AND s.released_at IS NULL AND {QUALIFIES}
			  ORDER BY s.assigned_at, s.id",
			subtree_cte()
		)))
		.bind(org_id)
		.bind(key)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(org_seat_row)
	}

	async fn seats_of_account(&self, account_id: i64) -> ClResult<Vec<(OrgId, String, Timestamp)>> {
		sqlx::query(
			"SELECT o.uid, s.key, s.assigned_at FROM seat_assignments s
			   JOIN orgs o ON o.id = s.org_id
			  WHERE s.account_id = $1 AND s.released_at IS NULL
			  ORDER BY s.assigned_at, s.id",
		)
		.bind(account_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(account_seat_row)
	}

	async fn seats_release_account(&self, account_id: i64, at: Timestamp) -> ClResult<u64> {
		let hit = sqlx::query(
			"UPDATE seat_assignments SET released_at = $1
			 WHERE account_id = $2 AND released_at IS NULL",
		)
		.bind(at.0)
		.bind(account_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(hit.rows_affected())
	}

	async fn seats_release_stale(
		&self,
		account_id: i64,
		at: Timestamp,
	) -> ClResult<Vec<(OrgId, String)>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{} UPDATE seat_assignments AS s SET released_at = $2 FROM orgs o
			  WHERE o.id = s.org_id AND s.account_id = $1 AND s.released_at IS NULL AND {STALE}
			  RETURNING o.uid, s.key",
			account_seats_cte()
		)))
		.bind(account_id)
		.bind(at.0)
		.fetch_all(&mut *self.conn().await?)
		.await
		.all(released_seat_row)
	}
}

/// `d`: the org `$1` and its active descendants, `MAX_ORG_DEPTH` deep.
fn subtree_cte() -> String {
	format!(
		"WITH RECURSIVE d(id, depth) AS (
		         SELECT $1::BIGINT, 0
		   UNION ALL
		         SELECT c.id, d.depth + 1 FROM orgs c JOIN d ON c.parent_id = d.id
		          WHERE c.status = 'ACTIVE' AND d.depth < {}
		 )",
		MAX_ORG_DEPTH - 1
	)
}

/// `d`: every org the account `$1` holds an active seat in (`root`), with its active
/// descendants, `MAX_ORG_DEPTH` deep.
fn account_seats_cte() -> String {
	format!(
		"WITH RECURSIVE d(root, id, depth) AS (
		         SELECT DISTINCT org_id, org_id, 0 FROM seat_assignments
		          WHERE account_id = $1::BIGINT AND released_at IS NULL
		   UNION ALL
		         SELECT d.root, c.id, d.depth + 1 FROM orgs c JOIN d ON c.parent_id = d.id
		          WHERE c.status = 'ACTIVE' AND d.depth < {}
		 )",
		MAX_ORG_DEPTH - 1
	)
}

/// Over [`account_seats_cte`]: the seat `s`'s holder is no accepted member of its org's `d`.
const STALE: &str = "NOT EXISTS (SELECT 1 FROM d JOIN memberships m ON m.org_id = d.id
	WHERE d.root = s.org_id AND m.account_id = s.account_id AND m.accepted_at IS NOT NULL)";

/// The seat `s`'s holder is an accepted member of a `d` org.
const QUALIFIES: &str = "EXISTS (SELECT 1 FROM d JOIN memberships m ON m.org_id = d.id
	WHERE m.account_id = s.account_id AND m.accepted_at IS NOT NULL)";

/// Binds `(org_id, account_id)`: an accepted membership in the org's subtree.
fn member_sql() -> String {
	format!(
		"{} SELECT EXISTS (SELECT 1 FROM d JOIN memberships m ON m.org_id = d.id
		                WHERE m.account_id = $2 AND m.accepted_at IS NOT NULL)",
		subtree_cte()
	)
}

// vim: ts=4
