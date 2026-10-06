// SPDX-License-Identifier: MPL-2.0
//! `InvoiceStore` over SQLite. Reads go through `reader()`, writes through `conn()`, and a
//! multi-statement write through `write_tx()`.
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
//! `mintworks-invoice` carries no driver dependency, so no row type here can be decoded by derive:
//! every query binds primitives and every framework row is built by hand in the `*_row` helpers
//! below. See `util.rs` for the conversion vocabulary — in particular that `Money` and `Qty`
//! cross as `i64` and come back through `read_money` / `read_qty`, which re-apply `bounded`.

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
	MonthBucket, NewInvoice, NewInvoiceLine, OverdueBucket, PartyPatch, RevenueMonth, Seller,
	SellerVersion, SellerVersionPatch, SellerVersionStatus, Service, ServiceDef, ServicePatch,
	StatusBucket, render_number,
};
use mintworks_invoice::vies::ViesResult;
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

use crate::SqliteStore;
use crate::util::{DbExt, RowExt, RowsExt, read_discount_value, read_money, read_qty};

/// The seven `currencies` columns both currency queries select, in order.
type CurrencyRow = (String, i64, Option<i64>, String, Option<i64>, i64, i64);

/// The columns each of `invoice_summary`'s three aggregates selects, in order.
type StatusRow = (String, String, i64, i64, i64, i64, i64);
type MonthRow = (String, String, i64, i64, i64);
type OverdueRow = (String, i64, i64);

/// The `vies_checks` columns `vies_cached` selects, in order.
type CachedRow = (i64, Option<String>, Option<String>, Option<String>, i64);

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

use crate::util::unique_as_conflict;

// ---------------------------------------------------------------- row mapping
//
// Every read below is **by column name**: `SELECT *` follows the DDL's column order, which a
// migration may change, so reading by index misaligns the first time one moves.

fn seller_row(row: &SqliteRow) -> ClResult<Seller> {
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

fn seller_version_row(row: &SqliteRow) -> ClResult<SellerVersion> {
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
		small_business: row.try_get("small_business").db()?,
		vat_scheme: row.try_get("vat_scheme").db()?,
		income_regime: row.try_get("income_regime").db()?,
		expense_ratio_pct: row.try_get("expense_ratio_pct").db()?,
		regime_since: row.try_get("regime_since").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		valid_from: row.try_get::<Option<i64>, _>("valid_from").db()?.map(Timestamp),
		superseded_at: row.try_get::<Option<i64>, _>("superseded_at").db()?.map(Timestamp),
	})
}

/// The one `DRAFT` or `CURRENT` row for a seller — both are unique by a partial index, so
/// this can never have to choose between two.
async fn version_by_status<'e, E: sqlx::Executor<'e, Database = sqlx::Sqlite>>(
	ex: E,
	seller_id: i64,
	status: &str,
) -> ClResult<Option<SellerVersion>> {
	sqlx::query("SELECT * FROM seller_versions WHERE seller_id = ? AND status = ?")
		.bind(seller_id)
		.bind(status)
		.fetch_optional(ex)
		.await
		.one(seller_version_row)
}

