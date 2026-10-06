//! The example's own store trait, implemented for `SqliteStore`. Legal under the orphan rule
//! because `BookingStore` is declared here — the consumer table shares the framework's
//! database file and its real transactions.
//!
//! Driver errors go through the adapter's own `util::DbExt::db()`, so a lock held past
//! `BUSY_TIMEOUT` on the shared writer connection is a retryable `E-CORE-UNAVAILABLE` here too;
//! this is the whole of what `adapters/store-sqlite/tests/consumer_extension.rs` proves,
//! running in an application instead of a test.

use async_trait::async_trait;
use mintworks_core::prelude::*;
use mintworks_store_sqlite::util::{DbExt, unique_as_conflict};
use mintworks_store_sqlite::{Fut, Module, SqliteStore};

/// This application's own schema, versioned independently of the framework's: the framework may
/// bump `saas` without touching this row, and vice versa.
pub const EXAMPLE: Module = Module { name: "example", version: 2, apply: apply_example };

fn apply_example(conn: &mut sqlx::SqliteConnection, from: i64) -> Fut<'_> {
	Box::pin(async move { if from == 0 { create(conn).await } else { upgrade(conn, from).await } })
}

/// `tenant_id` -> `org_id`, table rebuild rather than `ALTER TABLE … RENAME COLUMN`: a rename
/// keeps the constraint pointing at `tenants`, which the framework's version 8 drops, and the
/// migration then failed its `PRAGMA foreign_key_check` with every `bookings` row dangling.
async fn upgrade(conn: &mut sqlx::SqliteConnection, from: i64) -> ClResult<()> {
	if from < 2 {
		// A database created from the already-edited `create` above is at version 1 with the
		// new shape, so the column is what decides, not the recorded version.
		let old: i64 = sqlx::query_scalar(
			"SELECT COUNT(*) FROM pragma_table_info('bookings') WHERE name = 'tenant_id'",
		)
		.fetch_one(&mut *conn)
		.await
		.db()?;
		if old > 0 {
			sqlx::raw_sql(
				r"
CREATE TABLE bookings_new (
	id           INTEGER PRIMARY KEY,
	uid          TEXT    NOT NULL UNIQUE,
	org_id       INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
	service_code TEXT    NOT NULL,
	occurred_on  TEXT    NOT NULL,
	qty_e6       INTEGER NOT NULL,
	note         TEXT,
	invoice_uid  TEXT,
	created_at   INTEGER NOT NULL
);
INSERT INTO bookings_new (id, uid, org_id, service_code, occurred_on, qty_e6, note, invoice_uid, created_at)
  SELECT id, uid, tenant_id, service_code, occurred_on, qty_e6, note, invoice_uid, created_at FROM bookings;
DROP TABLE bookings;
ALTER TABLE bookings_new RENAME TO bookings;
CREATE INDEX idx_bookings_org ON bookings (org_id, occurred_on, id);
CREATE INDEX idx_bookings_invoice ON bookings (invoice_uid);
",
			)
			.execute(&mut *conn)
			.await
			.db()?;
		}
	}
	Ok(())
}

/// The example's one consumer table: a dated, billable session. It lives in the framework's
/// database file, under the framework's write lock, created in the framework's own migration
/// transaction — which is why `org_id` may reference `orgs(id)`.
async fn create(conn: &mut sqlx::SqliteConnection) -> ClResult<()> {
	sqlx::raw_sql(
		r"
CREATE TABLE bookings (
	id           INTEGER PRIMARY KEY,
	uid          TEXT    NOT NULL UNIQUE,
	org_id       INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
	service_code TEXT    NOT NULL,
	occurred_on  TEXT    NOT NULL,
	qty_e6       INTEGER NOT NULL,
	note         TEXT,
	invoice_uid  TEXT,
	created_at   INTEGER NOT NULL
);

-- The listing's full sort key, and the seek `claim_unbilled`'s substr() probe narrows to one
-- org with. Covers billed rows too, which the old partial index excluded — the ledger shows
-- what was charged, not only what is unbilled.
CREATE INDEX idx_bookings_org ON bookings (org_id, occurred_on, id);

-- `by_checkout`, `settle` and `release` all key on the claim, and the last two hold the single
-- writer connection while they do it.
CREATE INDEX idx_bookings_invoice ON bookings (invoice_uid);
",
	)
	.execute(conn)
	.await
	.db()?;
	Ok(())
}

/// A `bookings` row. `id` and `org_id` are internal keys: only `uid` goes on the wire.
#[derive(Clone, Debug, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Booking {
	#[serde(skip)]
	#[allow(dead_code)]
	pub id: i64,
	pub uid: String,
	#[serde(skip)]
	#[allow(dead_code)]
	pub org_id: i64,
	pub service_code: String,
	pub occurred_on: String,
	/// Scaled 1e6 like `Qty`: 2.5 hours is `2_500_000`. Never a float, on the wire or off it.
	pub qty_e6: i64,
	pub note: Option<String>,
	#[serde(serialize_with = "public_invoice_uid")]
	pub invoice_uid: Option<String>,
}

