// SPDX-License-Identifier: MPL-2.0
//! `NavStore` over SQLite.
//!
//! The two export selections read `invoices` only. They must never join `nav_submissions`: an
//! invoice that failed to report to NAV is still part of the seller's turnover and is still
//! statutorily exportable.
//!
//! A transition whose guard reads its own row alone is one autocommit `UPDATE`; everything else
//! is one `write_tx`, rolled back by `refuse` when its guard fails, so a refusal leaves nothing.

use async_trait::async_trait;
use mintworks_core::{ids::NavBatchId, prelude::*};
use mintworks_nav::store::NavStore;
use mintworks_nav::submission::{
	AdoptResult, BatchState, FilingState, NavArchive, NavBatch, NavFiling, NavOp, NavSellerHold,
	NavSubmission, NavVerdict, RowState,
};
use sqlx::{Row, sqlite::SqliteRow};

use crate::{
	SqliteStore, WriteTx,
	util::{DbExt, RowExt, RowsExt, placeholders},
};

/// `mintworks-nav` carries no driver dependency, so `NavSubmission` is built by hand. Read **by
/// column name**, as in `auth.rs`: `SELECT *` follows the DDL's column order, which a migration
/// may change. `op`, `state` and `verdict` are TEXT and go through the enums' `FromStr`.
fn submission_row(row: &SqliteRow) -> ClResult<NavSubmission> {
	Ok(NavSubmission {
		id: row.try_get("id").db()?,
		invoice_id: row.try_get("invoice_id").db()?,
		op: row.try_get::<String, _>("op").db()?.parse()?,
		batch_id: row.try_get("batch_id").db()?,
		idx: row.try_get("idx").db()?,
		state: row.try_get::<String, _>("state").db()?.parse()?,
		verdict: row
			.try_get::<Option<String>, _>("verdict")
			.db()?
			.map(|s| s.parse())
			.transpose()?,
		error_code: row.try_get("error_code").db()?,
		error_msg: row.try_get("error_msg").db()?,
		electronic: row.try_get::<Option<i64>, _>("electronic").db()?.map(|e| e != 0),
		adopt_checked_at: row.try_get::<Option<i64>, _>("adopt_checked_at").db()?.map(Timestamp),
		adopt_result: row
			.try_get::<Option<String>, _>("adopt_result")
			.db()?
			.map(|s| s.parse())
			.transpose()?,
		adopt_detail: row.try_get("adopt_detail").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		done_at: row.try_get::<Option<i64>, _>("done_at").db()?.map(Timestamp),
		resolved_at: row.try_get::<Option<i64>, _>("resolved_at").db()?.map(Timestamp),
	})
}

/// `p` is the column prefix: `filing_select!`'s join aliases the batch's columns, six of which
/// `nav_submissions` also has.
fn batch_cols(row: &SqliteRow, p: &str) -> ClResult<NavBatch> {
	let c = |name: &str| format!("{p}{name}");
	Ok(NavBatch {
		id: row.try_get(c("id").as_str()).db()?,
		uid: NavBatchId::from_trusted(row.try_get::<String, _>(c("uid").as_str()).db()?),
		seller_id: row.try_get(c("seller_id").as_str()).db()?,
		op: row.try_get::<String, _>(c("op").as_str()).db()?.parse()?,
		state: row.try_get::<String, _>(c("state").as_str()).db()?.parse()?,
		transaction_id: row.try_get(c("transaction_id").as_str()).db()?,
		posted_at: row.try_get::<Option<i64>, _>(c("posted_at").as_str()).db()?.map(Timestamp),
		error_code: row.try_get(c("error_code").as_str()).db()?,
		error_msg: row.try_get(c("error_msg").as_str()).db()?,
		fault_at: row.try_get::<Option<i64>, _>(c("fault_at").as_str()).db()?.map(Timestamp),
		created_at: Timestamp(row.try_get(c("created_at").as_str()).db()?),
		closed_at: row.try_get::<Option<i64>, _>(c("closed_at").as_str()).db()?.map(Timestamp),
	})
}

fn batch_row(row: &SqliteRow) -> ClResult<NavBatch> {
	batch_cols(row, "")
}

fn filing_row(row: &SqliteRow) -> ClResult<NavFiling> {
	let filing = submission_row(row)?;
	let batch = filing.batch_id.map(|_| batch_cols(row, "b_")).transpose()?;
	Ok(NavFiling { row: filing, batch })
}

/// A row with its batch, the `LEFT JOIN` because a never-batched `CANCELLED` row has none.
macro_rules! filing_select {
	() => {
		"SELECT s.*, b.id AS b_id, b.uid AS b_uid, b.seller_id AS b_seller_id, b.op AS b_op,
		        b.state AS b_state, b.transaction_id AS b_transaction_id,
		        b.posted_at AS b_posted_at, b.error_code AS b_error_code,
		        b.error_msg AS b_error_msg, b.fault_at AS b_fault_at,
		        b.created_at AS b_created_at, b.closed_at AS b_closed_at
		   FROM nav_submissions s LEFT JOIN nav_batches b ON b.id = s.batch_id"
	};
}

