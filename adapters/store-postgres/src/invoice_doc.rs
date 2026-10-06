// SPDX-License-Identifier: MPL-2.0
//! The draft, issue, storno, list and summary half of `impl InvoiceStore for PgStore`; the impl
//! block in `invoice.rs` delegates each method here by the same name.
//!
//! Every `UPDATE invoices` here carries `AND status = 'DRAFT'` (or `IN ('DRAFT','PENDING')` on
//! the issue freeze), except `update_notes` — the one column an issued invoice may still change.
//!
//! `SUM` over `BIGINT` is `NUMERIC` in PostgreSQL, so every aggregate is cast back `::BIGINT`.

use mintworks_core::error::StatusCode;
use mintworks_core::prelude::*;
use mintworks_invoice::draft::Priced;
use mintworks_invoice::store::{
	Invoice, InvoiceFilter, InvoiceLine, InvoicePatch, InvoiceStatus, InvoiceSummary,
	InvoiceVatGroup, IssueInvoice, ListedInvoice, MonthBucket, NewInvoice, NewInvoiceLine,
	OverdueBucket, RevenueMonth, StatusBucket, render_number,
};
use sqlx::{PgConnection, postgres::PgRow};

use crate::PgStore;
use crate::invoice::{
	INVOICE_EXT, already_stornoed, duplicate_request_id, generic_conflict, invoice_row, line_row,
	listed_invoice_row, not_a_draft, vat_group_row,
};
use crate::util::{DbExt, RowExt, RowsExt, read_money};

/// The columns each of `invoice_summary`'s three aggregates selects, in order.
type StatusRow = (String, String, i64, i64, i64, i64, i64);
type MonthRow = (String, String, i64, i64, i64);
type OverdueRow = (String, i64, i64);

/// Rows per multi-row `INSERT`: 17 binds a row stays far under PostgreSQL's 65 535-bind cap.
const INSERT_CHUNK: usize = 1000;

/// `"($1,$2,…),($n,…)"` for `rows` rows of `cols` binds each, numbered from `$1`.
fn values_clause(rows: usize, cols: usize) -> String {
	let mut s = String::new();
	for r in 0..rows {
		if r > 0 {
			s.push(',');
		}
		s.push('(');
		for c in 0..cols {
			if c > 0 {
				s.push(',');
			}
			s.push('$');
			s.push_str(&(r * cols + c + 1).to_string());
		}
		s.push(')');
	}
	s
}

/// The issued number, from `doc_series` inside the issue transaction. The global write lock
/// serializes issuers, and a rolled-back transaction consumes no number — gaplessness.
///
/// `format` carries forward from the most recent earlier year of the same series, else the
/// schema default: the key includes `year`, and a custom format must not revert each January.
async fn allocate_number(
	tx: &mut PgConnection,
	seller_id: i64,
	code: &str,
	year: i64,
) -> ClResult<String> {
	let (no, format): (i64, String) = sqlx::query_as(
		"INSERT INTO doc_series (seller_id, kind, code, year, next_no, format)
		 SELECT $1, 'INVOICE', $2, $3, 2,
		        COALESCE(
		            (SELECT format FROM doc_series
		              WHERE seller_id = $1 AND kind = 'INVOICE' AND code = $2 AND year < $3
		              ORDER BY year DESC LIMIT 1),
		            '{code}{year}/{no:06}'
		        )
		 ON CONFLICT (seller_id, kind, code, year)
		   DO UPDATE SET next_no = doc_series.next_no + 1
		 RETURNING next_no - 1, format",
	)
	.bind(seller_id)
	.bind(code)
	.bind(year)
	.fetch_one(&mut *tx)
	.await
	.db()?;

	Ok(render_number(&format, code, year, no))
}

/// Writes the whole line set of a draft; `line_no` is NAV's `lineNumber`, 1..n in slice order.
async fn insert_lines(
	tx: &mut PgConnection,
	invoice_id: i64,
	lines: &[NewInvoiceLine],
) -> ClResult<()> {
	for (chunk, batch) in lines.chunks(INSERT_CHUNK).enumerate() {
		let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
			"INSERT INTO invoice_lines
			 (invoice_id, line_no, service_id, description, unit, qty, unit_price,
			  discount_kind, discount_value, discount_amount, discount_description,
			  net, vat_code, vat_rate_bp, vat, gross, note)
			 VALUES {}",
			values_clause(batch.len(), 17)
		)));
		for (i, line) in batch.iter().enumerate() {
			let no = chunk * INSERT_CHUNK + i + 1;
			q = q
				.bind(invoice_id)
				.bind(i64::try_from(no).unwrap_or(i64::MAX))
				.bind(line.service_id)
				.bind(&line.description)
				.bind(&line.unit)
				.bind(line.qty.0)
				.bind(line.unit_price.0)
				.bind(line.discount_kind.map(mintworks_invoice::store::DiscountKind::as_str))
				.bind(line.discount_value)
				.bind(line.discount_amount.0)
				.bind(&line.discount_description)
				.bind(line.net.0)
				.bind(line.vat_code.as_str())
				.bind(line.vat_rate_bp)
				.bind(line.vat.0)
				.bind(line.gross.0)
				.bind(&line.note);
		}
		q.execute(&mut *tx).await.db()?;
	}
	Ok(())
}

