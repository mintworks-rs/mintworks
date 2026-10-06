//! `BillingStore` over PostgreSQL — the SQLite adapter's `billing.rs` in PG dialect.
//!
//! Every status write carries its own `status = ANY(…)` guard, so a replayed gateway callback
//! updates nothing and answers `false` rather than settling the same money twice.

use async_trait::async_trait;
use saas_billing::provider::PaymentState;
use saas_billing::store::{
	BillingStore, NewPayment, OverdueInvoice, Payment, PaymentAllocation, PaymentFilter,
	RefundRecord, Settlement,
};
use saas_core::prelude::*;
use sqlx::{Row, postgres::PgRow};

use crate::{
	PgStore,
	util::{DbExt, RowExt, RowsExt, read_money, unique_as_conflict},
};

/// Read by column name: `SELECT *` follows the DDL's column order.
fn payment_row(row: &PgRow) -> ClResult<Payment> {
	Ok(Payment {
		id: row.try_get("id").db()?,
		uid: PaymentId::from_trusted(row.try_get("uid").db()?),
		org_id: row.try_get("org_id").db()?,
		kind: row.try_get("kind").db()?,
		provider: row.try_get("provider").db()?,
		provider_ref: row.try_get("provider_ref").db()?,
		redirect_url: row.try_get("redirect_url").db()?,
		expires_at: row.try_get::<Option<i64>, _>("expires_at").db()?.map(Timestamp),
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

fn allocation_row(row: &PgRow) -> ClResult<PaymentAllocation> {
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

fn overdue_row(row: &PgRow) -> ClResult<OverdueInvoice> {
	Ok(OverdueInvoice {
		invoice_id: row.try_get("invoice_id").db()?,
		invoice_uid: InvoiceId::from_trusted(row.try_get("invoice_uid").db()?),
		org_id: row.try_get("org_id").db()?,
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

/// A guard's state list, bound as one `TEXT[]` for `status = ANY($n)`.
fn states(from: &[PaymentState]) -> Vec<&'static str> {
	from.iter().map(|s| s.as_str()).collect()
}

/// The allocation upsert `settle` and `record_refund` share. `payment_allocations.amount` is
/// qualified: a bare `amount` in `DO UPDATE SET` is ambiguous with `excluded.amount` in PG.
const UPSERT_ALLOCATION: &str = "INSERT INTO payment_allocations
	 (payment_id, invoice_id, amount, allocated_at, allocated_by)
	 VALUES ($1, $2, $3, $4, $5)
	 ON CONFLICT (payment_id, invoice_id) DO UPDATE SET
	   amount = payment_allocations.amount + excluded.amount,
	   allocated_at = excluded.allocated_at,
	   allocated_by = excluded.allocated_by";

#[async_trait]
impl BillingStore for PgStore {
	async fn create_payment(&self, new: &NewPayment) -> ClResult<Payment> {
		let uid = PaymentId::generate();
		let now = Timestamp::now();
		let tx = self.write_tx().await?;
		let id: i64 = sqlx::query_scalar(
			"INSERT INTO payments
			 (uid, org_id, kind, provider, provider_ref, request_id, status, amount,
			  currency, ext_ref, note, created_by, created_at, updated_at)
			 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
			 RETURNING id",
		)
		.bind(uid.as_str())
		.bind(new.org_id)
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
		.fetch_one(&mut *tx.lock().await?)
		.await
		.map_err(|err| unique_as_conflict(&err, "this payment request has already been made"))?;

		// `payments` has no invoice column, so this zero row is the only link from a payment
		// back to the invoice it was opened for. Zero, so it moves no sum.
		if let Some(invoice_id) = new.invoice_id {
			sqlx::query(
				"INSERT INTO payment_allocations
				 (payment_id, invoice_id, amount, allocated_at, allocated_by)
				 VALUES ($1, $2, 0, $3, $4)
				 ON CONFLICT (payment_id, invoice_id) DO NOTHING",
			)
			.bind(id)
			.bind(invoice_id)
			.bind(now.0)
			.bind(new.created_by)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}
		tx.commit().await?;

		Ok(Payment {
			id,
			uid,
			org_id: new.org_id,
			kind: new.kind.clone(),
			provider: new.provider.clone(),
			provider_ref: new.provider_ref.clone(),
			redirect_url: None,
			expires_at: None,
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
		sqlx::query("SELECT * FROM payments WHERE id = $1")
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(payment_row)
	}

	async fn payment_by_uid(
		&self,
		org_id: Option<i64>,
		uid: &PaymentId,
	) -> ClResult<Option<Payment>> {
		sqlx::query("SELECT * FROM payments WHERE uid = $1 AND ($2::BIGINT IS NULL OR org_id = $2)")
			.bind(uid.as_str())
			.bind(org_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(payment_row)
	}

	async fn payment_by_provider_ref(
		&self,
		provider: &str,
		provider_ref: &str,
	) -> ClResult<Option<Payment>> {
		sqlx::query("SELECT * FROM payments WHERE provider = $1 AND provider_ref = $2")
			.bind(provider)
			.bind(provider_ref)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(payment_row)
	}

	async fn payment_by_request_id(
		&self,
		org_id: i64,
		request_id: &str,
	) -> ClResult<Option<Payment>> {
		sqlx::query("SELECT * FROM payments WHERE request_id = $1 AND org_id = $2")
			.bind(request_id)
			.bind(org_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(payment_row)
	}

	async fn set_started(
		&self,
		id: i64,
		provider_ref: &str,
		redirect_url: Option<&str>,
		expires_at: Option<Timestamp>,
	) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE payments SET provider_ref = $1, redirect_url = $2, expires_at = $3,
			        updated_at = $4
			  WHERE id = $5 AND provider_ref IS NULL",
		)
		.bind(provider_ref)
		.bind(redirect_url)
		.bind(expires_at.map(|t| t.0))
		.bind(Timestamp::now().0)
		.bind(id)
		.execute(&mut *self.conn().await?)
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
		// An empty guard is refused rather than sent: it must never mean "any status".
		if from.is_empty() {
			return Ok(false);
		}
		let res = sqlx::query(
			"UPDATE payments SET status = $1, updated_at = $2
			  WHERE id = $3 AND status = ANY($4)",
		)
		.bind(to.as_str())
		.bind(Timestamp::now().0)
		.bind(id)
		.bind(states(from))
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn settle(&self, s: &Settlement) -> ClResult<bool> {
		if s.from.is_empty() {
			return Ok(false);
		}
		let now = Timestamp::now();
		let tx = self.write_tx().await?;

		let res = sqlx::query(
			"UPDATE payments SET status = $1, received_at = COALESCE(received_at, $2),
			        updated_at = $3
			  WHERE id = $4 AND status = ANY($5)",
		)
		.bind(s.to.as_str())
		.bind(s.at.0)
		.bind(now.0)
		.bind(s.payment_id)
		.bind(states(&s.from))
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		if res.rows_affected() != 1 {
			return Ok(false);
		}

		// Inside the transaction, never in the caller: the advisory lock `write_tx` holds
		// serialises this against every other writer, so two concurrent allocations of the whole
		// payment cannot both see zero allocated.
		if let Some(ceiling) = s.ceiling {
			let allocated: i64 = sqlx::query_scalar(
				"SELECT COALESCE(SUM(amount), 0)::BIGINT FROM payment_allocations
				  WHERE payment_id = $1",
			)
			.bind(s.payment_id)
			.fetch_one(&mut *tx.lock().await?)
			.await
			.db()?;
			// Dropping `tx` rolls the status move back with it.
			if allocated + s.amount.0 > ceiling.0 {
				return Ok(false);
			}
		}

		sqlx::query(UPSERT_ALLOCATION)
			.bind(s.payment_id)
			.bind(s.invoice_id)
			.bind(s.amount.0)
			.bind(s.at.0)
			.bind(s.allocated_by)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;

		// Duplicates `InvoiceStore::set_paid` so the allocation and the cache it feeds commit
		// together; this is the only writer of `PAID` on the gateway path.
		let res = sqlx::query(
			"UPDATE invoices SET
			     paid_amount = (SELECT COALESCE(SUM(amount), 0)::BIGINT FROM payment_allocations
			                     WHERE invoice_id = invoices.id),
			     paid_at = CASE WHEN (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
			                           WHERE invoice_id = invoices.id) >= gross
			                    THEN COALESCE(paid_at, $1) END,
			     status = CASE WHEN (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
			                          WHERE invoice_id = invoices.id) >= gross
			                   THEN 'PAID' ELSE 'ISSUED' END,
			     updated_at = $2
			  WHERE id = $3 AND status IN ('ISSUED','PAID')",
		)
		.bind(s.at.0)
		.bind(now.0)
		.bind(s.invoice_id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		// Dropping `tx` uncommitted rolls the payment row and the allocation back with it: an
		// allocation against a draft or a cancelled invoice must land nowhere at all.
		if res.rows_affected() != 1 {
			return Ok(false);
		}

		tx.commit().await?;
		Ok(true)
	}

	async fn list_payments(
		&self,
		org_id: i64,
		filter: &PaymentFilter<'_>,
	) -> ClResult<Vec<Payment>> {
		// An unknown or another org's cursor uid yields NULL and an empty page, never an error
		// that would tell the caller which uids exist.
		sqlx::query(
			"SELECT p.* FROM payments p
			  WHERE p.org_id = $1
			    AND ($2::TEXT IS NULL OR p.id < (SELECT id FROM payments WHERE uid = $2))
			    AND ($4::TEXT IS NULL OR p.status = $4)
			    AND ($5::TEXT IS NULL OR p.kind = $5)
			    AND ($6::TEXT IS NULL OR p.provider = $6)
			    AND ($7::TEXT IS NULL OR EXISTS (SELECT 1 FROM payment_allocations a
			                                       JOIN invoices i ON i.id = a.invoice_id
			                                      WHERE a.payment_id = p.id AND i.uid = $7))
			    AND ($8::BIGINT IS NULL OR COALESCE(p.received_at, p.updated_at) >= $8)
			    AND ($9::BIGINT IS NULL OR COALESCE(p.received_at, p.updated_at) <= $9)
			  ORDER BY p.id DESC LIMIT $3",
		)
		.bind(org_id)
		.bind(filter.before.map(PaymentId::as_str))
		.bind(filter.limit)
		.bind(filter.status.map(PaymentState::as_str))
		.bind(filter.kind)
		.bind(filter.provider)
		.bind(filter.invoice_uid.map(InvoiceId::as_str))
		.bind(filter.received_from.map(|t| t.0))
		.bind(filter.received_to.map(|t| t.0))
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(payment_row)
	}

	async fn allocations(&self, payment_id: i64) -> ClResult<Vec<PaymentAllocation>> {
		sqlx::query(
			"SELECT a.*, i.uid AS invoice_uid, i.number AS invoice_number
			   FROM payment_allocations a
			   JOIN invoices i ON i.id = a.invoice_id
			  WHERE a.payment_id = $1 ORDER BY a.invoice_id",
		)
		.bind(payment_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(allocation_row)
	}

	async fn allocations_for(&self, payment_ids: &[i64]) -> ClResult<Vec<PaymentAllocation>> {
		if payment_ids.is_empty() {
			return Ok(Vec::new());
		}
		sqlx::query(
			"SELECT a.*, i.uid AS invoice_uid, i.number AS invoice_number
			   FROM payment_allocations a
			   JOIN invoices i ON i.id = a.invoice_id
			  WHERE a.payment_id = ANY($1)
			  ORDER BY a.payment_id, a.invoice_id",
		)
		.bind(payment_ids)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(allocation_row)
	}

	async fn payments_by_invoice(&self, org_id: i64, invoice_id: i64) -> ClResult<Vec<Payment>> {
		// Through the link row: the zero row `create_payment` writes is what a still-unsettled
		// payment is reachable by at all.
		sqlx::query(
			"SELECT p.* FROM payments p
			   JOIN payment_allocations a ON a.payment_id = p.id
			  WHERE p.org_id = $1 AND a.invoice_id = $2
			  ORDER BY p.id DESC",
		)
		.bind(org_id)
		.bind(invoice_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(payment_row)
	}

	async fn org_id_by_uid(&self, uid: &OrgId) -> ClResult<Option<i64>> {
		sqlx::query_scalar("SELECT id FROM orgs WHERE uid = $1")
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn record_refund(&self, r: &RefundRecord) -> ClResult<bool> {
		if r.from.is_empty() {
			return Ok(false);
		}
		let now = Timestamp::now();
		let tx = self.write_tx().await?;

		// The ceiling is in the `WHERE` so an over-refund is `false`, not a `CHECK` violation;
		// `refunded_amount = $5` makes it a compare-and-set on the figure the gateway's
		// idempotency key was derived from.
		let res = sqlx::query(
			"UPDATE payments SET refunded_amount = refunded_amount + $1, status = $2,
			        updated_at = $3
			  WHERE id = $4 AND refunded_amount = $5 AND refunded_amount + $1 <= amount
			    AND status = ANY($6)",
		)
		.bind(r.amount.0)
		.bind(r.to.as_str())
		.bind(now.0)
		.bind(r.payment_id)
		.bind(r.expect_refunded.0)
		.bind(states(&r.from))
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		if res.rows_affected() != 1 {
			return Ok(false);
		}

		if let Some(invoice_id) = r.invoice_id {
			sqlx::query(UPSERT_ALLOCATION)
				.bind(r.payment_id)
				.bind(invoice_id)
				.bind(-r.reverse.0)
				.bind(r.at.0)
				.bind(r.by)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;

			// `settle`'s recompute without its rollback on zero rows: the gateway has already
			// paid the money back, so a refund against a since-stornoed invoice is still recorded.
			sqlx::query(
				"UPDATE invoices SET
				     paid_amount = (SELECT COALESCE(SUM(amount), 0)::BIGINT FROM payment_allocations
				                     WHERE invoice_id = invoices.id),
				     paid_at = CASE WHEN (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
				                           WHERE invoice_id = invoices.id) >= gross
				                    THEN paid_at END,
				     status = CASE WHEN (SELECT COALESCE(SUM(amount), 0) FROM payment_allocations
				                          WHERE invoice_id = invoices.id) >= gross
				                   THEN 'PAID' ELSE 'ISSUED' END,
				     updated_at = $1
				  WHERE id = $2 AND status IN ('ISSUED','PAID')",
			)
			.bind(now.0)
			.bind(invoice_id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}

		tx.commit().await?;
		Ok(true)
	}

	async fn overdue_invoices(
		&self,
		org_id: Option<i64>,
		after_invoice_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<OverdueInvoice>> {
		// Today in UTC, as SQLite's `date('now')`: `CURRENT_DATE` follows the session time zone.
		// `due_date` is TEXT `YYYY-MM-DD`, so it compares as a string and casts to `date`.
		sqlx::query(
			"SELECT i.id AS invoice_id, i.uid AS invoice_uid, i.org_id, i.number, i.due_date,
			        i.currency, i.buyer_name, i.buyer_country,
			        ((now() AT TIME ZONE 'UTC')::date - i.due_date::date)::BIGINT AS days_overdue,
			        i.gross - i.paid_amount AS outstanding,
			        b.email AS buyer_email
			   FROM invoices i
			   LEFT JOIN billing_parties b ON b.id = i.billing_party_id
			  WHERE i.status = 'ISSUED'
			    AND i.due_date IS NOT NULL
			    AND i.due_date < to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD')
			    AND i.paid_amount < i.gross
			    AND ($1::BIGINT IS NULL OR i.org_id = $1)
			    AND ($3::BIGINT IS NULL OR (i.due_date, i.id)
			                               > (SELECT due_date, id FROM invoices WHERE id = $3))
			  ORDER BY i.due_date ASC, i.id ASC
			  LIMIT $2",
		)
		.bind(org_id)
		.bind(limit)
		.bind(after_invoice_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(overdue_row)
	}

	async fn unallocated_payments(
		&self,
		received_before: Timestamp,
	) -> ClResult<(i64, Option<Timestamp>)> {
		// A payment whose allocations, net of refunds, do not cover it. `COALESCE(received_at,
		// updated_at)` because only `settle` stamps `received_at`.
		let row = sqlx::query(
			"SELECT COUNT(*) AS n, MIN(COALESCE(received_at, updated_at)) AS oldest FROM payments p
			  WHERE p.status IN ('SUCCEEDED','PARTIALLY_SUCCEEDED')
			    AND COALESCE(p.received_at, p.updated_at) < $1
			    AND (SELECT COALESCE(SUM(amount), 0)::BIGINT FROM payment_allocations
			          WHERE payment_id = p.id) < p.amount - p.refunded_amount",
		)
		.bind(received_before.0)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()?;
		Ok((row.try_get("n").db()?, row.try_get::<Option<i64>, _>("oldest").db()?.map(Timestamp)))
	}

	async fn refund_discrepancies(&self) -> ClResult<(i64, Option<Timestamp>)> {
		// Only the ones still outstanding: a later `PAYMENT_REFUND` is the retry succeeding.
		let row = sqlx::query(
			"SELECT COUNT(*) AS n, MIN(u.at) AS oldest FROM audit_logs u
			  WHERE u.action = 'PAYMENT_REFUND_UNRECORDED'
			    AND NOT EXISTS (SELECT 1 FROM audit_logs r
			                     WHERE r.action = 'PAYMENT_REFUND'
			                       AND r.entity_id = u.entity_id
			                       AND r.at > u.at)",
		)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()?;
		Ok((row.try_get("n").db()?, row.try_get::<Option<i64>, _>("oldest").db()?.map(Timestamp)))
	}

	async fn live_payments(
		&self,
		updated_before: Timestamp,
		after_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Payment>> {
		// An unindexed scan of `payments`; a partial index on the live statuses once the table
		// outgrows one sweep's batch.
		sqlx::query(
			"SELECT * FROM payments
			  WHERE provider_ref IS NOT NULL
			    AND status IN ('PENDING','AWAITING_USER','RESERVED','AUTHORIZED')
			    AND updated_at < $1
			    AND ($3::BIGINT IS NULL OR id > $3)
			  ORDER BY id LIMIT $2",
		)
		.bind(updated_before.0)
		.bind(limit)
		.bind(after_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(payment_row)
	}
}

// vim: ts=4
