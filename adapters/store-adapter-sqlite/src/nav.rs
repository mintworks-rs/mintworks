//! `NavStore` over SQLite.
//!
//! The two export selections read `invoices` only. They must never join `nav_submissions`: an
//! invoice that failed to report to NAV is still part of the seller's turnover and is still
//! statutorily exportable.

use async_trait::async_trait;
use saas_core::prelude::*;
use saas_nav::store::NavStore;
use saas_nav::submission::{NavArchive, NavOp, NavSubmission, NavVerdict};
use sqlx::{Row, sqlite::SqliteRow};

use crate::{
	SqliteStore,
	util::{DbExt, RowExt},
};

/// `saas-nav` carries no driver dependency, so `NavSubmission` is built by hand. Read **by
/// column name**, as in `auth.rs`: `SELECT *` follows the DDL's column order, which a migration
/// may change. `op` and `verdict` are TEXT and go through the enums' `FromStr`.
fn submission_row(row: &SqliteRow) -> ClResult<NavSubmission> {
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

/// Both selections take a set of in-range invoices and then close it over storno pairs in
/// both directions: the storno of an in-range invoice, and the original of an in-range
/// storno. A range showing one without the other would misstate turnover.
///
/// `number IS NOT NULL AND seller_id = ?1` is re-applied here, not only in `sel`: a row merely
/// *pointing* at an in-range invoice — another seller's, or an un-numbered DRAFT — entered the
/// statutory export. The parentheses around the OR chain are load-bearing; without them `AND`
/// binds tighter and the filter covers only the first disjunct.
macro_rules! close_over_storno_pairs {
	() => {
		"
	SELECT id FROM invoices
	 WHERE number IS NOT NULL AND seller_id = ?1
	   AND (id IN (SELECT id FROM sel)
	     OR original_invoice_id IN (SELECT id FROM sel)
	     OR id IN (SELECT original_invoice_id FROM invoices
	                WHERE id IN (SELECT id FROM sel) AND original_invoice_id IS NOT NULL))
	 ORDER BY id"
	};
}

/// The sweep's selection: issued invoices of one seller with **no `nav_submissions` row at
/// all** — the at-most-once gap at `saas_invoice::issue::enqueue_jobs`, where a crash between
/// COMMIT and enqueue leaves an issued invoice nothing ever pointed at NAV.
///
/// It deliberately does not pick up a filing that faulted: that is a `jobs` row retrying on its
/// own backoff, so re-driving it from here would file the invoice twice.
///
/// `number IS NOT NULL` is the issued test the export selections use. The `NOT EXISTS` rides
/// `idx_nav_submission_live` and the outer filter is exactly `idx_invoice_number`'s predicate,
/// so no new index is needed — `the_sweep_selection_is_served_by_indexes` holds the plan to it.
///
/// Binds, in order: `seller_id`, `limit`.
///
/// `pub` only so `tests/invoice.rs` can put it through `EXPLAIN QUERY PLAN`; not store API.
#[doc(hidden)]
pub const UNFILED: &str = "SELECT i.id FROM invoices i
	 WHERE i.seller_id = ? AND i.number IS NOT NULL
	   AND NOT EXISTS (SELECT 1 FROM nav_submissions s WHERE s.invoice_id = i.id)
	 ORDER BY i.id ASC LIMIT ?";

/// The batch leader's candidate selection: the same "issued, no `nav_submissions` row at all"
/// shape as [`UNFILED`], narrowed to `kind = 'NORMAL'` — a storno waits for the invoice it
/// cancels to be filed, so it can never ride in an arbitrary batch — and excluding the leader
/// itself, whose row the claim gets-or-creates separately.
///
/// The third bind is `require_document` as 0/1: `? = 0 OR EXISTS …` keeps one statement for both
/// sellers rather than two consts that can drift.
///
/// Binds, in order: `seller_id`, `exclude_invoice_id`, `require_document`, `limit`.
///
/// `pub` only so `tests/invoice.rs` can put it through `EXPLAIN QUERY PLAN`; not store API.
#[doc(hidden)]
pub const BATCH_CANDIDATES: &str = "SELECT i.id FROM invoices i
	 WHERE i.seller_id = ? AND i.number IS NOT NULL AND i.kind = 'NORMAL'
	   AND i.id <> ?
	   AND NOT EXISTS (SELECT 1 FROM nav_submissions s WHERE s.invoice_id = i.id)
	   AND (? = 0 OR EXISTS (SELECT 1 FROM invoice_documents d WHERE d.invoice_id = i.id))
	 ORDER BY i.id ASC LIMIT ?";

/// `issued_at` is a Unix timestamp, and the caller's `from`/`to` are Europe/Budapest calendar
/// dates — the dates the invoices themselves carry. `numbering::utc_span` turns the range into
/// the half-open instant range that covers those local days. `number IS NOT NULL` excludes
/// drafts.
///
/// Binds, in order: `seller_id`, `from`, `to`. Numbered, so the closure below can re-use `?1`
/// rather than asking every call site for a fourth bind.
///
/// `pub` only so `tests/invoice.rs` can put it through `EXPLAIN QUERY PLAN`; not store API.
#[doc(hidden)]
pub const BY_DATE: &str = concat!(
	"WITH sel AS (
	   SELECT id FROM invoices
	    WHERE seller_id = ?1 AND number IS NOT NULL
	      AND issued_at >= ?2 AND issued_at < ?3
	 )",
	close_over_storno_pairs!()
);

