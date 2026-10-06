// SPDX-License-Identifier: MPL-2.0
//! `mintworks_entitle::EntitleStore` over SQLite.

use async_trait::async_trait;
use mintworks_core::error::StatusCode;
use mintworks_core::ids::GrantId;
use mintworks_core::prelude::*;
use mintworks_entitle::{Debit, EntitleStore, Grant, NewGrant, Source};
use sqlx::{Row, sqlite::SqliteRow};

use crate::{
	SqliteStore,
	util::{DbExt, RowExt, RowsExt},
};

/// Every read carries what has been drawn from the grant.
const SELECT: &str = "SELECT g.*,
	(SELECT COALESCE(SUM(u.amount), 0) FROM usage u WHERE u.grant_id = g.id) AS used
	FROM grants g";

/// Active at `?` (bound twice), in drain order: soonest expiry first, forever last.
const ACTIVE: &str = "g.valid_from <= ? AND (g.valid_until IS NULL OR g.valid_until > ?)
	ORDER BY g.valid_until IS NULL, g.valid_until, g.id";

fn grant_row(row: &SqliteRow) -> ClResult<Grant> {
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
		 source_ref, created_at) VALUES (?, ?, ?, 0, ?, NULL, 'MANUAL', 'overdraft', ?)
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
		"SELECT id FROM grants WHERE org_id = ? AND key = ? AND source = 'MANUAL'
		 AND source_ref = 'overdraft'",
	)
	.bind(d.org_id)
	.bind(&d.key)
	.fetch_one(&mut *tx.lock().await?)
	.await
	.db()
}

#[async_trait]
impl EntitleStore for SqliteStore {
	async fn grant_insert(&self, new: &NewGrant) -> ClResult<Grant> {
		let mut conn = self.conn().await?;
		sqlx::query(
			"INSERT INTO grants (uid, org_id, key, amount, valid_from, valid_until, source,
			 source_ref, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
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
			"{SELECT} WHERE g.uid = ?
			 OR (g.org_id = ? AND g.key = ? AND g.source = ? AND g.source_ref = ?)"
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
			"{SELECT} WHERE g.org_id = ? AND (? IS NULL OR g.key = ?) AND {ACTIVE}"
		)))
		.bind(org_id)
		.bind(key)
		.bind(key)
		.bind(now.0)
		.bind(now.0)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(grant_row)
	}

	async fn grants_of_org(&self, org_id: i64) -> ClResult<Vec<Grant>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{SELECT} WHERE g.org_id = ? ORDER BY g.id DESC")))
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
			"UPDATE grants SET valid_until = ?1
			 WHERE org_id = ?2 AND source = ?3 AND source_ref = ?4
			   AND valid_until IS NOT NULL AND (?1 IS NULL OR valid_until < ?1)",
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
			"UPDATE grants SET valid_until = ?
			 WHERE org_id = ? AND source = ? AND source_ref = ?
			   AND (valid_until IS NULL OR valid_until > ?)",
		)
		.bind(at.0)
		.bind(org_id)
		.bind(source.as_str())
		.bind(source_ref)
		.bind(at.0)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(hit.rows_affected())
	}

	async fn usage_debit(&self, d: &Debit) -> ClResult<bool> {
		let tx = self.write_tx().await?;
		let seen: Vec<(String, i64)> = sqlx::query_as(
			"SELECT key, SUM(amount) FROM usage WHERE org_id = ? AND idem_key = ? GROUP BY key",
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
			"{SELECT} WHERE g.org_id = ? AND g.key = ? AND {ACTIVE}"
		)))
		.bind(d.org_id)
		.bind(&d.key)
		.bind(d.at.0)
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
				 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
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
		sqlx::query_scalar("SELECT id FROM orgs WHERE uid = ?")
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}
}

// vim: ts=4
