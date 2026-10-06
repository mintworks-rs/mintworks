//! `NavStore` over PostgreSQL — the SQLite adapter's `nav.rs` in PG dialect.
//!
//! The two export selections read `invoices` only. They must never join `nav_submissions`: an
//! invoice that failed to report to NAV is still part of the seller's turnover and is still
//! statutorily exportable.

use async_trait::async_trait;
use mintworks_core::prelude::*;
use mintworks_nav::store::NavStore;
use mintworks_nav::submission::{NavArchive, NavOp, NavSubmission, NavVerdict};
use sqlx::{Row, postgres::PgRow};

use crate::{
	PgStore,
	util::{DbExt, RowExt},
};

/// Read by column name: `SELECT *` follows the DDL's column order.
fn submission_row(row: &PgRow) -> ClResult<NavSubmission> {
	Ok(NavSubmission {
		id: row.try_get("id").db()?,
		invoice_id: row.try_get("invoice_id").db()?,
		op: row.try_get::<String, _>("op").db()?.parse()?,
		transaction_id: row.try_get("transaction_id").db()?,
		idx: row.try_get("idx").db()?,
		verdict: row
			.try_get::<Option<String>, _>("verdict")
			.db()?
			.map(|s| s.parse())
			.transpose()?,
		error_code: row.try_get("error_code").db()?,
		error_msg: row.try_get("error_msg").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		done_at: row.try_get::<Option<i64>, _>("done_at").db()?.map(Timestamp),
		resolved_at: row.try_get::<Option<i64>, _>("resolved_at").db()?.map(Timestamp),
		batch_uid: row.try_get("batch_uid").db()?,
	})
}

/// Closes an in-range set over storno pairs in both directions; a range showing one half of a
/// pair would misstate turnover. `number IS NOT NULL AND seller_id = $1` is re-applied so a row
/// merely *pointing* at an in-range invoice cannot enter; the parentheses are load-bearing.
macro_rules! close_over_storno_pairs {
	() => {
		"
	SELECT id FROM invoices
	 WHERE number IS NOT NULL AND seller_id = $1
	   AND (id IN (SELECT id FROM sel)
	     OR original_invoice_id IN (SELECT id FROM sel)
	     OR id IN (SELECT original_invoice_id FROM invoices
	                WHERE id IN (SELECT id FROM sel) AND original_invoice_id IS NOT NULL))
	 ORDER BY id"
	};
}

/// The sweep's selection: issued invoices of one seller with no `nav_submissions` row at all.
/// A faulted filing is not picked up — its `jobs` row retries on its own backoff.
///
/// Binds, in order: `seller_id`, `limit`. `pub` for adapter tests only; not store API.
#[doc(hidden)]
pub const UNFILED: &str = "SELECT i.id FROM invoices i
	 WHERE i.seller_id = $1 AND i.number IS NOT NULL
	   AND NOT EXISTS (SELECT 1 FROM nav_submissions s WHERE s.invoice_id = i.id)
	 ORDER BY i.id ASC LIMIT $2";

/// [`UNFILED`] narrowed to `kind = 'NORMAL'` (a storno waits for its original) and excluding
/// the leader. The third bind is `require_document` as 0/1.
///
/// Binds, in order: `seller_id`, `exclude_invoice_id`, `require_document`, `limit`.
#[doc(hidden)]
pub const BATCH_CANDIDATES: &str = "SELECT i.id FROM invoices i
	 WHERE i.seller_id = $1 AND i.number IS NOT NULL AND i.kind = 'NORMAL'
	   AND i.id <> $2
	   AND NOT EXISTS (SELECT 1 FROM nav_submissions s WHERE s.invoice_id = i.id)
	   AND ($3 = 0 OR EXISTS (SELECT 1 FROM invoice_documents d WHERE d.invoice_id = i.id))
	 ORDER BY i.id ASC LIMIT $4";

/// `from`/`to` are the half-open instant range `numbering::utc_span` makes of Budapest days.
///
/// Binds, in order: `seller_id`, `from`, `to`.
#[doc(hidden)]
pub const BY_DATE: &str = concat!(
	"WITH sel AS (
	   SELECT id FROM invoices
	    WHERE seller_id = $1 AND number IS NOT NULL
	      AND issued_at >= $2 AND issued_at < $3
	 )",
	close_over_storno_pairs!()
);

