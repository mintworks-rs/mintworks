// SPDX-License-Identifier: MPL-2.0
//! `mintworks_core::refs::RefStore` over SQLite.

use async_trait::async_trait;
use mintworks_core::ids::RefId;
use mintworks_core::prelude::*;
use mintworks_core::refs::{NewRef, Ref, RefStatus, RefStore, RefUse, slug_taken};
use sqlx::{Row, sqlite::SqliteRow};

use crate::{
	SqliteStore,
	util::{DbExt, RowExt, map_db},
};

/// A use held by a draft that is gone: unused, and reclaimed by the next redeem of its ref.
/// By `uid`: a deleted draft's rowid is reused by the next one, which would adopt the hold.
macro_rules! orphan {
	() => {
		"held = 1 AND NOT EXISTS (SELECT 1 FROM invoices i WHERE i.uid = ref_uses.invoice_uid)"
	};
}
const ORPHAN: &str = orphan!();

/// Every read joins the owner's name in for the public preview. `uses_left` counts orphans
/// back in, so a quote sees the use free before the next redeem reclaims it.
const SELECT: &str = concat!(
	"SELECT r.*, o.name AS org_name, r.uses_left + (SELECT COUNT(*) FROM ref_uses
	 WHERE ref_uses.ref_id = r.id AND ",
	orphan!(),
	") AS uses_free FROM refs r JOIN orgs o ON o.id = r.org_id"
);

fn ref_row(row: &SqliteRow) -> ClResult<Ref> {
	let params: String = row.try_get("params").db()?;
	Ok(Ref {
		id: row.try_get("id").db()?,
		uid: RefId::from_trusted(row.try_get::<String, _>("uid").db()?),
		code: row.try_get("code").db()?,
		ref_type: row.try_get("type").db()?,
		org_id: row.try_get("org_id").db()?,
		created_by: row.try_get("created_by").db()?,
		target: row.try_get("target").db()?,
		email: row.try_get("email").db()?,
		params: serde_json::from_str(&params)
			.map_err(|e| Error::internal(format!("refs.params: {e}")))?,
		uses_left: row.try_get("uses_free").db()?,
		expires_at: row.try_get::<Option<i64>, _>("expires_at").db()?.map(Timestamp),
		status: row.try_get::<String, _>("status").db()?.parse()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		org_name: row.try_get("org_name").db()?,
	})
}

fn use_row(row: &SqliteRow) -> ClResult<RefUse> {
	Ok(RefUse {
		id: row.try_get("id").db()?,
		ref_id: row.try_get("ref_id").db()?,
		account_id: row.try_get("account_id").db()?,
		org_id: row.try_get("org_id").db()?,
		at: Timestamp(row.try_get("at").db()?),
	})
}

#[async_trait]
impl RefStore for SqliteStore {
	async fn ref_insert(&self, new: &NewRef) -> ClResult<Ref> {
		let id: i64 = sqlx::query_scalar(
			"INSERT INTO refs (uid, code, type, org_id, created_by, target, email, params,
			 uses_left, expires_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
			 RETURNING id",
		)
		.bind(new.uid.as_str())
		.bind(&new.code)
		.bind(&new.ref_type)
		.bind(new.org_id)
		.bind(new.created_by)
		.bind(&new.target)
		.bind(&new.email)
		.bind(new.params.to_string())
		.bind(new.uses_left)
		.bind(new.expires_at.map(|t| t.0))
		.bind(Timestamp::now().0)
		.fetch_one(&mut *self.conn().await?)
		.await
		.map_err(|e| match &e {
			// `uid` is a fresh ULID, so the only unique a caller can hit is `code`.
			sqlx::Error::Database(db) if db.is_unique_violation() => slug_taken(),
			_ => map_db(&e),
		})?;
		sqlx::query(sqlx::AssertSqlSafe(format!("{SELECT} WHERE r.id = ?")))
			.bind(id)
			.fetch_optional(&mut *self.conn().await?)
			.await
			.one(ref_row)?
			.ok_or_else(|| Error::internal("inserted ref vanished"))
	}

