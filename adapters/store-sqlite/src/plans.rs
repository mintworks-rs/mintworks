//! `mintworks_plans::PlanStore` over SQLite.

use async_trait::async_trait;
use mintworks_core::ids::{OfferId, SubscriptionId};
use mintworks_core::prelude::*;
use mintworks_core::refs::RefStore;
use mintworks_plans::{
	LinkOutcome, NewOffer, NewPlanInvoice, Offer, OfferEntitlement, OfferPrice, PlanInvoice,
	PlanStore, SubStatus, Subscription,
};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

use crate::{
	SqliteStore,
	util::{DbExt, RowExt, RowsExt, read_money, unique_as_conflict},
};

fn offer_row(row: &SqliteRow) -> ClResult<Offer> {
	Ok(Offer {
		id: row.try_get("id").db()?,
		uid: OfferId::from_trusted(row.try_get::<String, _>("uid").db()?),
		seller_org_id: row.try_get("seller_org_id").db()?,
		code: row.try_get("code").db()?,
		name: row.try_get("name").db()?,
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		service_id: row.try_get("service_id").db()?,
		family: row.try_get("family").db()?,
		rank: row.try_get("rank").db()?,
		interval: row
			.try_get::<Option<String>, _>("interval")
			.db()?
			.map(|s| s.parse())
			.transpose()?,
		interval_count: row.try_get("interval_count").db()?,
		validity_days: row.try_get("validity_days").db()?,
		trial_days: row.try_get("trial_days").db()?,
		active: row.try_get("active").db()?,
		prices: Vec::new(),
		entitlements: Vec::new(),
		created_at: Timestamp(row.try_get("created_at").db()?),
		updated_at: Timestamp(row.try_get("updated_at").db()?),
	})
}

fn price_row(row: &SqliteRow) -> ClResult<(i64, OfferPrice)> {
	Ok((
		row.try_get("offer_id").db()?,
		OfferPrice {
			currency: CurrencyCode::from_trusted(row.try_get::<String, _>("currency").db()?),
			amount: read_money(row.try_get("amount").db()?)?,
		},
	))
}

fn entitlement_row(row: &SqliteRow) -> ClResult<(i64, OfferEntitlement)> {
	Ok((
		row.try_get("offer_id").db()?,
		OfferEntitlement {
			key: row.try_get("key").db()?,
			amount: row.try_get("amount").db()?,
			per_seat: row.try_get("per_seat").db()?,
		},
	))
}

fn sub_row(row: &SqliteRow) -> ClResult<Subscription> {
	Ok(Subscription {
		id: row.try_get("id").db()?,
		uid: SubscriptionId::from_trusted(row.try_get::<String, _>("uid").db()?),
		org_id: row.try_get("org_id").db()?,
		offer_id: row.try_get("offer_id").db()?,
		family: row.try_get("family").db()?,
		qty: row.try_get("qty").db()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,
		currency: CurrencyCode::from_trusted(row.try_get::<String, _>("currency").db()?),
		price: read_money(row.try_get("price").db()?)?,
		period_start: Timestamp(row.try_get("period_start").db()?),
		period_end: Timestamp(row.try_get("period_end").db()?),
		cancel_at_period_end: row.try_get("cancel_at_period_end").db()?,
		next_offer_id: row.try_get("next_offer_id").db()?,
		next_qty: row.try_get("next_qty").db()?,
		pay_method: row.try_get::<String, _>("pay_method").db()?.parse()?,
		provider: row.try_get("provider").db()?,
		recurrence_ref: row.try_get("recurrence_ref").db()?,
		coupon_ref_id: row.try_get("coupon_ref_id").db()?,
		coupon_periods_left: row.try_get("coupon_periods_left").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		updated_at: Timestamp(row.try_get("updated_at").db()?),
		billing_anchor: Timestamp(row.try_get("billing_anchor").db()?),
	})
}

const LINK: &str = "SELECT p.*, i.uid AS invoice_uid, i.org_id
	FROM plan_invoices p JOIN invoices i ON i.id = p.invoice_id";

