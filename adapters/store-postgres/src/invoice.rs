//! `InvoiceStore` over PostgreSQL — the SQLite adapter's `invoice.rs`, split in two because one
//! `impl` block cannot span files: this file holds the impl itself plus sellers, services,
//! parties, the post-issue status writes, documents and currencies; the draft, issue, storno,
//! list and summary bodies are one-line delegations into `crate::invoice_doc`.
//!
//! **Every mutating statement either carries `AND status = 'DRAFT'` or is one of the three
//! writes an issued invoice still permits** (`mark_paid`, `mark_stornoed`, `set_paid`).
//!
//! The schema's flags are `BIGINT` 0/1, not `BOOLEAN`, so a `bool` is bound as `i64::from(b)`
//! and read back as `!= 0`.

use async_trait::async_trait;
use mintworks_core::error::StatusCode;
use mintworks_core::ids::SellerId;
use mintworks_core::prelude::*;
use mintworks_invoice::currency::{Currency, RateMode};
use mintworks_invoice::draft::Priced;
use mintworks_invoice::mnb;
use mintworks_invoice::store::{
	BillingParty, Invoice, InvoiceDocument, InvoiceFilter, InvoiceLine, InvoicePatch,
	InvoiceStatus, InvoiceStore, InvoiceSummary, InvoiceVatGroup, IssueInvoice, ListedInvoice,
	NewInvoice, PartyPatch, RevenueMonth, Seller, SellerVersion, SellerVersionPatch,
	SellerVersionStatus, Service, ServiceDef, ServicePatch,
};
use mintworks_invoice::vies::ViesResult;
use sqlx::{PgConnection, Row, postgres::PgRow};

use crate::PgStore;
use crate::invoice_doc;
use crate::util::{
	DbExt, RowExt, RowsExt, read_discount_value, read_money, read_qty, unique_as_conflict,
};

/// The seven `currencies` columns both currency queries select, in order.
type CurrencyRow = (String, i64, Option<i64>, String, Option<i64>, i64, i64);

/// The `vies_checks` columns `vies_cached` selects, in order.
type CachedRow = (i64, Option<String>, Option<String>, Option<String>, i64);

/// The ext-blob object types `delete_party`/`delete_draft` sweep — the SQLite adapter's
/// `objects::ext_type("party")`/`("invoice")`; a script writing one spells the same name.
pub(crate) const PARTY_EXT: &str = "party.ext";
pub(crate) const INVOICE_EXT: &str = "invoice.ext";

fn currency_of(row: CurrencyRow) -> Currency {
	let (code, price_round_step, cash_round_step, mode, fixed_rate_e6, fee_bp, enabled) = row;
	Currency {
		code: CurrencyCode::from_trusted(code),
		price_round_step,
		cash_round_step,
		mode: if mode == "FIXED" { RateMode::Fixed } else { RateMode::Official },
		fixed_rate_e6,
		fee_bp,
		enabled: enabled != 0,
	}
}

fn flag(row: &PgRow, col: &str) -> ClResult<bool> {
	Ok(row.try_get::<i64, _>(col).db()? != 0)
}

// Row mapping. Every read is **by column name**: `SELECT *` follows the DDL's column order.