/// Only a real invoice uid goes on the wire. Between `claim_unbilled` and `settle` this column
/// holds a `chk_<ULID>` checkout claim — an internal idempotency token, not a public id — and the
/// SPA links `invoiceUid` straight to `/invoices/{uid}`.
#[allow(clippy::ref_option)] // serde hands a `serialize_with` fn the field by reference.
fn public_invoice_uid<S: serde::Serializer>(v: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
	serde::Serialize::serialize(&v.as_deref().filter(|u| u.starts_with("inv_")), s)
}

/// A booking before it has an id or a billing state.
#[derive(Clone, Debug)]
pub struct NewBooking {
	pub uid: String,
	pub org_id: i64,
	pub service_code: String,
	pub occurred_on: String,
	pub qty_e6: i64,
	pub note: Option<String>,
}

#[async_trait]
pub trait BookingStore: Send + Sync + 'static {
	async fn create(&self, new: &NewBooking) -> ClResult<Booking>;
	/// One page, newest first. `cursor` is the last uid of the previous page.
	async fn list_for_org(
		&self,
		org_id: i64,
		cursor: Option<&str>,
		limit: i64,
	) -> ClResult<Vec<Booking>>;
	/// Stamps the org's unbilled bookings with a fresh `chk_<ULID>` and returns it, or resumes
	/// the open claim if one is already stamped; `None` means there was nothing to bill. Capped at
	/// `mintworks_invoice::draft::MAX_LINES`, so an org over the cap simply checks out more than
	/// once.
	///
	/// The claim is what makes the checkout idempotent: it is taken *before* `Invoices::draft`,
	/// so a crash before [`settle`](BookingStore::settle) re-drafts the same set under the same
	/// `request_id` instead of a superset. It also replaces an "unbilled" query — `invoice_uid`
	/// is no longer NULL once claimed, so a claimed booking is out of the next checkout's set.
	async fn claim_unbilled(&self, org_id: i64) -> ClResult<Option<String>>;
	/// `org_id` on all three, as on `claim_unbilled`: these are the writes that move money,
	/// and another org's claim must read as absent rather than as someone else's rows.
	async fn by_checkout(&self, org_id: i64, claim: &str) -> ClResult<Vec<Booking>>;
	/// The checkout's last write, after `Invoices::draft` committed its own transaction: the
	/// claim becomes the invoice's uid. `false` means the claim was no longer on any row — a
	/// racing `release` or a second settle — so the invoice has no bookings pointing at it.
	async fn settle(&self, org_id: i64, claim: &str, invoice_uid: &str) -> ClResult<bool>;
	/// Undoes a claim. The claim commits before the draft, so a draft nothing can ever accept
	/// would otherwise be re-read identically by every later checkout.
	async fn release(&self, org_id: i64, claim: &str) -> ClResult<()>;
	/// How many bookings a lost [`settle`](BookingStore::settle) orphaned: still stamped with a
	/// `chk_` claim that an `invoices.request_id` already names, so the invoice exists and the
	/// bookings are unbilled again. Across every org — `crate::bookings::alerts` is an
	/// operator read, not a request. See `A-BOOKING-ORPHANED`.
	async fn orphaned_claims(&self) -> ClResult<i64>;
}

#[async_trait]
impl BookingStore for SqliteStore {
	async fn create(&self, new: &NewBooking) -> ClResult<Booking> {
		let tx = self.write_tx().await?;
		let row = sqlx::query_as(
			"INSERT INTO bookings
			   (uid, org_id, service_code, occurred_on, qty_e6, note, created_at)
			 VALUES (?, ?, ?, ?, ?, ?, strftime('%s', 'now'))
			 RETURNING id, uid, org_id, service_code, occurred_on, qty_e6, note, invoice_uid",
		)
		.bind(&new.uid)
		.bind(new.org_id)
		.bind(&new.service_code)
		.bind(&new.occurred_on)
		.bind(new.qty_e6)
		.bind(&new.note)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.map_err(|err| unique_as_conflict(&err, "booking uid already exists"))?;
		tx.commit().await?;
		Ok(row)
	}

