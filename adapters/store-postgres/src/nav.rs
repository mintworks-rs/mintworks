// SPDX-License-Identifier: MPL-2.0
//! `NavStore` over PostgreSQL — the SQLite adapter's `nav.rs` in PG dialect.
//!
//! The two export selections read `invoices` only. They must never join `nav_submissions`: an
//! invoice that failed to report to NAV is still part of the seller's turnover and is still
//! statutorily exportable.
//!
//! A transition whose guard reads its own row alone is one autocommit `UPDATE`; everything else
//! is one `write_tx`, rolled back by `refuse` when its guard fails, so a refusal leaves nothing.
//!
//! Autocommit runs outside `write_tx`'s lock. READ COMMITTED re-checks a guard under the row lock,
//! and no autocommit transition leaves `CLAIMED`, `SENT` or `HELD`, the states a `write_tx` reads
//! before it writes.

use async_trait::async_trait;
use mintworks_core::{ids::NavBatchId, prelude::*};
use mintworks_nav::store::NavStore;
use mintworks_nav::submission::{
	BatchState, FilingState, NavArchive, NavBatch, NavFiling, NavOp, NavSellerHold, NavSubmission,
	NavVerdict, RowState,
};
use sqlx::{Row, postgres::PgRow};

use crate::{
	PgStore, WriteTx,
	util::{DbExt, RowExt, RowsExt},
};

/// Read by column name: `SELECT *` follows the DDL's column order.
fn submission_row(row: &PgRow) -> ClResult<NavSubmission> {
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
		created_at: Timestamp(row.try_get("created_at").db()?),
		done_at: row.try_get::<Option<i64>, _>("done_at").db()?.map(Timestamp),
		resolved_at: row.try_get::<Option<i64>, _>("resolved_at").db()?.map(Timestamp),
	})
}

