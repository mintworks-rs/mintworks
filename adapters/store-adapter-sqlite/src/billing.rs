//! `BillingStore` over SQLite.
//!
//! Every status write carries its own `status IN (…)` guard, so a replayed gateway callback
//! updates nothing and answers `false` rather than settling the same money twice.

use async_trait::async_trait;
use saas_billing::provider::PaymentState;
use saas_billing::store::{
	BillingStore, NewPayment, OverdueInvoice, Payment, PaymentAllocation, PaymentFilter,
	RefundRecord, Settlement,
};
use saas_core::prelude::*;
use sqlx::{Row, sqlite::SqliteRow};

use crate::{
	SqliteStore,
	util::{DbExt, RowExt, RowsExt, read_money, unique_as_conflict},
};

/// `saas-billing` carries no driver dependency, so the rows are built by hand. Read **by
/// column name**, as in `nav.rs`: `SELECT *` follows the DDL's column order, which a migration
/// may change. `status` is TEXT and goes through [`PaymentState`]'s `FromStr`.
fn payment_row(row: &SqliteRow) -> ClResult<Payment> {
	Ok(Payment {
		id: row.try_get("id").db()?,
		uid: PaymentId::from_trusted(row.try_get("uid").db()?),
		tenant_id: row.try_get("tenant_id").db()?,
		kind: row.try_get("kind").db()?,
		provider: row.try_get("provider").db()?,
		provider_ref: row.try_get("provider_ref").db()?,
		redirect_url: row.try_get("redirect_url").db()?,
		request_id: row.try_get("request_id").db()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,
		amount: read_money(row.try_get("amount").db()?)?,
		currency: CurrencyCode::from_trusted(row.try_get("currency").db()?),
		refunded_amount: read_money(row.try_get("refunded_amount").db()?)?,
		received_at: row.try_get::<Option<i64>, _>("received_at").db()?.map(Timestamp),
		ext_ref: row.try_get("ext_ref").db()?,
		note: row.try_get("note").db()?,
		created_by: row.try_get("created_by").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		updated_at: Timestamp(row.try_get("updated_at").db()?),
	})
}

fn allocation_row(row: &SqliteRow) -> ClResult<PaymentAllocation> {
	Ok(PaymentAllocation {
		payment_id: row.try_get("payment_id").db()?,
		invoice_id: row.try_get("invoice_id").db()?,
		invoice_uid: InvoiceId::from_trusted(row.try_get("invoice_uid").db()?),
		invoice_number: row.try_get("invoice_number").db()?,
		amount: read_money(row.try_get("amount").db()?)?,
		allocated_at: Timestamp(row.try_get("allocated_at").db()?),
		allocated_by: row.try_get("allocated_by").db()?,
	})
}

fn overdue_row(row: &SqliteRow) -> ClResult<OverdueInvoice> {
	Ok(OverdueInvoice {
		invoice_id: row.try_get("invoice_id").db()?,
		invoice_uid: InvoiceId::from_trusted(row.try_get("invoice_uid").db()?),
		tenant_id: row.try_get("tenant_id").db()?,
		number: row.try_get("number").db()?,
		due_date: row.try_get("due_date").db()?,
		days_overdue: row.try_get("days_overdue").db()?,
		outstanding: read_money(row.try_get("outstanding").db()?)?,
		currency: CurrencyCode::from_trusted(row.try_get("currency").db()?),
		buyer_name: row.try_get("buyer_name").db()?,
		buyer_country: row.try_get("buyer_country").db()?,
		buyer_email: row.try_get("buyer_email").db()?,
	})
}

/// `?,?,…` for a guard's `status IN (…)`. The list is a caller-supplied slice of enum
/// variants, never user text, so it is length that varies and nothing else.
fn placeholders(n: usize) -> String {
	vec!["?"; n].join(",")
}