/// Rolls back and answers `answer`: the end of every failed guard inside a `write_tx`.
async fn refuse<T>(tx: WriteTx, answer: T) -> ClResult<T> {
	tx.rollback().await?;
	Ok(answer)
}

/// Opens a batch: `SENT` with `posted_at = at` when it carries `transaction_id`, else `CLAIMED`.
async fn insert_batch(
	tx: &WriteTx,
	seller_id: i64,
	op: NavOp,
	transaction_id: Option<&str>,
	at: Timestamp,
) -> ClResult<NavBatch> {
	let state = if transaction_id.is_some() { BatchState::Sent } else { BatchState::Claimed };
	let row = sqlx::query(
		"INSERT INTO nav_batches (uid, seller_id, op, state, transaction_id, posted_at, created_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?)
		 RETURNING *",
	)
	.bind(NavBatchId::generate().as_str())
	.bind(seller_id)
	.bind(op.as_str())
	.bind(state.as_str())
	.bind(transaction_id)
	.bind(transaction_id.map(|_| at.0))
	.bind(at.0)
	.fetch_one(&mut *tx.lock().await?)
	.await
	.db()?;
	batch_row(&row)
}

/// `false` when `idx_nav_submission_live` already holds a row for `(invoice_id, op)`. The caller
/// resolves any row this one replaces first, or that row is the conflict.
async fn insert_active(
	tx: &WriteTx,
	invoice_id: i64,
	op: NavOp,
	batch_id: i64,
	idx: Option<i64>,
	electronic: Option<i64>,
	at: Timestamp,
) -> ClResult<bool> {
	let done = sqlx::query(
		"INSERT INTO nav_submissions
			(invoice_id, op, batch_id, idx, state, electronic, created_at)
		 VALUES (?, ?, ?, ?, 'ACTIVE', ?, ?)
		 ON CONFLICT(invoice_id, op) WHERE resolved_at IS NULL DO NOTHING",
	)
	.bind(invoice_id)
	.bind(op.as_str())
	.bind(batch_id)
	.bind(idx)
	.bind(electronic)
	.bind(at.0)
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;
	Ok(done.rows_affected() > 0)
}

/// Closes `batch_id` if it is still `from` and no `ACTIVE` row remains. Scoped to `from`
/// because a `HELD` row can leave a `SENT` batch whose `ACTIVE` siblings still await the poll.
async fn close_if_empty(
	tx: &WriteTx,
	batch_id: i64,
	from: BatchState,
	at: Timestamp,
) -> ClResult<()> {
	sqlx::query(
		"UPDATE nav_batches SET state = 'CLOSED', closed_at = ?
		  WHERE id = ? AND state = ?
		    AND NOT EXISTS (SELECT 1 FROM nav_submissions s
		                     WHERE s.batch_id = nav_batches.id AND s.state = 'ACTIVE')",
	)
	.bind(at.0)
	.bind(batch_id)
	.bind(from.as_str())
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;
	Ok(())
}

/// The row `refile` and `attach_transaction` are about to take, with its batch's facts for
/// their guards.
struct Moved {
	id: i64,
	invoice_id: i64,
	op: NavOp,
	batch_id: i64,
	state: RowState,
	has_verdict: bool,
	electronic: Option<i64>,
	seller_id: i64,
	batch_state: BatchState,
}

impl Moved {
	/// The guard R4, R5 and R9 share: row `HELD`, or `ACTIVE` in a batch `HELD`.
	fn held(&self) -> bool {
		self.state == RowState::Held
			|| (self.state == RowState::Active && self.batch_state == BatchState::Held)
	}
}

/// Ascending `id`. The inner join drops a never-batched `CANCELLED` row, which is ineligible
/// anyway, so the caller's length check refuses it along with an unknown or repeated id.
async fn moved_rows(tx: &WriteTx, ids: &[i64]) -> ClResult<Vec<Moved>> {
	let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
		"SELECT s.id, s.invoice_id, s.op, s.batch_id, s.state, s.verdict, s.electronic,
		        b.seller_id, b.state AS batch_state
		   FROM nav_submissions s JOIN nav_batches b ON b.id = s.batch_id
		  WHERE s.id IN ({}) ORDER BY s.id ASC",
		placeholders(ids.len())
	)));
	for id in ids {
		q = q.bind(*id);
	}
	q.fetch_all(&mut *tx.lock().await?).await.all(|row| {
		Ok(Moved {
			id: row.try_get("id").db()?,
			invoice_id: row.try_get("invoice_id").db()?,
			op: row.try_get::<String, _>("op").db()?.parse()?,
			batch_id: row.try_get("batch_id").db()?,
			state: row.try_get::<String, _>("state").db()?.parse()?,
			has_verdict: row.try_get::<Option<String>, _>("verdict").db()?.is_some(),
			electronic: row.try_get("electronic").db()?,
			seller_id: row.try_get("seller_id").db()?,
			batch_state: row.try_get::<String, _>("batch_state").db()?.parse()?,
		})
	})
}