	async fn list_for_org(
		&self,
		org_id: i64,
		cursor: Option<&str>,
		limit: i64,
	) -> ClResult<Vec<Booking>> {
		// Resolved here rather than as a subquery: an unresolved uid made the row-value
		// comparison NULL, so a stale cursor returned an empty page the SPA read as the end of
		// the list.
		let after: Option<(String, i64)> = match cursor {
			Some(uid) => Some(
				sqlx::query_as("SELECT occurred_on, id FROM bookings WHERE uid = ? AND org_id = ?")
					.bind(uid)
					.bind(org_id)
					.fetch_optional(&mut *self.reader().await?)
					.await
					.db()?
					.ok_or(Error::NotFound)?,
			),
			None => None,
		};

		// Row values, because the order is a pair: a plain `id <` cursor skips rows whenever two
		// bookings share a date. SQLite has supported them since 3.15.
		sqlx::query_as(
			"SELECT id, uid, org_id, service_code, occurred_on, qty_e6, note, invoice_uid
			   FROM bookings
			  WHERE org_id = ?1
			    AND (?2 IS NULL OR (occurred_on, id) < (?2, ?3))
			  ORDER BY occurred_on DESC, id DESC LIMIT ?4",
		)
		.bind(org_id)
		.bind(after.as_ref().map(|a| a.0.as_str()))
		.bind(after.as_ref().map_or(0, |a| a.1))
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()
	}

	// A claim whose draft the customer then deletes leaves its bookings pointing at a dead uid
	// and out of the unbilled set; the recovery is to book them again.
	async fn claim_unbilled(&self, org_id: i64) -> ClResult<Option<String>> {
		let tx = self.write_tx().await?;
		// `substr`, not `LIKE 'chk_%'`: `_` is a LIKE wildcard, so that pattern also matches an
		// `inv_…` uid and would resume a claim that is already an invoice.
		let open: Option<String> = sqlx::query_scalar(
			"SELECT DISTINCT invoice_uid FROM bookings
			  WHERE org_id = ? AND substr(invoice_uid, 1, 4) = 'chk_' LIMIT 1",
		)
		.bind(org_id)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.db()?;
		if open.is_some() {
			tx.commit().await?;
			return Ok(open);
		}

		// Bounded by the framework's own per-invoice line cap: an unbounded claim handed
		// `Invoices::draft` every booking an org ever made, in one transaction on the single
		// writer connection. What is left over is simply the next checkout's set.
		let claim = format!("chk_{}", ulid::Ulid::new());
		let done = sqlx::query(
			"UPDATE bookings SET invoice_uid = ?
			  WHERE id IN (SELECT id FROM bookings
			                WHERE org_id = ? AND invoice_uid IS NULL
			                ORDER BY occurred_on, id LIMIT ?)",
		)
		.bind(&claim)
		.bind(org_id)
		.bind(i64::try_from(mintworks_invoice::draft::MAX_LINES).unwrap_or(i64::MAX))
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		tx.commit().await?;
		Ok((done.rows_affected() > 0).then_some(claim))
	}

	async fn by_checkout(&self, org_id: i64, claim: &str) -> ClResult<Vec<Booking>> {
		// Oldest first, so a retried checkout draws the lines in the same order.
		sqlx::query_as(
			"SELECT id, uid, org_id, service_code, occurred_on, qty_e6, note, invoice_uid
			   FROM bookings WHERE org_id = ? AND invoice_uid = ? ORDER BY occurred_on, id",
		)
		.bind(org_id)
		.bind(claim)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn settle(&self, org_id: i64, claim: &str, invoice_uid: &str) -> ClResult<bool> {
		let done =
			sqlx::query("UPDATE bookings SET invoice_uid = ? WHERE org_id = ? AND invoice_uid = ?")
				.bind(invoice_uid)
				.bind(org_id)
				.bind(claim)
				.execute(&mut *self.conn().await?)
				.await
				.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn release(&self, org_id: i64, claim: &str) -> ClResult<()> {
		sqlx::query("UPDATE bookings SET invoice_uid = NULL WHERE org_id = ? AND invoice_uid = ?")
			.bind(org_id)
			.bind(claim)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(())
	}

	async fn orphaned_claims(&self) -> ClResult<i64> {
		// Two shapes, both invisible to `claim_unbilled`, which only picks up `invoice_uid IS
		// NULL`. `substr`, not `LIKE 'chk_%'`, for the reason `claim_unbilled` gives: `_` is a
		// LIKE wildcard.
		//
		// The second is `Bookings::discard`'s crash window: `delete_draft` and `release` cannot
		// be one transaction, so a crash between them leaves bookings stamped `inv_…` with no
		// such invoice — never billable again, and not a lost settle, so the join above misses
		// them entirely.
		sqlx::query_scalar(
			"SELECT
			   (SELECT count(*) FROM bookings b
			      JOIN invoices i ON i.request_id = b.invoice_uid
			     WHERE substr(b.invoice_uid, 1, 4) = 'chk_')
			 + (SELECT count(*) FROM bookings b
			      LEFT JOIN invoices i ON i.uid = b.invoice_uid
			     WHERE b.invoice_uid IS NOT NULL
			       AND substr(b.invoice_uid, 1, 4) = 'inv_'
			       AND i.id IS NULL)",
		)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}
}

// vim: ts=4
