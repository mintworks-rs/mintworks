//! `mintworks_entitle::EntitleStore` over PostgreSQL — the SQLite adapter's `entitle.rs` in PG
//! dialect.

use async_trait::async_trait;
use mintworks_core::error::StatusCode;
use mintworks_core::ids::GrantId;
use mintworks_core::prelude::*;
use mintworks_entitle::{Debit, EntitleStore, Grant, NewGrant, Source};
use sqlx::{Row, postgres::PgRow};

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
}

// vim: ts=4