#[async_trait]
impl BillingStore for SqliteStore {
	async fn create_payment(&self, new: &NewPayment) -> ClResult<Payment> {
		let uid = PaymentId::generate();
		let now = Timestamp::now();
		let mut tx = self.write_tx().await?;
		let id: i64 = sqlx::query_scalar(
			"INSERT INTO payments
			 (uid, tenant_id, kind, provider, provider_ref, request_id, status, amount,
			  currency, ext_ref, note, created_by, created_at, updated_at)
			 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)
			 RETURNING id",
		)
		.bind(uid.as_str())
		.bind(new.tenant_id)
		.bind(&new.kind)
		.bind(new.provider.as_deref())
		.bind(new.provider_ref.as_deref())
		.bind(new.request_id.as_deref())
		.bind(new.status.as_str())
		.bind(new.amount.0)
		.bind(new.currency.as_str())
		.bind(new.ext_ref.as_deref())
		.bind(new.note.as_deref())
		.bind(new.created_by)
		.bind(now.0)
		.bind(now.0)
		.fetch_one(&mut *tx)
		.await
		.map_err(|err| unique_as_conflict(&err, "this payment request has already been made"))?;

		// `payments` has no invoice column, so this zero row is the only link from a payment
		// back to the invoice it was opened for — which is all a callback carrying a bare
		// `provider_ref` has to follow. Zero, so it moves no sum; `settle`'s upsert adds to it.
		if let Some(invoice_id) = new.invoice_id {
			sqlx::query(
				"INSERT INTO payment_allocations
				 (payment_id, invoice_id, amount, allocated_at, allocated_by)
				 VALUES (?, ?, 0, ?, ?)
				 ON CONFLICT(payment_id, invoice_id) DO NOTHING",
			)
			.bind(id)
			.bind(invoice_id)
			.bind(now.0)
			.bind(new.created_by)
			.execute(&mut *tx)
			.await
			.db()?;
		}
		tx.commit().await.db()?;