fn link_row(row: &SqliteRow) -> ClResult<PlanInvoice> {
	Ok(PlanInvoice {
		invoice_id: row.try_get("invoice_id").db()?,
		invoice_uid: InvoiceId::from_trusted(row.try_get::<String, _>("invoice_uid").db()?),
		org_id: row.try_get("org_id").db()?,
		subscription_id: row.try_get("subscription_id").db()?,
		offer_id: row.try_get("offer_id").db()?,
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		qty: row.try_get("qty").db()?,
		period_start: row.try_get::<Option<i64>, _>("period_start").db()?.map(Timestamp),
		period_end: row.try_get::<Option<i64>, _>("period_end").db()?.map(Timestamp),
		coupon_ref_id: row.try_get("coupon_ref_id").db()?,
		prev: row
			.try_get::<Option<i64>, _>("prev_offer_id")
			.db()?
			.zip(row.try_get::<Option<i64>, _>("prev_qty").db()?),
	})
}

/// Fills in the prices and entitlements of `offers`.
async fn with_children(
	conn: &mut SqliteConnection,
	mut offers: Vec<Offer>,
) -> ClResult<Vec<Offer>> {
	let ids = serde_json::to_string(&offers.iter().map(|o| o.id).collect::<Vec<_>>())
		.map_err(|e| Error::internal(e.to_string()))?;
	let prices = sqlx::query(
		"SELECT * FROM offer_prices WHERE offer_id IN (SELECT value FROM json_each(?))
		 ORDER BY currency",
	)
	.bind(&ids)
	.fetch_all(&mut *conn)
	.await
	.all(price_row)?;
	let ents = sqlx::query(
		"SELECT * FROM offer_entitlements WHERE offer_id IN (SELECT value FROM json_each(?))
		 ORDER BY key",
	)
	.bind(&ids)
	.fetch_all(&mut *conn)
	.await
	.all(entitlement_row)?;
	for o in &mut offers {
		o.prices = prices.iter().filter(|(id, _)| *id == o.id).map(|(_, p)| p.clone()).collect();
		o.entitlements =
			ents.iter().filter(|(id, _)| *id == o.id).map(|(_, e)| e.clone()).collect();
	}
	Ok(offers)
}

const OFFER_ORDER: &str = "ORDER BY family IS NULL, family, rank, code";

const SUB_SELECT: &str = "SELECT * FROM subscriptions";