/// `p` is the column prefix: `filing_select!`'s join aliases the batch's columns, six of which
/// `nav_submissions` also has.
fn batch_cols(row: &PgRow, p: &str) -> ClResult<NavBatch> {
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

fn batch_row(row: &PgRow) -> ClResult<NavBatch> {
	batch_cols(row, "")
}

fn filing_row(row: &PgRow) -> ClResult<NavFiling> {
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
		 VALUES ($1, $2, $3, $4, $5, $6, $7)
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
		 VALUES ($1, $2, $3, $4, 'ACTIVE', $5, $6)
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
		"UPDATE nav_batches SET state = 'CLOSED', closed_at = $1
		  WHERE id = $2 AND state = $3
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
	sqlx::query(
		"SELECT s.id, s.invoice_id, s.op, s.batch_id, s.state, s.verdict, s.electronic,
		        b.seller_id, b.state AS batch_state
		   FROM nav_submissions s JOIN nav_batches b ON b.id = s.batch_id
		  WHERE s.id = ANY($1) ORDER BY s.id ASC",
	)
	.bind(ids)
	.fetch_all(&mut *tx.lock().await?)
	.await
	.all(|row| {
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
	store: &PgStore,
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
		    SET state = $1, verdict = $2, error_code = $3, error_msg = $4, done_at = $5
		  WHERE id = $6 AND state = 'ACTIVE'
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
	 VALUES ($1, $2, $3, $4)
	 ON CONFLICT(seller_id) DO NOTHING";

/// Closes an in-range set over storno pairs in both directions; a range showing one half of a
/// pair would misstate turnover. `number IS NOT NULL AND seller_id = $1` is re-applied so a row
/// merely *pointing* at an in-range invoice cannot enter; the parentheses are load-bearing.
macro_rules! close_over_storno_pairs {
	() => {
		"
	SELECT id FROM invoices
	 WHERE number IS NOT NULL AND seller_id = $1 AND test_mode = 0
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
/// Binds, in order: `seller_id`, `limit`. `pub` for adapter tests only; not store API.
#[doc(hidden)]
pub const UNFILED: &str = "SELECT i.id FROM invoices i
	 WHERE i.seller_id = $1 AND i.number IS NOT NULL AND i.test_mode = 0
	   AND NOT EXISTS (SELECT 1 FROM nav_submissions s WHERE s.invoice_id = i.id)
	 ORDER BY i.id ASC LIMIT $2";

/// `UNFILED` over every seller, less the invoices a live `NAV_REPORT` already owns: what
/// `NAV_SWEEP` re-enqueues. The key is `mintworks_invoice`'s `invoice_job_key`; `jobs.dedup_key`
/// is UNIQUE, so the probe is one index lookup.
///
/// Binds: `limit`.
const ORPHANED: &str = "SELECT i.id FROM invoices i
	 WHERE i.number IS NOT NULL AND i.test_mode = 0
	   AND NOT EXISTS (SELECT 1 FROM nav_submissions s WHERE s.invoice_id = i.id)
	   AND NOT EXISTS (SELECT 1 FROM jobs j
	                    WHERE j.dedup_key = 'nav:invoice:' || i.id
	                      AND j.status IN ('PENDING','RUNNING'))
	 ORDER BY i.id ASC LIMIT $1";

/// What `NAV_REPORT` may assemble alongside its own invoice: the same "issued, no
/// `nav_submissions` row at all" shape as [`UNFILED`], narrowed to `kind = 'NORMAL'` — a storno
/// waits for the invoice it cancels to be filed, so it can never ride in an arbitrary batch — and
/// excluding the invoice being assembled for, which the job adds to the claim itself. The third
/// bind is `require_document` as 0/1.
///
/// Binds, in order: `seller_id`, `exclude_invoice_id`, `require_document`, `limit`.
#[doc(hidden)]
pub const BATCH_CANDIDATES: &str = "SELECT i.id FROM invoices i
	 WHERE i.seller_id = $1 AND i.number IS NOT NULL AND i.kind = 'NORMAL'
	   AND i.test_mode = 0
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
	    WHERE seller_id = $1 AND number IS NOT NULL AND test_mode = 0
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
	    WHERE seller_id = $1 AND number IS NOT NULL AND test_mode = 0
	      AND (length(number), number COLLATE \"C\") >= (length($2), $2 COLLATE \"C\")
	      AND (length(number), number COLLATE \"C\") <= (length($3), $3 COLLATE \"C\")
	 )",
	close_over_storno_pairs!()
);

#[async_trait]
impl NavStore for PgStore {
	async fn claim_batch(
		&self,
		seller_id: i64,
		op: NavOp,
		invoice_ids: &[i64],
		electronic: bool,
	) -> ClResult<Option<(NavBatch, Vec<i64>)>> {
		// One write transaction: `idx_nav_submission_live` is the only thing between two claims
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
			"SELECT id FROM nav_submissions WHERE batch_id = $1 AND state = 'ACTIVE' ORDER BY id",
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
			"UPDATE nav_batches SET state = 'POSTING', posted_at = $1
			  WHERE id = $2 AND state = 'CLAIMED'",
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
			sqlx::query("UPDATE nav_submissions SET idx = $1 WHERE id = $2")
				.bind(idx)
				.bind(id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}
		sqlx::query(
			"INSERT INTO nav_batch_xml (batch_id, request_xml) VALUES ($1, $2)
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
			    SET state = 'CLAIMED', error_code = COALESCE($1, error_code),
			        error_msg = COALESCE($2, error_msg),
			        fault_at = CASE WHEN $1 IS NULL THEN fault_at
			                        ELSE COALESCE(fault_at, $3) END
			  WHERE id = $4 AND state = 'POSTING'",
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
			    SET state = 'IN_DOUBT', error_code = $1, error_msg = $2, fault_at = $3
			  WHERE id = $4 AND state = 'POSTING'",
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
			    SET state = 'SENT', transaction_id = $1,
			        error_code = NULL, error_msg = NULL, fault_at = NULL
			  WHERE id = $2 AND state IN ('POSTING','IN_DOUBT')
			    AND NOT EXISTS (SELECT 1 FROM nav_batches o WHERE o.transaction_id = $1)",
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
			  WHERE id = $1 AND state = 'IN_DOUBT' AND error_code IS NULL",
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
			    SET state = 'HELD', error_code = $1, error_msg = $2,
			        fault_at = COALESCE(fault_at, $3)
			  WHERE id = $4 AND state = 'IN_DOUBT'",
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
			    SET state = 'CLOSED', error_code = $1, error_msg = $2,
			        fault_at = COALESCE(fault_at, $3), closed_at = $4
			  WHERE id = $5 AND state = 'POSTING'
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
			    SET state = 'FAILED', verdict = 'FAILED', error_code = $1, error_msg = $2,
			        done_at = $3, resolved_at = $4
			  WHERE batch_id = $5 AND state = 'ACTIVE'
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
			  WHERE id = $1 AND state = 'ACTIVE'
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
			"UPDATE nav_submissions SET state = 'CANCELLED', done_at = $1
			  WHERE invoice_id = $2 AND op = $3 AND state = 'ACTIVE'
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
			 SELECT $1, $2, 'CANCELLED', $3, $3
			  WHERE NOT EXISTS (SELECT 1 FROM nav_submissions
			                     WHERE invoice_id = $1 AND op = $2)",
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
		let done = sqlx::query("DELETE FROM nav_submissions WHERE id = $1 AND state = 'CANCELLED'")
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
					        error_code = CASE WHEN verdict IS NULL THEN $1 ELSE error_code END,
					        done_at = CASE WHEN verdict IS NULL THEN $2 ELSE done_at END,
					        verdict = COALESCE(verdict, 'FAILED'), resolved_at = $2
					  WHERE id = $3",
				)
				.bind(code)
				.bind(at.0)
				.bind(m.id),
				None => sqlx::query(
					"UPDATE nav_submissions SET resolved_at = COALESCE(resolved_at, $1)
					  WHERE id = $2",
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
	) -> ClResult<Option<NavBatch>> {
		if rows.is_empty() {
			return Ok(None);
		}
		let mut rows = rows.to_vec();
		rows.sort_unstable();
		let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
		let tx = self.write_tx().await?;
		let taken: bool = sqlx::query_scalar(
			"SELECT EXISTS(SELECT 1 FROM nav_batches WHERE transaction_id = $1)",
		)
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
			sqlx::query("UPDATE nav_submissions SET state = $1, resolved_at = $2 WHERE id = $3")
				.bind(state)
				.bind(at.0)
				.bind(m.id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
			insert_active(&tx, m.invoice_id, op, batch.id, Some(*idx), m.electronic, at).await?;
		}
		close_held(&tx, &old, at).await?;
		tx.commit().await?;
		Ok(Some(batch))
	}

	async fn stamp_adopt_checked(&self, row_ids: &[i64], at: Timestamp) -> ClResult<u64> {
		let done = sqlx::query(
			"UPDATE nav_submissions SET adopt_checked_at = $1
			  WHERE state = 'HELD' AND id = ANY($2)",
		)
		.bind(at.0)
		.bind(row_ids)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(done.rows_affected())
	}

	async fn resolve(&self, row_id: i64, at: Timestamp) -> ClResult<bool> {
		let done = sqlx::query(
			"UPDATE nav_submissions SET resolved_at = $1
			  WHERE id = $2 AND state IN ('REJECTED','FAILED') AND resolved_at IS NULL",
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
			    SET error_code = $1, error_msg = $2, fault_at = COALESCE(fault_at, $3)
			  WHERE id = $4 AND state <> 'CLOSED'",
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
		// The key is selected from `nav_batches` so an unknown id gives zero rows, not an FK error.
		// Add a `nav_batch_events` table if a dispute ever needs per-poll history: the archive
		// keeps the latest reply, which is the decisive one.
		sqlx::query(
			"INSERT INTO nav_batch_xml (batch_id, response_xml)
			   SELECT id, $1 FROM nav_batches WHERE id = $2
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
		let done = sqlx::query("DELETE FROM nav_seller_holds WHERE seller_id = $1")
			.bind(seller_id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn seller_hold(&self, seller_id: i64) -> ClResult<Option<NavSellerHold>> {
		sqlx::query("SELECT * FROM nav_seller_holds WHERE seller_id = $1")
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
		sqlx::query_scalar("SELECT COUNT(*) FROM nav_seller_holds")
			.fetch_one(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn batch_by_uid(&self, uid: &str) -> ClResult<Option<NavBatch>> {
		sqlx::query("SELECT * FROM nav_batches WHERE uid = $1")
			.bind(uid)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(batch_row)
	}

	async fn batch_by_transaction(&self, transaction_id: &str) -> ClResult<Option<NavBatch>> {
		sqlx::query("SELECT * FROM nav_batches WHERE transaction_id = $1")
			.bind(transaction_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(batch_row)
	}

	async fn batch_archive(&self, batch_id: i64) -> ClResult<Option<NavArchive>> {
		sqlx::query("SELECT request_xml, response_xml FROM nav_batch_xml WHERE batch_id = $1")
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
		sqlx::query("SELECT * FROM nav_submissions WHERE batch_id = $1 ORDER BY id ASC")
			.bind(batch_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(submission_row)
	}

	async fn filing_by_invoice(&self, invoice_id: i64) -> ClResult<Option<NavFiling>> {
		sqlx::query(concat!(
			filing_select!(),
			" WHERE s.invoice_id = $1 ORDER BY s.id DESC LIMIT 1"
		))
		.bind(invoice_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(filing_row)
	}

	async fn filings_by_invoice(&self, invoice_id: i64) -> ClResult<Vec<NavFiling>> {
		sqlx::query(concat!(filing_select!(), " WHERE s.invoice_id = $1 ORDER BY s.id DESC"))
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
		// `= ANY` over an empty array matches nothing, and empty `states` means all.
		let (filter, limit_at) = if states.is_empty() {
			("", "$2")
		} else {
			(" AND (CASE WHEN s.state = 'ACTIVE' THEN b.state ELSE s.state END) = ANY($2)", "$3")
		};
		let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
			concat!(
				filing_select!(),
				" WHERE s.resolved_at IS NULL AND s.id > $1{} ORDER BY s.id ASC LIMIT {}"
			),
			filter, limit_at
		)))
		.bind(after_id);
		if !states.is_empty() {
			q = q.bind(states.iter().map(|s| s.as_str()).collect::<Vec<_>>());
		}
		q.bind(limit).fetch_all(&mut *self.reader().await?).await.all(filing_row)
	}

	async fn supervised(&self, after_id: i64, limit: i64) -> ClResult<Vec<NavBatch>> {
		sqlx::query(
			"SELECT * FROM nav_batches
			  WHERE state IN ('CLAIMED','POSTING','IN_DOUBT','SENT') AND id > $1
			  ORDER BY id ASC LIMIT $2",
		)
		.bind(after_id)
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(batch_row)
	}

	async fn orphaned_invoices(&self, limit: i64) -> ClResult<Vec<i64>> {
		sqlx::query_scalar(ORPHANED)
			.bind(limit)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn adoptable(&self, limit: i64) -> ClResult<Vec<NavBatchId>> {
		let uids: Vec<String> = sqlx::query_scalar(
			"SELECT uid FROM nav_batches
			  WHERE id IN (SELECT batch_id FROM nav_submissions
			                WHERE state = 'HELD' AND adopt_checked_at IS NULL)
			  ORDER BY id ASC LIMIT $1",
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
		sqlx::query_scalar(BATCH_CANDIDATES)
			.bind(seller_id)
			.bind(exclude_invoice_id)
			.bind(i64::from(require_document))
			.bind(limit)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn unfiled_invoices(&self, seller_id: i64, limit: i64) -> ClResult<Vec<i64>> {
		sqlx::query_scalar(UNFILED)
			.bind(seller_id)
			.bind(limit)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn awaiting_operator(
		&self,
		seller_id: Option<i64>,
		fault_before: Timestamp,
	) -> ClResult<i64> {
		// `EXISTS`, not a join to the newest submission: the correlated form avoids re-aggregating
		// on each hourly tick. The inner `JOIN` is safe: only a `CANCELLED` row has no batch.
		// `None` probes every issued invoice; drive it from `nav_submissions` if it hurts.
		sqlx::query_scalar(
			"SELECT COUNT(*) FROM invoices i
			  WHERE i.seller_id = COALESCE($1, i.seller_id) AND i.number IS NOT NULL
			    AND EXISTS (SELECT 1 FROM nav_submissions s
			                  JOIN nav_batches b ON b.id = s.batch_id
			                 WHERE s.invoice_id = i.id AND s.resolved_at IS NULL
			                   AND ((s.state = 'HELD' AND s.adopt_checked_at IS NOT NULL)
			                     OR s.state IN ('REJECTED','FAILED')
			                     OR (s.state = 'ACTIVE'
			                         AND (b.state = 'HELD'
			                           OR (b.state = 'SENT' AND b.error_code IS NOT NULL)
			                           OR (b.state = 'CLAIMED' AND b.fault_at < $2)))))",
		)
		.bind(seller_id)
		.bind(fault_before.0)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn backlog(&self, seller_id: i64) -> ClResult<i64> {
		// `b.state = 'CLAIMED'` adds nothing to `posted_at IS NULL` but `idx_nav_batch_state`:
		// `begin_post` stamps the one on leaving the other, and `posted_at` never clears.
		sqlx::query_scalar(
			"SELECT (SELECT COUNT(*) FROM invoices i
			          WHERE i.seller_id = $1 AND i.number IS NOT NULL AND i.test_mode = 0
			            AND NOT EXISTS (SELECT 1 FROM nav_submissions s
			                             WHERE s.invoice_id = i.id))
			      + (SELECT COUNT(*) FROM nav_submissions s
			           JOIN nav_batches b ON b.id = s.batch_id
			          WHERE b.state = 'CLAIMED' AND b.seller_id = $1 AND b.posted_at IS NULL
			            AND s.state = 'ACTIVE')",
		)
		.bind(seller_id)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn oldest_open_fault(&self, seller_id: i64) -> ClResult<Option<String>> {
		sqlx::query_scalar(
			"SELECT error_code FROM nav_batches
			  WHERE seller_id = $1 AND state <> 'CLOSED' AND fault_at IS NOT NULL
			  ORDER BY fault_at ASC, id ASC LIMIT 1",
		)
		.bind(seller_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn open_batch_keys(&self, seller_id: i64) -> ClResult<Vec<String>> {
		sqlx::query_scalar(
			"SELECT 'nav:send:' || uid FROM nav_batches
			  WHERE state = 'CLAIMED' AND seller_id = $1 ORDER BY id ASC",
		)
		.bind(seller_id)
		.fetch_all(&mut *self.reader().await?)
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