// Length-then-lexical, not plain lexical: `render_number` overflows past its configured width
// rather than truncating, so a plain BETWEEN sorts `A2026/1000000` *below* `A2026/999999` and
// this export silently omits rows. `(length, text)` is correct numeric ordering for same-prefix
// numbers of any width. Binds, in order: `seller_id`, `from`, `to`.
//
// `pub` only so `tests/invoice.rs` can put it through `EXPLAIN QUERY PLAN`; not store API.
#[doc(hidden)]
pub const BY_NUMBER: &str = concat!(
	"WITH sel AS (
	   SELECT id FROM invoices
	    WHERE seller_id = ?1 AND number IS NOT NULL
	      AND (length(number), number) BETWEEN (length(?2), ?2) AND (length(?3), ?3)
	 )",
	close_over_storno_pairs!()
);

#[async_trait]
impl NavStore for SqliteStore {
	async fn create_submission(
		&self,
		invoice_id: i64,
		op: NavOp,
		request_xml: &str,
	) -> ClResult<Option<i64>> {
		// The row and its archive in one transaction: the trait contract is that the request is
		// on record before the send, so a row without its archive must never be visible.
		let mut tx = self.write_tx().await?;
		let inserted = sqlx::query_scalar(
			"INSERT INTO nav_submissions (invoice_id, op, created_at) VALUES (?, ?, ?) \
			 RETURNING id",
		)
		.bind(invoice_id)
		.bind(op.as_str())
		.bind(Timestamp::now().0)
		.fetch_one(&mut *tx)
		.await;

		let id = match inserted {
			Ok(id) => id,
			// `idx_nav_submission_live`: a filing record for this (invoice_id, op) already
			// exists. Not an error — the invoice is filed or in flight, and not sending is
			// exactly the right outcome.
			Err(sqlx::Error::Database(db)) if db.is_unique_violation() => return Ok(None),
			Err(e) => return Err(crate::util::map_db(&e)),
		};

		sqlx::query("INSERT INTO nav_submission_xml (submission_id, request_xml) VALUES (?, ?)")
			.bind(id)
			.bind(request_xml)
			.execute(&mut *tx)
			.await
			.db()?;
		tx.commit().await.db()?;
		Ok(Some(id))
	}

	async fn archive_request(&self, id: i64, request_xml: &str) -> ClResult<()> {
		// The key is selected from `nav_submissions` so a member `release_batch` deleted gives
		// zero rows rather than `FOREIGN KEY constraint failed`; the `WHERE` is also what lets
		// SQLite parse `ON CONFLICT` after an `INSERT … SELECT`.
		//
		// First write wins: NAV processes only the first request under a given `requestId`, so
		// the first attempt is what it holds and what a dispute is settled from.
		sqlx::query(
			"INSERT INTO nav_submission_xml (submission_id, request_xml)
			   SELECT id, ? FROM nav_submissions WHERE id = ?
			   ON CONFLICT(submission_id) DO UPDATE SET request_xml = excluded.request_xml
			     WHERE nav_submission_xml.request_xml IS NULL",
		)
		.bind(request_xml)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	async fn request_archived(&self, id: i64) -> ClResult<bool> {
		Ok(sqlx::query_scalar::<_, bool>(
			"SELECT EXISTS(SELECT 1 FROM nav_submission_xml
				WHERE submission_id = ? AND request_xml IS NOT NULL)",
		)
		.bind(id)
		.fetch_one(self.reader())
		.await
		.db()?)
	}