async fn replace_lines(
	tx: &mut PgConnection,
	invoice_id: i64,
	lines: &[NewInvoiceLine],
) -> ClResult<()> {
	sqlx::query("DELETE FROM invoice_lines WHERE invoice_id = $1")
		.bind(invoice_id)
		.execute(&mut *tx)
		.await
		.db()?;
	insert_lines(tx, invoice_id, lines).await
}

async fn replace_groups(
	tx: &mut PgConnection,
	invoice_id: i64,
	groups: &[InvoiceVatGroup],
) -> ClResult<()> {
	sqlx::query("DELETE FROM invoice_vat_groups WHERE invoice_id = $1")
		.bind(invoice_id)
		.execute(&mut *tx)
		.await
		.db()?;

	for batch in groups.chunks(INSERT_CHUNK) {
		let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
			"INSERT INTO invoice_vat_groups
			 (invoice_id, vat_code, vat_rate_bp, net, vat, gross, net_huf, vat_huf, gross_huf)
			 VALUES {}",
			values_clause(batch.len(), 9)
		)));
		for g in batch {
			q = q
				.bind(invoice_id)
				.bind(g.vat_code.as_str())
				.bind(g.vat_rate_bp)
				.bind(g.net.0)
				.bind(g.vat.0)
				.bind(g.gross.0)
				.bind(g.net_huf.map(|m| m.0))
				.bind(g.vat_huf.map(|m| m.0))
				.bind(g.gross_huf.map(|m| m.0));
		}
		q.execute(&mut *tx).await.db()?;
	}
	Ok(())
}

/// `DRAFT`/`PENDING` -> `ISSUED` (or `PAID` for `issue.paid`) with the number, dates, rate and
/// frozen buyer snapshot. Scoped to those two statuses, so a racing second issue finds no row.
async fn freeze(
	tx: &mut PgConnection,
	id: i64,
	number: &str,
	issue: &IssueInvoice,
) -> ClResult<Option<Invoice>> {
	sqlx::query(
		"UPDATE invoices SET
			status = CASE WHEN $1 THEN 'PAID' ELSE 'ISSUED' END,
			paid_amount = CASE WHEN $1 THEN $2 ELSE paid_amount END,
			paid_at = CASE WHEN $1 THEN $3 ELSE paid_at END,
			number = $4, series_code = $5, series_year = $6, issued_at = $3,
			fulfilment_date = $7, due_date = $8, period_start = $9, period_end = $10,
			rate_date = $11, rate_source = $12,
			huf_rate_e6 = $13, rate_e6 = COALESCE($14, rate_e6),
			net = $15, vat = $16, gross = $2, vat_note = $17, seller_ver = $18,
			buyer_kind = $19, buyer_name = $20, buyer_country = $21, buyer_tax_number = $22,
			buyer_eu_vat_id = $23, buyer_group_tax_no = $24, buyer_postcode = $25,
			buyer_city = $26, buyer_street = $27,
			buyer_vies_request_id = $28, buyer_vies_checked_at = $29,
			version = version + 1, updated_at = $3
		 WHERE id = $30 AND status IN ('DRAFT','PENDING')
		 RETURNING *",
	)
	.bind(issue.paid)
	.bind(issue.gross.0)
	.bind(issue.issued_at.0)
	.bind(number)
	.bind(&issue.series_code)
	.bind(issue.series_year)
	.bind(&issue.fulfilment_date)
	.bind(&issue.due_date)
	.bind(&issue.period_start)
	.bind(&issue.period_end)
	.bind(&issue.rate_date)
	.bind(issue.rate_source.map(mintworks_invoice::store::RateSource::as_str))
	.bind(issue.huf_rate_e6)
	.bind(issue.rate_e6)
	.bind(issue.net.0)
	.bind(issue.vat.0)
	.bind(&issue.vat_note)
	.bind(issue.seller_ver)
	.bind(issue.buyer.kind.as_str())
	.bind(&issue.buyer.name)
	.bind(&issue.buyer.country)
	.bind(&issue.buyer.tax_number)
	.bind(&issue.buyer.eu_vat_id)
	.bind(&issue.buyer.group_tax_no)
	.bind(&issue.buyer.postcode)
	.bind(&issue.buyer.city)
	.bind(&issue.buyer.street)
	.bind(&issue.buyer.vies_request_id)
	.bind(issue.buyer.vies_checked_at.map(|t| t.0))
	.bind(id)
	.fetch_optional(&mut *tx)
	.await
	.one(invoice_row)
}