#[async_trait]
impl PlanStore for SqliteStore {
	async fn plan_service_id(&self, seller_org_id: i64, code: &str) -> ClResult<Option<i64>> {
		sqlx::query_scalar("SELECT id FROM services WHERE org_id = ? AND code = ?")
			.bind(seller_org_id)
			.bind(code)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn offer_upsert(&self, new: &NewOffer) -> ClResult<Offer> {
		let tx = self.write_tx().await?;
		let now = Timestamp::now().0;
		// `IS NOT`: NULL-safe, so a family or interval going to or from NULL counts as a change.
		let id: Option<i64> = sqlx::query_scalar(
			"INSERT INTO offers (uid, seller_org_id, code, name, kind, service_id, family, rank,
			 interval, interval_count, validity_days, trial_days, active, created_at, updated_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)
			 ON CONFLICT (seller_org_id, code) DO UPDATE SET
				name = excluded.name, kind = excluded.kind, service_id = excluded.service_id,
				family = excluded.family, rank = excluded.rank, interval = excluded.interval,
				interval_count = excluded.interval_count, validity_days = excluded.validity_days,
				trial_days = excluded.trial_days, active = 1, updated_at = excluded.updated_at
			 WHERE offers.name IS NOT excluded.name OR offers.kind IS NOT excluded.kind
				OR offers.service_id IS NOT excluded.service_id
				OR offers.family IS NOT excluded.family OR offers.rank IS NOT excluded.rank
				OR offers.interval IS NOT excluded.interval
				OR offers.interval_count IS NOT excluded.interval_count
				OR offers.validity_days IS NOT excluded.validity_days
				OR offers.trial_days IS NOT excluded.trial_days OR offers.active = 0
			 RETURNING id",
		)
		.bind(new.uid.as_str())
		.bind(new.seller_org_id)
		.bind(&new.code)
		.bind(&new.name)
		.bind(new.kind.as_str())
		.bind(new.service_id)
		.bind(&new.family)
		.bind(new.rank)
		.bind(new.interval.map(mintworks_plans::Interval::as_str))
		.bind(new.interval_count)
		.bind(new.validity_days)
		.bind(new.trial_days)
		.bind(now)
		.bind(now)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.db()?;
		// `None`: unchanged, so the `WHERE` skipped the update and `RETURNING` yielded nothing.
		let id = match id {
			Some(id) => id,
			None => {
				sqlx::query_scalar("SELECT id FROM offers WHERE seller_org_id = ? AND code = ?")
					.bind(new.seller_org_id)
					.bind(&new.code)
					.fetch_one(&mut *tx.lock().await?)
					.await
					.db()?
			}
		};
		for sql in [
			"DELETE FROM offer_prices WHERE offer_id = ?",
			"DELETE FROM offer_entitlements WHERE offer_id = ?",
		] {
			sqlx::query(sql).bind(id).execute(&mut *tx.lock().await?).await.db()?;
		}
		for p in &new.prices {
			sqlx::query("INSERT INTO offer_prices (offer_id, currency, amount) VALUES (?, ?, ?)")
				.bind(id)
				.bind(p.currency.as_str())
				.bind(p.amount.0)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}
		for e in &new.entitlements {
			sqlx::query(
				"INSERT INTO offer_entitlements (offer_id, key, amount, per_seat)
				 VALUES (?, ?, ?, ?)",
			)
			.bind(id)
			.bind(&e.key)
			.bind(e.amount)
			.bind(e.per_seat)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}
		tx.commit().await?;
		self.offer_get(id)
			.await?
			.ok_or_else(|| Error::internal("offer_upsert: row vanished"))
	}

	async fn offers_deactivate_except(&self, seller_org_id: i64, keep: &[String]) -> ClResult<u64> {
		let keep = serde_json::to_string(keep).map_err(|e| Error::internal(e.to_string()))?;
		let hit = sqlx::query(
			"UPDATE offers SET active = 0, updated_at = ?
			 WHERE seller_org_id = ? AND active = 1
			   AND code NOT IN (SELECT value FROM json_each(?))",
		)
		.bind(Timestamp::now().0)
		.bind(seller_org_id)
		.bind(keep)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(hit.rows_affected())
	}

	async fn offer_get(&self, id: i64) -> ClResult<Option<Offer>> {
		let mut conn = self.reader().await?;
		let rows = sqlx::query("SELECT * FROM offers WHERE id = ?")
			.bind(id)
			.fetch_all(&mut *conn)
			.await
			.all(offer_row)?;
		Ok(with_children(&mut conn, rows).await?.pop())
	}

	async fn offer_by_code(&self, seller_org_id: i64, code: &str) -> ClResult<Option<Offer>> {
		let mut conn = self.reader().await?;
		let rows = sqlx::query("SELECT * FROM offers WHERE seller_org_id = ? AND code = ?")
			.bind(seller_org_id)
			.bind(code)
			.fetch_all(&mut *conn)
			.await
			.all(offer_row)?;
		Ok(with_children(&mut conn, rows).await?.pop())
	}