	async fn batch_candidates(
		&self,
		seller_id: i64,
		exclude_invoice_id: i64,
		require_document: bool,
		limit: i64,
	) -> ClResult<Vec<i64>> {
		Ok(sqlx::query_scalar(BATCH_CANDIDATES)
			.bind(seller_id)
			.bind(exclude_invoice_id)
			.bind(i64::from(require_document))
			.bind(limit)
			.fetch_all(self.reader())
			.await
			.db()?)
	}

	async fn claim_batch(
		&self,
		leader_invoice_id: i64,
		op: NavOp,
		batch_uid: &str,
		ids: &[i64],
	) -> ClResult<Vec<(i64, i64)>> {
		// One `BEGIN IMMEDIATE` around the whole claim: with the members' own `NAV_REPORT` rows
		// unclaimed, `idx_nav_submission_live` is the only thing between two leaders and the same
		// invoice in two batches.
		let mut tx = self.write_tx().await?;
		let now = Timestamp::now().0;
		let mut claimed = Vec::with_capacity(ids.len() + 1);

		// Get-or-claim: a leader retrying a filing that predates batching already owns a
		// row with `batch_uid IS NULL`, and a create-only insert would collide and skip the
		// leader out of its own batch. The `batch_uid = excluded.batch_uid` disjunct makes a
		// re-run of the same claim return the same row instead of nothing. `done_at IS NULL`
		// with it: a row `finish` settled without a verdict must not be re-claimed and re-POSTed.
		let leader: Option<i64> = sqlx::query_scalar(
			"INSERT INTO nav_submissions (invoice_id, op, created_at, batch_uid)
			 VALUES (?, ?, ?, ?)
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
		.fetch_optional(&mut *tx)
		.await
		.db()?;
		// Nothing is claimed when the leader is not: a member stamped with a leader that never
		// POSTs is refused by `may_send` and skipped by `UNFILED`, so its filing never happens.
		let Some(leader_id) = leader else {
			return Ok(Vec::new());
		};
		claimed.push((leader_id, leader_invoice_id));

		for &invoice_id in ids {
			// Create, or resume a row this same batch already claimed — a retry has to
			// resend every member under the same `requestId`. No `batch_uid IS NULL` disjunct,
			// unlike the leader above: a row that is not already ours is somebody else's
			// business, and the `WHERE` returning nothing is how it drops out of this batch.
			let id: Option<i64> = sqlx::query_scalar(
				"INSERT INTO nav_submissions (invoice_id, op, created_at, batch_uid)
				 VALUES (?, ?, ?, ?)
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
			.fetch_optional(&mut *tx)
			.await
			.db()?;
			if let Some(id) = id {
				claimed.push((id, invoice_id));
			}
		}

		tx.commit().await.db()?;
		Ok(claimed)
	}

	async fn submissions_by_batch(&self, batch_uid: &str) -> ClResult<Vec<NavSubmission>> {
		sqlx::query("SELECT * FROM nav_submissions WHERE batch_uid = ? ORDER BY id ASC")
			.bind(batch_uid)
			.fetch_all(self.reader())
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
		sqlx::query("SELECT * FROM nav_submissions WHERE transaction_id = ? ORDER BY id ASC")
			.bind(transaction_id)
			.fetch_all(self.reader())
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
		// Two shapes, one transaction: a member `finish` left open with a reason keeps the row
		// `awaiting_operator` counts, losing only its ownership by a dead leader, while every
		// other member reverts to never-attempted so `UNFILED` offers it again.
		//
		// `done_at IS NOT NULL`, not `error_code IS NOT NULL`: only `finish` settles a member, so
		// an `error_code` alone would keep a pristine member's row and make it unfilable.
		let mut tx = self.write_tx().await?;
		let mut released: Vec<i64> = sqlx::query_scalar(
			"UPDATE nav_submissions SET batch_uid = NULL
			  WHERE batch_uid = ? AND id <> ? AND transaction_id IS NULL AND verdict IS NULL
			    AND done_at IS NOT NULL
			 RETURNING invoice_id",
		)
		.bind(batch_uid)
		.bind(leader_submission_id)
		.fetch_all(&mut *tx)
		.await
		.db()?;
		released.extend(
			sqlx::query_scalar::<_, i64>(
				"DELETE FROM nav_submissions
				  WHERE batch_uid = ? AND id <> ? AND transaction_id IS NULL AND verdict IS NULL
				    AND done_at IS NULL
				 RETURNING invoice_id",
			)
			.bind(batch_uid)
			.bind(leader_submission_id)
			.fetch_all(&mut *tx)
			.await
			.db()?,
		);
		tx.commit().await.db()?;
		Ok(released)
	}

	async fn release_member(&self, batch_uid: &str, invoice_id: i64) -> ClResult<bool> {
		// The same two guards `release_batch` carries, for the same reason: a row with a
		// `transaction_id` or a verdict is at NAV and not this batch's to give back.
		let done = sqlx::query(
			"UPDATE nav_submissions SET batch_uid = NULL
			  WHERE batch_uid = ? AND invoice_id = ?
			    AND transaction_id IS NULL AND verdict IS NULL",
		)
		.bind(batch_uid)
		.bind(invoice_id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn submission(&self, id: i64) -> ClResult<Option<NavSubmission>> {
		sqlx::query("SELECT * FROM nav_submissions WHERE id = ?")
			.bind(id)
			.fetch_optional(self.reader())
			.await
			.one(submission_row)
	}

	async fn submission_by_invoice(&self, invoice_id: i64) -> ClResult<Option<NavSubmission>> {
		sqlx::query("SELECT * FROM nav_submissions WHERE invoice_id = ? ORDER BY id DESC LIMIT 1")
			.bind(invoice_id)
			.fetch_optional(self.reader())
			.await
			.one(submission_row)
	}

	async fn submission_archive(&self, id: i64) -> ClResult<Option<NavArchive>> {
		sqlx::query(
			"SELECT request_xml, response_xml FROM nav_submission_xml WHERE submission_id = ?",
		)
		.bind(id)
		.fetch_optional(self.reader())
		.await
		.one(|row| {
			Ok(NavArchive {
				request_xml: row.try_get("request_xml").db()?,
				response_xml: row.try_get("response_xml").db()?,
			})
		})
	}

	async fn archive_response(&self, id: i64, response_xml: &str) -> ClResult<()> {
		// The archive keeps the latest response only, which is the decisive one. Add
		// a `nav_submission_events` table if per-poll history is ever needed for a dispute.
		//
		// Same `INSERT … SELECT … ON CONFLICT` shape as `archive_request`, for the reason
		// spelled out there.
		sqlx::query(
			"INSERT INTO nav_submission_xml (submission_id, response_xml)
			   SELECT id, ? FROM nav_submissions WHERE id = ?
			   ON CONFLICT(submission_id) DO UPDATE SET response_xml = excluded.response_xml",
		)
		.bind(response_xml)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	async fn set_sent(&self, id: i64, transaction_id: &str, idx: i64) -> ClResult<bool> {
		// `AND transaction_id IS NULL`: two runners past the claim both POST, and the loser
		// overwrote the `transactionId` of the filing NAV actually accepted.
		let done = sqlx::query(
			"UPDATE nav_submissions SET transaction_id = ?, idx = ? \
			  WHERE id = ? AND transaction_id IS NULL",
		)
		.bind(transaction_id)
		.bind(idx)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn set_sent_batch(
		&self,
		rows: &[(i64, i64)],
		transaction_id: &str,
	) -> ClResult<Vec<i64>> {
		// One `BEGIN IMMEDIATE` around the whole stamp, like `claim_batch`: NAV holds the batch
		// under one `transactionId`, and a member left without one is a filing nothing in the
		// system can ever see again.
		let mut tx = self.write_tx().await?;
		let mut applied = Vec::with_capacity(rows.len());
		for (id, idx) in rows {
			// `AND transaction_id IS NULL` per row, as in `set_sent`: two runners past the claim
			// both POST, and the loser overwrote the id of the filing NAV actually accepted.
			let got: Option<i64> = sqlx::query_scalar(
				"UPDATE nav_submissions SET transaction_id = ?, idx = ? \
				  WHERE id = ? AND transaction_id IS NULL RETURNING id",
			)
			.bind(transaction_id)
			.bind(idx)
			.bind(id)
			.fetch_optional(&mut *tx)
			.await
			.db()?;
			applied.extend(got);
		}
		tx.commit().await.db()?;
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
		// `AND verdict IS NULL`: a verdict is terminal, so the first one NAV gives stands. A
		// `None` verdict leaves the row open on purpose and is still guarded by it — a settled
		// row must not be reopened.
		let done = sqlx::query(
			"UPDATE nav_submissions
			    SET verdict = ?, error_code = ?, error_msg = ?, done_at = ?
			  WHERE id = ? AND verdict IS NULL",
		)
		.bind(verdict.map(NavVerdict::as_str))
		.bind(code)
		.bind(msg)
		.bind(done_at.0)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn record_fault(&self, id: i64, code: &str, message: &str) -> ClResult<()> {
		// `AND verdict IS NULL`, like `finish`: a settled row keeps the reason NAV settled it on.
		sqlx::query(
			"UPDATE nav_submissions SET error_code = ?, error_msg = ? \
			  WHERE id = ? AND verdict IS NULL",
		)
		.bind(code)
		.bind(message)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	async fn resolve(&self, id: i64, at: Timestamp) -> ClResult<bool> {
		// The `WHERE` is `awaiting_operator`'s predicate: a row that never needed a person
		// cannot be "resolved", and neither can one that already was.
		let done = sqlx::query(
			"UPDATE nav_submissions SET resolved_at = ? \
			  WHERE id = ? AND resolved_at IS NULL \
			    AND (verdict IN ('REJECTED','FAILED') \
			      OR (verdict IS NULL AND error_code IS NOT NULL))",
		)
		.bind(at.0)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(done.rows_affected() == 1)
	}

	async fn unfiled_invoices(&self, seller_id: i64, limit: i64) -> ClResult<Vec<i64>> {
		Ok(sqlx::query_scalar(UNFILED)
			.bind(seller_id)
			.bind(limit)
			.fetch_all(self.reader())
			.await
			.db()?)
	}

	async fn awaiting_operator(&self, seller_id: i64) -> ClResult<i64> {
		// `EXISTS`, not a join to the newest submission: one row per (invoice_id, op), and the
		// correlated form avoids re-aggregating on each hourly tick. The second disjunct is the
		// open row `job::report` leaves on `REQUEST_ID_NOT_UNIQUE` — no verdict but a recorded
		// reason — which nothing else would ever surface.
		Ok(sqlx::query_scalar(
			"SELECT COUNT(*) FROM invoices i
			  WHERE i.seller_id = ? AND i.number IS NOT NULL
			    AND EXISTS (SELECT 1 FROM nav_submissions s
			                 WHERE s.invoice_id = i.id AND s.resolved_at IS NULL
			                   AND (s.verdict IN ('REJECTED','FAILED')
			                     OR (s.verdict IS NULL AND s.error_code IS NOT NULL)))",
		)
		.bind(seller_id)
		.fetch_one(self.reader())
		.await
		.db()?)
	}

	async fn export_ids_by_date(&self, seller_id: i64, from: &str, to: &str) -> ClResult<Vec<i64>> {
		let (from, to) = saas_invoice::numbering::utc_span(from, to)?;
		Ok(sqlx::query_scalar(BY_DATE)
			.bind(seller_id)
			.bind(from.0)
			.bind(to.0)
			.fetch_all(self.reader())
			.await
			.db()?)
	}

	async fn export_ids_by_number(
		&self,
		seller_id: i64,
		from: &str,
		to: &str,
	) -> ClResult<Vec<i64>> {
		Ok(sqlx::query_scalar(BY_NUMBER)
			.bind(seller_id)
			.bind(from)
			.bind(to)
			.fetch_all(self.reader())
			.await
			.db()?)
	}
}

// vim: ts=4