	async fn ref_by_code(&self, code: &str) -> ClResult<Option<Ref>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{SELECT} WHERE r.code = ?")))
			.bind(code)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(ref_row)
	}

	async fn ref_by_uid(&self, uid: &RefId) -> ClResult<Option<Ref>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{SELECT} WHERE r.uid = ?")))
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(ref_row)
	}

	async fn refs_of_org(&self, org_id: i64, ref_type: Option<&str>) -> ClResult<Vec<Ref>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{SELECT} WHERE r.org_id = ? AND (? IS NULL OR r.type = ?) ORDER BY r.id DESC"
		)))
		.bind(org_id)
		.bind(ref_type)
		.bind(ref_type)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?
		.iter()
		.map(ref_row)
		.collect()
	}

	async fn ref_set_status(&self, org_id: i64, uid: &RefId, status: RefStatus) -> ClResult<bool> {
		let hit = sqlx::query("UPDATE refs SET status = ? WHERE uid = ? AND org_id = ?")
			.bind(status.as_str())
			.bind(uid.as_str())
			.bind(org_id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(hit.rows_affected() > 0)
	}

	async fn ref_redeem(
		&self,
		ref_id: i64,
		account_id: i64,
		org_id: i64,
		hold: Option<i64>,
		now: Timestamp,
	) -> ClResult<Option<(RefUse, bool)>> {
		let tx = self.write_tx().await?;
		let freed = sqlx::query(sqlx::AssertSqlSafe(format!(
			"DELETE FROM ref_uses WHERE ref_id = ? AND {ORPHAN}"
		)))
		.bind(ref_id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?
		.rows_affected();
		if freed > 0 {
			sqlx::query(
				"UPDATE refs SET uses_left = uses_left + ? WHERE id = ? AND uses_left IS NOT NULL",
			)
			.bind(i64::try_from(freed).map_err(|_| Error::internal("ref_uses: count"))?)
			.bind(ref_id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}
		let existing = sqlx::query(
			"SELECT * FROM ref_uses WHERE ref_id = ? AND (account_id = ? OR org_id = ?)
			 ORDER BY account_id <> ? LIMIT 1",
		)
		.bind(ref_id)
		.bind(account_id)
		.bind(org_id)
		.bind(account_id)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.one(use_row)?;
		if let Some(u) = existing {
			tx.commit().await?;
			return Ok(Some((u, false)));
		}
		let hit = sqlx::query(
			"UPDATE refs SET uses_left = uses_left - 1
			 WHERE id = ? AND status = 'ACTIVE'
			   AND (uses_left IS NULL OR uses_left > 0)
			   AND (expires_at IS NULL OR expires_at > ?)",
		)
		.bind(ref_id)
		.bind(now.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		if hit.rows_affected() == 0 {
			tx.commit().await?;
			return Ok(None);
		}
		let used = sqlx::query(
			"INSERT INTO ref_uses (ref_id, account_id, org_id, at, invoice_uid, held)
			 VALUES (?, ?, ?, ?, (SELECT uid FROM invoices WHERE id = ?), ?) RETURNING *",
		)
		.bind(ref_id)
		.bind(account_id)
		.bind(org_id)
		.bind(now.0)
		.bind(hold)
		.bind(hold.is_some())
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		let used = use_row(&used)?;
		tx.commit().await?;
		Ok(Some((used, true)))
	}

	async fn ref_use_settle(&self, invoice_id: i64) -> ClResult<()> {
		sqlx::query(
			"UPDATE ref_uses SET held = 0
			 WHERE invoice_uid = (SELECT uid FROM invoices WHERE id = ?) AND held = 1",
		)
		.bind(invoice_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn ref_use_of(&self, ref_id: i64, account_id: i64) -> ClResult<Option<RefUse>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"SELECT * FROM ref_uses WHERE ref_id = ? AND account_id = ? AND NOT ({ORPHAN})"
		)))
		.bind(ref_id)
		.bind(account_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(use_row)
	}

	async fn ref_by_id(&self, id: i64) -> ClResult<Option<Ref>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{SELECT} WHERE r.id = ?")))
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(ref_row)
	}

	async fn ref_uses_of_org(&self, org_id: i64) -> ClResult<Vec<RefUse>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"SELECT * FROM ref_uses WHERE org_id = ? AND NOT ({ORPHAN}) ORDER BY id"
		)))
		.bind(org_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?
		.iter()
		.map(use_row)
		.collect()
	}
}

// vim: ts=4