/// The `invoices` UPDATE behind `update_draft`. `None` means absent or already issued.
async fn update_draft_row(
	conn: &mut PgConnection,
	id: i64,
	p: &InvoicePatch,
) -> ClResult<Option<Invoice>> {
	sqlx::query(
		"UPDATE invoices SET
			billing_party_id = COALESCE($1, billing_party_id),
			payment_method   = COALESCE($2, payment_method),
			fulfilment_date  = CASE WHEN $3 THEN $4 ELSE fulfilment_date END,
			due_date         = CASE WHEN $5 THEN $6 ELSE due_date END,
			period_start     = CASE WHEN $7 THEN $8 ELSE period_start END,
			period_end       = CASE WHEN $9 THEN $10 ELSE period_end END,
			notes            = CASE WHEN $11 THEN $12 ELSE notes END,
			currency         = COALESCE($13, currency),
			rate_e6          = COALESCE($14, rate_e6),
			discount_value   = COALESCE($15, discount_value),
			version          = version + 1,
			updated_at       = $16
		 WHERE id = $17 AND status = 'DRAFT' RETURNING *",
	)
	.bind(p.billing_party_id)
	.bind(p.payment_method.map(mintworks_invoice::store::PaymentMethod::as_str))
	.bind(!p.fulfilment_date.is_undefined())
	.bind(p.fulfilment_date.value())
	.bind(!p.due_date.is_undefined())
	.bind(p.due_date.value())
	.bind(!p.period_start.is_undefined())
	.bind(p.period_start.value())
	.bind(!p.period_end.is_undefined())
	.bind(p.period_end.value())
	.bind(!p.notes.is_undefined())
	.bind(p.notes.value())
	.bind(p.currency.as_ref().map(CurrencyCode::as_str))
	.bind(p.rate_e6)
	.bind(p.discount_value)
	.bind(Timestamp::now().0)
	.bind(id)
	.fetch_optional(&mut *conn)
	.await
	.one(invoice_row)
}

fn conflict_for(new: &NewInvoice) -> fn() -> Error {
	if new.request_id.is_some() { duplicate_request_id } else { generic_conflict }
}

/// Inserts a draft `invoices` row. `on_conflict` answers a unique violation: `UNIQUE (org_id,
/// request_id)` on the draft path, `idx_invoice_storno_once` on the storno path.
async fn insert_draft(
	tx: &mut PgConnection,
	new: &NewInvoice,
	on_conflict: fn() -> Error,
) -> ClResult<Invoice> {
	let now = Timestamp::now();
	let uid = InvoiceId::generate();
	let row = sqlx::query(
		"INSERT INTO invoices
		 (uid, request_id, org_id, seller_id, billing_party_id, kind,
		  original_invoice_id, currency, rate_e6, payment_method, notes,
		  discount_kind, discount_value, created_at, updated_at)
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $14)
		 RETURNING *",
	)
	.bind(uid.as_str())
	.bind(&new.request_id)
	.bind(new.org_id)
	.bind(new.seller_id)
	.bind(new.billing_party_id)
	.bind(new.kind.as_str())
	.bind(new.original_invoice_id)
	.bind(new.currency.as_str())
	.bind(new.rate_e6)
	.bind(new.payment_method.as_str())
	.bind(&new.notes)
	.bind(new.discount_kind.map(mintworks_invoice::store::DiscountKind::as_str))
	.bind(new.discount_value)
	.bind(now.0)
	.fetch_one(&mut *tx)
	.await
	.map_err(|e| match &e {
		sqlx::Error::Database(db) if db.is_unique_violation() => on_conflict(),
		_ => crate::util::map_db(&e),
	})?;
	invoice_row(&row)
}

/// `= ANY($1)` over `ids`, decoded with `f`; no chunking, the array is one bind.
async fn by_ids<T>(
	s: &PgStore,
	sql: &'static str,
	ids: &[i64],
	f: fn(&PgRow) -> ClResult<T>,
) -> ClResult<Vec<T>> {
	if ids.is_empty() {
		return Ok(Vec::new());
	}
	sqlx::query(sql).bind(ids).fetch_all(&mut *s.reader().await?).await.all(f)
}

// -- draft

pub(crate) async fn create_draft(s: &PgStore, new: &NewInvoice) -> ClResult<Invoice> {
	let tx = s.write_tx().await?;
	let invoice = insert_draft(&mut *tx.lock().await?, new, conflict_for(new)).await?;
	tx.commit().await?;
	Ok(invoice)
}

pub(crate) async fn create_draft_full(
	s: &PgStore,
	new: &NewInvoice,
	priced: &Priced,
	patch: Option<&InvoicePatch>,
) -> ClResult<Invoice> {
	let tx = s.write_tx().await?;

	// Scoped so the guard is released before `tx.rollback()` takes the same connection lock.
	let inserted = {
		let mut conn = tx.lock().await?;
		insert_draft(&mut conn, new, conflict_for(new)).await
	};
	let invoice = match inserted {
		Ok(invoice) => invoice,
		// A concurrent caller took the same idempotency key: its invoice is the answer, not a 409.
		Err(Error::Conflict(msg)) => {
			tx.rollback().await?;
			let Some(request_id) = &new.request_id else {
				return Err(Error::conflict(msg));
			};
			return invoice_by_request_id(s, new.org_id, request_id)
				.await?
				.ok_or_else(|| Error::conflict(msg));
		}
		Err(e) => return Err(e),
	};

	replace_lines(&mut *tx.lock().await?, invoice.id, &priced.lines).await?;
	replace_groups(&mut *tx.lock().await?, invoice.id, &priced.groups).await?;
	if let Some(p) = patch {
		update_draft_row(&mut *tx.lock().await?, invoice.id, p)
			.await?
			.ok_or_else(not_a_draft)?;
	}

	// Totals last, so the returned row already carries the patch.
	let row = sqlx::query(
		"UPDATE invoices SET net = $1, vat = $2, gross = $3, updated_at = $4
		 WHERE id = $5 AND status = 'DRAFT' RETURNING *",
	)
	.bind(priced.net.0)
	.bind(priced.vat.0)
	.bind(priced.gross.0)
	.bind(Timestamp::now().0)
	.bind(invoice.id)
	.fetch_one(&mut *tx.lock().await?)
	.await
	.db()?;
	let invoice = invoice_row(&row)?;

	tx.commit().await?;
	Ok(invoice)
}