/// Closes each `HELD` batch `old` came from once its last `ACTIVE` row has left; `old` can span
/// several.
async fn close_held(tx: &WriteTx, old: &[Moved], at: Timestamp) -> ClResult<()> {
	let mut ids: Vec<i64> = old.iter().map(|m| m.batch_id).collect();
	ids.sort_unstable();
	ids.dedup();
	for id in ids {
		close_if_empty(tx, id, BatchState::Held, at).await?;
	}
	Ok(())
}

/// R1, R2, R3: an `ACTIVE` row of a `SENT` batch takes `state` and `verdict`.
async fn settle(
	store: &SqliteStore,
	row_id: i64,
	state: RowState,
	verdict: NavVerdict,
	error: Option<(&str, &str)>,
	at: Timestamp,
) -> ClResult<bool> {
	let (code, msg) = error.unzip();
	let tx = store.write_tx().await?;
	let batch: Option<i64> = sqlx::query_scalar(
		"UPDATE nav_submissions
		    SET state = ?, verdict = ?, error_code = ?, error_msg = ?, done_at = ?
		  WHERE id = ? AND state = 'ACTIVE'
		    AND batch_id IN (SELECT id FROM nav_batches WHERE state = 'SENT')
		 RETURNING batch_id",
	)
	.bind(state.as_str())
	.bind(verdict.as_str())
	.bind(code)
	.bind(msg)
	.bind(at.0)
	.bind(row_id)
	.fetch_optional(&mut *tx.lock().await?)
	.await
	.db()?;
	let Some(batch) = batch else {
		return refuse(tx, false).await;
	};
	close_if_empty(&tx, batch, BatchState::Sent, at).await?;
	tx.commit().await?;
	Ok(true)
}

/// First hold stands: `spend_batch`'s contract.
const HOLD_SELLER: &str = "INSERT INTO nav_seller_holds (seller_id, code, message, held_at)
	 VALUES (?, ?, ?, ?)
	 ON CONFLICT(seller_id) DO NOTHING";

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
	 WHERE number IS NOT NULL AND seller_id = ?1 AND test_mode = 0
	   AND (id IN (SELECT id FROM sel)
	     OR original_invoice_id IN (SELECT id FROM sel)
	     OR id IN (SELECT original_invoice_id FROM invoices
	                WHERE id IN (SELECT id FROM sel) AND original_invoice_id IS NOT NULL))
	 ORDER BY id"
	};
}

/// One seller's issued invoices with **no `nav_submissions` row at all**: the backlog waiting on
/// `NAV_REPORT`. A filing that faulted has a row, so it never appears here: its batch's owner job
/// retries it, and re-driving it from here would file the invoice twice.
///
/// `number IS NOT NULL` is the issued test the export selections use. The `NOT EXISTS` rides
/// `idx_nav_submission_invoice` and the outer filter is `idx_invoice_number`'s predicate plus a
/// residual `test_mode = 0` (a test-mode invoice never reaches NAV), so no new index is needed —
/// `the_sweep_selection_is_served_by_indexes` holds the plan to it.
///
/// Binds, in order: `seller_id`, `limit`.
///
/// `pub` only so `tests/invoice.rs` can put it through `EXPLAIN QUERY PLAN`; not store API.
#[doc(hidden)]
pub const UNFILED: &str = "SELECT i.id FROM invoices i
	 WHERE i.seller_id = ? AND i.number IS NOT NULL AND i.test_mode = 0
	   AND NOT EXISTS (SELECT 1 FROM nav_submissions s WHERE s.invoice_id = i.id)
	 ORDER BY i.id ASC LIMIT ?";

/// `UNFILED` over every seller, less the invoices a live `NAV_REPORT` already owns: what
/// `NAV_SWEEP` re-enqueues. The key is `mintworks_invoice`'s `invoice_job_key`; `jobs.dedup_key`
/// is UNIQUE, so the probe is one index lookup.
///
/// Binds: `limit`.
///
/// `pub` only so `tests/invoice.rs` can put it through `EXPLAIN QUERY PLAN`; not store API.
#[doc(hidden)]
pub const ORPHANED: &str = "SELECT i.id FROM invoices i
	 WHERE i.number IS NOT NULL AND i.test_mode = 0
	   AND NOT EXISTS (SELECT 1 FROM nav_submissions s WHERE s.invoice_id = i.id)
	   AND NOT EXISTS (SELECT 1 FROM jobs j
	                    WHERE j.dedup_key = 'nav:invoice:' || i.id
	                      AND j.status IN ('PENDING','RUNNING'))
	 ORDER BY i.id ASC LIMIT ?";

/// What `NAV_REPORT` may assemble alongside its own invoice: the same "issued, no
/// `nav_submissions` row at all" shape as [`UNFILED`], narrowed to `kind = 'NORMAL'` — a storno
/// waits for the invoice it cancels to be filed, so it can never ride in an arbitrary batch — and
/// excluding the invoice being assembled for, which the job adds to the claim itself.
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
	   AND i.test_mode = 0
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
	    WHERE seller_id = ?1 AND number IS NOT NULL AND test_mode = 0
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
	    WHERE seller_id = ?1 AND number IS NOT NULL AND test_mode = 0
	      AND (length(number), number) BETWEEN (length(?2), ?2) AND (length(?3), ?3)
	 )",
	close_over_storno_pairs!()
);