fn party_row(row: &SqliteRow) -> ClResult<BillingParty> {
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
		is_default: row.try_get("is_default").db()?,
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

fn service_row(row: &SqliteRow) -> ClResult<Service> {
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
		note: row.try_get("note").db()?,
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

/// The `UNIQUE (org_id, request_id)` answer. [`SqliteStore::create_draft_full`] matches on
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

/// The `idx_invoice_storno_once` answer — the code `mintworks_invoice::storno` documents. Passed
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

/// Elements per `IN (…)` list. Well under SQLite's `SQLITE_MAX_VARIABLE_NUMBER` (32 766 since
/// 3.32), with room for the binds a caller adds around the list. Chunked here rather than
/// trusted to the caller: nothing but a doc comment bounded the next one.
pub(crate) const MAX_IN_LIST: usize = 1000;

impl SqliteStore {
	/// `<head> (?,?,…)<tail>` bound to `ids` and decoded with `f` — the three batched reads
	/// the audit export needs. Empty `ids` answers without a statement: `IN ()` is a syntax
	/// error.
	///
	/// `tail`'s `ORDER BY` holds **within** a chunk of [`MAX_IN_LIST`] ids only. Every caller
	/// groups the result by `invoice_id`, and one invoice's rows never straddle two chunks.
	async fn in_ids<T>(
		&self,
		head: &str,
		tail: &str,
		ids: &[i64],
		f: fn(&SqliteRow) -> ClResult<T>,
	) -> ClResult<Vec<T>> {
		let mut out = Vec::with_capacity(ids.len());
		for chunk in ids.chunks(MAX_IN_LIST) {
			let sql = format!("{head} {}{tail}", values_clause(1, chunk.len()));
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for id in chunk {
				q = q.bind(*id);
			}
			out.extend(q.fetch_all(&mut *self.reader().await?).await.all(f)?);
		}
		Ok(out)
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
			  net, vat_code, vat_rate_bp, vat, gross, note)
			 VALUES {}",
			values_clause(batch.len(), 17)
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

/// `DRAFT`/`PENDING` -> `ISSUED` (or `PAID` for `issue.paid`) with the number, dates, rate and
/// frozen buyer snapshot. Scoped to those two statuses, so a racing second issue finds no row
/// rather than renumbering one.
async fn freeze(
	tx: &mut SqliteConnection,
	id: i64,
	number: &str,
	issue: &IssueInvoice,
) -> ClResult<Option<Invoice>> {
	sqlx::query(
		"UPDATE invoices SET
			status = CASE ? WHEN 1 THEN 'PAID' ELSE 'ISSUED' END,
			paid_amount = CASE ? WHEN 1 THEN ? ELSE paid_amount END,
			paid_at = CASE ? WHEN 1 THEN ? ELSE paid_at END,
			number = ?, series_code = ?, series_year = ?, issued_at = ?,
			fulfilment_date = ?, due_date = ?, period_start = ?, period_end = ?,
			rate_date = ?, rate_source = ?,
			huf_rate_e6 = ?, rate_e6 = COALESCE(?, rate_e6),
			net = ?, vat = ?, gross = ?, vat_note = ?, seller_ver = ?,
			buyer_kind = ?, buyer_name = ?, buyer_country = ?, buyer_tax_number = ?,
			buyer_eu_vat_id = ?, buyer_group_tax_no = ?, buyer_postcode = ?,
			buyer_city = ?, buyer_street = ?,
			buyer_vies_request_id = ?, buyer_vies_checked_at = ?,
			version = version + 1, updated_at = ?
		 WHERE id = ? AND status IN ('DRAFT','PENDING')
		 RETURNING *",
	)
	.bind(issue.paid)
	.bind(issue.paid)
	.bind(issue.gross.0)
	.bind(issue.paid)
	.bind(issue.issued_at.0)
	.bind(number)
	.bind(&issue.series_code)
	.bind(issue.series_year)
	.bind(issue.issued_at.0)
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
	.bind(issue.gross.0)
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
			period_start     = CASE ? WHEN 1 THEN ? ELSE period_start END,
			period_end       = CASE ? WHEN 1 THEN ? ELSE period_end END,
			notes            = CASE ? WHEN 1 THEN ? ELSE notes END,
			currency         = COALESCE(?, currency),
			rate_e6          = COALESCE(?, rate_e6),
			discount_value   = COALESCE(?, discount_value),
			version          = version + 1,
			updated_at       = ?
		 WHERE id = ? AND status = 'DRAFT' RETURNING *",
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
/// different constraints: `UNIQUE (org_id, request_id)` on the draft path and
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
		 (uid, request_id, org_id, seller_id, billing_party_id, kind,
		  original_invoice_id, currency, rate_e6, payment_method, notes,
		  discount_kind, discount_value, created_at, updated_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
	.execute(&mut *store.conn().await?)
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
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(seller_row)
	}

	/// The nearest `sellers`-owning org at or above `org_id`. `depth` orders the walk: a
	/// business unit that owns a seller must not resolve to its parent's.
	async fn seller_for_org(&self, org_id: i64) -> ClResult<Option<Seller>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			// `depth` orders the walk below; the recursion cap is `core::MAX_ORG_DEPTH`.
			"{}
			 SELECT s.* FROM sellers s JOIN anc ON s.org_id = anc.id
			 ORDER BY anc.depth LIMIT 1",
			crate::core::ancestors("id = ?", true, true)
		)))
		.bind(org_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(seller_row)
	}

	async fn put_seller(&self, s: &Seller) -> ClResult<()> {
		let res = sqlx::query(
			// `org_id` and `uid` are insert-only, deliberately absent from the SET list: an
			// upsert that moved `org_id` would hand another org this taxpayer id, its NAV
			// credentials and its `doc_series` counter. The `WHERE` refuses that loudly.
			// `closed_at` is never written: boot re-puts every seller, which would reopen a company.
			"INSERT INTO sellers (id, uid, org_id, nav_base_url, nav_login, series_code, created_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?)
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
		.execute(&mut *self.conn().await?)
		.await
		.map_err(|e| {
			// `ON CONFLICT (id)` cannot catch this one: the `uid` column's own UNIQUE fires first,
			// and a raw driver error reads as a 500 on a path whose whole point is a stated failure.
			unique_as_conflict(&e, "another seller already carries this uid")
		})?;
		// An org- and uid-matching upsert always affects one row, so zero means one of the two
		// belongs elsewhere.
		if res.rows_affected() == 0 {
			return Err(Error::conflict(format!(
				"seller id {} is another org's or carries another uid",
				s.id
			)));
		}
		Ok(())
	}

	async fn create_seller(&self, s: &Seller) -> ClResult<bool> {
		let res = sqlx::query(
			"INSERT INTO sellers (id, uid, org_id, nav_base_url, nav_login, series_code, created_at)
			 SELECT ?, ?, id, ?, ?, ?, ? FROM orgs
			 WHERE id = ? AND kind = 'SHARED' AND status = 'ACTIVE'",
		)
		.bind(s.id)
		.bind(s.uid.as_str())
		.bind(&s.nav_base_url)
		.bind(&s.nav_login)
		.bind(&s.series_code)
		.bind(s.created_at.0)
		.bind(s.org_id)
		.execute(&mut *self.conn().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "a seller with this id or uid already exists"))?;
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
		sqlx::query("SELECT * FROM seller_versions WHERE seller_ver = ?")
			.bind(seller_ver)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(seller_version_row)
	}

	async fn seller_versions(&self, vers: &[i64]) -> ClResult<Vec<SellerVersion>> {
		self.in_ids(
			"SELECT * FROM seller_versions WHERE seller_ver IN",
			"",
			vers,
			seller_version_row,
		)
		.await
	}

	async fn seller_version_history(&self, seller_id: i64) -> ClResult<Vec<SellerVersion>> {
		// `status <> 'DRAFT'` is `idx_seller_version_history`'s own predicate, so the ordering
		// reads straight off the index.
		sqlx::query(
			"SELECT * FROM seller_versions WHERE seller_id = ? AND status <> 'DRAFT'
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
		// The draft wins as the base when there is one, so a second edit builds on the first
		// rather than on the live version and silently discards it.
		// One guard for both lookups: a `match` scrutinee's temporary outlives the arms, so a
		// second `tx.lock()` in the `None` arm would await the mutex the scrutinee still holds.
		let base = {
			let mut conn = tx.lock().await?;
			match version_by_status(&mut *conn, seller_id, "DRAFT").await? {
				Some(draft) => Some(draft),
				None => version_by_status(&mut *conn, seller_id, "CURRENT").await?,
			}
		};
		let merged = patch.merged(base.as_ref());
		let draft_ver =
			base.filter(|b| b.status == SellerVersionStatus::Draft).map(|b| b.seller_ver);

		// The fifteen statutory columns bind identically either way, so only the trailing binds
		// differ: the draft's id for the UPDATE, the seller and the creation instant for the
		// INSERT.
		let mut q = match draft_ver {
			// `AND status = 'DRAFT'`, like every write against an invoice: a published version
			// has no update path in this file at all.
			Some(_) => sqlx::query(
				"UPDATE seller_versions SET
					name = ?, country = ?, tax_number = ?, group_member_tax_no = ?,
					eu_vat_id = ?, postcode = ?, city = ?, street = ?, bank_account = ?,
					bank_name = ?, small_business = ?, vat_scheme = ?, income_regime = ?,
					expense_ratio_pct = ?, regime_since = ?
				 WHERE seller_ver = ? AND status = 'DRAFT' RETURNING *",
			),
			None => sqlx::query(
				"INSERT INTO seller_versions
				 (name, country, tax_number, group_member_tax_no, eu_vat_id, postcode, city,
				  street, bank_account, bank_name, small_business, vat_scheme, income_regime,
				  expense_ratio_pct, regime_since, seller_id, status, created_at, valid_from)
				 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'DRAFT', ?, NULL)
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
			.bind(merged.small_business)
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
			.ok_or_else(|| Error::internal("mintworks-store-sqlite: the seller draft vanished"))?;
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
		// Inside the transaction that promotes it: a check on a draft read beforehand let a
		// concurrent `save_seller_version_draft` rewrite it blank and freeze that onto an
		// immutable invoice. The `?` drops `tx`, which rolls back.
		let Some(draft) = version_by_status(&mut *tx.lock().await?, seller_id, "DRAFT").await?
		else {
			return Ok(None);
		};
		check(&draft)?;
		// Archive first: `idx_seller_version_current` is a unique index, so promoting into a
		// still-live CURRENT row is a constraint violation and not a second live version.
		sqlx::query(
			"UPDATE seller_versions SET status = 'ARCHIVED', superseded_at = ?
			 WHERE seller_id = ? AND status = 'CURRENT'",
		)
		.bind(now.0)
		.bind(seller_id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;

		let promoted: Option<i64> = sqlx::query_scalar(
			"UPDATE seller_versions SET status = 'CURRENT', valid_from = ?
			 WHERE seller_id = ? AND status = 'DRAFT' RETURNING seller_ver",
		)
		.bind(now.0)
		.bind(seller_id)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.db()?;
		// Nothing to promote: roll the archive back rather than leaving the seller with no
		// live version at all.
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
		// `BEGIN IMMEDIATE` around all three: the service's own probe is the cheap refusal, but a
		// `save_seller_version_draft` landing between it and the save publishes a half-typed edit.
		let (tx, bound) = self.begin().await?;
		let open = {
			let mut conn = tx.lock().await?;
			version_by_status(&mut *conn, seller_id, "DRAFT").await?.is_some()
		};
		if open {
			return Ok(None);
		}
		// Through the bound handle, so both take a savepoint in this transaction rather than a
		// second `BEGIN IMMEDIATE` that would block on the one writer connection.
		bound.save_seller_version_draft(seller_id, patch).await?;
		let ver = bound.publish_seller_version(seller_id, now, check).await?;
		tx.commit().await?;
		Ok(ver)
	}

	async fn discard_seller_version_draft(&self, seller_id: i64) -> ClResult<bool> {
		let res =
			sqlx::query("DELETE FROM seller_versions WHERE seller_id = ? AND status = 'DRAFT'")
				.bind(seller_id)
				.execute(&mut *self.conn().await?)
				.await
				.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn seller_has_issued(&self, seller_id: i64) -> ClResult<bool> {
		sqlx::query_scalar(
			"SELECT EXISTS(SELECT 1 FROM invoices WHERE seller_id = ? AND number IS NOT NULL)",
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
			None => sqlx::query("UPDATE sellers SET closed_at = NULL WHERE id = ?").bind(seller_id),
			Some(t) => sqlx::query(
				"UPDATE sellers SET closed_at = ? WHERE id = ? AND NOT EXISTS
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
		let res = sqlx::query("UPDATE sellers SET payment_days = ? WHERE id = ?")
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
			// `active` is deliberately absent from the SET list: withdrawing a service from
			// the consumer's code must not resurrect it, and must never delete the row that
			// issued invoice lines still point at.
			let uid = ServiceId::generate();
			sqlx::query(
				"INSERT INTO services
				 (uid, org_id, code, name, description, unit, unit_price, vat_code, created_at, updated_at)
				 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
		sqlx::query("SELECT * FROM services WHERE org_id = ? AND code = ?")
			.bind(org_id)
			.bind(code)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(service_row)
	}

	async fn services_by_codes(&self, org_id: i64, codes: &[&str]) -> ClResult<Vec<Service>> {
		if codes.is_empty() {
			return Ok(Vec::new());
		}
		let sql = format!(
			"SELECT * FROM services WHERE org_id = ? AND active AND code IN {}",
			values_clause(1, codes.len())
		);
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(org_id);
		for code in codes {
			q = q.bind(*code);
		}
		q.fetch_all(&mut *self.reader().await?).await.all(service_row)
	}

	async fn service_by_uid(&self, org_id: i64, uid: &ServiceId) -> ClResult<Option<Service>> {
		sqlx::query("SELECT * FROM services WHERE org_id = ? AND uid = ?")
			.bind(org_id)
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(service_row)
	}

	async fn create_service(&self, org_id: i64, d: &ServiceDef) -> ClResult<Service> {
		let now = Timestamp::now();
		let uid = ServiceId::generate();
		let row = sqlx::query(
			"INSERT INTO services
			 (uid, org_id, code, name, description, unit, unit_price, vat_code, created_at, updated_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
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
		.fetch_one(&mut *self.conn().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "service code already exists"))?;
		service_row(&row)
	}

	async fn update_service(
		&self,
		org_id: i64,
		uid: &ServiceId,
		p: &ServicePatch,
	) -> ClResult<Option<Service>> {
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
			 WHERE org_id = ? AND uid = ? RETURNING *",
		)
		.bind(!p.code.is_undefined())
		.bind(p.code.value())
		.bind(&p.name)
		.bind(!p.description.is_undefined())
		.bind(p.description.value())
		.bind(&p.unit)
		.bind(p.unit_price.map(|m| m.0))
		.bind(p.vat_code.map(mintworks_invoice::VatCode::as_str))
		.bind(p.active)
		.bind(Timestamp::now().0)
		.bind(org_id)
		.bind(uid.as_str())
		.fetch_optional(&mut *self.conn().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "service code already exists"))?
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
			"SELECT * FROM services WHERE org_id = ? AND (? = 0 OR active = 1)
			 ORDER BY name LIMIT ?",
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
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
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
		.bind(p.is_default.unwrap_or(false))
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
		sqlx::query("SELECT * FROM billing_parties WHERE org_id = ? AND uid = ?")
			.bind(org_id)
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(party_row)
	}

	async fn party_by_id(&self, id: i64) -> ClResult<Option<BillingParty>> {
		sqlx::query("SELECT * FROM billing_parties WHERE id = ?")
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
				payment_days = CASE ? WHEN 1 THEN ? ELSE payment_days END,
				payment_method = CASE ? WHEN 1 THEN ? ELSE payment_method END,
				updated_at   = ?
			 WHERE org_id = ? AND uid = ? RETURNING *",
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
		.bind(p.is_default)
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

		// Only commit when the row was actually ours: `clear_default_party` above has already
		// run, so committing a miss would leave the org with no default at all. Dropping
		// `tx` rolls back, which is what the miss path wants.
		if party.is_some() {
			tx.commit().await?;
		}
		Ok(party)
	}

	async fn delete_party(&self, org_id: i64, uid: &PartyId) -> ClResult<bool> {
		let tx = self.write_tx().await?;
		let done = sqlx::query("DELETE FROM billing_parties WHERE org_id = ? AND uid = ?")
			.bind(org_id)
			.bind(uid.as_str())
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		// Keyed to the row that actually went: the ext blob's `(type, uid)` pair carries no FK
		// (`crate::objects::ext_type`), so this is the only thing that removes it — `delete_org`
		// refuses an org that still holds `objects`, so its cascade never fires from there.
		if done.rows_affected() > 0 {
			sqlx::query("DELETE FROM objects WHERE org_id = ? AND type = ? AND uid = ?")
				.bind(org_id)
				.bind(crate::objects::ext_type("party"))
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
			 WHERE org_id = ? ORDER BY is_default DESC, name LIMIT ?",
		)
		.bind(org_id)
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(party_row)
	}

	async fn default_party(&self, org_id: i64) -> ClResult<Option<BillingParty>> {
		sqlx::query("SELECT * FROM billing_parties WHERE org_id = ? AND is_default = 1")
			.bind(org_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(party_row)
	}

	// -- invoices: draft

	async fn create_draft(&self, new: &NewInvoice) -> ClResult<Invoice> {
		let tx = self.write_tx().await?;
		let invoice = insert_draft(&mut *tx.lock().await?, new, conflict_for(new)).await?;
		tx.commit().await?;
		Ok(invoice)
	}

	async fn create_draft_full(
		&self,
		new: &NewInvoice,
		priced: &Priced,
		patch: Option<&InvoicePatch>,
	) -> ClResult<Invoice> {
		let tx = self.write_tx().await?;

		// Scoped so the guard is back in the mutex before the arms run: `tx.rollback()` below
		// takes the same connection lock, which a `match` scrutinee's temporary would still hold.
		let inserted = {
			let mut conn = tx.lock().await?;
			insert_draft(&mut conn, new, conflict_for(new)).await
		};
		let invoice = match inserted {
			Ok(invoice) => invoice,
			// `UNIQUE (org_id, request_id)`: a concurrent caller took the same idempotency
			// key between this caller's lookup and this insert. Its invoice is the right
			// answer, not a 409 — that is what makes the retry-by-machines guarantee hold.
			Err(Error::Conflict(msg)) => {
				tx.rollback().await?;
				let Some(request_id) = &new.request_id else {
					return Err(Error::conflict(msg));
				};
				return self
					.invoice_by_request_id(new.org_id, request_id)
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
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		let invoice = invoice_row(&row)?;

		tx.commit().await?;
		Ok(invoice)
	}

	async fn invoice_by_request_id(
		&self,
		org_id: i64,
		request_id: &str,
	) -> ClResult<Option<Invoice>> {
		sqlx::query("SELECT * FROM invoices WHERE org_id = ? AND request_id = ?")
			.bind(org_id)
			.bind(request_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(invoice_row)
	}

	async fn invoice_by_uid(
		&self,
		org_id: Option<i64>,
		uid: &InvoiceId,
	) -> ClResult<Option<Invoice>> {
		sqlx::query("SELECT * FROM invoices WHERE uid = ? AND (? IS NULL OR org_id = ?)")
			.bind(uid.as_str())
			.bind(org_id)
			.bind(org_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(invoice_row)
	}

	async fn invoice_by_id(&self, id: i64) -> ClResult<Option<Invoice>> {
		sqlx::query("SELECT * FROM invoices WHERE id = ?")
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(invoice_row)
	}

	async fn storno_of(&self, original_id: i64) -> ClResult<Option<Invoice>> {
		sqlx::query("SELECT * FROM invoices WHERE original_invoice_id = ? AND kind = 'STORNO'")
			.bind(original_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(invoice_row)
	}

	async fn update_draft(&self, id: i64, p: &InvoicePatch) -> ClResult<Option<Invoice>> {
		update_draft_row(&mut *self.conn().await?, id, p).await
	}

	/// The one write in this file with no `status = 'DRAFT'` predicate, and deliberately:
	/// `notes` is the single column an issued invoice may still change.
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
		.fetch_optional(&mut *self.conn().await?)
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
		let tx = self.write_tx().await?;

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

	async fn invoice_lines(&self, invoice_id: i64) -> ClResult<Vec<InvoiceLine>> {
		sqlx::query("SELECT * FROM invoice_lines WHERE invoice_id = ? ORDER BY line_no")
			.bind(invoice_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(line_row)
	}

	async fn invoice_vat_groups(&self, invoice_id: i64) -> ClResult<Vec<InvoiceVatGroup>> {
		sqlx::query("SELECT * FROM invoice_vat_groups WHERE invoice_id = ? ORDER BY vat_code")
			.bind(invoice_id)
			.fetch_all(&mut *self.reader().await?)
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
		let tx = self.write_tx().await?;
		// `payment_allocations.invoice_id` has no `ON DELETE CASCADE` — it is a money trail and
		// an issued invoice's rows must outlive nothing — so an abandoned card attempt's zero
		// link row made its own draft undeletable. Only a draft is reached here, and `settle`
		// refuses a draft outright, so every row this drops is a zero.
		sqlx::query(
			"DELETE FROM payment_allocations
			  WHERE invoice_id IN (SELECT id FROM invoices WHERE id = ? AND status = 'DRAFT')",
		)
		.bind(id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		// The invoice's own ext data goes with it, matched on the `objects` row's own uid and org so
		// the delete stays org-scoped. Before the row it is keyed by disappears, because the
		// `(type, uid)` pair carries no FK and nothing else would ever reach it.
		sqlx::query(
			"DELETE FROM objects
			  WHERE type = ?
			    AND EXISTS (SELECT 1 FROM invoices i
			                 WHERE i.uid = objects.uid AND i.org_id = objects.org_id
			                   AND i.id = ? AND i.status = 'DRAFT')",
		)
		.bind(crate::objects::ext_type("invoice"))
		.bind(id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		let done = sqlx::query("DELETE FROM invoices WHERE id = ? AND status = 'DRAFT'")
			.bind(id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		tx.commit().await?;
		Ok(done.rows_affected() > 0)
	}

	async fn sweep_drafts(&self, cutoff: Timestamp) -> ClResult<u64> {
		// Not a draft whose payment holds money — in flight, or already arrived: dropping the
		// link row sends a payment that later succeeds into `settle_full`'s "no invoice" branch,
		// and a `SUCCEEDED` one whose `issue_if_unissued` failed left the invoice `PENDING`
		// forever, so the sweep deleted an invoice that was paid for.
		const LIVE: &str = "AND NOT EXISTS (SELECT 1 FROM payment_allocations a
		                      JOIN payments p ON p.id = a.payment_id
		                     WHERE a.invoice_id = invoices.id
		                       AND p.status IN ('PENDING','AWAITING_USER','RESERVED','AUTHORIZED',
		                                        'SUCCEEDED','PARTIALLY_SUCCEEDED'))
		                     AND NOT EXISTS (SELECT 1 FROM plan_invoices pi
		                      JOIN subscriptions s ON s.id = pi.subscription_id
		                     WHERE pi.invoice_id = invoices.id AND pi.kind = 'RENEWAL'
		                       AND s.status <> 'CANCELED')";
		// `updated_at`, not `created_at`: the caller means *abandoned*, and a cart opened a
		// month ago and edited this morning is not. `idx_invoice_draft_age` is keyed on it too.
		//
		// `PENDING` alongside `DRAFT`, and `LIVE` above is what makes it safe: a locked invoice
		// whose payment is still live is never reached, so what this collects is a lock nothing
		// will ever unwind — past the sweep's horizon, no gateway is re-asked about it again.
		let tx = self.write_tx().await?;
		// The link rows first, for the reason `delete_draft` gives.
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"DELETE FROM payment_allocations
			  WHERE invoice_id IN
			        (SELECT id FROM invoices
			          WHERE status IN ('DRAFT','PENDING') AND updated_at < ? {LIVE})"
		)))
		.bind(cutoff.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		// The ext data of the drafts this sweep collects, for the reason `delete_draft` gives — and
		// before the rows that key it, which the subquery below needs to still see.
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"DELETE FROM objects
			  WHERE type = ?
			    AND EXISTS (SELECT 1 FROM invoices
			                 WHERE invoices.uid = objects.uid AND invoices.org_id = objects.org_id
			                   AND invoices.status IN ('DRAFT','PENDING') AND invoices.updated_at < ? {LIVE})"
		)))
		.bind(crate::objects::ext_type("invoice"))
		.bind(cutoff.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		let done = sqlx::query(sqlx::AssertSqlSafe(format!(
			"DELETE FROM invoices WHERE id IN
			        (SELECT id FROM invoices
			          WHERE status IN ('DRAFT','PENDING') AND updated_at < ? {LIVE})"
		)))
		.bind(cutoff.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		tx.commit().await?;
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
		.bind(mintworks_invoice::issue::KIND_RENDER_PDF)
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
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
		let tx = self.write_tx().await?;

		// Two causes, two codes: an invoice that is no longer a draft is `not_a_draft`, an
		// invoice that moved under the caller is retryable and says so.
		let seller_id: i64 = sqlx::query_scalar(
			"SELECT seller_id FROM invoices
			  WHERE id = ? AND status IN ('DRAFT','PENDING') AND version = ?",
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

		// The re-priced lines and the VAT summary both go in while the row is still a draft,
		// before `freeze` below flips the status. The probe above is the gate that makes that
		// safe — the child-table helpers carry no predicate of their own.
		replace_lines(&mut *tx.lock().await?, id, &issue.lines).await?;
		replace_groups(&mut *tx.lock().await?, id, &issue.groups).await?;

		let number = allocate_number(
			&mut *tx.lock().await?,
			seller_id,
			&issue.series_code,
			issue.series_year,
		)
		.await?;
		let invoice = freeze(&mut *tx.lock().await?, id, &number, issue)
			.await?
			.ok_or_else(not_a_draft)?;

		tx.commit().await?;
		Ok(invoice)
	}

	async fn storno(
		&self,
		original_id: i64,
		new: &NewInvoice,
		issue: &IssueInvoice,
	) -> ClResult<Invoice> {
		let tx = self.write_tx().await?;

		let status: InvoiceStatus =
			sqlx::query_scalar::<_, String>("SELECT status FROM invoices WHERE id = ?")
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

		// `idx_invoice_storno_once` is what actually guarantees at-most-once; the status
		// check above only turns the second attempt into a readable 409 sooner. The loser of
		// a concurrent pair gets past it and lands here.
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

		// The predecessor predicate is hardcoded because it is the transition, not an argument;
		// `PAID` is in the list because `storno::run` accepts a paid original. The
		// `rows_affected` check is an invariant check, not a race guard: without it a frozen
		// STORNO commits while its original is still `ISSUED`, and both rows are immutable.
		let flipped = sqlx::query(
			"UPDATE invoices SET status = 'STORNOED', updated_at = ?
			  WHERE id = ? AND status IN ('ISSUED','PAID')",
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

	// -- invoices: read and the three permitted post-issue writes

	async fn list_invoices(
		&self,
		org_id: i64,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Invoice>> {
		sqlx::query(
			"SELECT * FROM invoices
			 WHERE org_id = ? AND (? IS NULL OR id < ?)
			 ORDER BY id DESC LIMIT ?",
		)
		.bind(org_id)
		.bind(before_id)
		.bind(before_id)
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(invoice_row)
	}

	async fn list_invoices_page(
		&self,
		org_id: i64,
		filter: &InvoiceFilter,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<ListedInvoice>> {
		// `IN (...)` needs one placeholder per status, so this clause is built rather than
		// constant. The tags come from `InvoiceStatus`, never from request text.
		let statuses = if filter.statuses.is_empty() {
			String::new()
		} else {
			format!(" AND i.status IN ({})", "?,".repeat(filter.statuses.len() - 1) + "?")
		};
		// Unindexed `LIKE '%...%'` over three columns, demo scale; FTS5 if it hurts.
		let text = if filter.q.is_some() {
			" AND (i.number LIKE ? ESCAPE '\\' OR i.buyer_name LIKE ? ESCAPE '\\'
			       OR p.name LIKE ? ESCAPE '\\')"
		} else {
			""
		};
		// `i.*` first, so `invoice_row`'s `uid` still resolves to the invoice's own. The storno
		// join cannot duplicate a row: `idx_invoice_storno_once` is unique.
		let sql = format!(
			"SELECT i.*, p.uid AS party_uid, o.uid AS original_uid, s.uid AS storno_uid
			 FROM invoices i
			 LEFT JOIN billing_parties p ON p.id = i.billing_party_id
			 LEFT JOIN invoices o ON o.id = i.original_invoice_id
			 LEFT JOIN invoices s
			        ON s.original_invoice_id = i.id AND s.kind = 'STORNO'
			       AND i.status = 'STORNOED'
			 WHERE i.org_id = ? AND (? IS NULL OR i.id < ?){statuses}{text}
			 ORDER BY i.id DESC LIMIT ?"
		);
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(org_id)
			.bind(before_id)
			.bind(before_id);
		for st in &filter.statuses {
			q = q.bind(st.as_str());
		}
		if let Some(text) = &filter.q {
			// `%`, `_` and `\` typed by a user are literals: without this a `%` lists the whole
			// table and a `_` returns near-random rows.
			let pat =
				format!("%{}%", text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
			q = q.bind(pat.clone()).bind(pat.clone()).bind(pat);
		}
		q.bind(limit)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(listed_invoice_row)
	}

	async fn invoice_summary(
		&self,
		org_id: i64,
		from_month: &str,
		today: &str,
		this_month: (Timestamp, Timestamp),
	) -> ClResult<InvoiceSummary> {
		let statuses: Vec<StatusRow> = sqlx::query_as(
			"SELECT CASE WHEN kind = 'STORNO' THEN 'STORNOED' ELSE status END AS st, currency, \
			 COUNT(*), COALESCE(SUM(net), 0), COALESCE(SUM(vat), 0), \
			 COALESCE(SUM(gross), 0), COALESCE(SUM(paid_amount), 0) \
			 FROM invoices WHERE org_id = ? \
			 GROUP BY st, currency ORDER BY st, currency",
		)
		.bind(org_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;

		// `substr(fulfilment_date)`, not `strftime('%Y-%m', issued_at, 'unixepoch')`: the
		// fulfilment date is the statutory period and is already a local day, while `issued_at`
		// buckets in UTC and files a 00:30 CET invoice into the month before.
		let months: Vec<MonthRow> = sqlx::query_as(
			"SELECT substr(COALESCE(fulfilment_date, date(issued_at, 'unixepoch')), 1, 7) AS month, \
			 currency, COUNT(*), COALESCE(SUM(gross), 0), COALESCE(SUM(paid_amount), 0) \
			 FROM invoices \
			 WHERE org_id = ? AND number IS NOT NULL \
			   AND substr(COALESCE(fulfilment_date, date(issued_at, 'unixepoch')), 1, 7) >= ? \
			 GROUP BY month, currency ORDER BY month, currency",
		)
		.bind(org_id)
		.bind(from_month)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;

		let overdue: Vec<OverdueRow> = sqlx::query_as(
			"SELECT currency, COUNT(*), COALESCE(SUM(gross - paid_amount), 0) \
			 FROM invoices \
			 WHERE org_id = ? AND status = 'ISSUED' AND due_date IS NOT NULL AND due_date < ? \
			   AND paid_amount < gross \
			 GROUP BY currency ORDER BY currency",
		)
		.bind(org_id)
		.bind(today)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;

		// Counts invoices fully paid this month (`paid_at`); partial payments need
		// `payment_allocations` by `allocated_at`.
		let paid: Vec<(String, i64)> = sqlx::query_as(
			"SELECT currency, COALESCE(SUM(paid_amount), 0) FROM invoices \
			 WHERE org_id = ? AND paid_at >= ? AND paid_at < ? \
			 GROUP BY currency ORDER BY currency",
		)
		.bind(org_id)
		.bind(this_month.0.0)
		.bind(this_month.1.0)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;

		Ok(InvoiceSummary {
			paid_this_month: paid
				.into_iter()
				.map(|(currency, paid)| {
					Ok((CurrencyCode::from_trusted(currency), read_money(paid)?))
				})
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

	async fn invoice_revenue(
		&self,
		org_id: i64,
		year: i32,
		start: Timestamp,
		end: Timestamp,
	) -> ClResult<Vec<RevenueMonth>> {
		let y = format!("{year:04}");
		// `net_huf` is NULL exactly on an HUF invoice, where `net` already is HUF.
		let invoiced: Vec<(String, i64)> = sqlx::query_as(
			"SELECT substr(i.fulfilment_date, 1, 7), COALESCE(SUM(COALESCE(g.net_huf, g.net)), 0) \
			 FROM invoice_vat_groups g JOIN invoices i ON i.id = g.invoice_id \
			 WHERE i.org_id = ? AND i.number IS NOT NULL AND g.vat_code NOT IN ('EUFAD37','HO') \
			   AND substr(i.fulfilment_date, 1, 4) = ? \
			 GROUP BY 1",
		)
		.bind(org_id)
		.bind(&y)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;
		// Bucketed here, not with SQLite's `localtime`, which is the host's zone, not Budapest's.
		let received: Vec<(Option<i64>, Option<String>, String, i64)> = sqlx::query_as(
			"SELECT i.paid_at, i.fulfilment_date, i.payment_method, \
			   COALESCE(SUM(COALESCE(g.net_huf, g.net)), 0) \
			 FROM invoices i JOIN invoice_vat_groups g ON g.invoice_id = i.id \
			 WHERE i.org_id = ? AND i.number IS NOT NULL \
			   AND ((i.paid_at >= ? AND i.paid_at < ?) \
			     OR (i.payment_method = 'CASH' AND substr(i.fulfilment_date, 1, 4) = ?)) \
			 GROUP BY i.id",
		)
		.bind(org_id)
		.bind(start.0)
		.bind(end.0)
		.bind(&y)
		.fetch_all(&mut *self.reader().await?)
		.await
		.db()?;

		// An unpaid invoice fulfilled in an earlier year is not shown, though its
		// payment would count this year.
		let outstanding: Vec<(String, i64)> = sqlx::query_as(
			"SELECT substr(i.fulfilment_date, 1, 7), COALESCE(SUM(COALESCE(g.net_huf, g.net)), 0) \
			 FROM invoice_vat_groups g JOIN invoices i ON i.id = g.invoice_id \
			 WHERE i.org_id = ? AND i.kind = 'NORMAL' AND i.status = 'ISSUED' \
			   AND i.paid_at IS NULL AND i.payment_method <> 'CASH' \
			   AND substr(i.fulfilment_date, 1, 4) = ? \
			 GROUP BY 1",
		)
		.bind(org_id)
		.bind(&y)
		.fetch_all(&mut *self.reader().await?)
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
		// Partial payments are not counted — `paid_at` is only stamped once the whole
		// gross is in. Summing `payments` allocations by date is the upgrade.
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

	async fn mark_paid(&self, id: i64) -> ClResult<bool> {
		mark_status(self, id, InvoiceStatus::Paid).await
	}

	async fn mark_stornoed(&self, id: i64) -> ClResult<bool> {
		mark_status(self, id, InvoiceStatus::Stornoed).await
	}

	async fn set_status(&self, id: i64, from: InvoiceStatus, to: InvoiceStatus) -> ClResult<bool> {
		// The gateway lock and nothing else. `InvoiceStore` is public and a consumer holds it
		// directly, so an unconstrained pair is a way past ISSUED-immutability:
		// `set_status(id, Issued, Draft)` walks a numbered, NAV-filed invoice back to where
		// `replace_draft_lines` and `issue` both accept it and renumber it.
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
			"UPDATE invoices SET status = ?, updated_at = ? WHERE id = ? AND status = ?",
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
			"UPDATE invoices SET paid_amount = ?, paid_at = ?, updated_at = ?
			 WHERE id = ? AND status IN ('ISSUED','PAID')",
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
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn invoice_document(&self, invoice_id: i64) -> ClResult<Option<InvoiceDocument>> {
		sqlx::query("SELECT * FROM invoice_documents WHERE invoice_id = ? AND kind = 'PDF'")
			.bind(invoice_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(document_row)
	}

	async fn invoice_documents_for(&self, ids: &[i64]) -> ClResult<Vec<InvoiceDocument>> {
		self.in_ids(
			"SELECT * FROM invoice_documents WHERE kind = 'PDF' AND invoice_id IN",
			" ORDER BY invoice_id",
			ids,
			document_row,
		)
		.await
	}

	// -- currency

	async fn currency_get(&self, code: &str) -> ClResult<Option<Currency>> {
		let row: Option<CurrencyRow> = sqlx::query_as(
			"SELECT code, price_round_step, cash_round_step, mode, fixed_rate_e6, fee_bp, enabled \
			 FROM currencies WHERE code = ?",
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
			 FROM currencies WHERE ? = 1 OR enabled = 1 ORDER BY code",
		)
		.bind(i64::from(all))
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
			 WHERE pair = ? AND source = ? AND date <= ? ORDER BY date DESC LIMIT 1",
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
		sqlx::query_scalar("SELECT code FROM currencies WHERE enabled = 1 AND code <> ?")
			.bind(base)
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn mnb_max_date(&self, pair: &str) -> ClResult<Option<String>> {
		sqlx::query_scalar("SELECT max(date) FROM currency_rates WHERE pair = ? AND source = ?")
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
				 VALUES (?, ?, ?, ?, ?) ON CONFLICT (pair, date, source) DO UPDATE SET \
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
			 WHERE eu_vat_id = ? AND checked_at > ?",
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
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	// -- orgs

	async fn org_billing_currency(&self, org_id: i64) -> ClResult<Option<CurrencyCode>> {
		Ok(sqlx::query_scalar("SELECT billing_currency FROM orgs WHERE id = ?")
			.bind(org_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()?
			.flatten()
			.map(CurrencyCode::from_trusted))
	}
}

/// `idx_billing_party_default` allows one default per org, so promoting a party has to
/// demote the incumbent in the same transaction.
async fn clear_default_party(tx: &mut SqliteConnection, org_id: i64) -> ClResult<()> {
	sqlx::query("UPDATE billing_parties SET is_default = 0 WHERE org_id = ? AND is_default = 1")
		.bind(org_id)
		.execute(&mut *tx)
		.await
		.db()?;
	Ok(())
}

// vim: ts=4