/// Length-then-lexical: `render_number` overflows its width rather than truncating, so plain
/// text order puts `A2026/1000000` below `A2026/999999`. `COLLATE "C"` is SQLite's byte order;
/// the database's locale collation would reorder punctuation.
///
/// Binds, in order: `seller_id`, `from`, `to`.
#[doc(hidden)]
pub const BY_NUMBER: &str = concat!(
	"WITH sel AS (
	   SELECT id FROM invoices
	    WHERE seller_id = $1 AND number IS NOT NULL
	      AND (length(number), number COLLATE \"C\") >= (length($2), $2 COLLATE \"C\")
	      AND (length(number), number COLLATE \"C\") <= (length($3), $3 COLLATE \"C\")
	 )",
	close_over_storno_pairs!()
);

#[async_trait]
impl NavStore for PgStore {
	async fn create_submission(
		&self,
		invoice_id: i64,
		op: NavOp,
		request_xml: &str,
	) -> ClResult<Option<i64>> {
		// The row and its archive in one transaction: a row without its archive is never visible.
		let tx = self.write_tx().await?;
		let inserted = sqlx::query_scalar(
			"INSERT INTO nav_submissions (invoice_id, op, created_at) VALUES ($1, $2, $3) \
			 RETURNING id",
		)
		.bind(invoice_id)
		.bind(op.as_str())
		.bind(Timestamp::now().0)
		.fetch_one(&mut *tx.lock().await?)
		.await;

		let id: i64 = match inserted {
			Ok(id) => id,
			// `idx_nav_submission_live`: already filed or in flight — not sending is right. The
			// aborted transaction rolls back when `tx` drops.
			Err(sqlx::Error::Database(db)) if db.is_unique_violation() => return Ok(None),
			Err(e) => return Err(crate::util::map_db(&e)),
		};

		sqlx::query("INSERT INTO nav_submission_xml (submission_id, request_xml) VALUES ($1, $2)")
			.bind(id)
			.bind(request_xml)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		tx.commit().await?;
		Ok(Some(id))
	}

	async fn archive_request(&self, id: i64, request_xml: &str) -> ClResult<()> {
		// Keyed through `nav_submissions` so a released member gives zero rows, not an FK error.
		// First write wins: NAV processes only the first request under a `requestId`.
		sqlx::query(
			"INSERT INTO nav_submission_xml (submission_id, request_xml)
			   SELECT id, $1 FROM nav_submissions WHERE id = $2
			   ON CONFLICT(submission_id) DO UPDATE SET request_xml = excluded.request_xml
			     WHERE nav_submission_xml.request_xml IS NULL",
		)
		.bind(request_xml)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn request_archived(&self, id: i64) -> ClResult<bool> {
		sqlx::query_scalar::<_, bool>(
			"SELECT EXISTS(SELECT 1 FROM nav_submission_xml
				WHERE submission_id = $1 AND request_xml IS NOT NULL)",
		)
		.bind(id)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn batch_candidates(
		&self,
		seller_id: i64,
		exclude_invoice_id: i64,
		require_document: bool,
		limit: i64,
	) -> ClResult<Vec<i64>> {
		sqlx::query_scalar(BATCH_CANDIDATES)
			.bind(seller_id)
			.bind(exclude_invoice_id)
			.bind(i64::from(require_document))
			.bind(limit)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn claim_batch(
		&self,
		leader_invoice_id: i64,
		op: NavOp,
		batch_uid: &str,
		ids: &[i64],
	) -> ClResult<Vec<(i64, i64)>> {
		// One write transaction around the whole claim: `idx_nav_submission_live` is the only
		// thing between two leaders and the same invoice in two batches.
		let tx = self.write_tx().await?;
		let now = Timestamp::now().0;
		let mut claimed = Vec::with_capacity(ids.len() + 1);

		// Get-or-claim: a leader may already own a pre-batching row with `batch_uid IS NULL`;
		// `done_at IS NULL` keeps a row `finish` settled without a verdict from being re-POSTed.
		let leader: Option<i64> = sqlx::query_scalar(
			"INSERT INTO nav_submissions (invoice_id, op, created_at, batch_uid)
			 VALUES ($1, $2, $3, $4)
			 ON CONFLICT(invoice_id, op) DO UPDATE SET batch_uid = excluded.batch_uid
			   WHERE (nav_submissions.batch_uid IS NULL
			          OR nav_submissions.batch_uid = excluded.batch_uid)
			     AND nav_submissions.transaction_id IS NULL
			     AND nav_submissions.verdict IS NULL
			     AND nav_submissions.done_at IS NULL
			 RETURNING id",
		)
		.bind(leader_invoice_id)
		.bind(op.as_str())
		.bind(now)
		.bind(batch_uid)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.db()?;
		// Nothing is claimed when the leader is not.
		let Some(leader_id) = leader else {
			return Ok(Vec::new());
		};
		claimed.push((leader_id, leader_invoice_id));

		for &invoice_id in ids {
			// Create, or resume a row this same batch already claimed; anyone else's row drops out.
			let id: Option<i64> = sqlx::query_scalar(
				"INSERT INTO nav_submissions (invoice_id, op, created_at, batch_uid)
				 VALUES ($1, $2, $3, $4)
				 ON CONFLICT(invoice_id, op) DO UPDATE SET batch_uid = excluded.batch_uid
				   WHERE nav_submissions.batch_uid = excluded.batch_uid
				     AND nav_submissions.transaction_id IS NULL
				     AND nav_submissions.verdict IS NULL
				 RETURNING id",
			)
			.bind(invoice_id)
			.bind(op.as_str())
			.bind(now)
			.bind(batch_uid)
			.fetch_optional(&mut *tx.lock().await?)
			.await
			.db()?;
			if let Some(id) = id {
				claimed.push((id, invoice_id));
			}
		}

		tx.commit().await?;
		Ok(claimed)
	}

	async fn submissions_by_batch(&self, batch_uid: &str) -> ClResult<Vec<NavSubmission>> {
		sqlx::query("SELECT * FROM nav_submissions WHERE batch_uid = $1 ORDER BY id ASC")
			.bind(batch_uid)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()?
			.iter()
			.map(submission_row)
			.collect()
	}

	async fn submissions_by_transaction(
		&self,
		transaction_id: &str,
	) -> ClResult<Vec<NavSubmission>> {
		sqlx::query("SELECT * FROM nav_submissions WHERE transaction_id = $1 ORDER BY id ASC")
			.bind(transaction_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()?
			.iter()
			.map(submission_row)
			.collect()
	}

	async fn release_batch(
		&self,
		batch_uid: &str,
		leader_submission_id: i64,
	) -> ClResult<Vec<i64>> {
		// A member `finish` settled keeps its row (only losing its owner); every other member
		// reverts to never-attempted. `done_at`, not `error_code`: only `finish` settles a member.
		let tx = self.write_tx().await?;
		let mut released: Vec<i64> = sqlx::query_scalar(
			"UPDATE nav_submissions SET batch_uid = NULL
			  WHERE batch_uid = $1 AND id <> $2 AND transaction_id IS NULL AND verdict IS NULL
			    AND done_at IS NOT NULL
			 RETURNING invoice_id",
		)
		.bind(batch_uid)
		.bind(leader_submission_id)
		.fetch_all(&mut *tx.lock().await?)
		.await
		.db()?;
		released.extend(
			sqlx::query_scalar::<_, i64>(
				"DELETE FROM nav_submissions
				  WHERE batch_uid = $1 AND id <> $2 AND transaction_id IS NULL AND verdict IS NULL
				    AND done_at IS NULL
				 RETURNING invoice_id",
			)
			.bind(batch_uid)
			.bind(leader_submission_id)
			.fetch_all(&mut *tx.lock().await?)
			.await
			.db()?,
		);
		tx.commit().await?;
		Ok(released)
	}

	async fn release_member(&self, batch_uid: &str, invoice_id: i64) -> ClResult<bool> {
		// A row with a `transaction_id` or a verdict is at NAV and not this batch's to give back.
		let done = sqlx::query(
			"UPDATE nav_submissions SET batch_uid = NULL
			  WHERE batch_uid = $1 AND invoice_id = $2
			    AND transaction_id IS NULL AND verdict IS NULL",
		)
		.bind(batch_uid)
		.bind(invoice_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn submission(&self, id: i64) -> ClResult<Option<NavSubmission>> {
		sqlx::query("SELECT * FROM nav_submissions WHERE id = $1")
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(submission_row)
	}

	async fn submission_by_invoice(&self, invoice_id: i64) -> ClResult<Option<NavSubmission>> {
		sqlx::query("SELECT * FROM nav_submissions WHERE invoice_id = $1 ORDER BY id DESC LIMIT 1")
			.bind(invoice_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(submission_row)
	}

	async fn submission_archive(&self, id: i64) -> ClResult<Option<NavArchive>> {
		sqlx::query(
			"SELECT request_xml, response_xml FROM nav_submission_xml WHERE submission_id = $1",
		)
		.bind(id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(|row| {
			Ok(NavArchive {
				request_xml: row.try_get("request_xml").db()?,
				response_xml: row.try_get("response_xml").db()?,
			})
		})
	}

	async fn archive_response(&self, id: i64, response_xml: &str) -> ClResult<()> {
		// The latest response only, which is the decisive one.
		sqlx::query(
			"INSERT INTO nav_submission_xml (submission_id, response_xml)
			   SELECT id, $1 FROM nav_submissions WHERE id = $2
			   ON CONFLICT(submission_id) DO UPDATE SET response_xml = excluded.response_xml",
		)
		.bind(response_xml)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn set_sent(&self, id: i64, transaction_id: &str, idx: i64) -> ClResult<bool> {
		// `AND transaction_id IS NULL`: a losing second runner must not overwrite NAV's id.
		let done = sqlx::query(
			"UPDATE nav_submissions SET transaction_id = $1, idx = $2 \
			  WHERE id = $3 AND transaction_id IS NULL",
		)
		.bind(transaction_id)
		.bind(idx)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn set_sent_batch(
		&self,
		rows: &[(i64, i64)],
		transaction_id: &str,
	) -> ClResult<Vec<i64>> {
		// One transaction: a member left without the batch's `transactionId` is lost for good.
		let tx = self.write_tx().await?;
		let mut applied = Vec::with_capacity(rows.len());
		for (id, idx) in rows {
			let got: Option<i64> = sqlx::query_scalar(
				"UPDATE nav_submissions SET transaction_id = $1, idx = $2 \
				  WHERE id = $3 AND transaction_id IS NULL RETURNING id",
			)
			.bind(transaction_id)
			.bind(idx)
			.bind(id)
			.fetch_optional(&mut *tx.lock().await?)
			.await
			.db()?;
			applied.extend(got);
		}
		tx.commit().await?;
		Ok(applied)
	}

	async fn finish(
		&self,
		id: i64,
		verdict: Option<NavVerdict>,
		error: Option<(&str, &str)>,
		done_at: Timestamp,
	) -> ClResult<bool> {
		let (code, msg) = error.map_or((None, None), |(c, m)| (Some(c), Some(m)));
		// `AND verdict IS NULL`: a verdict is terminal, and a settled row is never reopened.
		let done = sqlx::query(
			"UPDATE nav_submissions
			    SET verdict = $1, error_code = $2, error_msg = $3, done_at = $4
			  WHERE id = $5 AND verdict IS NULL",
		)
		.bind(verdict.map(NavVerdict::as_str))
		.bind(code)
		.bind(msg)
		.bind(done_at.0)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn record_fault(&self, id: i64, code: &str, message: &str) -> ClResult<()> {
		// `AND verdict IS NULL`: a settled row keeps the reason NAV settled it on.
		sqlx::query(
			"UPDATE nav_submissions SET error_code = $1, error_msg = $2 \
			  WHERE id = $3 AND verdict IS NULL",
		)
		.bind(code)
		.bind(message)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn resolve(&self, id: i64, at: Timestamp) -> ClResult<bool> {
		// The `WHERE` is `awaiting_operator`'s predicate.
		let done = sqlx::query(
			"UPDATE nav_submissions SET resolved_at = $1 \
			  WHERE id = $2 AND resolved_at IS NULL \
			    AND (verdict IN ('REJECTED','FAILED') \
			      OR (verdict IS NULL AND error_code IS NOT NULL))",
		)
		.bind(at.0)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() == 1)
	}

	async fn unfiled_invoices(&self, seller_id: i64, limit: i64) -> ClResult<Vec<i64>> {
		sqlx::query_scalar(UNFILED)
			.bind(seller_id)
			.bind(limit)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn awaiting_operator(&self, seller_id: i64) -> ClResult<i64> {
		// The second disjunct is the open row `job::report` leaves on `REQUEST_ID_NOT_UNIQUE`.
		sqlx::query_scalar(
			"SELECT COUNT(*) FROM invoices i
			  WHERE i.seller_id = $1 AND i.number IS NOT NULL
			    AND EXISTS (SELECT 1 FROM nav_submissions s
			                 WHERE s.invoice_id = i.id AND s.resolved_at IS NULL
			                   AND (s.verdict IN ('REJECTED','FAILED')
			                     OR (s.verdict IS NULL AND s.error_code IS NOT NULL)))",
		)
		.bind(seller_id)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn export_ids_by_date(&self, seller_id: i64, from: &str, to: &str) -> ClResult<Vec<i64>> {
		let (from, to) = mintworks_invoice::numbering::utc_span(from, to)?;
		sqlx::query_scalar(BY_DATE)
			.bind(seller_id)
			.bind(from.0)
			.bind(to.0)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn export_ids_by_number(
		&self,
		seller_id: i64,
		from: &str,
		to: &str,
	) -> ClResult<Vec<i64>> {
		sqlx::query_scalar(BY_NUMBER)
			.bind(seller_id)
			.bind(from)
			.bind(to)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}
}

// vim: ts=4