pub(crate) async fn invoice_by_request_id(
	s: &PgStore,
	org_id: i64,
	request_id: &str,
) -> ClResult<Option<Invoice>> {
	sqlx::query("SELECT * FROM invoices WHERE org_id = $1 AND request_id = $2")
		.bind(org_id)
		.bind(request_id)
		.fetch_optional(&mut *s.reader().await?)
		.await
		.one(invoice_row)
}

pub(crate) async fn invoice_by_uid(
	s: &PgStore,
	org_id: Option<i64>,
	uid: &InvoiceId,
) -> ClResult<Option<Invoice>> {
	sqlx::query("SELECT * FROM invoices WHERE uid = $1 AND ($2::BIGINT IS NULL OR org_id = $2)")
		.bind(uid.as_str())
		.bind(org_id)
		.fetch_optional(&mut *s.reader().await?)
		.await
		.one(invoice_row)
}

pub(crate) async fn invoice_by_id(s: &PgStore, id: i64) -> ClResult<Option<Invoice>> {
	sqlx::query("SELECT * FROM invoices WHERE id = $1")
		.bind(id)
		.fetch_optional(&mut *s.reader().await?)
		.await
		.one(invoice_row)
}

pub(crate) async fn storno_of(s: &PgStore, original_id: i64) -> ClResult<Option<Invoice>> {
	sqlx::query("SELECT * FROM invoices WHERE original_invoice_id = $1 AND kind = 'STORNO'")
		.bind(original_id)
		.fetch_optional(&mut *s.reader().await?)
		.await
		.one(invoice_row)
}

pub(crate) async fn update_draft(
	s: &PgStore,
	id: i64,
	p: &InvoicePatch,
) -> ClResult<Option<Invoice>> {
	update_draft_row(&mut *s.conn().await?, id, p).await
}

/// No `status = 'DRAFT'` predicate, deliberately: `notes` is the one column an issued (or
/// stornoed) invoice may still change; the `version` bump keeps the rendered PDF in step.
pub(crate) async fn update_notes(
	s: &PgStore,
	id: i64,
	notes: Option<&str>,
) -> ClResult<Option<Invoice>> {
	sqlx::query(
		"UPDATE invoices SET notes = $1, version = version + 1, updated_at = $2
		 WHERE id = $3 RETURNING *",
	)
	.bind(notes)
	.bind(Timestamp::now().0)
	.bind(id)
	.fetch_optional(&mut *s.conn().await?)
	.await
	.one(invoice_row)
}

pub(crate) async fn replace_draft_lines(
	s: &PgStore,
	id: i64,
	patch: Option<&InvoicePatch>,
	priced: &Priced,
	expected_version: i64,
) -> ClResult<bool> {
	let tx = s.write_tx().await?;

	// Totals first, and the only gate: before `update_draft_row`, which bumps `version`.
	let done = sqlx::query(
		"UPDATE invoices SET net = $1, vat = $2, gross = $3, version = version + 1, updated_at = $4
		 WHERE id = $5 AND status = 'DRAFT' AND version = $6",
	)
	.bind(priced.net.0)
	.bind(priced.vat.0)
	.bind(priced.gross.0)
	.bind(Timestamp::now().0)
	.bind(id)
	.bind(expected_version)
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;

	if done.rows_affected() == 0 {
		return Ok(false);
	}

	if let Some(p) = patch
		&& update_draft_row(&mut *tx.lock().await?, id, p).await?.is_none()
	{
		return Ok(false);
	}

	replace_lines(&mut *tx.lock().await?, id, &priced.lines).await?;
	replace_groups(&mut *tx.lock().await?, id, &priced.groups).await?;

	tx.commit().await?;
	Ok(true)
}

pub(crate) async fn invoice_lines(s: &PgStore, invoice_id: i64) -> ClResult<Vec<InvoiceLine>> {
	sqlx::query("SELECT * FROM invoice_lines WHERE invoice_id = $1 ORDER BY line_no")
		.bind(invoice_id)
		.fetch_all(&mut *s.reader().await?)
		.await
		.all(line_row)
}

pub(crate) async fn invoice_vat_groups(
	s: &PgStore,
	invoice_id: i64,
) -> ClResult<Vec<InvoiceVatGroup>> {
	sqlx::query(
		"SELECT * FROM invoice_vat_groups WHERE invoice_id = $1 ORDER BY vat_code COLLATE \"C\"",
	)
	.bind(invoice_id)
	.fetch_all(&mut *s.reader().await?)
	.await
	.all(vat_group_row)
}

