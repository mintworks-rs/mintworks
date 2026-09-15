//! `NavStore` over SQLite.
//!
//! The two export selections read `invoices` only. They must never join `nav_submissions`:
//! an invoice that failed to report to NAV is still part of the seller's turnover and is
//! still statutorily exportable (`claude-docs/nav-mapping.md` §9.3).

use async_trait::async_trait;
use saas_core::prelude::*;
use saas_nav::store::NavStore;
use saas_nav::submission::{NavOp, NavSubmission, NavVerdict};
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
		request_xml: row.try_get("request_xml").db()?,
		response_xml: row.try_get("response_xml").db()?,
		error_code: row.try_get("error_code").db()?,
		error_msg: row.try_get("error_msg").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		done_at: row.try_get::<Option<i64>, _>("done_at").db()?.map(Timestamp),
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
		let inserted = sqlx::query_scalar(
			"INSERT INTO nav_submissions (invoice_id, op, request_xml, created_at)
			 VALUES (?, ?, ?, ?) RETURNING id",
		)
		.bind(invoice_id)
		.bind(op.as_str())
		.bind(request_xml)
		.bind(Timestamp::now().0)
		.fetch_one(self.writer())
		.await;

		match inserted {
			Ok(id) => Ok(Some(id)),
			// `idx_nav_submission_live`: a filing record for this (invoice_id, op) already
			// exists. Not an error — the invoice is filed or in flight, and not sending is
			// exactly the right outcome.
			Err(sqlx::Error::Database(db)) if db.is_unique_violation() => Ok(None),
			Err(e) => Err(crate::util::map_db(&e)),
		}
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

	async fn archive_response(&self, id: i64, response_xml: &str) -> ClResult<()> {
		// The row keeps the latest response only, which is the decisive one. Add
		// a `nav_submission_events` table if per-poll history is ever needed for a dispute.
		sqlx::query("UPDATE nav_submissions SET response_xml = ? WHERE id = ?")
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
			                 WHERE s.invoice_id = i.id
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