#[async_trait]
impl NavStore for SqliteStore {
	async fn claim_batch(
		&self,
		seller_id: i64,
		op: NavOp,
		invoice_ids: &[i64],
		electronic: bool,
	) -> ClResult<Option<(NavBatch, Vec<i64>)>> {
		// One `BEGIN IMMEDIATE`: `idx_nav_submission_live` is the only thing between two claims
		// and the same invoice in two batches, and an emptied claim must leave no batch.
		let tx = self.write_tx().await?;
		let now = Timestamp::now();
		let batch = insert_batch(&tx, seller_id, op, None, now).await?;
		let electronic = Some(i64::from(electronic));
		let mut got = Vec::with_capacity(invoice_ids.len());
		for &invoice_id in invoice_ids {
			if insert_active(&tx, invoice_id, op, batch.id, None, electronic, now).await? {
				got.push(invoice_id);
			}
		}
		if got.is_empty() {
			return refuse(tx, None).await;
		}
		tx.commit().await?;
		Ok(Some((batch, got)))
	}

	async fn begin_post(
		&self,
		batch_id: i64,
		request_xml: &str,
		rows: &[(i64, i64)],
		at: Timestamp,
	) -> ClResult<bool> {
		let tx = self.write_tx().await?;
		let active: Vec<i64> = sqlx::query_scalar(
			"SELECT id FROM nav_submissions WHERE batch_id = ? AND state = 'ACTIVE' ORDER BY id",
		)
		.bind(batch_id)
		.fetch_all(&mut *tx.lock().await?)
		.await
		.db()?;
		let mut listed: Vec<i64> = rows.iter().map(|r| r.0).collect();
		listed.sort_unstable();
		if listed != active {
			return refuse(tx, false).await;
		}
		let done = sqlx::query(
			"UPDATE nav_batches SET state = 'POSTING', posted_at = ?
			  WHERE id = ? AND state = 'CLAIMED'",
		)
		.bind(at.0)
		.bind(batch_id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		if done.rows_affected() == 0 {
			return refuse(tx, false).await;
		}
		for (id, idx) in rows {
			sqlx::query("UPDATE nav_submissions SET idx = ? WHERE id = ?")
				.bind(idx)
				.bind(id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}
		sqlx::query(
			"INSERT INTO nav_batch_xml (batch_id, request_xml) VALUES (?, ?)
			 ON CONFLICT(batch_id) DO UPDATE
			   SET request_xml = excluded.request_xml, response_xml = NULL",
		)
		.bind(batch_id)
		.bind(request_xml)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		tx.commit().await?;
		Ok(true)
	}

	async fn abort_post(
		&self,
		batch_id: i64,
		fault: Option<(&str, &str)>,
		at: Timestamp,
	) -> ClResult<bool> {
		let (code, msg) = fault.unzip();
		let done = sqlx::query(
			"UPDATE nav_batches
			    SET state = 'CLAIMED', error_code = COALESCE(?1, error_code),
			        error_msg = COALESCE(?2, error_msg),
			        fault_at = CASE WHEN ?1 IS NULL THEN fault_at
			                        ELSE COALESCE(fault_at, ?3) END
			  WHERE id = ?4 AND state = 'POSTING'",
		)
		.bind(code)
		.bind(msg)
		.bind(at.0)
		.bind(batch_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn mark_in_doubt(
		&self,
		batch_id: i64,
		fault: Option<(&str, &str)>,
		at: Timestamp,
	) -> ClResult<bool> {
		let (code, msg) = fault.unzip();
		let done = sqlx::query(
			"UPDATE nav_batches
			    SET state = 'IN_DOUBT', error_code = ?, error_msg = ?, fault_at = ?
			  WHERE id = ? AND state = 'POSTING'",
		)
		.bind(code)
		.bind(msg)
		.bind(fault.map(|_| at.0))
		.bind(batch_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn set_sent(&self, batch_id: i64, transaction_id: &str) -> ClResult<bool> {
		// A `write_tx`, unlike its neighbours: the guard reads another batch's row, and
		// `idx_nav_batch_transaction` must answer `false` here, never a unique violation.
		let tx = self.write_tx().await?;
		let done = sqlx::query(
			"UPDATE nav_batches
			    SET state = 'SENT', transaction_id = ?1,
			        error_code = NULL, error_msg = NULL, fault_at = NULL
			  WHERE id = ?2 AND state IN ('POSTING','IN_DOUBT')
			    AND NOT EXISTS (SELECT 1 FROM nav_batches o WHERE o.transaction_id = ?1)",
		)
		.bind(transaction_id)
		.bind(batch_id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		tx.commit().await?;
		Ok(done.rows_affected() > 0)
	}

	async fn clear_doubt(&self, batch_id: i64) -> ClResult<bool> {
		let done = sqlx::query(
			"UPDATE nav_batches SET state = 'CLAIMED'
			  WHERE id = ? AND state = 'IN_DOUBT' AND error_code IS NULL",
		)
		.bind(batch_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn hold_batch(
		&self,
		batch_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<bool> {
		let done = sqlx::query(
			"UPDATE nav_batches
			    SET state = 'HELD', error_code = ?, error_msg = ?,
			        fault_at = COALESCE(fault_at, ?)
			  WHERE id = ? AND state = 'IN_DOUBT'",
		)
		.bind(code)
		.bind(message)
		.bind(at.0)
		.bind(batch_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn spend_batch(
		&self,
		batch_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<Option<NavBatch>> {
		let tx = self.write_tx().await?;
		let spent = sqlx::query(
			"UPDATE nav_batches
			    SET state = 'CLOSED', error_code = ?, error_msg = ?,
			        fault_at = COALESCE(fault_at, ?), closed_at = ?
			  WHERE id = ? AND state = 'POSTING'
			 RETURNING *",
		)
		.bind(code)
		.bind(message)
		.bind(at.0)
		.bind(at.0)
		.bind(batch_id)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.one(batch_row)?;
		let Some(spent) = spent else {
			return refuse(tx, None).await;
		};
		let rows: Vec<(i64, Option<i64>)> = sqlx::query(
			"UPDATE nav_submissions
			    SET state = 'FAILED', verdict = 'FAILED', error_code = ?, error_msg = ?,
			        done_at = ?, resolved_at = ?
			  WHERE batch_id = ? AND state = 'ACTIVE'
			 RETURNING invoice_id, electronic",
		)
		.bind(code)
		.bind(message)
		.bind(at.0)
		.bind(at.0)
		.bind(batch_id)
		.fetch_all(&mut *tx.lock().await?)
		.await
		.all(|row| Ok((row.try_get("invoice_id").db()?, row.try_get("electronic").db()?)))?;
		let batch = insert_batch(&tx, spent.seller_id, spent.op, None, at).await?;
		for (invoice_id, electronic) in rows {
			insert_active(&tx, invoice_id, spent.op, batch.id, None, electronic, at).await?;
		}
		sqlx::query(HOLD_SELLER)
			.bind(spent.seller_id)
			.bind(code)
			.bind(message)
			.bind(at.0)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		tx.commit().await?;
		Ok(Some(batch))
	}

	async fn finish(
		&self,
		row_id: i64,
		verdict: NavVerdict,
		error: Option<(&str, &str)>,
		at: Timestamp,
	) -> ClResult<bool> {
		let state = match verdict {
			NavVerdict::Done | NavVerdict::Warn => RowState::Accepted,
			NavVerdict::Rejected => RowState::Rejected,
			NavVerdict::Failed => RowState::Failed,
		};
		settle(self, row_id, state, verdict, error, at).await
	}

	async fn finish_held(
		&self,
		row_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<bool> {
		settle(self, row_id, RowState::Held, NavVerdict::Rejected, Some((code, message)), at).await
	}

	async fn release_row(&self, row_id: i64) -> ClResult<bool> {
		let tx = self.write_tx().await?;
		let batch: Option<i64> = sqlx::query_scalar(
			"DELETE FROM nav_submissions
			  WHERE id = ? AND state = 'ACTIVE'
			    AND batch_id IN (SELECT id FROM nav_batches WHERE state = 'CLAIMED')
			 RETURNING batch_id",
		)
		.bind(row_id)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.db()?;
		let Some(batch) = batch else {
			return refuse(tx, false).await;
		};
		close_if_empty(&tx, batch, BatchState::Claimed, Timestamp::now()).await?;
		tx.commit().await?;
		Ok(true)
	}

	async fn cancel_filing(&self, invoice_id: i64, op: NavOp, at: Timestamp) -> ClResult<bool> {
		let tx = self.write_tx().await?;
		let batch: Option<i64> = sqlx::query_scalar(
			"UPDATE nav_submissions SET state = 'CANCELLED', done_at = ?
			  WHERE invoice_id = ? AND op = ? AND state = 'ACTIVE'
			    AND batch_id IN (SELECT id FROM nav_batches WHERE state = 'CLAIMED')
			 RETURNING batch_id",
		)
		.bind(at.0)
		.bind(invoice_id)
		.bind(op.as_str())
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.db()?;
		if let Some(batch) = batch {
			close_if_empty(&tx, batch, BatchState::Claimed, at).await?;
			tx.commit().await?;
			return Ok(true);
		}
		let done = sqlx::query(
			"INSERT INTO nav_submissions (invoice_id, op, state, created_at, done_at)
			 SELECT ?1, ?2, 'CANCELLED', ?3, ?3
			  WHERE NOT EXISTS (SELECT 1 FROM nav_submissions
			                     WHERE invoice_id = ?1 AND op = ?2)",
		)
		.bind(invoice_id)
		.bind(op.as_str())
		.bind(at.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		tx.commit().await?;
		Ok(done.rows_affected() > 0)
	}

	async fn reopen_cancelled(&self, row_id: i64) -> ClResult<bool> {
		let done = sqlx::query("DELETE FROM nav_submissions WHERE id = ? AND state = 'CANCELLED'")
			.bind(row_id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn refile(
		&self,
		row_ids: &[i64],
		code: Option<&str>,
		at: Timestamp,
	) -> ClResult<Option<NavBatch>> {
		if row_ids.is_empty() {
			return Ok(None);
		}
		let tx = self.write_tx().await?;
		let old = moved_rows(&tx, row_ids).await?;
		let Some(first) = old.first() else {
			return refuse(tx, None).await;
		};
		let (seller_id, op) = (first.seller_id, first.op);
		let eligible = old.len() == row_ids.len()
			&& old.iter().all(|m| {
				let state = if code.is_some() {
					m.held()
				} else {
					matches!(m.state, RowState::Rejected | RowState::Failed)
				};
				state && m.seller_id == seller_id && m.op == op
			});
		if !eligible {
			return refuse(tx, None).await;
		}
		let batch = insert_batch(&tx, seller_id, op, None, at).await?;
		for m in &old {
			let resolved = match code {
				Some(code) => sqlx::query(
					"UPDATE nav_submissions
					    SET state = CASE WHEN verdict IS NULL THEN 'FAILED' ELSE 'REJECTED' END,
					        error_code = CASE WHEN verdict IS NULL THEN ?1 ELSE error_code END,
					        done_at = CASE WHEN verdict IS NULL THEN ?2 ELSE done_at END,
					        verdict = COALESCE(verdict, 'FAILED'), resolved_at = ?2
					  WHERE id = ?3",
				)
				.bind(code)
				.bind(at.0)
				.bind(m.id),
				None => sqlx::query(
					"UPDATE nav_submissions SET resolved_at = COALESCE(resolved_at, ?)
					  WHERE id = ?",
				)
				.bind(at.0)
				.bind(m.id),
			};
			resolved.execute(&mut *tx.lock().await?).await.db()?;
			if !insert_active(&tx, m.invoice_id, op, batch.id, None, m.electronic, at).await? {
				return refuse(tx, None).await;
			}
		}
		close_held(&tx, &old, at).await?;
		tx.commit().await?;
		Ok(Some(batch))
	}

	async fn attach_transaction(
		&self,
		transaction_id: &str,
		seller_id: i64,
		op: NavOp,
		rows: &[(i64, i64)],
		at: Timestamp,
		result: AdoptResult,
	) -> ClResult<Option<NavBatch>> {
		if rows.is_empty() {
			return Ok(None);
		}
		let mut rows = rows.to_vec();
		rows.sort_unstable();
		let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
		let tx = self.write_tx().await?;
		let taken: bool =
			sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nav_batches WHERE transaction_id = ?)")
				.bind(transaction_id)
				.fetch_one(&mut *tx.lock().await?)
				.await
				.db()?;
		let old = moved_rows(&tx, &ids).await?;
		// The seller and `op` count as eligibility: the new batch is filed under both.
		let eligible = old.len() == rows.len()
			&& old.iter().all(|m| m.held() && m.seller_id == seller_id && m.op == op);
		if taken || !eligible {
			return refuse(tx, None).await;
		}
		let batch = insert_batch(&tx, seller_id, op, Some(transaction_id), at).await?;
		for (m, (_, idx)) in old.iter().zip(&rows) {
			// The old row stays on its batch, or `Nav::filing_archives` could not reach what we sent.
			let state = if m.has_verdict { "REJECTED" } else { "CANCELLED" };
			sqlx::query("UPDATE nav_submissions SET state = ?, resolved_at = ? WHERE id = ?")
				.bind(state)
				.bind(at.0)
				.bind(m.id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
			insert_active(&tx, m.invoice_id, op, batch.id, Some(*idx), m.electronic, at).await?;
		}
		sqlx::query(
			"UPDATE nav_submissions SET adopt_result = ?, adopt_checked_at = ? WHERE batch_id = ?",
		)
		.bind(result.as_str())
		.bind(at.0)
		.bind(batch.id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		close_held(&tx, &old, at).await?;
		tx.commit().await?;
		Ok(Some(batch))
	}

	async fn stamp_adopt_checked(
		&self,
		row_ids: &[i64],
		result: AdoptResult,
		detail: Option<&str>,
		at: Timestamp,
	) -> ClResult<u64> {
		if row_ids.is_empty() {
			return Ok(0);
		}
		let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
			"UPDATE nav_submissions
			    SET adopt_checked_at = ?, adopt_result = ?, adopt_detail = ?
			  WHERE state = 'HELD' AND id IN ({})",
			placeholders(row_ids.len())
		)))
		.bind(at.0)
		.bind(result.as_str())
		.bind(detail);
		for id in row_ids {
			q = q.bind(*id);
		}
		Ok(q.execute(&mut *self.conn().await?).await.db()?.rows_affected())
	}

	async fn clear_adopt_checked(&self, row_id: i64) -> ClResult<bool> {
		let done = sqlx::query(
			"UPDATE nav_submissions
			    SET adopt_checked_at = NULL, adopt_result = NULL, adopt_detail = NULL,
			        resolved_at = NULL
			  WHERE id = ? AND state = 'HELD'
			    AND NOT EXISTS (SELECT 1 FROM nav_submissions o
			                     WHERE o.invoice_id = nav_submissions.invoice_id
			                       AND o.op = nav_submissions.op AND o.resolved_at IS NULL
			                       AND o.id <> nav_submissions.id)",
		)
		.bind(row_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn resolve(&self, row_id: i64, at: Timestamp) -> ClResult<bool> {
		let done = sqlx::query(
			"UPDATE nav_submissions SET resolved_at = ?
			  WHERE id = ? AND state IN ('REJECTED','FAILED','HELD') AND resolved_at IS NULL",
		)
		.bind(at.0)
		.bind(row_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn record_batch_fault(
		&self,
		batch_id: i64,
		code: &str,
		message: &str,
		at: Timestamp,
	) -> ClResult<bool> {
		let done = sqlx::query(
			"UPDATE nav_batches
			    SET error_code = ?, error_msg = ?, fault_at = COALESCE(fault_at, ?)
			  WHERE id = ? AND state <> 'CLOSED'",
		)
		.bind(code)
		.bind(message)
		.bind(at.0)
		.bind(batch_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn archive_response(&self, batch_id: i64, response_xml: &str) -> ClResult<()> {
		// From `nav_batches` so an unknown id gives zero rows, not an FK error; the `WHERE` also
		// lets SQLite parse `ON CONFLICT` after `INSERT … SELECT`. A dispute wanting per-poll
		// history needs a `nav_batch_events` table: the archive keeps the latest, decisive reply.
		sqlx::query(
			"INSERT INTO nav_batch_xml (batch_id, response_xml)
			   SELECT id, ? FROM nav_batches WHERE id = ?
			   ON CONFLICT(batch_id) DO UPDATE SET response_xml = excluded.response_xml",
		)
		.bind(response_xml)
		.bind(batch_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn release_seller(&self, seller_id: i64) -> ClResult<bool> {
		let done = sqlx::query("DELETE FROM nav_seller_holds WHERE seller_id = ?")
			.bind(seller_id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn seller_hold(&self, seller_id: i64) -> ClResult<Option<NavSellerHold>> {
		sqlx::query("SELECT * FROM nav_seller_holds WHERE seller_id = ?")
			.bind(seller_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(|row| {
				Ok(NavSellerHold {
					seller_id: row.try_get("seller_id").db()?,
					code: row.try_get("code").db()?,
					message: row.try_get("message").db()?,
					held_at: Timestamp(row.try_get("held_at").db()?),
				})
			})
	}

	async fn seller_holds_count(&self) -> ClResult<i64> {
		Ok(sqlx::query_scalar("SELECT COUNT(*) FROM nav_seller_holds")
			.fetch_one(&mut *self.reader().await?)
			.await
			.db()?)
	}

	async fn batch_by_uid(&self, uid: &str) -> ClResult<Option<NavBatch>> {
		sqlx::query("SELECT * FROM nav_batches WHERE uid = ?")
			.bind(uid)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(batch_row)
	}

	async fn batch_by_transaction(&self, transaction_id: &str) -> ClResult<Option<NavBatch>> {
		sqlx::query("SELECT * FROM nav_batches WHERE transaction_id = ?")
			.bind(transaction_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(batch_row)
	}

	async fn batch_archive(&self, batch_id: i64) -> ClResult<Option<NavArchive>> {
		sqlx::query("SELECT request_xml, response_xml FROM nav_batch_xml WHERE batch_id = ?")
			.bind(batch_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(|row| {
				Ok(NavArchive {
					request_xml: row.try_get("request_xml").db()?,
					response_xml: row.try_get("response_xml").db()?,
				})
			})
	}

	async fn batch_rows(&self, batch_id: i64) -> ClResult<Vec<NavSubmission>> {
		sqlx::query("SELECT * FROM nav_submissions WHERE batch_id = ? ORDER BY id ASC")
			.bind(batch_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(submission_row)
	}

	async fn filing_by_invoice(&self, invoice_id: i64) -> ClResult<Option<NavFiling>> {
		sqlx::query(concat!(filing_select!(), " WHERE s.invoice_id = ? ORDER BY s.id DESC LIMIT 1"))
			.bind(invoice_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(filing_row)
	}

	async fn filings_by_invoice(&self, invoice_id: i64) -> ClResult<Vec<NavFiling>> {
		sqlx::query(concat!(filing_select!(), " WHERE s.invoice_id = ? ORDER BY s.id DESC"))
			.bind(invoice_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(filing_row)
	}

	async fn list_filings(
		&self,
		states: &[FilingState],
		after_id: i64,
		limit: i64,
	) -> ClResult<Vec<NavFiling>> {
		let filter = if states.is_empty() {
			String::new()
		} else {
			format!(
				" AND (CASE WHEN s.state = 'ACTIVE' THEN b.state ELSE s.state END) IN ({})",
				placeholders(states.len())
			)
		};
		let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
			concat!(
				filing_select!(),
				" WHERE s.resolved_at IS NULL AND s.id > ?{} ORDER BY s.id ASC LIMIT ?"
			),
			filter
		)))
		.bind(after_id);
		for state in states {
			q = q.bind(state.as_str());
		}
		q.bind(limit).fetch_all(&mut *self.reader().await?).await.all(filing_row)
	}

	async fn supervised(&self, after_id: i64, limit: i64) -> ClResult<Vec<NavBatch>> {
		sqlx::query(
			"SELECT * FROM nav_batches
			  WHERE state IN ('CLAIMED','POSTING','IN_DOUBT','SENT') AND id > ?
			  ORDER BY id ASC LIMIT ?",
		)
		.bind(after_id)
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(batch_row)
	}

	async fn orphaned_invoices(&self, limit: i64) -> ClResult<Vec<i64>> {
		Ok(sqlx::query_scalar(ORPHANED)
			.bind(limit)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()?)
	}

	async fn adoptable(&self, limit: i64) -> ClResult<Vec<NavBatchId>> {
		// `IN`, not `EXISTS` from the batch: `idx_nav_submission_state` drives it from the few
		// `HELD` rows rather than from every batch ever closed.
		let uids: Vec<String> = sqlx::query_scalar(
			"SELECT uid FROM nav_batches
			  WHERE id IN (SELECT batch_id FROM nav_submissions
			                WHERE state = 'HELD' AND adopt_checked_at IS NULL)
			  ORDER BY id ASC LIMIT ?",
		)
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(uids.into_iter().map(NavBatchId::from_trusted).collect())
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
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()?)
	}

	async fn unfiled_invoices(&self, seller_id: i64, limit: i64) -> ClResult<Vec<i64>> {
		Ok(sqlx::query_scalar(UNFILED)
			.bind(seller_id)
			.bind(limit)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()?)
	}

	async fn awaiting_operator(
		&self,
		seller_id: Option<i64>,
		fault_before: Timestamp,
	) -> ClResult<i64> {
		// `EXISTS`, not a join to the newest submission: the correlated form avoids re-aggregating
		// on each hourly tick. The inner `JOIN` is safe: only a `CANCELLED` row has no batch.
		// `None` probes every issued invoice; drive it from `nav_submissions` if it hurts.
		Ok(sqlx::query_scalar(
			"SELECT COUNT(*) FROM invoices i
			  WHERE i.seller_id = COALESCE(?, i.seller_id) AND i.number IS NOT NULL
			    AND EXISTS (SELECT 1 FROM nav_submissions s
			                  JOIN nav_batches b ON b.id = s.batch_id
			                 WHERE s.invoice_id = i.id AND s.resolved_at IS NULL
			                   AND ((s.state = 'HELD' AND s.adopt_checked_at IS NOT NULL)
			                     OR s.state IN ('REJECTED','FAILED')
			                     OR (s.state = 'ACTIVE'
			                         AND (b.state = 'HELD'
			                           OR (b.state = 'SENT' AND b.error_code IS NOT NULL)
			                           OR (b.state = 'CLAIMED' AND b.fault_at < ?)))))",
		)
		.bind(seller_id)
		.bind(fault_before.0)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()?)
	}

	async fn backlog(&self, seller_id: i64) -> ClResult<i64> {
		// `b.state = 'CLAIMED'` adds nothing to `posted_at IS NULL` but `idx_nav_batch_state`:
		// `begin_post` stamps the one on leaving the other, and `posted_at` never clears.
		Ok(sqlx::query_scalar(
			"SELECT (SELECT COUNT(*) FROM invoices i
			          WHERE i.seller_id = ?1 AND i.number IS NOT NULL AND i.test_mode = 0
			            AND NOT EXISTS (SELECT 1 FROM nav_submissions s
			                             WHERE s.invoice_id = i.id))
			      + (SELECT COUNT(*) FROM nav_submissions s
			           JOIN nav_batches b ON b.id = s.batch_id
			          WHERE b.state = 'CLAIMED' AND b.seller_id = ?1 AND b.posted_at IS NULL
			            AND s.state = 'ACTIVE')",
		)
		.bind(seller_id)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()?)
	}

	async fn oldest_open_fault(&self, seller_id: i64) -> ClResult<Option<String>> {
		Ok(sqlx::query_scalar(
			"SELECT error_code FROM nav_batches
			  WHERE seller_id = ? AND state <> 'CLOSED' AND fault_at IS NOT NULL
			  ORDER BY fault_at ASC, id ASC LIMIT 1",
		)
		.bind(seller_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?)
	}

	async fn open_batch_keys(&self, seller_id: i64) -> ClResult<Vec<String>> {
		Ok(sqlx::query_scalar(
			"SELECT 'nav:send:' || uid FROM nav_batches
			  WHERE state = 'CLAIMED' AND seller_id = ? ORDER BY id ASC",
		)
		.bind(seller_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?)
	}

	async fn export_ids_by_date(&self, seller_id: i64, from: &str, to: &str) -> ClResult<Vec<i64>> {
		let (from, to) = mintworks_invoice::numbering::utc_span(from, to)?;
		Ok(sqlx::query_scalar(BY_DATE)
			.bind(seller_id)
			.bind(from.0)
			.bind(to.0)
			.fetch_all(&mut *self.reader().await?)
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
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()?)
	}
}

// vim: ts=4