pub(crate) async fn invoices_by_ids(s: &PgStore, ids: &[i64]) -> ClResult<Vec<Invoice>> {
	by_ids(s, "SELECT * FROM invoices WHERE id = ANY($1)", ids, invoice_row).await
}

pub(crate) async fn invoice_lines_for(s: &PgStore, ids: &[i64]) -> ClResult<Vec<InvoiceLine>> {
	by_ids(
		s,
		"SELECT * FROM invoice_lines WHERE invoice_id = ANY($1) ORDER BY invoice_id, line_no",
		ids,
		line_row,
	)
	.await
}

pub(crate) async fn invoice_vat_groups_for(
	s: &PgStore,
	ids: &[i64],
) -> ClResult<Vec<InvoiceVatGroup>> {
	by_ids(
		s,
		"SELECT * FROM invoice_vat_groups WHERE invoice_id = ANY($1)
		 ORDER BY invoice_id, vat_code COLLATE \"C\"",
		ids,
		vat_group_row,
	)
	.await
}

pub(crate) async fn delete_draft(s: &PgStore, id: i64) -> ClResult<bool> {
	let tx = s.write_tx().await?;
	// `payment_allocations.invoice_id` has no cascade (a money trail); only a draft's zero link
	// rows are reached here, since `settle` refuses a draft.
	sqlx::query(
		"DELETE FROM payment_allocations
		  WHERE invoice_id IN (SELECT id FROM invoices WHERE id = $1 AND status = 'DRAFT')",
	)
	.bind(id)
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;
	// The ext data before the row that keys it: `(type, uid)` carries no FK.
	sqlx::query(
		"DELETE FROM objects
		  WHERE type = $1
		    AND EXISTS (SELECT 1 FROM invoices i
		                 WHERE i.uid = objects.uid AND i.org_id = objects.org_id
		                   AND i.id = $2 AND i.status = 'DRAFT')",
	)
	.bind(INVOICE_EXT)
	.bind(id)
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;
	let done = sqlx::query("DELETE FROM invoices WHERE id = $1 AND status = 'DRAFT'")
		.bind(id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
	tx.commit().await?;
	Ok(done.rows_affected() > 0)
}

pub(crate) async fn sweep_drafts(s: &PgStore, cutoff: Timestamp) -> ClResult<u64> {
	// Not a draft whose payment holds money, nor a live subscription's renewal: deleting it sent
	// a later-succeeding payment into `settle_full`'s "no invoice" branch.
	const LIVE: &str = "AND NOT EXISTS (SELECT 1 FROM payment_allocations a
	                      JOIN payments p ON p.id = a.payment_id
	                     WHERE a.invoice_id = invoices.id
	                       AND p.status IN ('PENDING','AWAITING_USER','RESERVED','AUTHORIZED',
	                                        'SUCCEEDED','PARTIALLY_SUCCEEDED'))
	                     AND NOT EXISTS (SELECT 1 FROM plan_invoices pi
	                      JOIN subscriptions s ON s.id = pi.subscription_id
	                     WHERE pi.invoice_id = invoices.id AND pi.kind = 'RENEWAL'
	                       AND s.status <> 'CANCELED')";
	let tx = s.write_tx().await?;
	sqlx::query(sqlx::AssertSqlSafe(format!(
		"DELETE FROM payment_allocations
		  WHERE invoice_id IN
		        (SELECT id FROM invoices
		          WHERE status IN ('DRAFT','PENDING') AND updated_at < $1 {LIVE})"
	)))
	.bind(cutoff.0)
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;
	sqlx::query(sqlx::AssertSqlSafe(format!(
		"DELETE FROM objects
		  WHERE type = $1
		    AND EXISTS (SELECT 1 FROM invoices
		                 WHERE invoices.uid = objects.uid AND invoices.org_id = objects.org_id
		                   AND invoices.status IN ('DRAFT','PENDING') AND invoices.updated_at < $2 {LIVE})"
	)))
	.bind(INVOICE_EXT)
	.bind(cutoff.0)
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;
	let done = sqlx::query(sqlx::AssertSqlSafe(format!(
		"DELETE FROM invoices WHERE id IN
		        (SELECT id FROM invoices
		          WHERE status IN ('DRAFT','PENDING') AND updated_at < $1 {LIVE})"
	)))
	.bind(cutoff.0)
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;
	tx.commit().await?;
	Ok(done.rows_affected())
}

pub(crate) async fn issued_without_document(s: &PgStore, limit: i64) -> ClResult<Vec<i64>> {
	// Ordered by the last render attempt, so permanently unrenderable invoices cannot hog the cap.
	sqlx::query_scalar(
		r#"SELECT i.id FROM invoices i
		 LEFT JOIN invoice_documents d ON d.invoice_id = i.id
		 WHERE i.number IS NOT NULL AND d.invoice_id IS NULL
		 ORDER BY (SELECT COALESCE(MAX(j.created_at), 0) FROM jobs j
			   WHERE j.kind = $1
			     AND j.payload = '{"invoiceId":' || i.id || '}') ASC,
			  i.id ASC
		 LIMIT $2"#,
	)
	.bind(mintworks_invoice::issue::KIND_RENDER_PDF)
	.bind(limit)
	.fetch_all(&mut *s.reader().await?)
	.await
	.db()
}

