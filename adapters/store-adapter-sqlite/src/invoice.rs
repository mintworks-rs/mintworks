//! `InvoiceStore` over SQLite. Reads go through `reader()`, writes through `writer()`.
//!
//! **Every mutating statement here either carries `AND status = 'DRAFT'` or is one of the
//! three writes an issued invoice still permits** (`mark_paid`, `mark_stornoed`, `set_paid`).
//! That is the *only* guard: no trigger backs it up, because a rule the application enforces
//! does not belong in the schema too.
//!
//! The three child-table helpers — `insert_lines`, `replace_lines`, `replace_groups` — are the
//! exception that proves it: they carry no predicate of their own, because a `status` predicate
//! on `invoice_lines` needs a correlated subquery per row. They are private to this file, and
//! **every caller holds a `status = 'DRAFT'` gate in the same transaction before reaching
//! them**: `create_draft_full` inserted the row itself moments earlier, `replace_draft_lines`
//! and `issue` both probe `AND status = 'DRAFT' AND version = ?` first, and `storno` writes
//! into a draft it has just inserted. Adding a caller means holding that gate.
//!
//! `saas-invoice` carries no driver dependency, so no row type here can be decoded by derive:
//! every query binds primitives and every framework row is built by hand in the `*_row` helpers
//! below. See `util.rs` for the conversion vocabulary — in particular that `Money` and `Qty`
//! cross as `i64` and come back through `read_money` / `read_qty`, which re-apply `bounded`.

use async_trait::async_trait;
use saas_core::error::StatusCode;
use saas_core::prelude::*;
use saas_invoice::currency::{Currency, RateMode};
use saas_invoice::draft::Priced;
use saas_invoice::mnb;
use saas_invoice::store::{
	BillingParty, Invoice, InvoiceDocument, InvoiceLine, InvoicePatch, InvoiceStatus, InvoiceStore,
	InvoiceVatGroup, IssueInvoice, ListedInvoice, NewInvoice, NewInvoiceLine, PartyPatch, Seller,
	Service, ServiceDef, ServicePatch, render_number,
};
use saas_invoice::vies::ViesResult;
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

use crate::SqliteStore;
use crate::util::{DbExt, RowExt, RowsExt, read_discount_value, read_money, read_qty};

/// The six `currencies` columns both currency queries select, in order.
type CurrencyRow = (String, i64, String, Option<i64>, i64, i64);

/// The `vies_checks` columns `vies_cached` selects, in order.
type CachedRow = (i64, Option<String>, Option<String>, Option<String>, i64);

fn currency_of(row: CurrencyRow) -> Currency {
	let (code, price_round_step, mode, fixed_rate_e6, fee_bp, enabled) = row;
	Currency {
		code: CurrencyCode::from_trusted(code),
		price_round_step,
		mode: if mode == "FIXED" { RateMode::Fixed } else { RateMode::Official },
		fixed_rate_e6,
		fee_bp,
		enabled: enabled != 0,
	}
}

use crate::util::unique_as_conflict;

// ---------------------------------------------------------------- row mapping
//
// Every read below is **by column name**: `SELECT *` follows the DDL's column order, which a
// migration may change, so reading by index misaligns the first time one moves.