		// Built here rather than read back: the INSERT already fixes every column, and the
		// round trip would be a second statement on the single writer connection.
		Ok(Payment {
			id,
			uid,
			tenant_id: new.tenant_id,
			kind: new.kind.clone(),
			provider: new.provider.clone(),
			provider_ref: new.provider_ref.clone(),
			redirect_url: None,
			request_id: new.request_id.clone(),
			status: new.status,
			amount: new.amount,
			currency: new.currency.clone(),
			refunded_amount: Money::ZERO,
			received_at: None,
			ext_ref: new.ext_ref.clone(),
			note: new.note.clone(),
			created_by: new.created_by,
			created_at: now,
			updated_at: now,
		})
	}

	async fn payment(&self, id: i64) -> ClResult<Option<Payment>> {
		sqlx::query("SELECT * FROM payments WHERE id = ?")
			.bind(id)
			.fetch_optional(self.reader())
			.await
			.one(payment_row)
	}

	async fn payment_by_uid(
		&self,
		tenant_id: Option<i64>,
		uid: &PaymentId,
	) -> ClResult<Option<Payment>> {
		sqlx::query("SELECT * FROM payments WHERE uid = ?1 AND (?2 IS NULL OR tenant_id = ?2)")
			.bind(uid.as_str())
			.bind(tenant_id)
			.fetch_optional(self.reader())
			.await
			.one(payment_row)
	}

	async fn payment_by_provider_ref(
		&self,
		provider: &str,
		provider_ref: &str,
	) -> ClResult<Option<Payment>> {
		sqlx::query("SELECT * FROM payments WHERE provider = ? AND provider_ref = ?")
			.bind(provider)
			.bind(provider_ref)
			.fetch_optional(self.reader())
			.await
			.one(payment_row)
	}

	async fn payment_by_request_id(
		&self,
		tenant_id: i64,
		request_id: &str,
	) -> ClResult<Option<Payment>> {
		sqlx::query("SELECT * FROM payments WHERE request_id = ? AND tenant_id = ?")
			.bind(request_id)
			.bind(tenant_id)
			.fetch_optional(self.reader())
			.await
			.one(payment_row)
	}

	async fn set_started(
		&self,
		id: i64,
		provider_ref: &str,
		redirect_url: Option<&str>,
	) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE payments SET provider_ref = ?, redirect_url = ?, updated_at = ?
			  WHERE id = ? AND provider_ref IS NULL",
		)
		.bind(provider_ref)
		.bind(redirect_url)
		.bind(Timestamp::now().0)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn advance_status(
		&self,
		id: i64,
		to: PaymentState,
		from: &[PaymentState],
	) -> ClResult<bool> {
		// An empty list renders `status IN ()`, which only SQLite accepts; refused here so the
		// guard cannot become an unguarded `UPDATE` on a second adapter.
		if from.is_empty() {
			return Ok(false);
		}
		let sql = format!(
			"UPDATE payments SET status = ?, updated_at = ?
			  WHERE id = ? AND status IN ({})",
			placeholders(from.len())
		);
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(to.as_str())
			.bind(Timestamp::now().0)
			.bind(id);
		for state in from {
			q = q.bind(state.as_str());
		}
		Ok(q.execute(self.writer()).await.db()?.rows_affected() == 1)
	}

	async fn settle(&self, s: &Settlement) -> ClResult<bool> {
		if s.from.is_empty() {
			return Ok(false);
		}
		let now = Timestamp::now();
		let mut tx = self.write_tx().await?;

		let sql = format!(
			"UPDATE payments SET status = ?, received_at = COALESCE(received_at, ?),
			        updated_at = ?
			  WHERE id = ? AND status IN ({})",
			placeholders(s.from.len())
		);
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(s.to.as_str())
			.bind(s.at.0)
			.bind(now.0)
			.bind(s.payment_id);
		for state in &s.from {
			q = q.bind(state.as_str());
		}
		if q.execute(&mut *tx).await.db()?.rows_affected() != 1 {
			return Ok(false);
		}

		// Inside the transaction, never in the caller: `write_tx` is `BEGIN IMMEDIATE` on the one
		// writer connection, so this is serialised against every other writer. Read on a reader
		// connection and checked in Rust, two concurrent allocations of the whole payment both
		// saw zero allocated and both committed.
		if let Some(ceiling) = s.ceiling {
			let allocated: i64 = sqlx::query_scalar(
				"SELECT COALESCE(SUM(amount), 0) FROM payment_allocations WHERE payment_id = ?",
			)
			.bind(s.payment_id)
			.fetch_one(&mut *tx)
			.await
			.db()?;
			// Dropping `tx` rolls the status move back with it.
			if allocated + s.amount.0 > ceiling.0 {
				return Ok(false);
			}
		}

		// `PRIMARY KEY (payment_id, invoice_id)` admits one row per pair, so a reversal adds
		// its negative to the row already there instead of becoming a second one.
		sqlx::query(
			"INSERT INTO payment_allocations
			 (payment_id, invoice_id, amount, allocated_at, allocated_by)
			 VALUES (?, ?, ?, ?, ?)
			 ON CONFLICT(payment_id, invoice_id) DO UPDATE SET
			   amount = amount + excluded.amount,
			   allocated_at = excluded.allocated_at,
			   allocated_by = excluded.allocated_by",
		)
		.bind(s.payment_id)
		.bind(s.invoice_id)
		.bind(s.amount.0)
		.bind(s.at.0)
		.bind(s.allocated_by)
		.execute(&mut *tx)
		.await
		.db()?;

		// `InvoiceStore::set_paid` writes these same columns under this same predicate, but on
		// its own connection — a store method cannot be handed a transaction, so the allocation
		// and the cache it feeds could not have committed together. The duplication buys that
		// atomicity. Both `paid_at` and `status` are derived from the new sum, so a reversal walks
		// the invoice back to `ISSUED` as it clears `paid_at`; this is the only writer of `PAID`
		// on the gateway path.
		let res = sqlx::query(
			"UPDATE invoices SET
			     paid_amount = (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
			                     WHERE invoice_id = invoices.id),
			     paid_at = CASE WHEN (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
			                           WHERE invoice_id = invoices.id) >= gross
			                    THEN COALESCE(paid_at, ?) END,
			     status = CASE WHEN (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
			                          WHERE invoice_id = invoices.id) >= gross
			                   THEN 'PAID' ELSE 'ISSUED' END,
			     updated_at = ?
			  WHERE id = ? AND status IN ('ISSUED','PAID')",
		)
		.bind(s.at.0)
		.bind(now.0)
		.bind(s.invoice_id)
		.execute(&mut *tx)
		.await
		.db()?;
		// Dropping `tx` uncommitted rolls the payment row and the allocation back with it: an
		// allocation against a draft or a cancelled invoice must land nowhere at all.
		if res.rows_affected() != 1 {
			return Ok(false);
		}

		tx.commit().await.db()?;
		Ok(true)
	}

	async fn list_payments(
		&self,
		tenant_id: i64,
		filter: &PaymentFilter<'_>,
	) -> ClResult<Vec<Payment>> {
		// One statement for every combination: each filter is `(?n IS NULL OR …)`, so the first
		// page and the next ride `idx_payment_tenant(tenant_id, id DESC)` either way. The cursor
		// resolves in the `SELECT`: an unknown or another tenant's uid yields NULL and the page
		// comes back empty, never an error that would tell the caller which uids exist.
		sqlx::query(
			"SELECT p.* FROM payments p
			  WHERE p.tenant_id = ?1
			    AND (?2 IS NULL OR p.id < (SELECT id FROM payments WHERE uid = ?2))
			    AND (?4 IS NULL OR p.status = ?4)
			    AND (?5 IS NULL OR p.kind = ?5)
			    AND (?6 IS NULL OR p.provider = ?6)
			    AND (?7 IS NULL OR EXISTS (SELECT 1 FROM payment_allocations a
			                                 JOIN invoices i ON i.id = a.invoice_id
			                                WHERE a.payment_id = p.id AND i.uid = ?7))
			    AND (?8 IS NULL OR COALESCE(p.received_at, p.updated_at) >= ?8)
			    AND (?9 IS NULL OR COALESCE(p.received_at, p.updated_at) <= ?9)
			  ORDER BY p.id DESC LIMIT ?3",
		)
		.bind(tenant_id)
		.bind(filter.before.map(PaymentId::as_str))
		.bind(filter.limit)
		.bind(filter.status.map(PaymentState::as_str))
		.bind(filter.kind)
		.bind(filter.provider)
		.bind(filter.invoice_uid.map(InvoiceId::as_str))
		.bind(filter.received_from.map(|t| t.0))
		.bind(filter.received_to.map(|t| t.0))
		.fetch_all(self.reader())
		.await
		.all(payment_row)
	}

	async fn allocations(&self, payment_id: i64) -> ClResult<Vec<PaymentAllocation>> {
		sqlx::query(
			"SELECT a.*, i.uid AS invoice_uid, i.number AS invoice_number
			   FROM payment_allocations a
			   JOIN invoices i ON i.id = a.invoice_id
			  WHERE a.payment_id = ? ORDER BY a.invoice_id",
		)
		.bind(payment_id)
		.fetch_all(self.reader())
		.await
		.all(allocation_row)
	}

	async fn allocations_for(&self, payment_ids: &[i64]) -> ClResult<Vec<PaymentAllocation>> {
		// `IN ()` is the empty-list trap `advance_status` guards against, one statement over.
		if payment_ids.is_empty() {
			return Ok(Vec::new());
		}
		let sql = format!(
			"SELECT a.*, i.uid AS invoice_uid, i.number AS invoice_number
			   FROM payment_allocations a
			   JOIN invoices i ON i.id = a.invoice_id
			  WHERE a.payment_id IN ({})
			  ORDER BY a.payment_id, a.invoice_id",
			placeholders(payment_ids.len())
		);
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
		for id in payment_ids {
			q = q.bind(*id);
		}
		q.fetch_all(self.reader()).await.all(allocation_row)
	}

	async fn payments_by_invoice(&self, tenant_id: i64, invoice_id: i64) -> ClResult<Vec<Payment>> {
		// Joined through the link row rather than filtered on a payments column: there is no
		// such column, and the zero row `create_payment` writes is what a still-unsettled
		// payment is reachable by at all.
		sqlx::query(
			"SELECT p.* FROM payments p
			   JOIN payment_allocations a ON a.payment_id = p.id
			  WHERE p.tenant_id = ? AND a.invoice_id = ?
			  ORDER BY p.id DESC",
		)
		.bind(tenant_id)
		.bind(invoice_id)
		.fetch_all(self.reader())
		.await
		.all(payment_row)
	}

	async fn tenant_id_by_uid(&self, uid: &TenantId) -> ClResult<Option<i64>> {
		sqlx::query_scalar("SELECT id FROM tenants WHERE uid = ?")
			.bind(uid.as_str())
			.fetch_optional(self.reader())
			.await
			.db()
	}

	async fn record_refund(&self, r: &RefundRecord) -> ClResult<bool> {
		if r.from.is_empty() {
			return Ok(false);
		}
		let now = Timestamp::now();
		let mut tx = self.write_tx().await?;

		// The ceiling is in the `WHERE`, not left to the table's `CHECK`: an over-refund has to
		// come back as `false` the service turns into `E-PAY-AMOUNT`, not as a driver error.
		// `refunded_amount = ?` makes the whole call a compare-and-set: the caller derived the
		// gateway's idempotency key from that figure, so a second refund reading the same one
		// would record a payout the gateway deduped into a single one.
		let sql = format!(
			"UPDATE payments SET refunded_amount = refunded_amount + ?, status = ?,
			        updated_at = ?
			  WHERE id = ? AND refunded_amount = ? AND refunded_amount + ? <= amount
			    AND status IN ({})",
			placeholders(r.from.len())
		);
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(r.amount.0)
			.bind(r.to.as_str())
			.bind(now.0)
			.bind(r.payment_id)
			.bind(r.expect_refunded.0)
			.bind(r.amount.0);
		for state in &r.from {
			q = q.bind(state.as_str());
		}
		if q.execute(&mut *tx).await.db()?.rows_affected() != 1 {
			return Ok(false);
		}

		if let Some(invoice_id) = r.invoice_id {
			// Negative on the same `(payment_id, invoice_id)` pair, so the reversal lands on the
			// allocation row rather than becoming a second one.
			sqlx::query(
				"INSERT INTO payment_allocations
				 (payment_id, invoice_id, amount, allocated_at, allocated_by)
				 VALUES (?, ?, ?, ?, ?)
				 ON CONFLICT(payment_id, invoice_id) DO UPDATE SET
				   amount = amount + excluded.amount,
				   allocated_at = excluded.allocated_at,
				   allocated_by = excluded.allocated_by",
			)
			.bind(r.payment_id)
			.bind(invoice_id)
			.bind(-r.reverse.0)
			.bind(r.at.0)
			.bind(r.by)
			.execute(&mut *tx)
			.await
			.db()?;

			// The recompute `settle` does, for the same reason — but *not* its rollback on zero
			// rows: the gateway has already given the money back, so a refund against an invoice
			// that has since been stornoed must still be recorded on the payment.
			sqlx::query(
				"UPDATE invoices SET
				     paid_amount = (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
				                     WHERE invoice_id = invoices.id),
				     paid_at = CASE WHEN (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
				                           WHERE invoice_id = invoices.id) >= gross
				                    THEN paid_at END,
				     status = CASE WHEN (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
				                          WHERE invoice_id = invoices.id) >= gross
				                   THEN 'PAID' ELSE 'ISSUED' END,
				     updated_at = ?
				  WHERE id = ? AND status IN ('ISSUED','PAID')",
			)
			.bind(now.0)
			.bind(invoice_id)
			.execute(&mut *tx)
			.await
			.db()?;
		}

		tx.commit().await.db()?;
		Ok(true)
	}

	async fn overdue_invoices(
		&self,
		tenant_id: Option<i64>,
		after_invoice_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<OverdueInvoice>> {
		// `date('now')` rather than a date from the caller: `due_date` is TEXT and
		// `days_overdue` has to be computed here anyway, so both sides come from one clock.
		// `PAID` is excluded by `paid_amount < gross` as well as by status — a partially paid
		// invoice is still `ISSUED` and still overdue for the remainder.
		sqlx::query(
			"SELECT i.id AS invoice_id, i.uid AS invoice_uid, i.tenant_id, i.number, i.due_date,
			        i.currency, i.buyer_name, i.buyer_country,
			        CAST(julianday(date('now')) - julianday(i.due_date) AS INTEGER) AS days_overdue,
			        i.gross - i.paid_amount AS outstanding,
			        b.email AS buyer_email
			   FROM invoices i
			   LEFT JOIN billing_parties b ON b.id = i.billing_party_id
			  WHERE i.status = 'ISSUED'
			    AND i.due_date IS NOT NULL AND i.due_date < date('now')
			    AND i.paid_amount < i.gross
			    AND (?1 IS NULL OR i.tenant_id = ?1)
			    AND (?3 IS NULL OR (i.due_date, i.id)
			                       > (SELECT due_date, id FROM invoices WHERE id = ?3))
			  ORDER BY i.due_date ASC, i.id ASC
			  LIMIT ?2",
		)
		.bind(tenant_id)
		.bind(limit)
		.bind(after_invoice_id)
		.fetch_all(self.reader())
		.await
		.all(overdue_row)
	}

	async fn unallocated_payments(
		&self,
		received_before: Timestamp,
	) -> ClResult<(i64, Option<Timestamp>)> {
		// The correlated subquery `settle`'s recompute uses, read the other way round: a payment
		// whose allocations do not cover it. The zero link row sums to zero, so a payment that
		// only ever got one counts, and refunds are subtracted — a partial refund lowers the sum
		// while the status stays SUCCEEDED. `PARTIALLY_SUCCEEDED` too: `allocate::apply_state`
		// allocates nothing for it by design. `COALESCE(received_at, updated_at)` because only
		// `settle` stamps `received_at`, and the row this alert is *for* never reached one.
		let row = sqlx::query(
			"SELECT COUNT(*) AS n, MIN(COALESCE(received_at, updated_at)) AS oldest FROM payments p
			  WHERE p.status IN ('SUCCEEDED','PARTIALLY_SUCCEEDED')
			    AND COALESCE(p.received_at, p.updated_at) < ?1
			    AND (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
			          WHERE payment_id = p.id) < p.amount - p.refunded_amount",
		)
		.bind(received_before.0)
		.fetch_one(self.reader())
		.await
		.db()?;
		Ok((row.try_get("n").db()?, row.try_get::<Option<i64>, _>("oldest").db()?.map(Timestamp)))
	}

	async fn refund_discrepancies(&self) -> ClResult<(i64, Option<Timestamp>)> {
		let row = sqlx::query(
			// Only the ones still outstanding: a later `PAYMENT_REFUND` for the same payment is
			// the retried route succeeding, and without the `NOT EXISTS` the ERROR alert stayed
			// up for the life of the audit log — an alarm nobody can clear is one nobody reads.
			"SELECT COUNT(*) AS n, MIN(u.at) AS oldest FROM audit_logs u
			  WHERE u.action = 'PAYMENT_REFUND_UNRECORDED'
			    AND NOT EXISTS (SELECT 1 FROM audit_logs r
			                     WHERE r.action = 'PAYMENT_REFUND'
			                       AND r.entity_id = u.entity_id
			                       AND r.at > u.at)",
		)
		.fetch_one(self.reader())
		.await
		.db()?;
		Ok((row.try_get("n").db()?, row.try_get::<Option<i64>, _>("oldest").db()?.map(Timestamp)))
	}

	async fn live_payments(
		&self,
		updated_before: Timestamp,
		canceled_before: Timestamp,
		created_after: Timestamp,
		after_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Payment>> {
		// ponytail: an unindexed scan of `payments`. A partial index on the live statuses is the
		// upgrade once the table outgrows one sweep's batch.
		//
		// `CANCELED` is in the list: `allocate::abandon` writes it locally and tells the gateway
		// nothing, so a payment the gateway went on to capture is only found by re-asking.
		// `created_at > ?2` and `updated_at < ?1` are what stop that re-asking forever, and
		// `?5` is the slower clock a cancelled row gets so it cannot crowd out a live one.
		sqlx::query(
			"SELECT * FROM payments
			  WHERE provider_ref IS NOT NULL
			    AND status IN ('PENDING','AWAITING_USER','RESERVED','AUTHORIZED','CANCELED')
			    AND updated_at < ?1 AND created_at > ?2
			    AND (status <> 'CANCELED' OR updated_at < ?5)
			    AND (?4 IS NULL OR id > ?4)
			  ORDER BY id LIMIT ?3",
		)
		.bind(updated_before.0)
		.bind(created_after.0)
		.bind(limit)
		.bind(after_id)
		.bind(canceled_before.0)
		.fetch_all(self.reader())
		.await
		.all(payment_row)
	}
}

// vim: ts=4