// -- issue

pub(crate) async fn issue(
	s: &PgStore,
	id: i64,
	issue: &IssueInvoice,
	expected_version: i64,
) -> ClResult<Invoice> {
	let tx = s.write_tx().await?;

	// `FOR UPDATE`: `update_draft` runs in autocommit, outside the write lock, and would
	// otherwise change the lines between this probe and the totals the issue writes.
	let seller_id: i64 = sqlx::query_scalar(
		"SELECT seller_id FROM invoices
		  WHERE id = $1 AND status IN ('DRAFT','PENDING') AND version = $2 FOR UPDATE",
	)
	.bind(id)
	.bind(expected_version)
	.fetch_optional(&mut *tx.lock().await?)
	.await
	.db()?
	.ok_or_else(|| {
		Error::coded(
			StatusCode::CONFLICT,
			"E-INV-CHANGED",
			"the draft changed while it was being issued",
		)
	})?;

	// The probe above is the gate: the child-table helpers carry no predicate of their own.
	replace_lines(&mut *tx.lock().await?, id, &issue.lines).await?;
	replace_groups(&mut *tx.lock().await?, id, &issue.groups).await?;

	let number =
		allocate_number(&mut *tx.lock().await?, seller_id, &issue.series_code, issue.series_year)
			.await?;
	let invoice = freeze(&mut *tx.lock().await?, id, &number, issue)
		.await?
		.ok_or_else(not_a_draft)?;

	tx.commit().await?;
	Ok(invoice)
}

pub(crate) async fn storno(
	s: &PgStore,
	original_id: i64,
	new: &NewInvoice,
	issue: &IssueInvoice,
) -> ClResult<Invoice> {
	let tx = s.write_tx().await?;

	let status: InvoiceStatus =
		sqlx::query_scalar::<_, String>("SELECT status FROM invoices WHERE id = $1")
			.bind(original_id)
			.fetch_optional(&mut *tx.lock().await?)
			.await
			.db()?
			.ok_or(Error::NotFound)?
			.parse()?;

	if !matches!(status, InvoiceStatus::Issued | InvoiceStatus::Paid) {
		return Err(Error::coded(
			StatusCode::CONFLICT,
			"E-INV-NOT-STORNOABLE",
			"only an issued or paid invoice can be cancelled",
		));
	}

	// `idx_invoice_storno_once` is the at-most-once guarantee; the check above only answers sooner.
	let draft = insert_draft(&mut *tx.lock().await?, new, already_stornoed).await?;

	insert_lines(&mut *tx.lock().await?, draft.id, &issue.lines).await?;
	replace_groups(&mut *tx.lock().await?, draft.id, &issue.groups).await?;

	let number = allocate_number(
		&mut *tx.lock().await?,
		new.seller_id,
		&issue.series_code,
		issue.series_year,
	)
	.await?;
	let storno = freeze(&mut *tx.lock().await?, draft.id, &number, issue)
		.await?
		.ok_or_else(not_a_draft)?;

	// An invariant check, not a race guard: a frozen STORNO must never commit beside an
	// original still `ISSUED`, since both rows are immutable.
	let flipped = sqlx::query(
		"UPDATE invoices SET status = 'STORNOED', updated_at = $1
		  WHERE id = $2 AND status IN ('ISSUED','PAID')",
	)
	.bind(Timestamp::now().0)
	.bind(original_id)
	.execute(&mut *tx.lock().await?)
	.await
	.db()?;
	if flipped.rows_affected() == 0 {
		return Err(Error::internal(format!(
			"storno {}: original invoice {original_id} left the issued state mid-transaction",
			storno.id
		)));
	}

	tx.commit().await?;
	Ok(storno)
}

// -- read

pub(crate) async fn list_invoices(
	s: &PgStore,
	org_id: i64,
	before_id: Option<i64>,
	limit: i64,
) -> ClResult<Vec<Invoice>> {
	sqlx::query(
		"SELECT * FROM invoices
		 WHERE org_id = $1 AND ($2::BIGINT IS NULL OR id < $2)
		 ORDER BY id DESC LIMIT $3",
	)
	.bind(org_id)
	.bind(before_id)
	.bind(limit)
	.fetch_all(&mut *s.reader().await?)
	.await
	.all(invoice_row)
}