fn seller_row(row: &SqliteRow) -> ClResult<Seller> {
	Ok(Seller {
		id: row.try_get("id").db()?,
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
		nav_base_url: row.try_get("nav_base_url").db()?,
		nav_login: row.try_get("nav_login").db()?,
		small_business: row.try_get("small_business").db()?,
		vat_scheme: row.try_get("vat_scheme").db()?,
		series_code: row.try_get("series_code").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn party_row(row: &SqliteRow) -> ClResult<BillingParty> {
	Ok(BillingParty {
		id: row.try_get("id").db()?,
		uid: PartyId::from_trusted(row.try_get::<String, _>("uid").db()?),
		tenant_id: row.try_get("tenant_id").db()?,
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
		is_default: row.try_get("is_default").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		updated_at: Timestamp(row.try_get("updated_at").db()?),
	})
}

fn service_row(row: &SqliteRow) -> ClResult<Service> {
	Ok(Service {
		id: row.try_get("id").db()?,
		uid: ServiceId::from_trusted(row.try_get::<String, _>("uid").db()?),
		code: row.try_get("code").db()?,
		name: row.try_get("name").db()?,
		description: row.try_get("description").db()?,
		unit: row.try_get("unit").db()?,
		unit_price: read_money(row.try_get("unit_price").db()?)?,
		vat_code: row.try_get::<String, _>("vat_code").db()?.parse()?,
		active: row.try_get("active").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		updated_at: Timestamp(row.try_get("updated_at").db()?),
	})
}

/// [`invoice_row`] plus the three joined uid columns of `list_invoices_page`.
fn listed_invoice_row(row: &SqliteRow) -> ClResult<ListedInvoice> {
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

fn invoice_row(row: &SqliteRow) -> ClResult<Invoice> {
	Ok(Invoice {
		id: row.try_get("id").db()?,
		uid: InvoiceId::from_trusted(row.try_get::<String, _>("uid").db()?),
		request_id: row.try_get("request_id").db()?,
		tenant_id: row.try_get("tenant_id").db()?,
		seller_id: row.try_get("seller_id").db()?,
		billing_party_id: row.try_get("billing_party_id").db()?,
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,

		series_code: row.try_get("series_code").db()?,
		series_year: row.try_get("series_year").db()?,
		number: row.try_get("number").db()?,
		issued_at: row.try_get::<Option<i64>, _>("issued_at").db()?.map(Timestamp),
		fulfilment_date: row.try_get("fulfilment_date").db()?,
		due_date: row.try_get("due_date").db()?,
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

fn line_row(row: &SqliteRow) -> ClResult<InvoiceLine> {
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
	})
}

fn vat_group_row(row: &SqliteRow) -> ClResult<InvoiceVatGroup> {
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

fn document_row(row: &SqliteRow) -> ClResult<InvoiceDocument> {
	Ok(InvoiceDocument {
		invoice_id: row.try_get("invoice_id").db()?,
		kind: row.try_get("kind").db()?,
		sha256: row.try_get("sha256").db()?,
		bytes: row.try_get("bytes").db()?,
		template_version: row.try_get("template_version").db()?,
		rendered_at: Timestamp(row.try_get("rendered_at").db()?),
	})
}

/// The `UNIQUE (tenant_id, request_id)` answer. [`SqliteStore::create_draft_full`] matches on
/// this variant to turn a lost idempotency race into the winner's invoice.
fn duplicate_request_id() -> Error {
	Error::conflict("an invoice already exists for this request_id")
}

/// The same answer for a caller that sent **no** `request_id`: with none, any unique violation
/// on this insert is a `uid` or storno-once collision, and [`duplicate_request_id`] named a key
/// the caller never sent.
fn generic_conflict() -> Error {
	Error::conflict("an invoice with these keys already exists")
}

/// The `idx_invoice_storno_once` answer — the code `saas_invoice::storno` documents. Passed
/// into `insert_draft`, whose two callers hit different unique constraints; mapping every
/// violation to [`duplicate_request_id`] instead would answer a double storno with the wrong one.
fn already_stornoed() -> Error {
	Error::coded(StatusCode::CONFLICT, "E-INV-ALREADY-STORNOED", "the invoice is already cancelled")
}

/// The invoice is no longer a draft, so the write that wanted it must not land.
fn not_a_draft() -> Error {
	Error::coded(StatusCode::CONFLICT, "E-INV-IMMUTABLE", "the invoice is no longer a draft")
}

/// Allocates the next number of `(seller_id, 'INVOICE', code, year)` and renders it.
///
/// Called only from inside the write transaction that flips an invoice to `ISSUED`. The
/// series row is created on first use of the year; `next_no - 1` is the number just taken,
/// on both the insert and the conflict path. A rolled-back transaction consumes no number,
/// which is what gaplessness requires — no lock and no reservation row, because SQLite
/// serializes writers.
///
/// `format` is carried forward from the most recent earlier year of the same series, and only
/// falls back to the schema default for a series that has never been used. The series primary
/// key includes `year`, so without that a hand-set custom format silently reverted to
/// `{code}{year}/{no:06}` on the first invoice of every January — on numbers that are
/// statutorily immutable once issued.
async fn allocate_number(
	tx: &mut SqliteConnection,
	seller_id: i64,
	code: &str,
	year: i64,
) -> ClResult<String> {
	let (no, format): (i64, String) = sqlx::query_as(
		"INSERT INTO doc_series (seller_id, kind, code, year, next_no, format)
		 SELECT ?1, 'INVOICE', ?2, ?3, 2,
		        COALESCE(
		            (SELECT format FROM doc_series
		              WHERE seller_id = ?1 AND kind = 'INVOICE' AND code = ?2 AND year < ?3
		              ORDER BY year DESC LIMIT 1),
		            '{code}{year}/{no:06}'
		        )
		 ON CONFLICT (seller_id, kind, code, year) DO UPDATE SET next_no = next_no + 1
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

/// Rows per multi-row `INSERT`. Well under SQLite's 32 766 variable cap at the widest row
/// here (16 columns), so raising `MAX_LINES` cannot trip it silently.
const INSERT_CHUNK: usize = 1000;

impl SqliteStore {
	/// `<head> (?,?,…)<tail>` bound to `ids` and decoded with `f` — the three batched reads
	/// the audit export needs. Empty `ids` answers without a statement: `IN ()` is a syntax
	/// error.
	async fn in_ids<T>(
		&self,
		head: &str,
		tail: &str,
		ids: &[i64],
		f: fn(&SqliteRow) -> ClResult<T>,
	) -> ClResult<Vec<T>> {
		if ids.is_empty() {
			return Ok(Vec::new());
		}
		let sql = format!("{head} {}{tail}", values_clause(1, ids.len()));
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
		for id in ids {
			q = q.bind(*id);
		}
		q.fetch_all(self.reader()).await.all(f)
	}
}

/// `"(?,?,…),(?,?,…)"` for `rows` rows of `cols` binds each.
fn values_clause(rows: usize, cols: usize) -> String {
	let one = format!("({})", "?,".repeat(cols - 1) + "?");
	let mut s = String::with_capacity(rows * (one.len() + 1));
	for i in 0..rows {
		if i > 0 {
			s.push(',');
		}
		s.push_str(&one);
	}
	s
}

/// Writes the whole line set of a draft, numbering lines from the slice order.
///
/// One statement per chunk rather than one per line: this runs inside `write_tx`, so a
/// 500-line draft held the process-wide single writer for 500 round-trips.
async fn insert_lines(
	tx: &mut SqliteConnection,
	invoice_id: i64,
	lines: &[NewInvoiceLine],
) -> ClResult<()> {
	for (chunk, batch) in lines.chunks(INSERT_CHUNK).enumerate() {
		let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
			"INSERT INTO invoice_lines
			 (invoice_id, line_no, service_id, description, unit, qty, unit_price,
			  discount_kind, discount_value, discount_amount, discount_description,
			  net, vat_code, vat_rate_bp, vat, gross)
			 VALUES {}",
			values_clause(batch.len(), 16)
		)));
		for (i, line) in batch.iter().enumerate() {
			// `line_no` is NAV's `lineNumber` and must stay 1..n contiguous in slice order.
			let no = chunk * INSERT_CHUNK + i + 1;
			q = q
				.bind(invoice_id)
				.bind(i64::try_from(no).unwrap_or(i64::MAX))
				.bind(line.service_id)
				.bind(&line.description)
				.bind(&line.unit)
				.bind(line.qty.0)
				.bind(line.unit_price.0)
				.bind(line.discount_kind.map(saas_invoice::store::DiscountKind::as_str))
				.bind(line.discount_value)
				.bind(line.discount_amount.0)
				.bind(&line.discount_description)
				.bind(line.net.0)
				.bind(line.vat_code.as_str())
				.bind(line.vat_rate_bp)
				.bind(line.vat.0)
				.bind(line.gross.0);
		}
		q.execute(&mut *tx).await.db()?;
	}
	Ok(())
}

/// Replaces the whole line set of an invoice that is still a draft.
async fn replace_lines(
	tx: &mut SqliteConnection,
	invoice_id: i64,
	lines: &[NewInvoiceLine],
) -> ClResult<()> {
	sqlx::query("DELETE FROM invoice_lines WHERE invoice_id = ?")
		.bind(invoice_id)
		.execute(&mut *tx)
		.await
		.db()?;
	insert_lines(tx, invoice_id, lines).await
}

/// Replaces the VAT summary of an invoice that is still a draft.
async fn replace_groups(
	tx: &mut SqliteConnection,
	invoice_id: i64,
	groups: &[InvoiceVatGroup],
) -> ClResult<()> {
	sqlx::query("DELETE FROM invoice_vat_groups WHERE invoice_id = ?")
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

/// `DRAFT` -> `ISSUED` with the number, dates, rate and frozen buyer snapshot. Scoped
/// `AND status = 'DRAFT'`, so a racing second issue finds no row rather than renumbering one.
async fn freeze(
	tx: &mut SqliteConnection,
	id: i64,
	number: &str,
	issue: &IssueInvoice,
) -> ClResult<Option<Invoice>> {
	sqlx::query(
		"UPDATE invoices SET
			status = 'ISSUED', number = ?, series_code = ?, series_year = ?, issued_at = ?,
			fulfilment_date = ?, due_date = ?, rate_date = ?, rate_source = ?,
			huf_rate_e6 = ?, rate_e6 = COALESCE(?, rate_e6),
			net = ?, vat = ?, gross = ?, vat_note = ?,
			buyer_kind = ?, buyer_name = ?, buyer_country = ?, buyer_tax_number = ?,
			buyer_eu_vat_id = ?, buyer_group_tax_no = ?, buyer_postcode = ?,
			buyer_city = ?, buyer_street = ?,
			buyer_vies_request_id = ?, buyer_vies_checked_at = ?,
			version = version + 1, updated_at = ?
		 WHERE id = ? AND status = 'DRAFT'
		 RETURNING *",
	)
	.bind(number)
	.bind(&issue.series_code)
	.bind(issue.series_year)
	.bind(issue.issued_at.0)
	.bind(&issue.fulfilment_date)
	.bind(&issue.due_date)
	.bind(&issue.rate_date)
	.bind(issue.rate_source.map(saas_invoice::store::RateSource::as_str))
	.bind(issue.huf_rate_e6)
	.bind(issue.rate_e6)
	.bind(issue.net.0)
	.bind(issue.vat.0)
	.bind(issue.gross.0)
	.bind(&issue.vat_note)
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
	.bind(issue.issued_at.0)
	.bind(id)
	.fetch_optional(&mut *tx)
	.await
	.one(invoice_row)
}

/// The `invoices` UPDATE behind `update_draft`, as a free function so `replace_draft_lines`
/// can run it inside its own transaction. `None` means the row is absent or already issued.
async fn update_draft_row<'e, E: sqlx::Executor<'e, Database = sqlx::Sqlite>>(
	ex: E,
	id: i64,
	p: &InvoicePatch,
) -> ClResult<Option<Invoice>> {
	sqlx::query(
		"UPDATE invoices SET
			billing_party_id = COALESCE(?, billing_party_id),
			payment_method   = COALESCE(?, payment_method),
			fulfilment_date  = CASE ? WHEN 1 THEN ? ELSE fulfilment_date END,
			due_date         = CASE ? WHEN 1 THEN ? ELSE due_date END,
			notes            = CASE ? WHEN 1 THEN ? ELSE notes END,
			currency         = COALESCE(?, currency),
			rate_e6          = COALESCE(?, rate_e6),
			discount_value   = COALESCE(?, discount_value),
			version          = version + 1,
			updated_at       = ?
		 WHERE id = ? AND status = 'DRAFT' RETURNING *",
	)
	.bind(p.billing_party_id)
	.bind(p.payment_method.map(saas_invoice::store::PaymentMethod::as_str))
	.bind(!p.fulfilment_date.is_undefined())
	.bind(p.fulfilment_date.value())
	.bind(!p.due_date.is_undefined())
	.bind(p.due_date.value())
	.bind(!p.notes.is_undefined())
	.bind(p.notes.value())
	.bind(p.currency.as_ref().map(CurrencyCode::as_str))
	.bind(p.rate_e6)
	.bind(p.discount_value)
	.bind(Timestamp::now().0)
	.bind(id)
	.fetch_optional(ex)
	.await
	.one(invoice_row)
}

/// Which unique violation [`insert_draft`] is about to see on the draft path.
fn conflict_for(new: &NewInvoice) -> fn() -> Error {
	if new.request_id.is_some() { duplicate_request_id } else { generic_conflict }
}

/// Inserts a draft `invoices` row. Shared by `create_draft` and the storno path.
///
/// `on_conflict` names the answer for a unique violation, because the two callers hit
/// different constraints: `UNIQUE (tenant_id, request_id)` on the draft path and
/// `idx_invoice_storno_once` on the storno path.
async fn insert_draft(
	tx: &mut SqliteConnection,
	new: &NewInvoice,
	on_conflict: fn() -> Error,
) -> ClResult<Invoice> {
	let now = Timestamp::now();
	let uid = InvoiceId::generate();
	let row = sqlx::query(
		"INSERT INTO invoices
		 (uid, request_id, tenant_id, seller_id, billing_party_id, kind,
		  original_invoice_id, currency, rate_e6, payment_method, notes,
		  discount_kind, discount_value, created_at, updated_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		 RETURNING *",
	)
	.bind(uid.as_str())
	.bind(&new.request_id)
	.bind(new.tenant_id)
	.bind(new.seller_id)
	.bind(new.billing_party_id)
	.bind(new.kind.as_str())
	.bind(new.original_invoice_id)
	.bind(new.currency.as_str())
	.bind(new.rate_e6)
	.bind(new.payment_method.as_str())
	.bind(&new.notes)
	.bind(new.discount_kind.map(saas_invoice::store::DiscountKind::as_str))
	.bind(new.discount_value)
	.bind(now.0)
	.bind(now.0)
	.fetch_one(&mut *tx)
	.await
	.map_err(|e| match &e {
		sqlx::Error::Database(db) if db.is_unique_violation() => on_conflict(),
		_ => crate::util::map_db(&e),
	})?;
	invoice_row(&row)
}

/// The shared body of `mark_paid` and `mark_stornoed`.
///
/// The predecessor is `'ISSUED'` unconditionally, and there is no `match` left to get wrong:
/// the trait has no method that could name `-> DRAFT`, so `update_draft`,
/// `replace_draft_lines` and `delete_draft` cannot be reopened on a numbered invoice by any
/// argument a caller is able to pass.
async fn mark_status(store: &SqliteStore, id: i64, status: InvoiceStatus) -> ClResult<bool> {
	let res = sqlx::query(
		"UPDATE invoices SET status = ?, updated_at = ? WHERE id = ? AND status = 'ISSUED'",
	)
	.bind(status.as_str())
	.bind(Timestamp::now().0)
	.bind(id)
	.execute(store.writer())
	.await
	.db()?;
	Ok(res.rows_affected() == 1)
}

#[async_trait]
impl InvoiceStore for SqliteStore {
	// -- seller

	async fn seller_by_id(&self, id: i64) -> ClResult<Option<Seller>> {
		sqlx::query("SELECT * FROM sellers WHERE id = ?")
			.bind(id)
			.fetch_optional(self.reader())
			.await
			.one(seller_row)
	}

	async fn put_seller(&self, s: &Seller) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO sellers
			 (id, name, country, tax_number, group_member_tax_no, eu_vat_id, postcode, city,
			  street, bank_account, bank_name, nav_base_url, nav_login, small_business,
			  vat_scheme, series_code, created_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
			 ON CONFLICT (id) DO UPDATE SET
				name = excluded.name, country = excluded.country,
				tax_number = excluded.tax_number,
				group_member_tax_no = excluded.group_member_tax_no,
				eu_vat_id = excluded.eu_vat_id, postcode = excluded.postcode,
				city = excluded.city, street = excluded.street,
				bank_account = excluded.bank_account, bank_name = excluded.bank_name,
				nav_base_url = excluded.nav_base_url, nav_login = excluded.nav_login,
				small_business = excluded.small_business, vat_scheme = excluded.vat_scheme,
				series_code = excluded.series_code",
		)
		.bind(s.id)
		.bind(&s.name)
		.bind(&s.country)
		.bind(&s.tax_number)
		.bind(&s.group_member_tax_no)
		.bind(&s.eu_vat_id)
		.bind(&s.postcode)
		.bind(&s.city)
		.bind(&s.street)
		.bind(&s.bank_account)
		.bind(&s.bank_name)
		.bind(&s.nav_base_url)
		.bind(&s.nav_login)
		.bind(s.small_business)
		.bind(&s.vat_scheme)
		.bind(&s.series_code)
		.bind(s.created_at.0)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	// -- services

	async fn sync_services(&self, defs: &[ServiceDef]) -> ClResult<()> {
		let now = Timestamp::now();
		let mut tx = self.write_tx().await?;
		for d in defs {
			// `active` is deliberately absent from the SET list: withdrawing a service from
			// the consumer's code must not resurrect it, and must never delete the row that
			// issued invoice lines still point at.
			let uid = ServiceId::generate();
			sqlx::query(
				"INSERT INTO services
				 (uid, code, name, description, unit, unit_price, vat_code, created_at, updated_at)
				 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
				 ON CONFLICT (code) DO UPDATE SET
					name = excluded.name, description = excluded.description,
					unit = excluded.unit, unit_price = excluded.unit_price,
					vat_code = excluded.vat_code, updated_at = excluded.updated_at",
			)
			.bind(uid.as_str())
			.bind(&d.code)
			.bind(&d.name)
			.bind(&d.description)
			.bind(&d.unit)
			.bind(d.unit_price.0)
			.bind(d.vat_code.as_str())
			.bind(now.0)
			.bind(now.0)
			.execute(&mut *tx)
			.await
			.db()?;
		}
		tx.commit().await.db()?;
		Ok(())
	}

	async fn service_by_code(&self, code: &str) -> ClResult<Option<Service>> {
		sqlx::query("SELECT * FROM services WHERE code = ?")
			.bind(code)
			.fetch_optional(self.reader())
			.await
			.one(service_row)
	}

	async fn services_by_codes(&self, codes: &[&str]) -> ClResult<Vec<Service>> {
		if codes.is_empty() {
			return Ok(Vec::new());
		}
		let sql = format!(
			"SELECT * FROM services WHERE active AND code IN {}",
			values_clause(1, codes.len())
		);
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
		for code in codes {
			q = q.bind(*code);
		}
		q.fetch_all(self.reader()).await.all(service_row)
	}

	async fn service_by_uid(&self, uid: &ServiceId) -> ClResult<Option<Service>> {
		sqlx::query("SELECT * FROM services WHERE uid = ?")
			.bind(uid.as_str())
			.fetch_optional(self.reader())
			.await
			.one(service_row)
	}

	async fn create_service(&self, d: &ServiceDef) -> ClResult<Service> {
		let now = Timestamp::now();
		let uid = ServiceId::generate();
		let row = sqlx::query(
			"INSERT INTO services
			 (uid, code, name, description, unit, unit_price, vat_code, created_at, updated_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
		)
		.bind(uid.as_str())
		.bind(&d.code)
		.bind(&d.name)
		.bind(&d.description)
		.bind(&d.unit)
		.bind(d.unit_price.0)
		.bind(d.vat_code.as_str())
		.bind(now.0)
		.bind(now.0)
		.fetch_one(self.writer())
		.await
		.map_err(|e| unique_as_conflict(&e, "service code already exists"))?;
		service_row(&row)
	}

	async fn update_service(&self, uid: &ServiceId, p: &ServicePatch) -> ClResult<Option<Service>> {
		sqlx::query(
			"UPDATE services SET
				code        = CASE ? WHEN 1 THEN ? ELSE code END,
				name        = COALESCE(?, name),
				description = CASE ? WHEN 1 THEN ? ELSE description END,
				unit        = COALESCE(?, unit),
				unit_price  = COALESCE(?, unit_price),
				vat_code    = COALESCE(?, vat_code),
				active      = COALESCE(?, active),
				updated_at  = ?
			 WHERE uid = ? RETURNING *",
		)
		.bind(!p.code.is_undefined())
		.bind(p.code.value())
		.bind(&p.name)
		.bind(!p.description.is_undefined())
		.bind(p.description.value())
		.bind(&p.unit)
		.bind(p.unit_price.map(|m| m.0))
		.bind(p.vat_code.map(saas_invoice::VatCode::as_str))
		.bind(p.active)
		.bind(Timestamp::now().0)
		.bind(uid.as_str())
		.fetch_optional(self.writer())
		.await
		.map_err(|e| unique_as_conflict(&e, "service code already exists"))?
		.as_ref()
		.map(service_row)
		.transpose()
	}

	async fn list_services(&self, active_only: bool, limit: i64) -> ClResult<Vec<Service>> {
		sqlx::query("SELECT * FROM services WHERE (? = 0 OR active = 1) ORDER BY name LIMIT ?")
			.bind(active_only)
			.bind(limit)
			.fetch_all(self.reader())
			.await
			.all(service_row)
	}

	// -- billing parties

	async fn create_party(&self, tenant_id: i64, p: &PartyPatch) -> ClResult<BillingParty> {
		let now = Timestamp::now();
		let mut tx = self.write_tx().await?;

		if p.is_default == Some(true) {
			clear_default_party(&mut tx, tenant_id).await?;
		}

		let uid = PartyId::generate();
		let row = sqlx::query(
			"INSERT INTO billing_parties
			 (uid, tenant_id, kind, name, country, tax_number, eu_vat_id, group_tax_no,
			  postcode, city, street, email, is_default, created_at, updated_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
		)
		.bind(uid.as_str())
		.bind(tenant_id)
		.bind(p.kind.map(saas_invoice::store::PartyKind::as_str))
		.bind(&p.name)
		.bind(&p.country)
		.bind(p.tax_number.value())
		.bind(p.eu_vat_id.value())
		.bind(p.group_tax_no.value())
		.bind(p.postcode.value())
		.bind(p.city.value())
		.bind(p.street.value())
		.bind(p.email.value())
		.bind(p.is_default.unwrap_or(false))
		.bind(now.0)
		.bind(now.0)
		.fetch_one(&mut *tx)
		.await
		.map_err(|e| unique_as_conflict(&e, "a billing party with this tax number exists"))?;
		let party = party_row(&row)?;

		tx.commit().await.db()?;
		Ok(party)
	}

	async fn party_by_uid(&self, tenant_id: i64, uid: &PartyId) -> ClResult<Option<BillingParty>> {
		sqlx::query("SELECT * FROM billing_parties WHERE tenant_id = ? AND uid = ?")
			.bind(tenant_id)
			.bind(uid.as_str())
			.fetch_optional(self.reader())
			.await
			.one(party_row)
	}

	async fn party_by_id(&self, id: i64) -> ClResult<Option<BillingParty>> {
		sqlx::query("SELECT * FROM billing_parties WHERE id = ?")
			.bind(id)
			.fetch_optional(self.reader())
			.await
			.one(party_row)
	}

	async fn update_party(
		&self,
		tenant_id: i64,
		uid: &PartyId,
		p: &PartyPatch,
	) -> ClResult<Option<BillingParty>> {
		let mut tx = self.write_tx().await?;

		if p.is_default == Some(true) {
			clear_default_party(&mut tx, tenant_id).await?;
		}

		let party: Option<BillingParty> = sqlx::query(
			"UPDATE billing_parties SET
				kind         = COALESCE(?, kind),
				name         = COALESCE(?, name),
				country      = COALESCE(?, country),
				tax_number   = CASE ? WHEN 1 THEN ? ELSE tax_number END,
				eu_vat_id    = CASE ? WHEN 1 THEN ? ELSE eu_vat_id END,
				group_tax_no = CASE ? WHEN 1 THEN ? ELSE group_tax_no END,
				postcode     = CASE ? WHEN 1 THEN ? ELSE postcode END,
				city         = CASE ? WHEN 1 THEN ? ELSE city END,
				street       = CASE ? WHEN 1 THEN ? ELSE street END,
				email        = CASE ? WHEN 1 THEN ? ELSE email END,
				is_default   = COALESCE(?, is_default),
				updated_at   = ?
			 WHERE tenant_id = ? AND uid = ? RETURNING *",
		)
		.bind(p.kind.map(saas_invoice::store::PartyKind::as_str))
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
		.bind(p.is_default)
		.bind(Timestamp::now().0)
		.bind(tenant_id)
		.bind(uid.as_str())
		.fetch_optional(&mut *tx)
		.await
		.map_err(|e| unique_as_conflict(&e, "a billing party with this tax number exists"))?
		.as_ref()
		.map(party_row)
		.transpose()?;

		// Only commit when the row was actually ours: `clear_default_party` above has already
		// run, so committing a miss would leave the tenant with no default at all. Dropping
		// `tx` rolls back, which is what the miss path wants.
		if party.is_some() {
			tx.commit().await.db()?;
		}
		Ok(party)
	}

	async fn delete_party(&self, tenant_id: i64, uid: &PartyId) -> ClResult<bool> {
		let done = sqlx::query("DELETE FROM billing_parties WHERE tenant_id = ? AND uid = ?")
			.bind(tenant_id)
			.bind(uid.as_str())
			.execute(self.writer())
			.await
			.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn list_parties(&self, tenant_id: i64, limit: i64) -> ClResult<Vec<BillingParty>> {
		sqlx::query(
			"SELECT * FROM billing_parties
			 WHERE tenant_id = ? ORDER BY is_default DESC, name LIMIT ?",
		)
		.bind(tenant_id)
		.bind(limit)
		.fetch_all(self.reader())
		.await
		.all(party_row)
	}

	async fn default_party(&self, tenant_id: i64) -> ClResult<Option<BillingParty>> {
		sqlx::query("SELECT * FROM billing_parties WHERE tenant_id = ? AND is_default = 1")
			.bind(tenant_id)
			.fetch_optional(self.reader())
			.await
			.one(party_row)
	}

	// -- invoices: draft

	async fn create_draft(&self, new: &NewInvoice) -> ClResult<Invoice> {
		let mut tx = self.write_tx().await?;
		let invoice = insert_draft(&mut tx, new, conflict_for(new)).await?;
		tx.commit().await.db()?;
		Ok(invoice)
	}

	async fn create_draft_full(
		&self,
		new: &NewInvoice,
		priced: &Priced,
		patch: Option<&InvoicePatch>,
	) -> ClResult<Invoice> {
		let mut tx = self.write_tx().await?;

		let invoice = match insert_draft(&mut tx, new, conflict_for(new)).await {
			Ok(invoice) => invoice,
			// `UNIQUE (tenant_id, request_id)`: a concurrent caller took the same idempotency
			// key between this caller's lookup and this insert. Its invoice is the right
			// answer, not a 409 — that is what makes the retry-by-machines guarantee hold.
			Err(Error::Conflict(msg)) => {
				tx.rollback().await.db()?;
				let Some(request_id) = &new.request_id else {
					return Err(Error::conflict(msg));
				};
				return self
					.invoice_by_request_id(new.tenant_id, request_id)
					.await?
					.ok_or_else(|| Error::conflict(msg));
			}
			Err(e) => return Err(e),
		};

		replace_lines(&mut tx, invoice.id, &priced.lines).await?;
		replace_groups(&mut tx, invoice.id, &priced.groups).await?;
		if let Some(p) = patch {
			update_draft_row(&mut *tx, invoice.id, p).await?.ok_or_else(not_a_draft)?;
		}

		// Totals last, so the row it returns already carries the patch. The `status` predicate
		// cannot fail — the row was inserted DRAFT in this transaction — but it is what keeps
		// the module doc's blanket claim true, and `fetch_one` makes an impossible miss loud.
		let row = sqlx::query(
			"UPDATE invoices SET net = ?, vat = ?, gross = ?, updated_at = ?
			 WHERE id = ? AND status = 'DRAFT' RETURNING *",
		)
		.bind(priced.net.0)
		.bind(priced.vat.0)
		.bind(priced.gross.0)
		.bind(Timestamp::now().0)
		.bind(invoice.id)
		.fetch_one(&mut *tx)
		.await
		.db()?;
		let invoice = invoice_row(&row)?;

		tx.commit().await.db()?;
		Ok(invoice)
	}

	async fn invoice_by_request_id(
		&self,
		tenant_id: i64,
		request_id: &str,
	) -> ClResult<Option<Invoice>> {
		sqlx::query("SELECT * FROM invoices WHERE tenant_id = ? AND request_id = ?")
			.bind(tenant_id)
			.bind(request_id)
			.fetch_optional(self.reader())
			.await
			.one(invoice_row)
	}

	async fn invoice_by_uid(
		&self,
		tenant_id: Option<i64>,
		uid: &InvoiceId,
	) -> ClResult<Option<Invoice>> {
		sqlx::query("SELECT * FROM invoices WHERE uid = ? AND (? IS NULL OR tenant_id = ?)")
			.bind(uid.as_str())
			.bind(tenant_id)
			.bind(tenant_id)
			.fetch_optional(self.reader())
			.await
			.one(invoice_row)
	}

	async fn invoice_by_id(&self, id: i64) -> ClResult<Option<Invoice>> {
		sqlx::query("SELECT * FROM invoices WHERE id = ?")
			.bind(id)
			.fetch_optional(self.reader())
			.await
			.one(invoice_row)
	}

	async fn storno_of(&self, original_id: i64) -> ClResult<Option<Invoice>> {
		sqlx::query("SELECT * FROM invoices WHERE original_invoice_id = ? AND kind = 'STORNO'")
			.bind(original_id)
			.fetch_optional(self.reader())
			.await
			.one(invoice_row)
	}

	async fn update_draft(&self, id: i64, p: &InvoicePatch) -> ClResult<Option<Invoice>> {
		update_draft_row(self.writer(), id, p).await
	}

	/// The one write in this file with no `status = 'DRAFT'` predicate, and deliberately:
	/// `notes` is the single column `api-surface.md` §5.5 leaves writable after issue.
	/// `STORNOED` is included on purpose — a cancelled invoice's note is still a note.
	/// The `version` bump is what keeps the note and the rendered PDF in step: see
	/// `put_invoice_document`.
	async fn update_notes(&self, id: i64, notes: Option<&str>) -> ClResult<Option<Invoice>> {
		sqlx::query(
			"UPDATE invoices SET notes = ?, version = version + 1, updated_at = ?
			 WHERE id = ? RETURNING *",
		)
		.bind(notes)
		.bind(Timestamp::now().0)
		.bind(id)
		.fetch_optional(self.writer())
		.await
		.one(invoice_row)
	}

	async fn replace_draft_lines(
		&self,
		id: i64,
		patch: Option<&InvoicePatch>,
		priced: &Priced,
		expected_version: i64,
	) -> ClResult<bool> {
		let mut tx = self.write_tx().await?;

		// Totals first, and the transaction's only gate: scoped to DRAFT and to
		// `expected_version`, so an issued invoice and one another writer edited both match
		// nothing. Before `update_draft_row`, which bumps `version` and defeats the comparison.
		let done = sqlx::query(
			"UPDATE invoices SET net = ?, vat = ?, gross = ?, version = version + 1, updated_at = ?
			 WHERE id = ? AND status = 'DRAFT' AND version = ?",
		)
		.bind(priced.net.0)
		.bind(priced.vat.0)
		.bind(priced.gross.0)
		.bind(Timestamp::now().0)
		.bind(id)
		.bind(expected_version)
		.execute(&mut *tx)
		.await
		.db()?;

		if done.rows_affected() == 0 {
			return Ok(false);
		}

		if let Some(p) = patch
			&& update_draft_row(&mut *tx, id, p).await?.is_none()
		{
			return Ok(false);
		}

		replace_lines(&mut tx, id, &priced.lines).await?;
		replace_groups(&mut tx, id, &priced.groups).await?;

		tx.commit().await.db()?;
		Ok(true)
	}

	async fn invoice_lines(&self, invoice_id: i64) -> ClResult<Vec<InvoiceLine>> {
		sqlx::query("SELECT * FROM invoice_lines WHERE invoice_id = ? ORDER BY line_no")
			.bind(invoice_id)
			.fetch_all(self.reader())
			.await
			.all(line_row)
	}

	async fn invoice_vat_groups(&self, invoice_id: i64) -> ClResult<Vec<InvoiceVatGroup>> {
		sqlx::query("SELECT * FROM invoice_vat_groups WHERE invoice_id = ? ORDER BY vat_code")
			.bind(invoice_id)
			.fetch_all(self.reader())
			.await
			.all(vat_group_row)
	}

	async fn invoices_by_ids(&self, ids: &[i64]) -> ClResult<Vec<Invoice>> {
		self.in_ids("SELECT * FROM invoices WHERE id IN", "", ids, invoice_row).await
	}

	async fn invoice_lines_for(&self, ids: &[i64]) -> ClResult<Vec<InvoiceLine>> {
		self.in_ids(
			"SELECT * FROM invoice_lines WHERE invoice_id IN",
			" ORDER BY invoice_id, line_no",
			ids,
			line_row,
		)
		.await
	}

	async fn invoice_vat_groups_for(&self, ids: &[i64]) -> ClResult<Vec<InvoiceVatGroup>> {
		self.in_ids(
			"SELECT * FROM invoice_vat_groups WHERE invoice_id IN",
			" ORDER BY invoice_id, vat_code",
			ids,
			vat_group_row,
		)
		.await
	}

	async fn delete_draft(&self, id: i64) -> ClResult<bool> {
		let done = sqlx::query("DELETE FROM invoices WHERE id = ? AND status = 'DRAFT'")
			.bind(id)
			.execute(self.writer())
			.await
			.db()?;
		Ok(done.rows_affected() > 0)
	}

	async fn sweep_drafts(&self, cutoff: Timestamp) -> ClResult<u64> {
		// `updated_at`, not `created_at`: the caller means *abandoned*, and a cart opened a
		// month ago and edited this morning is not. `idx_invoice_draft_age` is keyed on it too.
		let done = sqlx::query("DELETE FROM invoices WHERE status = 'DRAFT' AND updated_at < ?")
			.bind(cutoff.0)
			.execute(self.writer())
			.await
			.db()?;
		Ok(done.rows_affected())
	}

	async fn issued_without_document(&self, limit: i64) -> ClResult<Vec<i64>> {
		// `ORDER BY` the last render attempt, not by id: lowest-id-first let 50 permanently
		// unrenderable invoices occupy the whole `invoice.pdf_sweep_batch` cap on every daily
		// tick. Matched on the payload, not `dedup_key`, because an unkeyed enqueue has no key;
		// `idx_job_kind_payload` serves the subquery, so this does not scan `jobs` per row.
		sqlx::query_scalar(
			r#"SELECT i.id FROM invoices i
			 LEFT JOIN invoice_documents d ON d.invoice_id = i.id
			 WHERE i.number IS NOT NULL AND d.invoice_id IS NULL
			 ORDER BY (SELECT COALESCE(MAX(j.created_at), 0) FROM jobs j
				   WHERE j.kind = ?
				     AND j.payload = '{"invoiceId":' || i.id || '}') ASC,
				  i.id ASC
			 LIMIT ?"#,
		)
		.bind(saas_invoice::issue::KIND_RENDER_PDF)
		.bind(limit)
		.fetch_all(self.reader())
		.await
		.db()
	}

	// -- invoices: issue

	async fn issue(
		&self,
		id: i64,
		issue: &IssueInvoice,
		expected_version: i64,
	) -> ClResult<Invoice> {
		let mut tx = self.write_tx().await?;

		// Two causes, two codes: an invoice that is no longer a draft is `not_a_draft`, an
		// invoice that moved under the caller is retryable and says so.
		let seller_id: i64 = sqlx::query_scalar(
			"SELECT seller_id FROM invoices WHERE id = ? AND status = 'DRAFT' AND version = ?",
		)
		.bind(id)
		.bind(expected_version)
		.fetch_optional(&mut *tx)
		.await
		.db()?
		.ok_or_else(|| {
			Error::coded(
				StatusCode::CONFLICT,
				"E-INV-CHANGED",
				"the draft changed while it was being issued",
			)
		})?;

		// The re-priced lines and the VAT summary both go in while the row is still a draft,
		// before `freeze` below flips the status. The probe above is the gate that makes that
		// safe — the child-table helpers carry no predicate of their own.
		replace_lines(&mut tx, id, &issue.lines).await?;
		replace_groups(&mut tx, id, &issue.groups).await?;

		let number =
			allocate_number(&mut tx, seller_id, &issue.series_code, issue.series_year).await?;
		let invoice = freeze(&mut tx, id, &number, issue).await?.ok_or_else(not_a_draft)?;

		tx.commit().await.db()?;
		Ok(invoice)
	}

	async fn storno(
		&self,
		original_id: i64,
		new: &NewInvoice,
		issue: &IssueInvoice,
	) -> ClResult<Invoice> {
		let mut tx = self.write_tx().await?;

		let status: InvoiceStatus =
			sqlx::query_scalar::<_, String>("SELECT status FROM invoices WHERE id = ?")
				.bind(original_id)
				.fetch_optional(&mut *tx)
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

		// `idx_invoice_storno_once` is what actually guarantees at-most-once; the status
		// check above only turns the second attempt into a readable 409 sooner. The loser of
		// a concurrent pair gets past it and lands here.
		let draft = insert_draft(&mut tx, new, already_stornoed).await?;

		insert_lines(&mut tx, draft.id, &issue.lines).await?;
		replace_groups(&mut tx, draft.id, &issue.groups).await?;

		let number =
			allocate_number(&mut tx, new.seller_id, &issue.series_code, issue.series_year).await?;
		let storno = freeze(&mut tx, draft.id, &number, issue).await?.ok_or_else(not_a_draft)?;

		// The predecessor predicate is hardcoded because it is the transition, not an argument;
		// `PAID` is in the list because `storno::run` accepts a paid original. The
		// `rows_affected` check is an invariant check, not a race guard — kept because
		// discarding it would commit a frozen STORNO whose original is still `ISSUED`, and both
		// rows are immutable.
		let flipped = sqlx::query(
			"UPDATE invoices SET status = 'STORNOED', updated_at = ?
			  WHERE id = ? AND status IN ('ISSUED','PAID')",
		)
		.bind(Timestamp::now().0)
		.bind(original_id)
		.execute(&mut *tx)
		.await
		.db()?;
		if flipped.rows_affected() == 0 {
			return Err(Error::internal(format!(
				"storno {}: original invoice {original_id} left the issued state mid-transaction",
				storno.id
			)));
		}

		tx.commit().await.db()?;
		Ok(storno)
	}

	// -- invoices: read and the three permitted post-issue writes

	async fn list_invoices(
		&self,
		tenant_id: i64,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Invoice>> {
		sqlx::query(
			"SELECT * FROM invoices
			 WHERE tenant_id = ? AND (? IS NULL OR id < ?)
			 ORDER BY id DESC LIMIT ?",
		)
		.bind(tenant_id)
		.bind(before_id)
		.bind(before_id)
		.bind(limit)
		.fetch_all(self.reader())
		.await
		.all(invoice_row)
	}

	async fn list_invoices_page(
		&self,
		tenant_id: i64,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<ListedInvoice>> {
		// `i.*` first, so `invoice_row`'s `uid` still resolves to the invoice's own. The storno
		// join cannot duplicate a row: `idx_invoice_storno_once` is unique.
		sqlx::query(
			"SELECT i.*, p.uid AS party_uid, o.uid AS original_uid, s.uid AS storno_uid
			 FROM invoices i
			 LEFT JOIN billing_parties p ON p.id = i.billing_party_id
			 LEFT JOIN invoices o ON o.id = i.original_invoice_id
			 LEFT JOIN invoices s
			        ON s.original_invoice_id = i.id AND s.kind = 'STORNO'
			       AND i.status = 'STORNOED'
			 WHERE i.tenant_id = ? AND (? IS NULL OR i.id < ?)
			 ORDER BY i.id DESC LIMIT ?",
		)
		.bind(tenant_id)
		.bind(before_id)
		.bind(before_id)
		.bind(limit)
		.fetch_all(self.reader())
		.await
		.all(listed_invoice_row)
	}

	async fn mark_paid(&self, id: i64) -> ClResult<bool> {
		mark_status(self, id, InvoiceStatus::Paid).await
	}

	async fn mark_stornoed(&self, id: i64) -> ClResult<bool> {
		mark_status(self, id, InvoiceStatus::Stornoed).await
	}

	async fn set_paid(
		&self,
		id: i64,
		paid_amount: Money,
		paid_at: Option<Timestamp>,
	) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE invoices SET paid_amount = ?, paid_at = ?, updated_at = ?
			 WHERE id = ? AND status IN ('ISSUED','PAID')",
		)
		.bind(paid_amount.0)
		.bind(paid_at.map(|t| t.0))
		.bind(Timestamp::now().0)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	// -- documents

	async fn put_invoice_document(&self, doc: &InvoiceDocument, version: i64) -> ClResult<bool> {
		// `SELECT … WHERE version = ?`, not a plain `VALUES`: the render reads the notes,
		// compiles typst for seconds, then lands here, and a note edit inside that window
		// froze the pre-edit note into a PDF served `immutable`.
		let res = sqlx::query(
			"INSERT INTO invoice_documents
			 (invoice_id, kind, sha256, bytes, template_version, rendered_at)
			 SELECT ?, 'PDF', ?, ?, ?, ? FROM invoices WHERE id = ? AND version = ?
			 -- The first render wins. `003_invoice.sql` calls this row the rendered PDF as
			 -- immutable evidence, and an upsert made a re-driven `RENDER_PDF` after a
			 -- template change replace the hash of a PDF already delivered to the buyer,
			 -- orphaning the old file on disk. A no-op here means already rendered.
			 ON CONFLICT (invoice_id, kind) DO NOTHING",
		)
		.bind(doc.invoice_id)
		.bind(&doc.sha256)
		.bind(doc.bytes)
		.bind(&doc.template_version)
		.bind(doc.rendered_at.0)
		.bind(doc.invoice_id)
		.bind(version)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn invoice_document(&self, invoice_id: i64) -> ClResult<Option<InvoiceDocument>> {
		sqlx::query("SELECT * FROM invoice_documents WHERE invoice_id = ? AND kind = 'PDF'")
			.bind(invoice_id)
			.fetch_optional(self.reader())
			.await
			.one(document_row)
	}

	// -- currency

	async fn currency_get(&self, code: &str) -> ClResult<Option<Currency>> {
		let row: Option<CurrencyRow> = sqlx::query_as(
			"SELECT code, price_round_step, mode, fixed_rate_e6, fee_bp, enabled \
			 FROM currencies WHERE code = ?",
		)
		.bind(code)
		.fetch_optional(self.reader())
		.await
		.db()?;
		Ok(row.map(currency_of))
	}

	async fn currency_list(&self, all: bool) -> ClResult<Vec<Currency>> {
		let rows: Vec<CurrencyRow> = sqlx::query_as(
			"SELECT code, price_round_step, mode, fixed_rate_e6, fee_bp, enabled \
			 FROM currencies WHERE ? = 1 OR enabled = 1 ORDER BY code",
		)
		.bind(i64::from(all))
		.fetch_all(self.reader())
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
			 WHERE pair = ? AND source = ? AND date <= ? ORDER BY date DESC LIMIT 1",
		)
		.bind(pair)
		.bind(source)
		.bind(on)
		.fetch_optional(self.reader())
		.await
		.db()
	}

	// -- MNB rate fetch

	async fn mnb_currencies(&self, base: &str) -> ClResult<Vec<String>> {
		sqlx::query_scalar("SELECT code FROM currencies WHERE enabled = 1 AND code <> ?")
			.bind(base)
			.fetch_all(self.reader())
			.await
			.db()
	}

	async fn mnb_max_date(&self, pair: &str) -> ClResult<Option<String>> {
		sqlx::query_scalar("SELECT max(date) FROM currency_rates WHERE pair = ? AND source = ?")
			.bind(pair)
			.bind(mnb::SOURCE)
			.fetch_one(self.reader())
			.await
			.db()
	}

	async fn mnb_upsert_rates(&self, pair: &str, rows: &[(String, i64)]) -> ClResult<()> {
		let mut tx = self.write_tx().await?;
		for (date, rate_e6) in rows {
			sqlx::query(
				"INSERT INTO currency_rates (pair, date, source, rate_e6, fetched_at) \
				 VALUES (?, ?, ?, ?, ?) ON CONFLICT (pair, date, source) DO UPDATE SET \
				 rate_e6 = excluded.rate_e6, fetched_at = excluded.fetched_at",
			)
			.bind(pair)
			.bind(date)
			.bind(mnb::SOURCE)
			.bind(rate_e6)
			.bind(Timestamp::now().0)
			.execute(&mut *tx)
			.await
			.db()?;
		}
		tx.commit().await.db()?;
		Ok(())
	}

	// -- VIES

	async fn vies_cached(&self, full: &str, ttl: i64) -> ClResult<Option<ViesResult>> {
		let row: Option<CachedRow> = sqlx::query_as(
			"SELECT valid, name, address, request_id, checked_at FROM vies_checks \
			 WHERE eu_vat_id = ? AND checked_at > ?",
		)
		.bind(full)
		.bind(Timestamp::now().0 - ttl)
		.fetch_optional(self.reader())
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
			 VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (eu_vat_id) DO UPDATE SET \
			 valid = excluded.valid, name = excluded.name, address = excluded.address, \
			 request_id = excluded.request_id, checked_at = excluded.checked_at",
		)
		.bind(&r.eu_vat_id)
		.bind(i64::from(r.valid))
		.bind(r.name.as_deref())
		.bind(r.address.as_deref())
		.bind(r.request_id.as_deref())
		.bind(r.checked_at.0)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	// -- tenants

	async fn tenant_billing_currency(&self, tenant_id: i64) -> ClResult<Option<CurrencyCode>> {
		Ok(sqlx::query_scalar("SELECT billing_currency FROM tenants WHERE id = ?")
			.bind(tenant_id)
			.fetch_optional(self.reader())
			.await
			.db()?
			.flatten()
			.map(CurrencyCode::from_trusted))
	}
}

/// `idx_billing_party_default` allows one default per tenant, so promoting a party has to
/// demote the incumbent in the same transaction.
async fn clear_default_party(tx: &mut SqliteConnection, tenant_id: i64) -> ClResult<()> {
	sqlx::query("UPDATE billing_parties SET is_default = 0 WHERE tenant_id = ? AND is_default = 1")
		.bind(tenant_id)
		.execute(&mut *tx)
		.await
		.db()?;
	Ok(())
}

// vim: ts=4