	async fn offers_active(&self, seller_org_id: i64) -> ClResult<Vec<Offer>> {
		let mut conn = self.reader().await?;
		let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
			"SELECT * FROM offers WHERE seller_org_id = ? AND active = 1 {OFFER_ORDER}"
		)))
		.bind(seller_org_id)
		.fetch_all(&mut *conn)
		.await
		.all(offer_row)?;
		with_children(&mut conn, rows).await
	}

	async fn sub_insert(&self, s: &Subscription) -> ClResult<Subscription> {
		let now = Timestamp::now().0;
		sqlx::query(
			"INSERT INTO subscriptions (uid, org_id, offer_id, family, qty, status, currency,
			 price, period_start, period_end, cancel_at_period_end, next_offer_id, next_qty,
			 pay_method, provider, recurrence_ref, coupon_ref_id, coupon_periods_left,
			 created_at, updated_at, billing_anchor)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
			 RETURNING *",
		)
		.bind(s.uid.as_str())
		.bind(s.org_id)
		.bind(s.offer_id)
		.bind(&s.family)
		.bind(s.qty)
		.bind(s.status.as_str())
		.bind(s.currency.as_str())
		.bind(s.price.0)
		.bind(s.period_start.0)
		.bind(s.period_end.0)
		.bind(s.cancel_at_period_end)
		.bind(s.next_offer_id)
		.bind(s.next_qty)
		.bind(s.pay_method.as_str())
		.bind(&s.provider)
		.bind(&s.recurrence_ref)
		.bind(s.coupon_ref_id)
		.bind(s.coupon_periods_left)
		.bind(now)
		.bind(now)
		.bind(s.billing_anchor.0)
		.fetch_one(&mut *self.conn().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "a live subscription exists in this family"))
		.and_then(|row| sub_row(&row))
	}

	async fn sub_delete(&self, id: i64) -> ClResult<()> {
		sqlx::query("DELETE FROM subscriptions WHERE id = ?")
			.bind(id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(())
	}

	async fn sub_get(&self, id: i64) -> ClResult<Option<Subscription>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{SUB_SELECT} WHERE id = ?")))
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(sub_row)
	}

	async fn sub_by_uid(&self, uid: &SubscriptionId) -> ClResult<Option<Subscription>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{SUB_SELECT} WHERE uid = ?")))
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(sub_row)
	}

	async fn subs_of_org(&self, org_id: i64) -> ClResult<Vec<Subscription>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{SUB_SELECT} WHERE org_id = ? ORDER BY id DESC")))
			.bind(org_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(sub_row)
	}

	async fn sub_live_in_family(
		&self,
		org_id: i64,
		family: &str,
	) -> ClResult<Option<Subscription>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{SUB_SELECT} WHERE org_id = ? AND family = ? AND status <> 'CANCELED'"
		)))
		.bind(org_id)
		.bind(family)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(sub_row)
	}

	async fn sub_ever_in_family(&self, org_id: i64, family: &str) -> ClResult<bool> {
		sqlx::query_scalar(
			"SELECT EXISTS (SELECT 1 FROM subscriptions s JOIN orgs o ON o.id = s.org_id
			 WHERE s.family = ?2 AND (s.org_id = ?1 OR o.owner_account_id =
			   (SELECT owner_account_id FROM orgs WHERE id = ?1 AND owner_account_id IS NOT NULL)))",
		)
		.bind(org_id)
		.bind(family)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn subs_due(&self, now: Timestamp) -> ClResult<Vec<Subscription>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{SUB_SELECT} WHERE status IN ('TRIALING', 'ACTIVE', 'PAST_DUE') AND period_end <= ?
			 ORDER BY period_end, id"
		)))
		.bind(now.0)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(sub_row)
	}

	async fn subs_with_status(&self, statuses: &[SubStatus]) -> ClResult<Vec<Subscription>> {
		let list = serde_json::to_string(&statuses.iter().map(|s| s.as_str()).collect::<Vec<_>>())
			.map_err(|e| Error::internal(e.to_string()))?;
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{SUB_SELECT} WHERE status IN (SELECT value FROM json_each(?)) ORDER BY id"
		)))
		.bind(list)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(sub_row)
	}

	async fn sub_save(&self, s: &Subscription) -> ClResult<Option<Subscription>> {
		sqlx::query(
			"UPDATE subscriptions SET offer_id = ?, family = ?, qty = ?, status = ?,
				currency = ?, price = ?, period_start = ?, period_end = ?,
				cancel_at_period_end = ?, next_offer_id = ?, next_qty = ?, pay_method = ?,
				provider = ?, recurrence_ref = ?, coupon_ref_id = ?, coupon_periods_left = ?,
				billing_anchor = ?, updated_at = MAX(?, updated_at + 1)
			 WHERE id = ? AND updated_at = ?
			 RETURNING *",
		)
		.bind(s.offer_id)
		.bind(&s.family)
		.bind(s.qty)
		.bind(s.status.as_str())
		.bind(s.currency.as_str())
		.bind(s.price.0)
		.bind(s.period_start.0)
		.bind(s.period_end.0)
		.bind(s.cancel_at_period_end)
		.bind(s.next_offer_id)
		.bind(s.next_qty)
		.bind(s.pay_method.as_str())
		.bind(&s.provider)
		.bind(&s.recurrence_ref)
		.bind(s.coupon_ref_id)
		.bind(s.coupon_periods_left)
		.bind(s.billing_anchor.0)
		.bind(Timestamp::now().0)
		.bind(s.id)
		.bind(s.updated_at.0)
		.fetch_optional(&mut *self.conn().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "a live subscription exists in this family"))?
		.map(|row| sub_row(&row))
		.transpose()
	}

	async fn plan_invoice_insert(&self, n: &NewPlanInvoice) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO plan_invoices (invoice_id, subscription_id, offer_id, kind, qty,
			 period_start, period_end, coupon_ref_id, prev_offer_id, prev_qty)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
		)
		.bind(n.invoice_id)
		.bind(n.subscription_id)
		.bind(n.offer_id)
		.bind(n.kind.as_str())
		.bind(n.qty)
		.bind(n.period_start.map(|t| t.0))
		.bind(n.period_end.map(|t| t.0))
		.bind(n.coupon_ref_id)
		.bind(n.prev.map(|p| p.0))
		.bind(n.prev.map(|p| p.1))
		.execute(&mut *self.conn().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "the invoice is already linked"))?;
		Ok(())
	}

	async fn plan_invoice_link(
		&self,
		n: &NewPlanInvoice,
		redeem: Option<(i64, i64, i64)>,
	) -> ClResult<LinkOutcome> {
		let by_id = format!("{LINK} WHERE p.invoice_id = ?");
		let (tx, bound) = self.begin().await?;
		match bound.plan_invoice_insert(n).await {
			Ok(()) => {}
			Err(Error::Conflict(_)) => {
				drop(tx);
				let row = sqlx::query(sqlx::AssertSqlSafe(by_id))
					.bind(n.invoice_id)
					.fetch_optional(&mut *self.reader().await?)
					.await
					.one(link_row)?
					.ok_or_else(|| Error::internal("plan_invoice_link: linked row vanished"))?;
				return Ok(LinkOutcome::Exists(row));
			}
			Err(e) => return Err(e),
		}
		if let Some((ref_id, account_id, org_id)) = redeem {
			let used = bound
				.ref_redeem(ref_id, account_id, org_id, Some(n.invoice_id), Timestamp::now())
				.await?;
			if !matches!(used, Some((_, true))) {
				return Ok(LinkOutcome::CouponInvalid);
			}
		}
		let row = sqlx::query(sqlx::AssertSqlSafe(by_id))
			.bind(n.invoice_id)
			.fetch_one(&mut *bound.conn().await?)
			.await
			.db()
			.and_then(|r| link_row(&r))?;
		tx.commit().await?;
		Ok(LinkOutcome::Linked(row))
	}

	async fn plan_invoices_paid_since(
		&self,
		since: Timestamp,
	) -> ClResult<Vec<(InvoiceId, PaymentId)>> {
		let rows: Vec<(String, String)> = sqlx::query_as(
			"SELECT i.uid, pay.uid FROM plan_invoices p
			 JOIN invoices i ON i.id = p.invoice_id
			 JOIN payment_allocations a ON a.invoice_id = i.id
			 JOIN payments pay ON pay.id = a.payment_id
			 WHERE i.status = 'PAID' AND i.paid_at >= ? AND pay.status = 'SUCCEEDED'
			 ORDER BY i.id, pay.id",
		)
		.bind(since.0)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(rows
			.into_iter()
			.map(|(i, p)| (InvoiceId::from_trusted(i), PaymentId::from_trusted(p)))
			.collect())
	}

	async fn plan_invoice_get(&self, invoice: &InvoiceId) -> ClResult<Option<PlanInvoice>> {
		sqlx::query(sqlx::AssertSqlSafe(format!("{LINK} WHERE i.uid = ?")))
			.bind(invoice.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(link_row)
	}

	async fn plan_invoice_latest(&self, subscription_id: i64) -> ClResult<Option<PlanInvoice>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{LINK} WHERE p.subscription_id = ? AND p.kind <> 'UPGRADE'
			 ORDER BY p.period_start DESC, p.invoice_id DESC LIMIT 1"
		)))
		.bind(subscription_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(link_row)
	}

	async fn plan_invoice_oldest_unpaid(
		&self,
		subscription_id: i64,
	) -> ClResult<Option<PlanInvoice>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{LINK} WHERE p.subscription_id = ? AND i.paid_amount < i.gross AND (i.status = 'ISSUED'
			   OR (i.status IN ('DRAFT','PENDING') AND p.kind <> 'UPGRADE'))
			 ORDER BY p.period_start, p.invoice_id LIMIT 1"
		)))
		.bind(subscription_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(link_row)
	}

	async fn plan_invoice_upgrades(
		&self,
		subscription_id: i64,
		from: Timestamp,
	) -> ClResult<Vec<PlanInvoice>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{LINK} WHERE p.subscription_id = ? AND p.kind = 'UPGRADE' AND p.period_start >= ?
			 ORDER BY p.invoice_id"
		)))
		.bind(subscription_id)
		.bind(from.0)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(link_row)
	}

	async fn plan_invoice_set_period(
		&self,
		invoice_id: i64,
		start: Timestamp,
		end: Timestamp,
	) -> ClResult<()> {
		sqlx::query(
			"UPDATE plan_invoices SET period_start = ?, period_end = ? WHERE invoice_id = ?",
		)
		.bind(start.0)
		.bind(end.0)
		.bind(invoice_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn offer_reprice(
		&self,
		offer_id: i64,
		currency: &CurrencyCode,
		amount: Money,
	) -> ClResult<Vec<Subscription>> {
		let tx = self.write_tx().await?;
		sqlx::query(
			"INSERT INTO offer_prices (offer_id, currency, amount) VALUES (?, ?, ?)
			 ON CONFLICT (offer_id, currency) DO UPDATE SET amount = excluded.amount",
		)
		.bind(offer_id)
		.bind(currency.as_str())
		.bind(amount.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		let subs = sqlx::query(
			"UPDATE subscriptions SET price = ?, updated_at = MAX(?, updated_at + 1)
			 WHERE offer_id = ? AND currency = ? AND status <> 'CANCELED' AND price <> ?
			 RETURNING *",
		)
		.bind(amount.0)
		.bind(Timestamp::now().0)
		.bind(offer_id)
		.bind(currency.as_str())
		.bind(amount.0)
		.fetch_all(&mut *tx.lock().await?)
		.await
		.all(sub_row)?;
		tx.commit().await?;
		Ok(subs)
	}

	async fn plan_org_id(&self, uid: &OrgId) -> ClResult<Option<i64>> {
		sqlx::query_scalar("SELECT id FROM orgs WHERE uid = ?")
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn plan_org_uid(&self, id: i64) -> ClResult<Option<OrgId>> {
		let uid: Option<String> = sqlx::query_scalar("SELECT uid FROM orgs WHERE id = ?")
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()?;
		Ok(uid.map(OrgId::from_trusted))
	}
}

// vim: ts=4