pub(crate) async fn list_invoices_page(
	s: &PgStore,
	org_id: i64,
	filter: &InvoiceFilter,
	before_id: Option<i64>,
	limit: i64,
) -> ClResult<Vec<ListedInvoice>> {
	// One `TEXT[]` bind for the statuses; the tags come from `InvoiceStatus`, never request text.
	let statuses: Vec<&str> = filter.statuses.iter().map(|st| st.as_str()).collect();
	// `ILIKE` for SQLite's `LIKE`, but it also folds non-ASCII case; unindexed, demo scale.
	let text = if filter.q.is_some() {
		" AND (i.number ILIKE $5 ESCAPE '\\' OR i.buyer_name ILIKE $5 ESCAPE '\\'
		       OR p.name ILIKE $5 ESCAPE '\\')"
	} else {
		""
	};
	// `i.*` first, so `invoice_row`'s `uid` resolves to the invoice's own. The storno join
	// cannot duplicate a row: `idx_invoice_storno_once` is unique.
	let sql = format!(
		"SELECT i.*, p.uid AS party_uid, o.uid AS original_uid, s.uid AS storno_uid
		 FROM invoices i
		 LEFT JOIN billing_parties p ON p.id = i.billing_party_id
		 LEFT JOIN invoices o ON o.id = i.original_invoice_id
		 LEFT JOIN invoices s
		        ON s.original_invoice_id = i.id AND s.kind = 'STORNO'
		       AND i.status = 'STORNOED'
		 WHERE i.org_id = $1 AND ($2::BIGINT IS NULL OR i.id < $2)
		   AND (cardinality($3::TEXT[]) = 0 OR i.status = ANY($3)){text}
		 ORDER BY i.id DESC LIMIT $4"
	);
	let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
		.bind(org_id)
		.bind(before_id)
		.bind(statuses)
		.bind(limit);
	if let Some(text) = &filter.q {
		// `%`, `_` and `\` typed by a user are literals, not wildcards.
		q = q.bind(format!(
			"%{}%",
			text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
		));
	}
	q.fetch_all(&mut *s.reader().await?).await.all(listed_invoice_row)
}

pub(crate) async fn invoice_summary(
	s: &PgStore,
	org_id: i64,
	from_month: &str,
	today: &str,
	this_month: (Timestamp, Timestamp),
) -> ClResult<InvoiceSummary> {
	let statuses: Vec<StatusRow> = sqlx::query_as(
		"SELECT CASE WHEN kind = 'STORNO' THEN 'STORNOED' ELSE status END AS st, currency, \
		 COUNT(*), COALESCE(SUM(net), 0)::BIGINT, COALESCE(SUM(vat), 0)::BIGINT, \
		 COALESCE(SUM(gross), 0)::BIGINT, COALESCE(SUM(paid_amount), 0)::BIGINT \
		 FROM invoices WHERE org_id = $1 \
		 GROUP BY 1, 2 ORDER BY 1, 2",
	)
	.bind(org_id)
	.fetch_all(&mut *s.reader().await?)
	.await
	.db()?;

	// The fulfilment date, not `issued_at`: it is the statutory period and already a local day,
	// while `issued_at` buckets in UTC. The fallback day is UTC, as SQLite's `date(…,'unixepoch')`.
	let months: Vec<MonthRow> = sqlx::query_as(
		"SELECT substr(COALESCE(fulfilment_date, \
		   to_char(to_timestamp(issued_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD')), 1, 7) AS month, \
		 currency, COUNT(*), COALESCE(SUM(gross), 0)::BIGINT, \
		 COALESCE(SUM(paid_amount), 0)::BIGINT \
		 FROM invoices \
		 WHERE org_id = $1 AND number IS NOT NULL \
		   AND substr(COALESCE(fulfilment_date, \
		     to_char(to_timestamp(issued_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD')), 1, 7) >= $2 \
		 GROUP BY 1, 2 ORDER BY 1, 2",
	)
	.bind(org_id)
	.bind(from_month)
	.fetch_all(&mut *s.reader().await?)
	.await
	.db()?;

	let overdue: Vec<OverdueRow> = sqlx::query_as(
		"SELECT currency, COUNT(*), COALESCE(SUM(gross - paid_amount), 0)::BIGINT \
		 FROM invoices \
		 WHERE org_id = $1 AND status = 'ISSUED' AND due_date IS NOT NULL AND due_date < $2 \
		   AND paid_amount < gross \
		 GROUP BY 1 ORDER BY 1",
	)
	.bind(org_id)
	.bind(today)
	.fetch_all(&mut *s.reader().await?)
	.await
	.db()?;

	// Invoices fully paid this month (`paid_at`); partial payments would need allocations.
	let paid: Vec<(String, i64)> = sqlx::query_as(
		"SELECT currency, COALESCE(SUM(paid_amount), 0)::BIGINT FROM invoices \
		 WHERE org_id = $1 AND paid_at >= $2 AND paid_at < $3 \
		 GROUP BY 1 ORDER BY 1",
	)
	.bind(org_id)
	.bind(this_month.0.0)
	.bind(this_month.1.0)
	.fetch_all(&mut *s.reader().await?)
	.await
	.db()?;

	Ok(InvoiceSummary {
		paid_this_month: paid
			.into_iter()
			.map(|(currency, paid)| Ok((CurrencyCode::from_trusted(currency), read_money(paid)?)))
			.collect::<ClResult<Vec<_>>>()?,
		statuses: statuses
			.into_iter()
			.map(|(status, currency, count, net, vat, gross, paid)| {
				Ok(StatusBucket {
					status: status.parse()?,
					currency: CurrencyCode::from_trusted(currency),
					count,
					net: read_money(net)?,
					vat: read_money(vat)?,
					gross: read_money(gross)?,
					paid: read_money(paid)?,
				})
			})
			.collect::<ClResult<Vec<_>>>()?,
		months: months
			.into_iter()
			.map(|(month, currency, count, gross, paid)| {
				Ok(MonthBucket {
					month,
					currency: CurrencyCode::from_trusted(currency),
					count,
					gross: read_money(gross)?,
					paid: read_money(paid)?,
				})
			})
			.collect::<ClResult<Vec<_>>>()?,
		overdue: overdue
			.into_iter()
			.map(|(currency, count, outstanding)| {
				Ok(OverdueBucket {
					currency: CurrencyCode::from_trusted(currency),
					count,
					outstanding: read_money(outstanding)?,
				})
			})
			.collect::<ClResult<Vec<_>>>()?,
	})
}