fn seller_row(row: &PgRow) -> ClResult<Seller> {
	Ok(Seller {
		id: row.try_get("id").db()?,
		uid: SellerId::from_trusted(row.try_get::<String, _>("uid").db()?),
		org_id: row.try_get("org_id").db()?,
		nav_base_url: row.try_get("nav_base_url").db()?,
		nav_login: row.try_get("nav_login").db()?,
		series_code: row.try_get("series_code").db()?,
		closed_at: row.try_get::<Option<i64>, _>("closed_at").db()?.map(Timestamp),
		payment_days: row.try_get("payment_days").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn seller_version_row(row: &PgRow) -> ClResult<SellerVersion> {
	Ok(SellerVersion {
		seller_ver: row.try_get("seller_ver").db()?,
		seller_id: row.try_get("seller_id").db()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,
		name: row.try_get("name").db()?,
		country: row.try_get("country").db()?,
		tax_number: row.try_get("tax_number").db()?,
		group_member_tax_no: row.try_get("group_member_tax_no").db()?,
		eu_vat_id: row.try_get("eu_vat_id").db()?,
		postcode: row.try_get("postcode").db()?,
		city: row.try_get("city").db()?,
		street: row.try_get("street").db()?,
		bank_account: row.try_get("bank_account").db()?,
		bank_name: row.try_get("bank_name").db()?,
		small_business: flag(row, "small_business")?,
		vat_scheme: row.try_get("vat_scheme").db()?,
		income_regime: row.try_get("income_regime").db()?,
		expense_ratio_pct: row.try_get("expense_ratio_pct").db()?,
		regime_since: row.try_get("regime_since").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		valid_from: row.try_get::<Option<i64>, _>("valid_from").db()?.map(Timestamp),
		superseded_at: row.try_get::<Option<i64>, _>("superseded_at").db()?.map(Timestamp),
	})
}

/// The one `DRAFT` or `CURRENT` row for a seller — both are unique by a partial index.
async fn version_by_status(
	conn: &mut PgConnection,
	seller_id: i64,
	status: &str,
) -> ClResult<Option<SellerVersion>> {
	sqlx::query("SELECT * FROM seller_versions WHERE seller_id = $1 AND status = $2")
		.bind(seller_id)
		.bind(status)
		.fetch_optional(conn)
		.await
		.one(seller_version_row)
}

fn party_row(row: &PgRow) -> ClResult<BillingParty> {
	Ok(BillingParty {
		id: row.try_get("id").db()?,
		uid: PartyId::from_trusted(row.try_get::<String, _>("uid").db()?),
		org_id: row.try_get("org_id").db()?,
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		name: row.try_get("name").db()?,
		country: row.try_get("country").db()?,
		tax_number: row.try_get("tax_number").db()?,
		eu_vat_id: row.try_get("eu_vat_id").db()?,
		group_tax_no: row.try_get("group_tax_no").db()?,
		postcode: row.try_get("postcode").db()?,
		city: row.try_get("city").db()?,
		street: row.try_get("street").db()?,
		email: row.try_get("email").db()?,
		is_default: flag(row, "is_default")?,
		payment_days: row.try_get("payment_days").db()?,
		payment_method: row
			.try_get::<Option<String>, _>("payment_method")
			.db()?
			.map(|m| m.parse())
			.transpose()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		updated_at: Timestamp(row.try_get("updated_at").db()?),
	})
}

fn service_row(row: &PgRow) -> ClResult<Service> {
	Ok(Service {
		id: row.try_get("id").db()?,
		uid: ServiceId::from_trusted(row.try_get::<String, _>("uid").db()?),
		org_id: row.try_get("org_id").db()?,
		code: row.try_get("code").db()?,
		name: row.try_get("name").db()?,
		description: row.try_get("description").db()?,
		unit: row.try_get("unit").db()?,
		unit_price: read_money(row.try_get("unit_price").db()?)?,
		vat_code: row.try_get::<String, _>("vat_code").db()?.parse()?,
		active: flag(row, "active")?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		updated_at: Timestamp(row.try_get("updated_at").db()?),
	})
}

/// [`invoice_row`] plus the three joined uid columns of `list_invoices_page`.
pub(crate) fn listed_invoice_row(row: &PgRow) -> ClResult<ListedInvoice> {
	let uid = |col| -> ClResult<Option<InvoiceId>> {
		Ok(row.try_get::<Option<String>, _>(col).db()?.map(InvoiceId::from_trusted))
	};
	Ok(ListedInvoice {
		invoice: invoice_row(row)?,
		party_uid: row.try_get::<Option<String>, _>("party_uid").db()?.map(PartyId::from_trusted),
		original_invoice_uid: uid("original_uid")?,
		storno_invoice_uid: uid("storno_uid")?,
	})
}

pub(crate) fn invoice_row(row: &PgRow) -> ClResult<Invoice> {
	Ok(Invoice {
		id: row.try_get("id").db()?,
		uid: InvoiceId::from_trusted(row.try_get::<String, _>("uid").db()?),
		request_id: row.try_get("request_id").db()?,
		org_id: row.try_get("org_id").db()?,
		seller_id: row.try_get("seller_id").db()?,
		seller_ver: row.try_get("seller_ver").db()?,
		billing_party_id: row.try_get("billing_party_id").db()?,
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,

		series_code: row.try_get("series_code").db()?,
		series_year: row.try_get("series_year").db()?,
		number: row.try_get("number").db()?,
		issued_at: row.try_get::<Option<i64>, _>("issued_at").db()?.map(Timestamp),
		fulfilment_date: row.try_get("fulfilment_date").db()?,
		due_date: row.try_get("due_date").db()?,
		period_start: row.try_get("period_start").db()?,
		period_end: row.try_get("period_end").db()?,
		payment_method: row.try_get::<String, _>("payment_method").db()?.parse()?,

		original_invoice_id: row.try_get("original_invoice_id").db()?,
		modification_index: row.try_get("modification_index").db()?,

		currency: CurrencyCode::from_trusted(row.try_get("currency").db()?),
		rate_e6: row.try_get("rate_e6").db()?,
		rate_date: row.try_get("rate_date").db()?,
		rate_source: row
			.try_get::<Option<String>, _>("rate_source")
			.db()?
			.map(|s| s.parse())
			.transpose()?,
		huf_rate_e6: row.try_get("huf_rate_e6").db()?,

		net: read_money(row.try_get("net").db()?)?,
		vat: read_money(row.try_get("vat").db()?)?,
		gross: read_money(row.try_get("gross").db()?)?,
		paid_amount: read_money(row.try_get("paid_amount").db()?)?,
		paid_at: row.try_get::<Option<i64>, _>("paid_at").db()?.map(Timestamp),

		vat_note: row.try_get("vat_note").db()?,
		notes: row.try_get("notes").db()?,

		discount_kind: row
			.try_get::<Option<String>, _>("discount_kind")
			.db()?
			.map(|s| s.parse())
			.transpose()?,
		discount_value: read_discount_value(row.try_get("discount_value").db()?)?,

		buyer_kind: row
			.try_get::<Option<String>, _>("buyer_kind")
			.db()?
			.map(|s| s.parse())
			.transpose()?,
		buyer_name: row.try_get("buyer_name").db()?,
		buyer_country: row.try_get("buyer_country").db()?,
		buyer_tax_number: row.try_get("buyer_tax_number").db()?,
		buyer_eu_vat_id: row.try_get("buyer_eu_vat_id").db()?,
		buyer_group_tax_no: row.try_get("buyer_group_tax_no").db()?,
		buyer_postcode: row.try_get("buyer_postcode").db()?,
		buyer_city: row.try_get("buyer_city").db()?,
		buyer_street: row.try_get("buyer_street").db()?,
		buyer_vies_request_id: row.try_get("buyer_vies_request_id").db()?,
		buyer_vies_checked_at: row
			.try_get::<Option<i64>, _>("buyer_vies_checked_at")
			.db()?
			.map(Timestamp),

		created_at: Timestamp(row.try_get("created_at").db()?),
		updated_at: Timestamp(row.try_get("updated_at").db()?),
		version: row.try_get("version").db()?,
	})
}

pub(crate) fn line_row(row: &PgRow) -> ClResult<InvoiceLine> {
	Ok(InvoiceLine {
		id: row.try_get("id").db()?,
		invoice_id: row.try_get("invoice_id").db()?,
		line_no: row.try_get("line_no").db()?,
		service_id: row.try_get("service_id").db()?,
		description: row.try_get("description").db()?,
		unit: row.try_get("unit").db()?,
		qty: read_qty(row.try_get("qty").db()?)?,
		unit_price: read_money(row.try_get("unit_price").db()?)?,
		discount_kind: row
			.try_get::<Option<String>, _>("discount_kind")
			.db()?
			.map(|s| s.parse())
			.transpose()?,
		discount_value: read_discount_value(row.try_get("discount_value").db()?)?,
		discount_amount: read_money(row.try_get("discount_amount").db()?)?,
		discount_description: row.try_get("discount_description").db()?,
		net: read_money(row.try_get("net").db()?)?,
		vat_code: row.try_get::<String, _>("vat_code").db()?.parse()?,
		vat_rate_bp: row.try_get("vat_rate_bp").db()?,
		vat: read_money(row.try_get("vat").db()?)?,
		gross: read_money(row.try_get("gross").db()?)?,
		note: row.try_get("note").db()?,
	})
}

pub(crate) fn vat_group_row(row: &PgRow) -> ClResult<InvoiceVatGroup> {
	Ok(InvoiceVatGroup {
		invoice_id: row.try_get("invoice_id").db()?,
		vat_code: row.try_get::<String, _>("vat_code").db()?.parse()?,
		vat_rate_bp: row.try_get("vat_rate_bp").db()?,
		net: read_money(row.try_get("net").db()?)?,
		vat: read_money(row.try_get("vat").db()?)?,
		gross: read_money(row.try_get("gross").db()?)?,
		net_huf: row.try_get::<Option<i64>, _>("net_huf").db()?.map(read_money).transpose()?,
		vat_huf: row.try_get::<Option<i64>, _>("vat_huf").db()?.map(read_money).transpose()?,
		gross_huf: row.try_get::<Option<i64>, _>("gross_huf").db()?.map(read_money).transpose()?,
	})
}

fn document_row(row: &PgRow) -> ClResult<InvoiceDocument> {
	Ok(InvoiceDocument {
		invoice_id: row.try_get("invoice_id").db()?,
		kind: row.try_get("kind").db()?,
		sha256: row.try_get("sha256").db()?,
		bytes: row.try_get("bytes").db()?,
		template_version: row.try_get("template_version").db()?,
		rendered_at: Timestamp(row.try_get("rendered_at").db()?),
	})
}

/// The `UNIQUE (org_id, request_id)` answer; `create_draft_full` matches on this variant to
/// turn a lost idempotency race into the winner's invoice.
pub(crate) fn duplicate_request_id() -> Error {
	Error::conflict("an invoice already exists for this request_id")
}

/// The same answer for a caller that sent **no** `request_id`.
pub(crate) fn generic_conflict() -> Error {
	Error::conflict("an invoice with these keys already exists")
}

/// The `idx_invoice_storno_once` answer — the code `mintworks_invoice::storno` documents.
pub(crate) fn already_stornoed() -> Error {
	Error::coded(StatusCode::CONFLICT, "E-INV-ALREADY-STORNOED", "the invoice is already cancelled")
}

/// The invoice is no longer a draft, so the write that wanted it must not land.
pub(crate) fn not_a_draft() -> Error {
	Error::coded(StatusCode::CONFLICT, "E-INV-IMMUTABLE", "the invoice is no longer a draft")
}

/// The shared body of `mark_paid` and `mark_stornoed`: the predecessor is `'ISSUED'`
/// unconditionally, so no argument can reopen a numbered invoice.
async fn mark_status(store: &PgStore, id: i64, status: InvoiceStatus) -> ClResult<bool> {
	let res = sqlx::query(
		"UPDATE invoices SET status = $1, updated_at = $2 WHERE id = $3 AND status = 'ISSUED'",
	)
	.bind(status.as_str())
	.bind(Timestamp::now().0)
	.bind(id)
	.execute(&mut *store.conn().await?)
	.await
	.db()?;
	Ok(res.rows_affected() == 1)
}

/// `idx_billing_party_default` allows one default per org, so promoting a party has to
/// demote the incumbent in the same transaction.
async fn clear_default_party(conn: &mut PgConnection, org_id: i64) -> ClResult<()> {
	sqlx::query("UPDATE billing_parties SET is_default = 0 WHERE org_id = $1 AND is_default = 1")
		.bind(org_id)
		.execute(conn)
		.await
		.db()?;
	Ok(())
}

#[async_trait]
impl InvoiceStore for PgStore {
	// -- seller

	async fn seller_by_id(&self, id: i64) -> ClResult<Option<Seller>> {
		sqlx::query("SELECT * FROM sellers WHERE id = $1")
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(seller_row)
	}

	/// The nearest `sellers`-owning org at or above `org_id`. `depth` orders the walk: a
	/// business unit that owns a seller must not resolve to its parent's.
	async fn seller_for_org(&self, org_id: i64) -> ClResult<Option<Seller>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{}
			 SELECT s.* FROM sellers s JOIN anc ON s.org_id = anc.id
			 ORDER BY anc.depth LIMIT 1",
			crate::core::ancestors("id = $1", true, true)
		)))
		.bind(org_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(seller_row)
	}

	async fn put_seller(&self, s: &Seller) -> ClResult<()> {
		let res = self
			.recoverable(async |c| {
				sqlx::query(
			// `org_id` and `uid` are insert-only and the `WHERE` refuses a move loudly: an upsert
			// that moved `org_id` would hand another org this taxpayer id and its `doc_series`.
			// `closed_at` is never written: boot re-puts every seller, which would reopen a company.
			"INSERT INTO sellers (id, uid, org_id, nav_base_url, nav_login, series_code, created_at)
			 VALUES ($1, $2, $3, $4, $5, $6, $7)
			 ON CONFLICT (id) DO UPDATE SET
				nav_base_url = excluded.nav_base_url, nav_login = excluded.nav_login,
				series_code = excluded.series_code
			 WHERE sellers.org_id = excluded.org_id AND sellers.uid = excluded.uid",
		)
		.bind(s.id)
		.bind(s.uid.as_str())
		.bind(s.org_id)
		.bind(&s.nav_base_url)
		.bind(&s.nav_login)
		.bind(&s.series_code)
		.bind(s.created_at.0)
		.execute(c)
		.await
		// `ON CONFLICT (id)` cannot catch a `uid` collision on another id.
		.map_err(|e| unique_as_conflict(&e, "another seller already carries this uid"))
			})
			.await?;
		if res.rows_affected() == 0 {
			return Err(Error::conflict(format!(
				"seller id {} is another org's or carries another uid",
				s.id
			)));
		}
		Ok(())
	}

	async fn create_seller(&self, s: &Seller) -> ClResult<bool> {
		let res = self
			.recoverable(async |c| {
				sqlx::query(
			"INSERT INTO sellers (id, uid, org_id, nav_base_url, nav_login, series_code, created_at)
			 SELECT $1, $2, id, $3, $4, $5, $6 FROM orgs
			 WHERE id = $7 AND kind = 'SHARED' AND status = 'ACTIVE'",
		)
		.bind(s.id)
		.bind(s.uid.as_str())
		.bind(&s.nav_base_url)
		.bind(&s.nav_login)
		.bind(&s.series_code)
		.bind(s.created_at.0)
		.bind(s.org_id)
		.execute(c)
		.await
		.map_err(|e| unique_as_conflict(&e, "a seller with this id or uid already exists"))
			})
			.await?;
		Ok(res.rows_affected() == 1)
	}

	// -- seller versions

	async fn current_seller_version(&self, seller_id: i64) -> ClResult<Option<SellerVersion>> {
		version_by_status(&mut *self.reader().await?, seller_id, "CURRENT").await
	}

	async fn draft_seller_version(&self, seller_id: i64) -> ClResult<Option<SellerVersion>> {
		version_by_status(&mut *self.reader().await?, seller_id, "DRAFT").await
	}

	async fn seller_version(&self, seller_ver: i64) -> ClResult<Option<SellerVersion>> {
		sqlx::query("SELECT * FROM seller_versions WHERE seller_ver = $1")
			.bind(seller_ver)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(seller_version_row)
	}

	async fn seller_versions(&self, vers: &[i64]) -> ClResult<Vec<SellerVersion>> {
		sqlx::query("SELECT * FROM seller_versions WHERE seller_ver = ANY($1)")
			.bind(vers)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(seller_version_row)
	}

	async fn seller_version_history(&self, seller_id: i64) -> ClResult<Vec<SellerVersion>> {
		// `status <> 'DRAFT'` is `idx_seller_version_history`'s own predicate.
		sqlx::query(
			"SELECT * FROM seller_versions WHERE seller_id = $1 AND status <> 'DRAFT'
			 ORDER BY valid_from DESC",
		)
		.bind(seller_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(seller_version_row)
	}

	async fn save_seller_version_draft(
		&self,
		seller_id: i64,
		patch: &SellerVersionPatch,
	) -> ClResult<SellerVersion> {
		let tx = self.write_tx().await?;
		// The draft wins as the base, so a second edit builds on the first. One guard for both
		// lookups: a `match` scrutinee's temporary would hold the mutex into the `None` arm.
		let base = {
			let mut conn = tx.lock().await?;
			match version_by_status(&mut conn, seller_id, "DRAFT").await? {
				Some(draft) => Some(draft),
				None => version_by_status(&mut conn, seller_id, "CURRENT").await?,
			}
		};
		let merged = patch.merged(base.as_ref());
		let draft_ver =
			base.filter(|b| b.status == SellerVersionStatus::Draft).map(|b| b.seller_ver);

		// The fifteen statutory columns bind identically either way; only the trailing binds differ.
		let mut q = match draft_ver {
			// `AND status = 'DRAFT'`: a published version has no update path at all.
			Some(_) => sqlx::query(
				"UPDATE seller_versions SET
					name = $1, country = $2, tax_number = $3, group_member_tax_no = $4,
					eu_vat_id = $5, postcode = $6, city = $7, street = $8, bank_account = $9,
					bank_name = $10, small_business = $11, vat_scheme = $12, income_regime = $13,
					expense_ratio_pct = $14, regime_since = $15
				 WHERE seller_ver = $16 AND status = 'DRAFT' RETURNING *",
			),
			None => sqlx::query(
				"INSERT INTO seller_versions
				 (name, country, tax_number, group_member_tax_no, eu_vat_id, postcode, city,
				  street, bank_account, bank_name, small_business, vat_scheme, income_regime,
				  expense_ratio_pct, regime_since, seller_id, status, created_at, valid_from)
				 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
				         'DRAFT', $17, NULL)
				 RETURNING *",
			),
		};
		q = q
			.bind(&merged.name)
			.bind(&merged.country)
			.bind(&merged.tax_number)
			.bind(&merged.group_member_tax_no)
			.bind(&merged.eu_vat_id)
			.bind(&merged.postcode)
			.bind(&merged.city)
			.bind(&merged.street)
			.bind(&merged.bank_account)
			.bind(&merged.bank_name)
			.bind(i64::from(merged.small_business))
			.bind(&merged.vat_scheme)
			.bind(&merged.income_regime)
			.bind(merged.expense_ratio_pct)
			.bind(&merged.regime_since);
		q = match draft_ver {
			Some(ver) => q.bind(ver),
			None => q.bind(seller_id).bind(Timestamp::now().0),
		};
		let row = q
			.fetch_optional(&mut *tx.lock().await?)
			.await
			.one(seller_version_row)?
			.ok_or_else(|| {
				Error::internal("mintworks-store-postgres: the seller draft vanished")
			})?;
		tx.commit().await?;
		Ok(row)
	}

	async fn publish_seller_version(
		&self,
		seller_id: i64,
		now: Timestamp,
		check: &(dyn for<'a> Fn(&'a SellerVersion) -> ClResult<()> + Send + Sync),
	) -> ClResult<Option<i64>> {
		let tx = self.write_tx().await?;
		// Checked inside the promoting transaction: a draft read beforehand could be rewritten
		// blank by a concurrent save and frozen onto an immutable invoice.
		let Some(draft) = version_by_status(&mut *tx.lock().await?, seller_id, "DRAFT").await?
		else {
			return Ok(None);
		};
		check(&draft)?;
		// Archive first: `idx_seller_version_current` is unique.
		sqlx::query(
			"UPDATE seller_versions SET status = 'ARCHIVED', superseded_at = $1
			 WHERE seller_id = $2 AND status = 'CURRENT'",
		)
		.bind(now.0)
		.bind(seller_id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;

		let promoted: Option<i64> = sqlx::query_scalar(
			"UPDATE seller_versions SET status = 'CURRENT', valid_from = $1
			 WHERE seller_id = $2 AND status = 'DRAFT' RETURNING seller_ver",
		)
		.bind(now.0)
		.bind(seller_id)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.db()?;
		// Nothing promoted: dropping `tx` rolls the archive back.
		if promoted.is_none() {
			return Ok(None);
		}
		tx.commit().await?;
		Ok(promoted)
	}

	async fn sync_seller_version(
		&self,
		seller_id: i64,
		now: Timestamp,
		patch: &SellerVersionPatch,
		check: &(dyn for<'a> Fn(&'a SellerVersion) -> ClResult<()> + Send + Sync),
	) -> ClResult<Option<i64>> {
		// One transaction around all three: a save landing between the probe and the publish
		// would publish a half-typed edit.
		let (tx, bound) = self.begin().await?;
		let open = {
			let mut conn = tx.lock().await?;
			version_by_status(&mut conn, seller_id, "DRAFT").await?.is_some()
		};
		if open {
			return Ok(None);
		}
		// Through the bound handle, so both nest as savepoints in this transaction.
		bound.save_seller_version_draft(seller_id, patch).await?;
		let ver = bound.publish_seller_version(seller_id, now, check).await?;
		tx.commit().await?;
		Ok(ver)
	}

	async fn discard_seller_version_draft(&self, seller_id: i64) -> ClResult<bool> {
		let res =
			sqlx::query("DELETE FROM seller_versions WHERE seller_id = $1 AND status = 'DRAFT'")
				.bind(seller_id)
				.execute(&mut *self.conn().await?)
				.await
				.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn seller_has_issued(&self, seller_id: i64) -> ClResult<bool> {
		sqlx::query_scalar(
			"SELECT EXISTS(SELECT 1 FROM invoices WHERE seller_id = $1 AND number IS NOT NULL)",
		)
		.bind(seller_id)
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn set_seller_closed(
		&self,
		seller_id: i64,
		closed_at: Option<Timestamp>,
	) -> ClResult<bool> {
		let res = match closed_at {
			None => {
				sqlx::query("UPDATE sellers SET closed_at = NULL WHERE id = $1").bind(seller_id)
			}
			Some(t) => sqlx::query(
				"UPDATE sellers SET closed_at = $1 WHERE id = $2 AND NOT EXISTS
					(SELECT 1 FROM invoices WHERE seller_id = sellers.id AND status = 'PENDING')",
			)
			.bind(t.0)
			.bind(seller_id),
		}
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn set_seller_payment_days(&self, seller_id: i64, days: Option<i64>) -> ClResult<bool> {
		let res = sqlx::query("UPDATE sellers SET payment_days = $1 WHERE id = $2")
			.bind(days)
			.bind(seller_id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(res.rows_affected() == 1)
	}

	// -- services

	async fn sync_services(&self, org_id: i64, defs: &[ServiceDef]) -> ClResult<()> {
		let now = Timestamp::now();
		let tx = self.write_tx().await?;
		for d in defs {
			// `active` is absent from the SET list: withdrawing a service from the consumer's
			// code must not resurrect it.
			let uid = ServiceId::generate();
			sqlx::query(
				"INSERT INTO services
				 (uid, org_id, code, name, description, unit, unit_price, vat_code, created_at, updated_at)
				 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
				 ON CONFLICT (org_id, code) DO UPDATE SET
					name = excluded.name, description = excluded.description,
					unit = excluded.unit, unit_price = excluded.unit_price,
					vat_code = excluded.vat_code, updated_at = excluded.updated_at",
			)
			.bind(uid.as_str())
			.bind(org_id)
			.bind(&d.code)
			.bind(&d.name)
			.bind(&d.description)
			.bind(&d.unit)
			.bind(d.unit_price.0)
			.bind(d.vat_code.as_str())
			.bind(now.0)
			.bind(now.0)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}
		tx.commit().await?;
		Ok(())
	}

	async fn service_by_code(&self, org_id: i64, code: &str) -> ClResult<Option<Service>> {
		sqlx::query("SELECT * FROM services WHERE org_id = $1 AND code = $2")
			.bind(org_id)
			.bind(code)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(service_row)
	}

	async fn services_by_codes(&self, org_id: i64, codes: &[&str]) -> ClResult<Vec<Service>> {
		sqlx::query("SELECT * FROM services WHERE org_id = $1 AND active = 1 AND code = ANY($2)")
			.bind(org_id)
			.bind(codes)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(service_row)
	}

	async fn service_by_uid(&self, org_id: i64, uid: &ServiceId) -> ClResult<Option<Service>> {
		sqlx::query("SELECT * FROM services WHERE org_id = $1 AND uid = $2")
			.bind(org_id)
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(service_row)
	}

	async fn create_service(&self, org_id: i64, d: &ServiceDef) -> ClResult<Service> {
		let now = Timestamp::now();
		let uid = ServiceId::generate();
		let row = self
			.recoverable(async |c| {
				sqlx::query(
					"INSERT INTO services
			 (uid, org_id, code, name, description, unit, unit_price, vat_code, created_at, updated_at)
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *",
				)
				.bind(uid.as_str())
				.bind(org_id)
				.bind(&d.code)
				.bind(&d.name)
				.bind(&d.description)
				.bind(&d.unit)
				.bind(d.unit_price.0)
				.bind(d.vat_code.as_str())
				.bind(now.0)
				.bind(now.0)
				.fetch_one(c)
				.await
				.map_err(|e| unique_as_conflict(&e, "service code already exists"))
			})
			.await?;
		service_row(&row)
	}

	async fn update_service(
		&self,
		org_id: i64,
		uid: &ServiceId,
		p: &ServicePatch,
	) -> ClResult<Option<Service>> {
		self.recoverable(async |c| {
			sqlx::query(
				"UPDATE services SET
				code        = CASE WHEN $1 THEN $2 ELSE code END,
				name        = COALESCE($3, name),
				description = CASE WHEN $4 THEN $5 ELSE description END,
				unit        = COALESCE($6, unit),
				unit_price  = COALESCE($7, unit_price),
				vat_code    = COALESCE($8, vat_code),
				active      = COALESCE($9, active),
				updated_at  = $10
			 WHERE org_id = $11 AND uid = $12 RETURNING *",
			)
			.bind(!p.code.is_undefined())
			.bind(p.code.value())
			.bind(&p.name)
			.bind(!p.description.is_undefined())
			.bind(p.description.value())
			.bind(&p.unit)
			.bind(p.unit_price.map(|m| m.0))
			.bind(p.vat_code.map(mintworks_invoice::VatCode::as_str))
			.bind(p.active.map(i64::from))
			.bind(Timestamp::now().0)
			.bind(org_id)
			.bind(uid.as_str())
			.fetch_optional(c)
			.await
			.map_err(|e| unique_as_conflict(&e, "service code already exists"))
		})
		.await?
		.as_ref()
		.map(service_row)
		.transpose()
	}

	async fn list_services(
		&self,
		org_id: i64,
		active_only: bool,
		limit: i64,
	) -> ClResult<Vec<Service>> {
		sqlx::query(
			"SELECT * FROM services WHERE org_id = $1 AND (NOT $2 OR active = 1)
			 ORDER BY name LIMIT $3",
		)
		.bind(org_id)
		.bind(active_only)
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(service_row)
	}

	// -- billing parties

	async fn create_party(&self, org_id: i64, p: &PartyPatch) -> ClResult<BillingParty> {
		let now = Timestamp::now();
		let tx = self.write_tx().await?;

		if p.is_default == Some(true) {
			clear_default_party(&mut *tx.lock().await?, org_id).await?;
		}

		let uid = PartyId::generate();
		let row = sqlx::query(
			"INSERT INTO billing_parties
			 (uid, org_id, kind, name, country, tax_number, eu_vat_id, group_tax_no,
			  postcode, city, street, email, is_default, payment_days, payment_method,
			  created_at, updated_at)
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
			 RETURNING *",
		)
		.bind(uid.as_str())
		.bind(org_id)
		.bind(p.kind.map(mintworks_invoice::store::PartyKind::as_str))
		.bind(&p.name)
		.bind(&p.country)
		.bind(p.tax_number.value())
		.bind(p.eu_vat_id.value())
		.bind(p.group_tax_no.value())
		.bind(p.postcode.value())
		.bind(p.city.value())
		.bind(p.street.value())
		.bind(p.email.value())
		.bind(i64::from(p.is_default.unwrap_or(false)))
		.bind(p.payment_days.value())
		.bind(p.payment_method.value().map(|m| m.as_str()))
		.bind(now.0)
		.bind(now.0)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "a billing party with this tax number exists"))?;
		let party = party_row(&row)?;

		tx.commit().await?;
		Ok(party)
	}

	async fn party_by_uid(&self, org_id: i64, uid: &PartyId) -> ClResult<Option<BillingParty>> {
		sqlx::query("SELECT * FROM billing_parties WHERE org_id = $1 AND uid = $2")
			.bind(org_id)
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(party_row)
	}

	async fn party_by_id(&self, id: i64) -> ClResult<Option<BillingParty>> {
		sqlx::query("SELECT * FROM billing_parties WHERE id = $1")
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(party_row)
	}

	async fn update_party(
		&self,
		org_id: i64,
		uid: &PartyId,
		p: &PartyPatch,
	) -> ClResult<Option<BillingParty>> {
		let tx = self.write_tx().await?;

		if p.is_default == Some(true) {
			clear_default_party(&mut *tx.lock().await?, org_id).await?;
		}

		let party: Option<BillingParty> = sqlx::query(
			"UPDATE billing_parties SET
				kind           = COALESCE($1, kind),
				name           = COALESCE($2, name),
				country        = COALESCE($3, country),
				tax_number     = CASE WHEN $4 THEN $5 ELSE tax_number END,
				eu_vat_id      = CASE WHEN $6 THEN $7 ELSE eu_vat_id END,
				group_tax_no   = CASE WHEN $8 THEN $9 ELSE group_tax_no END,
				postcode       = CASE WHEN $10 THEN $11 ELSE postcode END,
				city           = CASE WHEN $12 THEN $13 ELSE city END,
				street         = CASE WHEN $14 THEN $15 ELSE street END,
				email          = CASE WHEN $16 THEN $17 ELSE email END,
				is_default     = COALESCE($18, is_default),
				payment_days   = CASE WHEN $19 THEN $20 ELSE payment_days END,
				payment_method = CASE WHEN $21 THEN $22 ELSE payment_method END,
				updated_at     = $23
			 WHERE org_id = $24 AND uid = $25 RETURNING *",
		)
		.bind(p.kind.map(mintworks_invoice::store::PartyKind::as_str))
		.bind(&p.name)
		.bind(&p.country)
		.bind(!p.tax_number.is_undefined())
		.bind(p.tax_number.value())
		.bind(!p.eu_vat_id.is_undefined())
		.bind(p.eu_vat_id.value())
		.bind(!p.group_tax_no.is_undefined())
		.bind(p.group_tax_no.value())
		.bind(!p.postcode.is_undefined())
		.bind(p.postcode.value())
		.bind(!p.city.is_undefined())
		.bind(p.city.value())
		.bind(!p.street.is_undefined())
		.bind(p.street.value())
		.bind(!p.email.is_undefined())
		.bind(p.email.value())
		.bind(p.is_default.map(i64::from))
		.bind(!p.payment_days.is_undefined())
		.bind(p.payment_days.value())
		.bind(!p.payment_method.is_undefined())
		.bind(p.payment_method.value().map(|m| m.as_str()))
		.bind(Timestamp::now().0)
		.bind(org_id)
		.bind(uid.as_str())
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "a billing party with this tax number exists"))?
		.as_ref()
		.map(party_row)
		.transpose()?;

		// Commit only a hit: `clear_default_party` already ran, and a committed miss would leave
		// the org with no default. Dropping `tx` rolls back.
		if party.is_some() {
			tx.commit().await?;
		}
		Ok(party)
	}

	async fn delete_party(&self, org_id: i64, uid: &PartyId) -> ClResult<bool> {
		let tx = self.write_tx().await?;
		let done = sqlx::query("DELETE FROM billing_parties WHERE org_id = $1 AND uid = $2")
			.bind(org_id)
			.bind(uid.as_str())
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		// The ext blob's `(type, uid)` pair carries no FK, so this is the only thing removing it.
		if done.rows_affected() > 0 {
			sqlx::query("DELETE FROM objects WHERE org_id = $1 AND type = $2 AND uid = $3")
				.bind(org_id)
				.bind(PARTY_EXT)
				.bind(uid.as_str())
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}
		tx.commit().await?;
		Ok(done.rows_affected() > 0)
	}

	async fn list_parties(&self, org_id: i64, limit: i64) -> ClResult<Vec<BillingParty>> {
		sqlx::query(
			"SELECT * FROM billing_parties
			 WHERE org_id = $1 ORDER BY is_default DESC, name LIMIT $2",
		)
		.bind(org_id)
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(party_row)
	}

	async fn default_party(&self, org_id: i64) -> ClResult<Option<BillingParty>> {
		sqlx::query("SELECT * FROM billing_parties WHERE org_id = $1 AND is_default = 1")
			.bind(org_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(party_row)
	}

	// -- invoices: draft, reads, issue, storno, lists — bodies in `invoice_doc.rs`

	async fn create_draft(&self, new: &NewInvoice) -> ClResult<Invoice> {
		invoice_doc::create_draft(self, new).await
	}

	async fn create_draft_full(
		&self,
		new: &NewInvoice,
		priced: &Priced,
		patch: Option<&InvoicePatch>,
	) -> ClResult<Invoice> {
		invoice_doc::create_draft_full(self, new, priced, patch).await
	}

	async fn invoice_by_request_id(
		&self,
		org_id: i64,
		request_id: &str,
	) -> ClResult<Option<Invoice>> {
		invoice_doc::invoice_by_request_id(self, org_id, request_id).await
	}

	async fn invoice_by_uid(
		&self,
		org_id: Option<i64>,
		uid: &InvoiceId,
	) -> ClResult<Option<Invoice>> {
		invoice_doc::invoice_by_uid(self, org_id, uid).await
	}

	async fn invoice_by_id(&self, id: i64) -> ClResult<Option<Invoice>> {
		invoice_doc::invoice_by_id(self, id).await
	}

	async fn storno_of(&self, original_id: i64) -> ClResult<Option<Invoice>> {
		invoice_doc::storno_of(self, original_id).await
	}

	async fn update_draft(&self, id: i64, p: &InvoicePatch) -> ClResult<Option<Invoice>> {
		invoice_doc::update_draft(self, id, p).await
	}

	async fn update_notes(&self, id: i64, notes: Option<&str>) -> ClResult<Option<Invoice>> {
		invoice_doc::update_notes(self, id, notes).await
	}

	async fn replace_draft_lines(
		&self,
		id: i64,
		patch: Option<&InvoicePatch>,
		priced: &Priced,
		expected_version: i64,
	) -> ClResult<bool> {
		invoice_doc::replace_draft_lines(self, id, patch, priced, expected_version).await
	}

	async fn invoice_lines(&self, invoice_id: i64) -> ClResult<Vec<InvoiceLine>> {
		invoice_doc::invoice_lines(self, invoice_id).await
	}

	async fn invoice_vat_groups(&self, invoice_id: i64) -> ClResult<Vec<InvoiceVatGroup>> {
		invoice_doc::invoice_vat_groups(self, invoice_id).await
	}

	async fn invoices_by_ids(&self, ids: &[i64]) -> ClResult<Vec<Invoice>> {
		invoice_doc::invoices_by_ids(self, ids).await
	}

	async fn invoice_lines_for(&self, ids: &[i64]) -> ClResult<Vec<InvoiceLine>> {
		invoice_doc::invoice_lines_for(self, ids).await
	}

	async fn invoice_vat_groups_for(&self, ids: &[i64]) -> ClResult<Vec<InvoiceVatGroup>> {
		invoice_doc::invoice_vat_groups_for(self, ids).await
	}

	async fn delete_draft(&self, id: i64) -> ClResult<bool> {
		invoice_doc::delete_draft(self, id).await
	}

	async fn sweep_drafts(&self, cutoff: Timestamp) -> ClResult<u64> {
		invoice_doc::sweep_drafts(self, cutoff).await
	}

	async fn issued_without_document(&self, limit: i64) -> ClResult<Vec<i64>> {
		invoice_doc::issued_without_document(self, limit).await
	}

	async fn issue(
		&self,
		id: i64,
		issue: &IssueInvoice,
		expected_version: i64,
	) -> ClResult<Invoice> {
		invoice_doc::issue(self, id, issue, expected_version).await
	}

	async fn storno(
		&self,
		original_id: i64,
		new: &NewInvoice,
		issue: &IssueInvoice,
	) -> ClResult<Invoice> {
		invoice_doc::storno(self, original_id, new, issue).await
	}

	async fn list_invoices(
		&self,
		org_id: i64,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Invoice>> {
		invoice_doc::list_invoices(self, org_id, before_id, limit).await
	}

	async fn list_invoices_page(
		&self,
		org_id: i64,
		filter: &InvoiceFilter,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<ListedInvoice>> {
		invoice_doc::list_invoices_page(self, org_id, filter, before_id, limit).await
	}

	async fn invoice_summary(
		&self,
		org_id: i64,
		from_month: &str,
		today: &str,
		this_month: (Timestamp, Timestamp),
	) -> ClResult<InvoiceSummary> {
		invoice_doc::invoice_summary(self, org_id, from_month, today, this_month).await
	}

	async fn invoice_revenue(
		&self,
		org_id: i64,
		year: i32,
		start: Timestamp,
		end: Timestamp,
	) -> ClResult<Vec<RevenueMonth>> {
		invoice_doc::invoice_revenue(self, org_id, year, start, end).await
	}

	// -- the three permitted post-issue writes and the gateway lock

	async fn mark_paid(&self, id: i64) -> ClResult<bool> {
		mark_status(self, id, InvoiceStatus::Paid).await
	}

	async fn mark_stornoed(&self, id: i64) -> ClResult<bool> {
		mark_status(self, id, InvoiceStatus::Stornoed).await
	}

	async fn set_status(&self, id: i64, from: InvoiceStatus, to: InvoiceStatus) -> ClResult<bool> {
		// The gateway lock and nothing else: `set_status(id, Issued, Draft)` would walk a
		// numbered, NAV-filed invoice back to where `issue` renumbers it.
		if !matches!(
			(from, to),
			(InvoiceStatus::Draft, InvoiceStatus::Pending)
				| (InvoiceStatus::Pending, InvoiceStatus::Draft)
		) {
			return Err(Error::internal(format!(
				"set_status is the gateway lock only, not {from:?} -> {to:?}"
			)));
		}
		let res = sqlx::query(
			"UPDATE invoices SET status = $1, updated_at = $2 WHERE id = $3 AND status = $4",
		)
		.bind(to.as_str())
		.bind(Timestamp::now().0)
		.bind(id)
		.bind(from.as_str())
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() > 0)
	}

	async fn set_paid(
		&self,
		id: i64,
		paid_amount: Money,
		paid_at: Option<Timestamp>,
	) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE invoices SET paid_amount = $1, paid_at = $2, updated_at = $3
			 WHERE id = $4 AND status IN ('ISSUED','PAID')",
		)
		.bind(paid_amount.0)
		.bind(paid_at.map(|t| t.0))
		.bind(Timestamp::now().0)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	// -- documents

	async fn put_invoice_document(&self, doc: &InvoiceDocument, version: i64) -> ClResult<bool> {
		// `SELECT … WHERE version = $n`: a note edit during the seconds-long render must not
		// freeze the pre-edit note into a PDF served `immutable`. The first render wins — the
		// row is immutable evidence, so a re-driven render after a template change is a no-op.
		let res = sqlx::query(
			"INSERT INTO invoice_documents
			 (invoice_id, kind, sha256, bytes, template_version, rendered_at)
			 SELECT $1, 'PDF', $2, $3, $4, $5 FROM invoices WHERE id = $1 AND version = $6
			 ON CONFLICT (invoice_id, kind) DO NOTHING",
		)
		.bind(doc.invoice_id)
		.bind(&doc.sha256)
		.bind(doc.bytes)
		.bind(&doc.template_version)
		.bind(doc.rendered_at.0)
		.bind(version)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn invoice_document(&self, invoice_id: i64) -> ClResult<Option<InvoiceDocument>> {
		sqlx::query("SELECT * FROM invoice_documents WHERE invoice_id = $1 AND kind = 'PDF'")
			.bind(invoice_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(document_row)
	}

	async fn invoice_documents_for(&self, ids: &[i64]) -> ClResult<Vec<InvoiceDocument>> {
		sqlx::query(
			"SELECT * FROM invoice_documents WHERE kind = 'PDF' AND invoice_id = ANY($1)
			 ORDER BY invoice_id",
		)
		.bind(ids)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(document_row)
	}

	// -- currency

	async fn currency_get(&self, code: &str) -> ClResult<Option<Currency>> {
		let row: Option<CurrencyRow> = sqlx::query_as(
			"SELECT code, price_round_step, cash_round_step, mode, fixed_rate_e6, fee_bp, enabled \
			 FROM currencies WHERE code = $1",
		)
		.bind(code)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(row.map(currency_of))
	}

	async fn currency_list(&self, all: bool) -> ClResult<Vec<Currency>> {
		let rows: Vec<CurrencyRow> = sqlx::query_as(
			"SELECT code, price_round_step, cash_round_step, mode, fixed_rate_e6, fee_bp, enabled \
			 FROM currencies WHERE $1 OR enabled = 1 ORDER BY code",
		)
		.bind(all)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(rows.into_iter().map(currency_of).collect())
	}

	async fn currency_rate(
		&self,
		pair: &str,
		source: &str,
		on: &str,
	) -> ClResult<Option<(i64, String)>> {
		sqlx::query_as(
			"SELECT rate_e6, date FROM currency_rates \
			 WHERE pair = $1 AND source = $2 AND date <= $3 ORDER BY date DESC LIMIT 1",
		)
		.bind(pair)
		.bind(source)
		.bind(on)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()
	}

	// -- MNB rate fetch

	async fn mnb_currencies(&self, base: &str) -> ClResult<Vec<String>> {
		sqlx::query_scalar("SELECT code FROM currencies WHERE enabled = 1 AND code <> $1")
			.bind(base)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn mnb_max_date(&self, pair: &str) -> ClResult<Option<String>> {
		sqlx::query_scalar("SELECT max(date) FROM currency_rates WHERE pair = $1 AND source = $2")
			.bind(pair)
			.bind(mnb::SOURCE)
			.fetch_one(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn mnb_upsert_rates(&self, pair: &str, rows: &[(String, i64)]) -> ClResult<()> {
		let tx = self.write_tx().await?;
		for (date, rate_e6) in rows {
			sqlx::query(
				"INSERT INTO currency_rates (pair, date, source, rate_e6, fetched_at) \
				 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (pair, date, source) DO UPDATE SET \
				 rate_e6 = excluded.rate_e6, fetched_at = excluded.fetched_at",
			)
			.bind(pair)
			.bind(date)
			.bind(mnb::SOURCE)
			.bind(rate_e6)
			.bind(Timestamp::now().0)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}
		tx.commit().await?;
		Ok(())
	}

	// -- VIES

	async fn vies_cached(&self, full: &str, ttl: i64) -> ClResult<Option<ViesResult>> {
		let row: Option<CachedRow> = sqlx::query_as(
			"SELECT valid, name, address, request_id, checked_at FROM vies_checks \
			 WHERE eu_vat_id = $1 AND checked_at > $2",
		)
		.bind(full)
		.bind(Timestamp::now().0 - ttl)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(row.map(|(valid, name, address, request_id, checked_at)| ViesResult {
			eu_vat_id: full.to_owned(),
			valid: valid != 0,
			name,
			address,
			request_id,
			checked_at: Timestamp(checked_at),
			cached: true,
		}))
	}

	async fn vies_store(&self, r: &ViesResult) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO vies_checks (eu_vat_id, valid, name, address, request_id, checked_at) \
			 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (eu_vat_id) DO UPDATE SET \
			 valid = excluded.valid, name = excluded.name, address = excluded.address, \
			 request_id = excluded.request_id, checked_at = excluded.checked_at",
		)
		.bind(&r.eu_vat_id)
		.bind(i64::from(r.valid))
		.bind(r.name.as_deref())
		.bind(r.address.as_deref())
		.bind(r.request_id.as_deref())
		.bind(r.checked_at.0)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	// -- orgs

	async fn org_billing_currency(&self, org_id: i64) -> ClResult<Option<CurrencyCode>> {
		Ok(sqlx::query_scalar("SELECT billing_currency FROM orgs WHERE id = $1")
			.bind(org_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()?
			.flatten()
			.map(CurrencyCode::from_trusted))
	}
}

// vim: ts=4