pub(crate) async fn invoice_revenue(
	s: &PgStore,
	org_id: i64,
	year: i32,
	start: Timestamp,
	end: Timestamp,
) -> ClResult<Vec<RevenueMonth>> {
	let y = format!("{year:04}");
	// `net_huf` is NULL exactly on an HUF invoice, where `net` already is HUF.
	let invoiced: Vec<(String, i64)> = sqlx::query_as(
		"SELECT substr(i.fulfilment_date, 1, 7), \
		   COALESCE(SUM(COALESCE(g.net_huf, g.net)), 0)::BIGINT \
		 FROM invoice_vat_groups g JOIN invoices i ON i.id = g.invoice_id \
		 WHERE i.org_id = $1 AND i.number IS NOT NULL AND g.vat_code NOT IN ('EUFAD37','HO') \
		   AND substr(i.fulfilment_date, 1, 4) = $2 \
		 GROUP BY 1",
	)
	.bind(org_id)
	.bind(&y)
	.fetch_all(&mut *s.reader().await?)
	.await
	.db()?;
	// Bucketed in Rust by `numbering::date_of`, Budapest's day, not the database session's zone.
	let received: Vec<(Option<i64>, Option<String>, String, i64)> = sqlx::query_as(
		"SELECT i.paid_at, i.fulfilment_date, i.payment_method, \
		   COALESCE(SUM(COALESCE(g.net_huf, g.net)), 0)::BIGINT \
		 FROM invoices i JOIN invoice_vat_groups g ON g.invoice_id = i.id \
		 WHERE i.org_id = $1 AND i.number IS NOT NULL \
		   AND ((i.paid_at >= $2 AND i.paid_at < $3) \
		     OR (i.payment_method = 'CASH' AND substr(i.fulfilment_date, 1, 4) = $4)) \
		 GROUP BY i.id",
	)
	.bind(org_id)
	.bind(start.0)
	.bind(end.0)
	.bind(&y)
	.fetch_all(&mut *s.reader().await?)
	.await
	.db()?;

	// An unpaid invoice fulfilled in an earlier year is not shown, though its payment would
	// count this year.
	let outstanding: Vec<(String, i64)> = sqlx::query_as(
		"SELECT substr(i.fulfilment_date, 1, 7), \
		   COALESCE(SUM(COALESCE(g.net_huf, g.net)), 0)::BIGINT \
		 FROM invoice_vat_groups g JOIN invoices i ON i.id = g.invoice_id \
		 WHERE i.org_id = $1 AND i.kind = 'NORMAL' AND i.status = 'ISSUED' \
		   AND i.paid_at IS NULL AND i.payment_method <> 'CASH' \
		   AND substr(i.fulfilment_date, 1, 4) = $2 \
		 GROUP BY 1",
	)
	.bind(org_id)
	.bind(&y)
	.fetch_all(&mut *s.reader().await?)
	.await
	.db()?;

	let mut months: Vec<(String, i64, i64, i64)> =
		(1..=12).map(|m| (format!("{y}-{m:02}"), 0, 0, 0)).collect();
	let slot = |month: &str| months.iter().position(|(m, ..)| m == month);
	let mut add = Vec::new();
	for (month, net) in invoiced {
		if let Some(i) = slot(&month) {
			add.push((i, net, 0, 0));
		}
	}
	for (month, net) in outstanding {
		if let Some(i) = slot(&month) {
			add.push((i, 0, 0, net));
		}
	}
	// Partial payments are not counted — `paid_at` is only stamped once the whole gross is in.
	for (paid_at, fulfilment, method, net) in received {
		let day = match (method.as_str(), paid_at, fulfilment) {
			("CASH", _, Some(f)) => f,
			(_, Some(at), _) => mintworks_invoice::numbering::date_of(Timestamp(at))?,
			_ => continue,
		};
		if let Some(i) = day.get(..7).and_then(slot) {
			add.push((i, 0, net, 0));
		}
	}
	for (i, inv, rec, out) in add {
		months[i].1 += inv;
		months[i].2 += rec;
		months[i].3 += out;
	}
	months
		.into_iter()
		.map(|(month, inv, rec, out)| {
			Ok(RevenueMonth {
				month,
				invoiced_huf: read_money(inv)?,
				received_huf: read_money(rec)?,
				outstanding_huf: read_money(out)?,
			})
		})
		.collect()
}

// vim: ts=4
