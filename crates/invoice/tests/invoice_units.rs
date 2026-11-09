// SPDX-License-Identifier: MPL-2.0
//! The `mintworks-invoice` unit tests that need a real database: the draft-sweep seed and the two
//! `currency` cases that read published rates.
//!
//! An integration test, not an inline `mod tests`: the `#[cfg(test)]` build of a crate is a
//! distinct crate from the one `mintworks-store-sqlite` links against, so the store impls would
//! not unify (`E0599`). The adapter is therefore a dev-dependency, a cycle Cargo permits.
//!
//! The harness is a real file database in a temp dir, never `sqlite::memory:` — an in-memory
//! URL gives each connection its own database, so two stores would never contend for the
//! write lock.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mintworks_core::app::{App, AppBuilder};
use mintworks_core::config::Config;
use mintworks_core::ctx::Ctx;
use mintworks_core::error::Error;
use mintworks_core::ids::{RefId, SellerId};
use mintworks_core::money::{CurrencyCode, Money, Qty};
use mintworks_core::refs::{NewRef, RefStore, Refs};
use mintworks_core::store::CoreStore;
use mintworks_core::types::{Patch, Timestamp};
use mintworks_invoice::currency::{Currency, RateMode, effective_rate_e6, rate_on};
use mintworks_invoice::draft::{self, IssueNow, KIND_SWEEP, Line, NewDraft, Party, seed};
use mintworks_invoice::numbering;
use mintworks_invoice::routes::{InvoicePatchBody, InvoiceView};
use mintworks_invoice::service_api::{Invoices, MAX_PAGE_LIMIT};
use mintworks_invoice::store::{
	InvoiceDocument, InvoiceFilter, InvoicePatch, InvoiceStore, PartyKind, PartyPatch, Seller,
	SellerVersionPatch, ServiceDef,
};
use mintworks_invoice::vat::VatCode;
use mintworks_store_sqlite::SqliteStore;

/// The fixture's one seller. `put_seller` does not autoincrement, so the id is chosen here.
const SELLER: i64 = 1;

/// The platform root, moved off its natural id 1 so the fixture's own org can have it. The
/// framework finds the root by `kind = 'ROOT'` and never by its value.
const ROOT: i64 = 0;

/// A temp directory that takes the database with it: the reader pool opens
/// `create_if_missing(false)`, so `sqlite::memory:` is not an option.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-invoice-unit-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [7; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			// The temp dir, not `String::new()`: `pdf::doc_path` fans out from `data_dir`, so an
			// empty one wrote a rendered PDF into the crate root, where `Drop` never found it.
			data_dir: self.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: String::new(),
			jobs_workers: None,
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn fresh(name: &str) -> (TmpDb, SqliteStore) {
	let db = TmpDb::new(name);
	let sql = SqliteStore::open(&db.config()).await.unwrap();
	sql.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	(db, sql)
}

fn huf() -> CurrencyCode {
	CurrencyCode::huf()
}

/// `CurrencyCode` normalizes at the wire, which is the only place it normalizes: `currencies`
/// has no `COLLATE NOCASE` and an operator seeds the table by SQL, so a `huf` that reached a
/// stored row took the wrong branch in the NAV writer and failed an already-issued invoice.
#[test]
fn a_lowercase_wire_currency_is_normalized_at_the_door() {
	let body: InvoicePatchBody =
		serde_json::from_value(serde_json::json!({"currency": "eur"})).unwrap();
	assert_eq!(body.currency.unwrap(), "EUR");
	assert!(
		serde_json::from_value::<InvoicePatchBody>(serde_json::json!({"currency": "eu"})).is_err()
	);
}

fn eur() -> Currency {
	Currency {
		code: CurrencyCode::parse("EUR").unwrap(),
		price_round_step: 1,
		cash_round_step: None,
		mode: RateMode::Official,
		fixed_rate_e6: None,
		fee_bp: 0,
		enabled: true,
	}
}

/// `seed` used to enqueue under a fixed dedup key, which survives `DONE`: once the first
/// chain stopped, every later boot collided silently and drafts were never swept again.
#[tokio::test]
async fn seed_is_idempotent_and_revives_a_dead_chain() {
	let (_db, sql) = fresh("draft-seed").await;
	let store: Arc<dyn CoreStore> = Arc::new(sql.clone());

	let count = |sql: SqliteStore, sql_text: &'static str| async move {
		sqlx::query_scalar::<_, i64>(sql_text)
			.bind(KIND_SWEEP)
			.fetch_one(sql.read_pool())
			.await
			.unwrap()
	};

	seed(&store).await.unwrap();
	seed(&store).await.unwrap();
	assert_eq!(
		count(sql.clone(), "SELECT count(*) FROM jobs WHERE kind = ?").await,
		1,
		"a live chain is not re-seeded"
	);

	sqlx::query("UPDATE jobs SET status = 'FAILED' WHERE kind = ?")
		.bind(KIND_SWEEP)
		.execute(sql.write_pool())
		.await
		.unwrap();
	seed(&store).await.unwrap();
	assert_eq!(
		count(sql.clone(), "SELECT count(*) FROM jobs WHERE kind = ? AND status = 'PENDING'").await,
		1,
		"a chain that reached a terminal state restarts"
	);
}

/// `fixed_rate_e6` is defined against `settings['currency.base']` and nothing else. The
/// `Fixed` branch used to ignore `base` entirely, so on a `currency.base = 'EUR'`
/// deployment the seeded HUF row reported 1 HUF = 1 EUR and froze that onto every
/// invoice as the statutory HUF VAT figure.
#[tokio::test]
async fn a_fixed_currency_refuses_a_base_it_has_no_rate_against() {
	let (_db, sql) = fresh("currency-fixed-base").await;
	let mut usd = eur();
	usd.code = CurrencyCode::parse("USD").unwrap();
	usd.mode = RateMode::Fixed;
	usd.fixed_rate_e6 = Some(350_000_000);

	// The default HUF-base deployment, which is what `issue::plan` and
	// `Invoices::huf_rate_e6` rely on: unchanged. The `Fixed` branch never reaches the store.
	assert_eq!(
		effective_rate_e6(&sql, &usd, &huf(), &huf(), "BANK", "2026-01-01", 7)
			.await
			.unwrap(),
		350_000_000
	);
	// The same call on a `currency.base = 'EUR'` deployment, where the fixed rate says
	// nothing about HUF.
	let err = effective_rate_e6(&sql, &usd, &huf(), &eur().code, "BANK", "2026-01-01", 7)
		.await
		.expect_err("a FIXED currency cannot answer for an arbitrary base");
	assert_eq!(err.parts().1, "E-INV-NO-RATE");
}

/// `WHERE date <= ?` serves the last row that ever landed, so a fetch that silently
/// stopped froze a months-old rate onto new invoices as their `exchangeRate` and their
/// statutory HUF VAT. `max_age_days` is what makes that a refusal instead.
#[tokio::test]
async fn a_rate_past_the_staleness_bound_is_no_rate_at_all() {
	let (_db, sql) = fresh("currency-staleness").await;
	// `mnb_upsert_rates` publishes under `mnb::SOURCE`, which is `"MNB"`.
	let publish = |date: &'static str| {
		let sql = sql.clone();
		async move { sql.mnb_upsert_rates("EURHUF", &[(date.to_owned(), 400_000_000)]).await.unwrap() }
	};

	publish("2026-05-04").await;
	let stale = rate_on(&sql, "EURHUF", "MNB", "2026-06-03", 7).await;
	assert_eq!(stale.unwrap_err().parts().1, "E-INV-NO-RATE");
	// The same row inside a wider bound is still an answer.
	assert_eq!(rate_on(&sql, "EURHUF", "MNB", "2026-06-03", 60).await.unwrap(), 400_000_000);

	// A weekend gap is the case the 7-day default exists for.
	publish("2026-05-31").await;
	assert_eq!(rate_on(&sql, "EURHUF", "MNB", "2026-06-03", 7).await.unwrap(), 400_000_000);
}

/// `fresh` plus everything an `Invoices` handle needs to reach a draft: the store extension
/// and the chain the foreign keys demand — one account, org 1, its **default** billing
/// party (`Party::OrgDefault`) and seller 1. The same shape as `mintworks-nav`'s `setup`, minus
/// the `NavStore` extension.
async fn app_for(db: &TmpDb, sql: &SqliteStore) -> App {
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(sql.clone()) as Arc<dyn CoreStore>)
		.settings(mintworks_invoice::SETTINGS)
		.settings(mintworks_core::blob::SETTINGS)
		.extension(Arc::new(sql.clone()) as Arc<dyn InvoiceStore>)
		.extension(Arc::new(sql.clone()) as Arc<dyn RefStore>)
		.build()
		.await
		.unwrap();

	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();
	// A fresh install seeds the root org at id 1, which this fixture wants for its own;
	// the framework finds the root by `kind = 'ROOT'`, never by its value.
	sqlx::query("UPDATE orgs SET id = ? WHERE kind = 'ROOT'")
		.bind(ROOT)
		.execute(sql.write_pool())
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (1, 'org_t', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', 1, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 1, 0, 0)",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();
	sql.put_seller(&Seller {
		id: SELLER,
		uid: SellerId::generate(),
		org_id: 1,
		nav_base_url: String::new(),
		nav_login: None,
		series_code: "A".into(),
		test_mode: false,
		closed_at: None,
		payment_days: None,
		created_at: Timestamp::now(),
	})
	.await
	.unwrap();
	// One published version, because `issue` refuses a seller that has none.
	sql.save_seller_version_draft(
		SELLER,
		&SellerVersionPatch {
			name: Some("Teszt Kft.".into()),
			country: Some("HU".into()),
			tax_number: Some("12345678242".into()),
			eu_vat_id: Patch::Value("HU12345678".into()),
			postcode: Some("1011".into()),
			city: Some("Budapest".into()),
			street: Some("Fo utca 1.".into()),
			..Default::default()
		},
	)
	.await
	.unwrap();
	sql.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(())).await.unwrap();

	app
}

/// `change_currency` resolved the new rate on **today**, while `draft` and `rewrite` resolve
/// on the fulfilment date: the draft then carried a `rate_e6` no date on the row describes,
/// and on an HUF-base deployment its lines were priced at one rate and `exchangeRate` filed at
/// another. Both dates are derived from now, so the 365-day `MAX_FULFILMENT_DRIFT_DAYS` window
/// and the 7-day rate-staleness bound stay satisfied whenever this runs.
#[tokio::test]
async fn a_currency_change_prices_on_the_fulfilment_date() {
	const ON_THE_DAY: i64 = 400_000_000;
	const TODAY: i64 = 450_000_000;

	let (db, sql) = fresh("currency-change-dating").await;
	let app = app_for(&db, &sql).await;
	// `mnb_upsert_rates` is the trait's only rate writer and publishes under `MNB`, while
	// `currency.rate_source` defaults to `BANK`.
	app.settings.set("currency.rate_source", "MNB", None).await.unwrap();
	// `003_invoice.sql` seeds HUF alone, and there is no upsert on the trait. `OFFICIAL`, or
	// `effective_rate_e6` short-circuits on the fixed rate and reads no published row at all.
	sqlx::query(
		"INSERT INTO currencies (code, price_round_step, mode, fee_bp, enabled)
		 VALUES ('EUR', 1, 'OFFICIAL', 0, 1) ON CONFLICT (code) DO UPDATE SET price_round_step = excluded.price_round_step, mode = excluded.mode, fee_bp = excluded.fee_bp, enabled = excluded.enabled",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();
	opt_in_eur(&sql).await;

	let now = Timestamp::now().0;
	let today = numbering::date_of(Timestamp(now)).unwrap();
	let fulfilment = numbering::date_of(Timestamp(now - 3 * 86_400)).unwrap();
	sql.mnb_upsert_rates("EURHUF", &[(fulfilment.clone(), ON_THE_DAY), (today, TODAY)])
		.await
		.unwrap();

	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app.clone());
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				request_id: None,
				billing_party: Party::OrgDefault,
				lines: vec![Line {
					code: None,
					description: "Tanacsadas".into(),
					unit: "ora".into(),
					qty: Qty(1_000_000),
					unit_price: Some(Money(100_000)),
					vat_code: Some(VatCode::Std27),
					discount: None,
					discount_description: None,
					note: None,
				}],
				discount: None,
				payment_method: None,
				currency: None,
				fulfilment_date: Some(fulfilment.clone()),
				due_date: None,
				notes: None,
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(draft.currency, "HUF");

	let changed = invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch {
				// Lowercase on purpose: the service takes a `CurrencyCode`, so the draft is
				// re-denominated into `EUR` and the stored row can only hold the upper form.
				currency: Some(CurrencyCode::parse("eur").unwrap()),
				..InvoicePatch::default()
			},
		)
		.await
		.unwrap();

	assert_eq!(changed.currency, "EUR");
	assert_eq!(changed.fulfilment_date.as_deref(), Some(fulfilment.as_str()));
	assert_eq!(
		changed.rate_e6, ON_THE_DAY,
		"the fulfilment date prices the currency change, not today"
	);
}

/// `issue::plan` re-resolved `rate_e6` at issue while the lines kept their stored
/// `unit_price`, so an immutable `ISSUED` row claimed a pricing rate its own prices were never
/// computed at — the inconsistency `Invoices::patch` refuses on a draft. `huf_rate_e6` is the
/// statutory figure and does move to the fulfilment date (Áfa tv. 172. §).
#[tokio::test]
async fn issuing_keeps_the_rate_the_lines_were_priced_at() {
	const DRAFTED_AT: i64 = 400_000_000;
	const ISSUED_AT: i64 = 450_000_000;

	let (db, sql) = fresh("issue-rate-dating").await;
	let app = app_for(&db, &sql).await;
	app.settings.set("currency.rate_source", "MNB", None).await.unwrap();
	sqlx::query(
		"INSERT INTO currencies (code, price_round_step, mode, fee_bp, enabled)
		 VALUES ('EUR', 1, 'OFFICIAL', 0, 1) ON CONFLICT (code) DO UPDATE SET price_round_step = excluded.price_round_step, mode = excluded.mode, fee_bp = excluded.fee_bp, enabled = excluded.enabled",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();

	let today = numbering::date_of(Timestamp::now()).unwrap();
	sql.mnb_upsert_rates("EURHUF", &[(today.clone(), DRAFTED_AT)]).await.unwrap();

	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app.clone());
	// No `fulfilment_date`: the draft prices on today and `issue::plan` defaults the date to
	// today too, which is the pair that has to agree.
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				request_id: None,
				billing_party: Party::OrgDefault,
				lines: vec![Line {
					code: None,
					description: "Tanacsadas".into(),
					unit: "ora".into(),
					qty: Qty(1_000_000),
					unit_price: Some(Money(100_000)),
					vat_code: Some(VatCode::Std27),
					discount: None,
					discount_description: None,
					note: None,
				}],
				discount: None,
				payment_method: None,
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				fulfilment_date: None,
				due_date: None,
				notes: None,
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(draft.rate_e6, DRAFTED_AT);

	// The day's official rate is restated before the draft is issued — the same divergence a
	// draft left overnight produces, without a clock to move.
	sql.mnb_upsert_rates("EURHUF", &[(today.clone(), ISSUED_AT)]).await.unwrap();

	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
	assert_eq!(issued.rate_date.as_deref(), Some(today.as_str()));
	assert_eq!(
		issued.rate_e6, DRAFTED_AT,
		"the lines were priced at the draft's rate, so that is the rate the row may claim"
	);
	assert_eq!(
		issued.huf_rate_e6,
		Some(ISSUED_AT),
		"the statutory HUF rate is the one resolved on the fulfilment date"
	);
	// And the prices the customer saw did not move underneath them.
	assert_eq!(issued.net, draft.net);
	assert_eq!(issued.gross, draft.gross);
}

/// Nothing bounded the line count on any path. axum's 2 MB `Json` cap bounded a *wire* draft
/// at roughly 20k lines and `Invoices::draft` called from Rust at nothing — and `pdf::run`
/// hands the table to `typst::compile` with no timeout, eight retries behind it, on an
/// invoice that is immutable by the time the renders start failing.
#[tokio::test]
async fn a_draft_past_the_line_cap_is_refused() {
	let (db, sql) = fresh("draft-line-cap").await;
	let app = app_for(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app.clone());

	let lines = |n: usize| {
		(0..n)
			.map(|i| Line {
				code: None,
				description: format!("Tanacsadas {i}"),
				unit: "ora".into(),
				qty: Qty(1_000_000),
				unit_price: Some(Money(100)),
				vat_code: Some(VatCode::Std27),
				discount: None,
				discount_description: None,
				note: None,
			})
			.collect::<Vec<_>>()
	};
	let draft = |lines: Vec<Line>| NewDraft {
		request_id: None,
		billing_party: Party::OrgDefault,
		lines,
		discount: None,
		payment_method: None,
		currency: None,
		fulfilment_date: None,
		due_date: None,
		notes: None,
		..NewDraft::default()
	};

	let err = invoices.draft(&ctx, &draft(lines(draft::MAX_LINES + 1))).await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-LINE");
	// The cap itself is accepted, so this is a boundary and not a blanket refusal.
	assert!(invoices.draft(&ctx, &draft(lines(draft::MAX_LINES))).await.is_ok());
}

/// `currency::to_base` **multiplies** by `rate_e6`, so a stored `0` did not fail — it made
/// `net_huf`, `vat_huf` and `gross_huf` all `0.00`, satisfied the `huf_rate_e6 IS NOT NULL`
/// CHECK, and froze onto an ISSUED invoice as the Áfa tv. 172. § figure.
///
/// The row is inserted past the table's own `CHECK (rate_e6 > 0)` on purpose: MANUAL/BANK
/// rows and data imports are what the Rust guard exists for, and the two layers are
/// independent.
#[tokio::test]
async fn a_non_positive_stored_rate_is_no_rate_at_all() {
	let (_db, sql) = fresh("currency-nonpositive").await;
	sqlx::query("PRAGMA ignore_check_constraints = ON")
		.execute(sql.write_pool())
		.await
		.unwrap();
	for rate in [0_i64, -400_000_000] {
		sqlx::query(
			"INSERT OR REPLACE INTO currency_rates (pair, date, source, rate_e6, fetched_at)
			 VALUES ('EURHUF', '2026-06-01', 'MANUAL', ?, 0)",
		)
		.bind(rate)
		.execute(sql.write_pool())
		.await
		.unwrap();
		let err = rate_on(&sql, "EURHUF", "MANUAL", "2026-06-01", 7)
			.await
			.expect_err("a non-positive rate must not price an invoice");
		assert_eq!(err.parts().1, "E-INV-NO-RATE", "{rate}");
	}
}

/// `common.xsd`'s `VatCodeType` is `[1-5]{1}`, but `groupTaxNo` was checked for a digit count
/// only. `mintworks_nav::xml` files it as `groupMemberTaxNumber` through the same splitter as the
/// buyer's own number, so a `9` in the 9th position failed the schema on every attempt of an
/// invoice that already carried a number.
#[test]
fn vat_code_ok_accepts_only_the_codes_nav_files() {
	for good in ["123456781", "123456782", "123456783", "123456784", "123456785"] {
		assert!(mintworks_invoice::vat_code_ok(good), "{good}");
	}
	for bad in ["123456780", "123456786", "123456787", "123456788", "123456789"] {
		assert!(!mintworks_invoice::vat_code_ok(bad), "{bad}");
	}
	// No 9th digit is no `base:vatCode` at all — the element is optional.
	assert!(mintworks_invoice::vat_code_ok("12345678"));
	assert!(mintworks_invoice::vat_code_ok(""));
}

/// The group number reaches `base:vatCode` exactly as the buyer's own does, but only the
/// buyer's side carried the `[1-5]` rule — so the party was stored and every invoice drawn
/// from it was unfilable forever (`jobs.max_attempts.NAV_REPORT` is 0).
#[tokio::test]
async fn a_group_tax_number_with_an_unfilable_vat_code_is_refused() {
	let (db, sql) = fresh("party-group-vat-code").await;
	let app = app_for(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app);

	let party = |group: &str| mintworks_invoice::store::PartyPatch {
		kind: Some(mintworks_invoice::store::PartyKind::Company),
		name: Some("Csoport Kft".into()),
		country: Some("HU".into()),
		group_tax_no: Patch::Value(group.to_owned()),
		..Default::default()
	};
	let err = invoices.create_party(&ctx, &party("123456789012")).await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-BAD-TEXT");

	let ok = invoices.create_party(&ctx, &party("123456781012")).await.unwrap();
	assert_eq!(ok.group_tax_no.as_deref(), Some("123456781012"));
}

/// `draft::resolve` discards a catalogue line's price and tax code, and `Line` used to carry
/// `Money::ZERO`/`Std27` sentinels that were indistinguishable from a caller's assertion — so
/// the router refused the combination and `Invoices::draft`, the actual trust boundary,
/// billed at the catalogue price with no diagnostic.
#[tokio::test]
async fn a_catalogue_line_cannot_assert_its_own_price_or_tax() {
	let (db, sql) = fresh("catalogue-line-assert").await;
	let app = app_for(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app);

	let with = |unit_price, vat_code| NewDraft {
		request_id: None,
		billing_party: Party::OrgDefault,
		lines: vec![Line { unit_price, vat_code, ..Line::code("HOUR", Qty(1_000_000)) }],
		discount: None,
		payment_method: None,
		currency: None,
		fulfilment_date: None,
		due_date: None,
		notes: None,
		..NewDraft::default()
	};
	for req in [
		with(Some(Money(100)), None),
		with(None, Some(VatCode::Aam)),
		with(Some(Money(100)), Some(VatCode::Aam)),
	] {
		let err = invoices.draft(&ctx, &req).await.unwrap_err();
		assert_eq!(err.parts().1, "E-INV-LINE");
	}
	// Unset still resolves — the refusal is about the assertion, not about catalogue lines.
	// No `services` row exists, so this gets as far as the lookup and no further.
	let err = invoices.draft(&ctx, &with(None, None)).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-NOTFOUND");
}

/// `vies_checks` is keyed by EU VAT id and upserted on every refresh, so it cannot be the
/// evidence for a five-year-old EUFAD37 invoice: the consultation number that justified the
/// reverse charge has to be frozen onto the invoice with the rest of the buyer snapshot.
#[tokio::test]
async fn the_vies_consultation_number_is_frozen_onto_the_invoice() {
	let (db, sql) = fresh("vies-snapshot").await;
	let app = app_for(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app.clone());

	// A fresh cache hit, so `vies::check` never reaches the network.
	let cache = |request_id: &str| {
		let sql = sql.clone();
		let request_id = request_id.to_owned();
		async move {
			sqlx::query(
				"INSERT OR REPLACE INTO vies_checks
				 (eu_vat_id, valid, name, address, request_id, requester, checked_at)
				 VALUES ('DE811907980', 1, 'Kaufer GmbH', NULL, ?, 'HU12345678', ?)",
			)
			.bind(request_id)
			.bind(Timestamp::now().0)
			.execute(sql.write_pool())
			.await
			.unwrap();
		}
	};
	cache("WAPIAAAAXpXH8Ex1").await;

	let buyer = invoices
		.create_party(
			&ctx,
			&mintworks_invoice::store::PartyPatch {
				kind: Some(mintworks_invoice::store::PartyKind::Company),
				name: Some("Kaufer GmbH".into()),
				country: Some("DE".into()),
				tax_number: mintworks_core::types::Patch::Value("811907980".into()),
				eu_vat_id: mintworks_core::types::Patch::Value("DE811907980".into()),
				postcode: mintworks_core::types::Patch::Value("10115".into()),
				city: mintworks_core::types::Patch::Value("Berlin".into()),
				street: mintworks_core::types::Patch::Value("Unter den Linden 1.".into()),
				is_default: Some(true),
				..Default::default()
			},
		)
		.await
		.unwrap();

	let issued = invoices
		.issue_now(
			&ctx,
			&NewDraft {
				request_id: None,
				billing_party: Party::Uid(buyer.uid.clone()),
				lines: vec![Line::adhoc(
					"Tanacsadas",
					"ora",
					Qty(1_000_000),
					Money(100_000),
					VatCode::Std27,
				)],
				discount: None,
				payment_method: None,
				currency: None,
				fulfilment_date: None,
				due_date: None,
				notes: None,
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(issued.buyer_vies_request_id.as_deref(), Some("WAPIAAAAXpXH8Ex1"));
	assert!(issued.buyer_vies_checked_at.is_some());

	// The refresh that used to erase the evidence.
	cache("WAPIAAAAsomethingelse").await;
	let reloaded = sql.invoice_by_id(issued.id).await.unwrap().unwrap();
	assert_eq!(reloaded.buyer_vies_request_id.as_deref(), Some("WAPIAAAAXpXH8Ex1"));
}

/// VIES issues no consultation number without a requester, so the requester-less check must
/// not hand back the one another seller's check cached: it would be frozen as this seller's.
#[tokio::test]
async fn a_requester_less_check_does_not_inherit_a_cached_consultation_number() {
	let (db, sql) = fresh("vies-no-requester").await;
	let app = app_for(&db, &sql).await;
	sqlx::query(
		"INSERT INTO vies_checks
		 (eu_vat_id, valid, name, address, request_id, requester, checked_at)
		 VALUES ('DE811907980', 1, NULL, NULL, 'WAPIAAAAXpXH8Ex1', 'AT12345678', ?)",
	)
	.bind(Timestamp::now().0)
	.execute(sql.write_pool())
	.await
	.unwrap();

	let hit = mintworks_invoice::vies::check(&app, "DE811907980", None, None).await.unwrap();
	assert!(hit.valid && hit.cached);
	assert_eq!(hit.request_id, None);
}

/// `Invoices::vies` is budgeted per org (`invoice.vies`), charged only on a real VIES call: a
/// drained budget still answers from the cache, and refuses an uncached id before any call.
#[tokio::test]
async fn vies_lookups_are_rate_limited_per_org_on_cache_misses_only() {
	let (db, sql) = fresh("vies-ratelimit").await;
	let app = app_for(&db, &sql).await;
	sqlx::query(
		"INSERT INTO vies_checks (eu_vat_id, valid, name, address, request_id, requester, checked_at)
		 VALUES ('DE811907980', 1, NULL, NULL, NULL, NULL, ?)",
	)
	.bind(Timestamp::now().0)
	.execute(sql.write_pool())
	.await
	.unwrap();
	let inv = Invoices::new(app.clone());
	let a = Ctx::system("test").with_org(1);
	for _ in 0..30 {
		inv.vies(&a, "DE811907980").await.unwrap();
	}
	// Thirty cache hits spent nothing; drain the budget as thirty misses would.
	for _ in 0..30 {
		app.limits.check(&app.settings, "invoice.vies", "1").await.unwrap();
	}
	inv.vies(&a, "DE811907980").await.unwrap();
	let err = inv.vies(&a, "ATU12345678").await.unwrap_err();
	assert!(matches!(err, Error::RateLimit(_)), "{err:?}");
}

/// `003_invoice.sql` seeds `HUF` as `FIXED` at 1.000000 — "base units per 1 HUF", true only
/// when `currency.base` *is* HUF. On a EUR base `effective_rate_e6` still answered with it,
/// because the guard there only refuses a base other than the configured one, so a 10.00 EUR
/// item billed as 10.00 HUF and `A-RATE-MISSING` never fired: the lookup had succeeded.
#[tokio::test]
async fn a_eur_base_refuses_to_start_on_the_seeded_huf_row() {
	let (db, sql) = fresh("currency-base-check").await;
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(sql.clone()) as Arc<dyn CoreStore>)
		.settings(mintworks_invoice::SETTINGS)
		.extension(Arc::new(sql.clone()) as Arc<dyn InvoiceStore>)
		.build()
		.await
		.unwrap();

	// A HUF base is what the seed means, so it boots.
	mintworks_invoice::check_currency_settings(&app).await.unwrap();

	app.settings.set("currency.base", "EUR", None).await.unwrap();
	let err = mintworks_invoice::check_currency_settings(&app)
		.await
		.expect_err("1 HUF = 1 EUR must not price an invoice");
	assert!(err.to_string().contains("EUR"), "the message has to name the fix: {err}");

	sqlx::query("UPDATE currencies SET mode = 'OFFICIAL', fixed_rate_e6 = NULL WHERE code = 'HUF'")
		.execute(sql.write_pool())
		.await
		.unwrap();
	mintworks_invoice::check_currency_settings(&app).await.unwrap();
}

/// `currency.enabled` enables the seeded-but-disabled EUR and USD, skips a code it cannot parse
/// and blank entries, and is idempotent across boots.
#[tokio::test]
async fn currency_enabled_enables_listed_codes_and_skips_the_rest() {
	let (db, sql) = fresh("currency-enabled").await;
	let app = app_for(&db, &sql).await;
	let enabled = async || -> Vec<String> {
		sqlx::query_scalar("SELECT code FROM currencies WHERE enabled = 1 ORDER BY code")
			.fetch_all(sql.read_pool())
			.await
			.unwrap()
	};
	assert_eq!(enabled().await, ["HUF"], "EUR and USD are seeded disabled");

	app.settings.set("currency.enabled", "EUR, bogus,,USD", None).await.unwrap();
	mintworks_invoice::enable_configured_currencies(&app).await.unwrap();
	assert_eq!(enabled().await, ["EUR", "HUF", "USD"]);
	mintworks_invoice::enable_configured_currencies(&app).await.unwrap();
	assert_eq!(enabled().await, ["EUR", "HUF", "USD"]);
}

/// `update_notes` carries no kind predicate and `Patch::Null` reached it, so an explicit
/// `{"notes": null}` erased the statutory justification `storno::run` wrote on a numbered,
/// immutable counter-invoice.
#[tokio::test]
async fn a_stornos_cancellation_reason_cannot_be_cleared() {
	let (db, sql) = fresh("storno-notes").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());

	let issued = invoices
		.issue_now(
			&ctx,
			&NewDraft {
				request_id: None,
				billing_party: Party::OrgDefault,
				lines: vec![Line::adhoc(
					"Tanacsadas",
					"ora",
					Qty(1_000_000),
					Money(100_000),
					VatCode::Std27,
				)],
				discount: None,
				payment_method: None,
				currency: None,
				fulfilment_date: None,
				due_date: None,
				notes: None,
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	let storno = invoices.storno(&ctx, issued.uid.as_str(), "elirt osszeg").await.unwrap();
	assert_eq!(storno.notes.as_deref(), Some("elirt osszeg"));

	let err = invoices
		.patch(
			&ctx,
			storno.uid.as_str(),
			&InvoicePatch { notes: Patch::Null, ..InvoicePatch::default() },
		)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-IMMUTABLE");
	let reloaded = sql.invoice_by_id(storno.id).await.unwrap().unwrap();
	assert_eq!(reloaded.notes.as_deref(), Some("elirt osszeg"));

	// Replacing the text stays legal — only the clear is refused.
	let edited = invoices
		.patch(
			&ctx,
			storno.uid.as_str(),
			&InvoicePatch { notes: Patch::Value("javitva".into()), ..InvoicePatch::default() },
		)
		.await
		.unwrap();
	assert_eq!(edited.notes.as_deref(), Some("javitva"));
}

/// One `NewDraft` with a single ad-hoc line, the shape every test below wants.
fn one_line_draft() -> NewDraft {
	NewDraft {
		request_id: None,
		billing_party: Party::OrgDefault,
		lines: vec![Line::adhoc(
			"Tanacsadas",
			"ora",
			Qty(1_000_000),
			Money(100_000),
			VatCode::Std27,
		)],
		discount: None,
		payment_method: None,
		currency: None,
		fulfilment_date: None,
		due_date: None,
		notes: None,
		..NewDraft::default()
	}
}

/// `update_draft` binds `discount_value = COALESCE(?, discount_value)` and re-prices nothing,
/// so a lone `discount_value` moved the column while `{net,vat,gross}` and every line's
/// `discount_amount` kept the old figures — and with `discount_kind` NULL, `discount_of` then
/// raised `E-INV-DISCOUNT` on every issue attempt, with `COALESCE` unable to put the column
/// back. The draft was unissuable and uncorrectable until `SWEEP_DRAFTS` collected it.
#[tokio::test]
async fn a_lone_discount_patch_is_refused_not_left_unpriced() {
	let (db, sql) = fresh("lone-discount-patch").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());

	let draft = invoices.draft(&ctx, &one_line_draft()).await.unwrap();
	let err = invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch { discount_value: Some(1_000), ..InvoicePatch::default() },
		)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-DISCOUNT");
	assert_eq!(err.parts().0.as_u16(), 400);

	// Untouched, and still issuable — the point of refusing rather than half-applying.
	let reloaded = sql.invoice_by_id(draft.id).await.unwrap().unwrap();
	assert_eq!(reloaded.discount_value, None);
	invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
}

/// §5.5 lets an `ISSUED` invoice take a note, and `update_notes` carries no status predicate —
/// but `pdf::run` short-circuits on an existing `invoice_documents` row and the PDF is served
/// `immutable, max-age=31536000`. So a note edited after rendering left `GET /invoices/{uid}`
/// reporting one text and the statutory document showing another, forever.
#[tokio::test]
async fn a_note_cannot_be_edited_once_the_document_is_rendered() {
	let (db, sql) = fresh("note-after-render").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());

	let issued = invoices.issue_now(&ctx, &one_line_draft()).await.unwrap();
	// Before `RENDER_PDF` runs, the note is still the caller's to correct.
	let edited = invoices
		.patch(
			&ctx,
			issued.uid.as_str(),
			&InvoicePatch { notes: Patch::Value("elso".into()), ..InvoicePatch::default() },
		)
		.await
		.unwrap();
	assert_eq!(edited.notes.as_deref(), Some("elso"));

	assert!(
		sql.put_invoice_document(
			&InvoiceDocument {
				invoice_id: issued.id,
				kind: "PDF".to_owned(),
				sha256: "aaa".to_owned(),
				bytes: 10,
				template_version: "v1".to_owned(),
				rendered_at: Timestamp(100),
			},
			edited.version,
		)
		.await
		.unwrap()
	);

	let err = invoices
		.patch(
			&ctx,
			issued.uid.as_str(),
			&InvoicePatch { notes: Patch::Value("masodik".into()), ..InvoicePatch::default() },
		)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-IMMUTABLE");
	assert_eq!(err.parts().0.as_u16(), 409);
	let reloaded = sql.invoice_by_id(issued.id).await.unwrap().unwrap();
	assert_eq!(reloaded.notes.as_deref(), Some("elso"), "the rendered note stands");
}

/// Every `Invoices` method funnels through the private `invoice(ctx, uid)` helper, whose
/// whole org isolation is `invoice_by_uid(Some(org), uid)` — and the trait takes
/// `Option<i64>`, so `None` (the legitimate operator path) is one keystroke away. Nothing
/// drove another org's *invoice* uid through any of them.
///
/// `E-CORE-NOTFOUND`, never 403: another org's uid must be indistinguishable from one that does
/// not exist.
#[tokio::test]
async fn another_orgs_invoice_is_not_found_not_forbidden() {
	let (db, sql) = fresh("cross-org-invoice").await;
	let app = app_for(&db, &sql).await;
	let invoices = Invoices::new(app.clone());

	// Org 2, with its own default billing party — `app_for` seeds org 1's.
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (2, 'org_u', 1, 'SHARED', 'Masik', 1, 0)",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (2, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0C', 2, 'C', 'Masik Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 1, 0, 0)",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();

	let mut b = Ctx::system("test").with_org(2);
	b.auth_at = Some(Timestamp::now().0);
	let theirs = invoices.draft(&b, &one_line_draft()).await.unwrap();

	let mut a = Ctx::system("test").with_org(1);
	a.auth_at = Some(Timestamp::now().0);
	let uid = theirs.uid.as_str();
	let line = theirs.id; // only used to prove nothing moved

	let notes = InvoicePatch { notes: Patch::Value("bele".into()), ..InvoicePatch::default() };
	let refusals = [
		invoices.full(&a, uid).await.err(),
		invoices.patch(&a, uid, &notes).await.err(),
		invoices.patch_by_uid(&a, uid, None, None, &InvoicePatch::default()).await.err(),
		invoices
			.add_line(&a, uid, Line::adhoc("X", "db", Qty(1_000_000), Money(1_000), VatCode::Std27))
			.await
			.err(),
		invoices.delete_draft(&a, uid).await.err(),
		invoices.issue(&a, uid).await.err(),
		invoices.storno(&a, uid, "nem az enyem").await.err(),
	];
	for err in refusals {
		let err = err.expect("another org's invoice must not be reachable");
		assert_eq!(err.parts().1, "E-CORE-NOTFOUND", "403 discloses that the uid exists");
	}

	// And not one of them moved the row.
	let reloaded = sql.invoice_by_id(line).await.unwrap().unwrap();
	assert_eq!(
		(reloaded.status, reloaded.version, reloaded.number),
		(theirs.status, theirs.version, theirs.number)
	);
}

/// `read_money` bounds what comes *out* of the database, so an amount written past
/// `MAX_MINOR` makes the row undecodable — and `set_paid` reads the invoice first, so it
/// cannot put it back.
#[tokio::test]
async fn set_paid_refuses_an_amount_the_read_path_cannot_decode() {
	let (db, sql) = fresh("set-paid-envelope").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());

	let issued = invoices.issue_now(&ctx, &one_line_draft()).await.unwrap();
	let err = invoices
		.set_paid(&ctx, issued.uid.as_str(), Money(mintworks_core::money::MAX_MINOR + 1), None)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-VALIDATION");

	invoices
		.full(&ctx, issued.uid.as_str())
		.await
		.expect("the row is still readable");
}

/// One service row past the envelope fails `read_money` for the *whole* catalogue, so every
/// draft naming any code 400s — not just the bad one.
#[tokio::test]
async fn a_service_price_past_the_envelope_is_refused_not_stored() {
	let (db, sql) = fresh("service-price-envelope").await;
	let app = app_for(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app.clone());

	let def = ServiceDef {
		code: "PLAN".to_owned(),
		name: "Elofizetes".to_owned(),
		description: None,
		unit: "ho".to_owned(),
		unit_price: Money(mintworks_core::money::MAX_MINOR + 1),
		vat_code: VatCode::Std27,
		name_en: None,
		prices: None,
	};
	assert_eq!(
		invoices.create_service(&ctx, &def).await.unwrap_err().parts().1,
		"E-CORE-VALIDATION"
	);
	assert_eq!(
		invoices.sync_services(&ctx, &[def]).await.unwrap_err().parts().1,
		"E-CORE-VALIDATION"
	);

	assert!(invoices.list_services(&ctx, false, None).await.unwrap().is_empty());
}

/// The handle is the trust boundary: `seller` took `_ctx` and derived nothing from it, so
/// `SellerView`'s tax number and bank account were one forgotten route layer away from a public
/// caller.
#[tokio::test]
async fn the_seller_is_not_readable_without_an_org() {
	let (db, sql) = fresh("seller-authz").await;
	let app = app_for(&db, &sql).await;
	let invoices = Invoices::new(app.clone());

	let err = invoices.seller(&Ctx::public("test")).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");
	invoices.seller(&Ctx::system("test").with_org(1)).await.unwrap();
}

/// The documented `issue_now` example, compiled but never run. The documented call is the
/// contract, and the version before this one named a 4-argument `Line::adhoc` and passed
/// `IssueNow` by value. Compiling is the whole assertion — this harness seeds no catalogue, so
/// running it could only assert the refusal that absence produces.
#[allow(dead_code)]
async fn _doc_example(app: App, t: i64, order_id: i64) {
	const PLAN_PRO: &str = "PLAN_PRO";

	// `Qty` is scaled 1e6 and `Money` is minor units, so one unit is `Qty(1_000_000)` and 5 000 Ft
	// is `Money(500_000)` — no floats anywhere.
	let invoices = Invoices::new(app);
	let _inv = invoices
		.issue_now(
			&Ctx::system("checkout").with_org(t),
			&IssueNow {
				request_id: Some(format!("order-{order_id}")),
				billing_party: Party::OrgDefault,
				lines: vec![
					Line::code(PLAN_PRO, Qty(1_000_000)),
					Line::adhoc("Setup fee", "db", Qty(1_000_000), Money(500_000), VatCode::Std27),
				],
				..Default::default()
			},
		)
		.await;
}

/// SQLite reads a negative `LIMIT` as unbounded, and `list_full`/`list_invoices` passed the
/// caller's `limit` straight into `ORDER BY id DESC LIMIT ?`. The only clamp was in the route
/// bundle most consumers never mount, and validation belongs in the handle. The catalogue and
/// the billing parties had no `LIMIT` at all, so the same defect stood behind
/// `GET /api/services` and `GET /api/billing-parties`: an org grows both itself.
#[tokio::test]
async fn every_list_is_clamped_to_the_page_ceiling() {
	let (db, sql) = fresh("page-limit-clamp").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());

	for i in 0..3 {
		invoices.draft(&ctx, &one_line_draft()).await.unwrap();
		sql.create_service(
			1,
			&ServiceDef {
				code: format!("PLAN{i}"),
				name: "Elofizetes".to_owned(),
				description: None,
				unit: "ho".to_owned(),
				unit_price: Money(100_000),
				vat_code: VatCode::Std27,
				name_en: None,
				prices: None,
			},
		)
		.await
		.unwrap();
		sql.create_party(
			1,
			&PartyPatch {
				kind: Some(PartyKind::Company),
				name: Some(format!("Vevo {i} Kft.")),
				country: Some("HU".to_owned()),
				..PartyPatch::default()
			},
		)
		.await
		.unwrap();
	}

	// The invoice lists take the caller's limit, so both ends of the clamp are theirs.
	for (got, want) in [
		(
			invoices
				.list_full(&ctx, &InvoiceFilter::default(), None, -1)
				.await
				.unwrap()
				.len(),
			1,
		),
		(invoices.list_invoices(&ctx, None, -1).await.unwrap().len(), 1),
		(
			invoices
				.list_full(&ctx, &InvoiceFilter::default(), None, i64::MAX)
				.await
				.unwrap()
				.len(),
			3,
		),
		(invoices.list_invoices(&ctx, None, i64::MAX).await.unwrap().len(), 3),
	] {
		assert_eq!(got, want);
	}

	// The catalogue and the parties take no caller limit at all — the handle passes
	// `MAX_PAGE_LIMIT` — so what has to bind is the store's own `LIMIT`.
	assert_eq!(sql.list_services(1, false, None, 2).await.unwrap().len(), 2);
	assert_eq!(sql.list_services(1, false, None, MAX_PAGE_LIMIT).await.unwrap().len(), 3);
	// One party past the three: `app_for` seeds the org's default.
	assert_eq!(sql.list_parties(1, 2).await.unwrap().len(), 2);
	assert_eq!(sql.list_parties(1, MAX_PAGE_LIMIT).await.unwrap().len(), 4);
}

/// `pdf::run` read the invoice, compiled typst for seconds, then wrote the document row.
/// A note edit inside that window passed the reader-pool guard, and the PDF — served
/// `immutable, max-age=31536000` — kept the old note forever.
#[tokio::test]
async fn a_note_edit_mid_render_does_not_freeze_the_old_note() {
	let (db, sql) = fresh("note-edit-mid-render").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());

	let issued = invoices.issue_now(&ctx, &one_line_draft()).await.unwrap();
	// What `pdf::run` reads before it hands typst the document.
	let rendered_from = sql.invoice_by_id(issued.id).await.unwrap().unwrap();

	// …and the edit that commits while typst is still compiling.
	invoices
		.patch(
			&ctx,
			issued.uid.as_str(),
			&InvoicePatch { notes: Patch::Value("javitott".into()), ..InvoicePatch::default() },
		)
		.await
		.unwrap();

	let landed = sql
		.put_invoice_document(
			&InvoiceDocument {
				invoice_id: issued.id,
				kind: "PDF".to_owned(),
				sha256: "stale".to_owned(),
				bytes: 10,
				template_version: "v1".to_owned(),
				rendered_at: Timestamp(100),
			},
			rendered_from.version,
		)
		.await
		.unwrap();
	assert!(!landed, "a PDF built from the pre-edit note must not land");
	assert!(sql.invoice_document(issued.id).await.unwrap().is_none());

	// The re-render, against the note as it now stands, does land.
	let reread = sql.invoice_by_id(issued.id).await.unwrap().unwrap();
	assert_eq!(reread.notes.as_deref(), Some("javitott"));
	assert!(
		sql.put_invoice_document(
			&InvoiceDocument {
				invoice_id: issued.id,
				kind: "PDF".to_owned(),
				sha256: "fresh".to_owned(),
				bytes: 10,
				template_version: "v1".to_owned(),
				rendered_at: Timestamp(100),
			},
			reread.version,
		)
		.await
		.unwrap()
	);
}

/// `draft.rs` stores a draft line's *product* code, so a reverse-charge draft served
/// `{"vatCode":"STD27","vatRateBp":0}` beside a `vatSummary` entry keyed `EUFAD37` — and
/// `vatCode` is the join key between the two, so nothing reconciled.
/// Fixed in the view alone: the stored product code is what lets a later buyer change
/// re-derive the verdict from scratch.
#[tokio::test]
async fn a_reverse_charge_drafts_line_vat_code_matches_its_vat_summary_group() {
	let (db, sql) = fresh("draft-effective-vat-code").await;
	let app = app_for(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app.clone());

	// A fresh VIES hit, so `vies::check` never reaches the network — the reverse charge needs
	// a validated community VAT id.
	sqlx::query(
		"INSERT INTO vies_checks
		 (eu_vat_id, valid, name, address, request_id, requester, checked_at)
		 VALUES ('DE811907980', 1, 'Kaufer GmbH', NULL, 'WAPIAAAAXpXH8Ex1', 'HU12345678', ?)",
	)
	.bind(Timestamp::now().0)
	.execute(sql.write_pool())
	.await
	.unwrap();
	let buyer = invoices
		.create_party(
			&ctx,
			&PartyPatch {
				kind: Some(PartyKind::Company),
				name: Some("Kaufer GmbH".into()),
				country: Some("DE".into()),
				tax_number: Patch::Value("811907980".into()),
				eu_vat_id: Patch::Value("DE811907980".into()),
				postcode: Patch::Value("10115".into()),
				city: Patch::Value("Berlin".into()),
				street: Patch::Value("Unter den Linden 1.".into()),
				..Default::default()
			},
		)
		.await
		.unwrap();

	let draft = invoices
		.draft(&ctx, &NewDraft { billing_party: Party::Uid(buyer.uid.clone()), ..one_line_draft() })
		.await
		.unwrap();
	let view = InvoiceView::of(invoices.full(&ctx, draft.uid.as_str()).await.unwrap());
	let groups = view.vat_summary.as_ref().unwrap();
	assert_eq!(groups.len(), 1, "an Override verdict is one group");
	assert_eq!(groups[0].vat_code, VatCode::Eufad37);
	assert_eq!(
		view.lines.as_ref().unwrap()[0].vat_code,
		VatCode::Eufad37,
		"the line must join to its own summary row"
	);

	// The `Verdict::Product` side: a domestic buyer with two rates keeps each line's own code.
	let mixed = invoices
		.draft(
			&ctx,
			&NewDraft {
				lines: vec![
					Line::adhoc(
						"Tanacsadas",
						"ora",
						Qty(1_000_000),
						Money(100_000),
						VatCode::Std27,
					),
					Line::adhoc("Konyv", "db", Qty(1_000_000), Money(50_000), VatCode::Red05),
				],
				..one_line_draft()
			},
		)
		.await
		.unwrap();
	let view = InvoiceView::of(invoices.full(&ctx, mixed.uid.as_str()).await.unwrap());
	assert_eq!(view.vat_summary.as_ref().unwrap().len(), 2);
	let codes: Vec<_> = view.lines.as_ref().unwrap().iter().map(|l| l.vat_code).collect();
	assert_eq!(codes, vec![VatCode::Std27, VatCode::Red05]);
}

/// `hydrate` took a `_ctx` it ignored, and `party_by_id`/`invoice_by_id` carry no org predicate
/// — so the one `pub` method a consumer renders an invoice through would resolve another org's
/// party uid onto the wire. Absent, never 403.
#[tokio::test]
async fn hydrate_does_not_resolve_another_orgs_party_uid() {
	let (db, sql) = fresh("hydrate-org-scope").await;
	let app = app_for(&db, &sql).await;
	let invoices = Invoices::new(app.clone());

	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (2, 'org_other', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Masik', 1, 0)",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (2, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0C', 2, 'C', 'Masik Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 1, 0, 0)",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();

	let ctx = Ctx::system("test").with_org(1);
	let draft = invoices.draft(&ctx, &one_line_draft()).await.unwrap();
	// The cross-org pointing no service method would make: `hydrate` is `pub`, so it must
	// not trust the row it is handed.
	sqlx::query("UPDATE invoices SET billing_party_id = 2 WHERE id = ?")
		.bind(draft.id)
		.execute(sql.write_pool())
		.await
		.unwrap();

	let row = sql.invoice_by_id(draft.id).await.unwrap().unwrap();
	let full = invoices.hydrate(&ctx, row, true).await.unwrap();
	assert!(full.party_uid.is_none(), "another org's party must read as absent");
}

/// The single-invoice read carries a `document` block, and `InvoiceView` had no such field —
/// so a client learned whether a PDF existed only by calling `GET …/pdf` and handling
/// `E-INV-PDF-PENDING`. On `with_lines`, so a listing pays no per-row read.
#[tokio::test]
async fn the_single_invoice_read_carries_its_document_block() {
	let (db, sql) = fresh("invoice-document-block").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());

	let issued = invoices.issue_now(&ctx, &one_line_draft()).await.unwrap();
	// Issued but not yet rendered: the field is absent, not an error.
	let view = InvoiceView::of(invoices.full(&ctx, issued.uid.as_str()).await.unwrap());
	assert!(view.document.is_none());

	assert!(
		sql.put_invoice_document(
			&InvoiceDocument {
				invoice_id: issued.id,
				kind: "PDF".to_owned(),
				sha256: "c0ffee".to_owned(),
				bytes: 84_213,
				template_version: "2026-09-01".to_owned(),
				rendered_at: Timestamp(1_759_230_249),
			},
			issued.version,
		)
		.await
		.unwrap()
	);

	let view = InvoiceView::of(invoices.full(&ctx, issued.uid.as_str()).await.unwrap());
	let doc = view.document.expect("the rendered document is on the single read");
	assert_eq!((doc.kind.as_str(), doc.sha256.as_str(), doc.bytes), ("PDF", "c0ffee", 84_213));
	assert_eq!(doc.template_version, "2026-09-01");

	// The listing must not pay a per-row read for it.
	let page = invoices
		.list_full(&ctx, &InvoiceFilter::default(), None, MAX_PAGE_LIMIT)
		.await
		.unwrap();
	assert!(page.iter().all(|f| f.document.is_none()), "a listing carries no document block");
}

/// The branch tested `rate_e6 != 1_000_000` as a stand-in for "not the base currency". A
/// non-base currency published at exactly 1.000000 on the draft's day therefore fell through
/// to `rewrite`, which leaves `rate_e6` alone by design — so the row claimed a rate its lines
/// were never priced at, and that compounded onto an immutable invoice at issue.
#[tokio::test]
async fn a_foreign_currency_at_exactly_one_still_reprices_on_a_fulfilment_date_patch() {
	const ON_THE_DAY: i64 = 1_000_000;
	const LATER: i64 = 400_000_000;

	let (db, sql) = fresh("fulfilment-patch-base-check").await;
	let app = app_for(&db, &sql).await;
	app.settings.set("currency.rate_source", "MNB", None).await.unwrap();
	sqlx::query(
		"INSERT INTO currencies (code, price_round_step, mode, fee_bp, enabled)
		 VALUES ('EUR', 1, 'OFFICIAL', 0, 1) ON CONFLICT (code) DO UPDATE SET price_round_step = excluded.price_round_step, mode = excluded.mode, fee_bp = excluded.fee_bp, enabled = excluded.enabled",
	)
	.execute(sql.write_pool())
	.await
	.unwrap();
	opt_in_eur(&sql).await;

	let now = Timestamp::now().0;
	let first = numbering::date_of(Timestamp(now - 3 * 86_400)).unwrap();
	let second = numbering::date_of(Timestamp(now - 86_400)).unwrap();
	sql.mnb_upsert_rates("EURHUF", &[(first.clone(), ON_THE_DAY), (second.clone(), LATER)])
		.await
		.unwrap();

	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app.clone());
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				fulfilment_date: Some(first),
				..one_line_draft()
			},
		)
		.await
		.unwrap();
	assert_eq!(draft.rate_e6, ON_THE_DAY, "the pathological published rate this is about");

	let patched = invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch { fulfilment_date: Patch::Value(second), ..InvoicePatch::default() },
		)
		.await
		.unwrap();
	assert_eq!(patched.rate_e6, LATER, "the moved date must re-resolve the rate");
	// The HUF trio is what the stale rate corrupted — Áfa tv. 172. § requires that figure.
	let full = invoices.full(&ctx, patched.uid.as_str()).await.unwrap();
	let group = &full.groups.as_ref().unwrap()[0];
	assert_eq!(group.net_huf, Some(Money(group.net.0 * 400)));
}

/// `NAV_REPORT` answers `Unavailable` until `RENDER_PDF` has landed, so one `run_at` for both
/// made every invoice pay a `2^attempts` backoff step for a race it always loses.
#[tokio::test]
async fn the_nav_filing_is_queued_behind_the_pdf_render() {
	let (db, sql) = fresh("job-run-at").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());

	let draft = invoices.draft(&ctx, &one_line_draft()).await.unwrap();
	invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();

	// `ORDER BY kind` puts NAV_REPORT first.
	let queued: Vec<(String, i64)> = sqlx::query_as(
		"SELECT kind, run_at FROM jobs WHERE kind IN ('NAV_REPORT', 'RENDER_PDF') ORDER BY kind",
	)
	.fetch_all(sql.read_pool())
	.await
	.unwrap();
	assert_eq!(queued.len(), 2, "issue queues both jobs");
	assert!(queued[0].1 >= queued[1].1 + 15, "the filing waits for the render: {queued:?}");
}

/// `RENDER_PDF` stores through the blob store, and `Invoices::document` finds the file there
/// rather than at the legacy `documents/` path, which only pre-`invoice-5` rows still use.
#[tokio::test]
async fn the_rendered_pdf_lands_in_the_blob_store() {
	let (db, sql) = fresh("pdf-blob-store").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());

	let issued = invoices.issue_now(&ctx, &one_line_draft()).await.unwrap();
	mintworks_invoice::pdf::run(&app, &sql, issued.id).await.unwrap();

	let (_, doc, path) = invoices.document(&ctx, issued.uid.as_str()).await.unwrap();
	let data_dir = &app.config.data_dir;
	assert_eq!(doc.template_version, mintworks_invoice::TEMPLATE_VERSION);
	assert_eq!(path, mintworks_core::blob::blob_path(data_dir, &doc.sha256).unwrap());
	assert!(std::fs::read(&path).unwrap().starts_with(b"%PDF"));
	let legacy = mintworks_invoice::pdf::doc_path(data_dir, &doc.sha256).unwrap();
	assert!(!legacy.exists(), "written to the legacy documents/ tree");
}

/// The email's link serves the issued file to its code alone, answers `NotFound` for anything
/// else, and every download — link or app — leaves a `DOWNLOAD` audit row; the link's carries the
/// caller's IP.
#[tokio::test]
async fn the_download_link_serves_the_pdf_and_every_download_is_audited() {
	let (db, sql) = fresh("pdf-download-link").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());
	let issued = invoices.issue_now(&ctx, &one_line_draft()).await.unwrap();
	mintworks_invoice::pdf::run(&app, &sql, issued.id).await.unwrap();
	let uid = issued.uid.as_str();

	let (_, doc, url) = invoices.document_link(&ctx, uid).await.unwrap();
	let code = url.rsplit('/').nth(1).unwrap();
	assert!(url.ends_with(&format!("/api/public/invoices/{code}/pdf")), "{url}");
	let refs = Refs::from_app(&app).unwrap();
	let link = refs.by_code(code).await.unwrap().unwrap();
	assert_eq!((link.ref_type.as_str(), link.org_id), ("invoice_download", 1));
	assert_eq!(link.target.as_deref(), Some(uid));
	assert_eq!(link.uses_left, None);
	assert!(link.expires_at.is_some_and(|t| t > Timestamp::now()));

	let ip: std::net::IpAddr = "203.0.113.7".parse().unwrap();
	let public = Ctx::public("invoice-download").with_ip(ip);
	let (_, got, _) = invoices.download_by_link(&public, code).await.unwrap();
	assert_eq!(got.sha256, doc.sha256);

	let e = invoices.download_by_link(&public, "NOSUCHCODE00").await.unwrap_err();
	assert!(matches!(e, Error::NotFound), "unknown code");

	// One thing wrong each, on an otherwise good row.
	let good = NewRef {
		uid: RefId::generate(),
		code: String::new(),
		ref_type: "invoice_download".to_owned(),
		org_id: 1,
		created_by: None,
		target: Some(uid.to_owned()),
		email: None,
		params: serde_json::json!({ "sha256": doc.sha256 }),
		uses_left: None,
		expires_at: None,
	};
	let bad = [
		("expired", NewRef { expires_at: Some(Timestamp(1)), ..good.clone() }),
		("wrong-type", NewRef { ref_type: "coupon".to_owned(), ..good.clone() }),
		("other-org", NewRef { org_id: ROOT, ..good.clone() }),
		("other-file", NewRef { params: serde_json::json!({ "sha256": "00" }), ..good.clone() }),
		("no-target", NewRef { target: None, ..good.clone() }),
	];
	for (name, row) in bad {
		let row = NewRef { uid: RefId::generate(), code: name.to_owned(), ..row };
		sql.ref_insert(&row).await.unwrap();
		let e = invoices.download_by_link(&public, name).await.unwrap_err();
		assert!(matches!(e, Error::NotFound), "{name}");
	}

	let e = refs.preview(code, None).await.unwrap_err();
	assert_eq!(e.parts().1, "E-CORE-REF-INVALID", "a probe cannot tell a live link");

	let (_, _, second) = invoices.document_link(&ctx, uid).await.unwrap();
	let second = second.rsplit('/').nth(1).unwrap();
	let row = refs.by_code(second).await.unwrap().unwrap();
	refs.revoke(&ctx, row.uid.as_str()).await.unwrap();
	let e = invoices.download_by_link(&public, second).await.unwrap_err();
	assert!(matches!(e, Error::NotFound), "revoked");

	invoices.download(&ctx, uid).await.unwrap();
	let rows: Vec<(Option<String>, String, Option<i64>)> = sqlx::query_as(
		"SELECT ip, detail, org_id FROM audit_logs WHERE entity = 'invoice' AND entity_id = ?
		   AND action = 'DOWNLOAD' ORDER BY id",
	)
	.bind(uid)
	.fetch_all(sql.read_pool())
	.await
	.unwrap();
	assert_eq!(rows.len(), 2, "{rows:?}");
	assert_eq!(rows[0].0.as_deref(), Some("203.0.113.7"));
	assert!(rows[0].1.contains("\"via\":\"link\""), "{}", rows[0].1);
	assert_eq!(rows[0].2, Some(1), "a public download has no org of its own");
	assert!(rows[1].1.contains("\"via\":\"app\""), "{}", rows[1].1);
}

/// A suspended org's links stop serving, like every other dead link.
#[tokio::test]
async fn a_suspended_orgs_link_is_not_found_and_not_audited() {
	let (db, sql) = fresh("pdf-download-suspended").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());
	let issued = invoices.issue_now(&ctx, &one_line_draft()).await.unwrap();
	mintworks_invoice::pdf::run(&app, &sql, issued.id).await.unwrap();
	let (_, _, url) = invoices.document_link(&ctx, issued.uid.as_str()).await.unwrap();
	let code = url.rsplit('/').nth(1).unwrap();

	sqlx::query("UPDATE orgs SET status = 'SUSPENDED' WHERE id = 1")
		.execute(sql.write_pool())
		.await
		.unwrap();
	let public = Ctx::public("invoice-download");
	let e = invoices.download_by_link(&public, code).await.unwrap_err();
	assert!(matches!(e, Error::NotFound), "{e:?}");

	let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_logs WHERE action = 'DOWNLOAD'")
		.fetch_one(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(rows, 0);
}

/// A live link whose file is gone is a dead link, not a 503 a probe can tell apart, and nothing
/// was downloaded, so no `DOWNLOAD` row is written.
#[tokio::test]
async fn a_link_to_a_missing_file_is_not_found_and_not_audited() {
	let (db, sql) = fresh("pdf-download-missing").await;
	let app = app_for(&db, &sql).await;
	let mut ctx = Ctx::system("test").with_org(1);
	ctx.auth_at = Some(Timestamp::now().0);
	let invoices = Invoices::new(app.clone());
	let issued = invoices.issue_now(&ctx, &one_line_draft()).await.unwrap();
	mintworks_invoice::pdf::run(&app, &sql, issued.id).await.unwrap();
	let uid = issued.uid.as_str();
	let (_, _, url) = invoices.document_link(&ctx, uid).await.unwrap();
	let code = url.rsplit('/').nth(1).unwrap();

	let (_, _, path) = invoices.document(&ctx, uid).await.unwrap();
	std::fs::remove_file(&path).unwrap();

	let public = Ctx::public("invoice-download");
	let e = invoices.download_by_link(&public, code).await.unwrap_err();
	assert!(matches!(e, Error::NotFound), "{e:?}");
	let e = invoices.download(&ctx, uid).await.unwrap_err();
	assert!(matches!(e, Error::Unavailable(_)), "{e:?}");

	let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_logs WHERE action = 'DOWNLOAD'")
		.fetch_one(sql.read_pool())
		.await
		.unwrap();
	assert_eq!(rows, 0);
}

/// One blob row of `size` bytes charged to `org`, standing in for whatever filled the quota.
async fn hold_bytes(sql: &SqliteStore, org: i64, size: i64) {
	sqlx::query(
		"INSERT INTO blobs (uid, org_id, sha256, mime, size, created_at, unref_at)
		 VALUES ('blb_01JCZ5X8K9N7QW3M6R2T4V8Y0F', ?, 'ab', 'application/octet-stream', ?, 0, NULL)",
	)
	.bind(org)
	.bind(size)
	.execute(sql.write_pool())
	.await
	.unwrap();
}

/// The invoice PDF may exceed the quota, so an org already over it cannot issue.
#[tokio::test]
async fn an_org_over_its_blob_quota_cannot_issue() {
	let (db, sql) = fresh("quota-issue").await;
	let app = app_for(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app.clone());
	app.settings.set("blob.max_org_bytes", "1048576", None).await.unwrap();
	hold_bytes(&sql, 1, 1_048_576).await;

	let draft = invoices.draft(&ctx, &one_line_draft()).await.unwrap();
	let err = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-QUOTA");
}

/// Issued under the quota, the PDF stores even when it pushes the org past it.
#[tokio::test]
async fn the_invoice_pdf_stores_past_the_blob_quota() {
	let (db, sql) = fresh("quota-pdf").await;
	let app = app_for(&db, &sql).await;
	let ctx = Ctx::system("test").with_org(1);
	let invoices = Invoices::new(app.clone());
	app.settings.set("blob.max_org_bytes", "1048576", None).await.unwrap();
	hold_bytes(&sql, 1, 1_048_575).await;

	let issued = invoices.issue_now(&ctx, &one_line_draft()).await.unwrap();
	mintworks_invoice::pdf::run(&app, &sql, issued.id).await.unwrap();
	assert!(sql.blob_org_bytes(1).await.unwrap() > 1_048_576);
	assert!(sql.invoice_document(issued.id).await.unwrap().is_some());
}

/// Org 1 (the seller's) opts in to converting catalogue prices into EUR; off by default.
async fn opt_in_eur(sql: &SqliteStore) {
	sqlx::query("INSERT INTO org_currencies (org_id, code, convert) VALUES (1, 'EUR', 1)")
		.execute(sql.write_pool())
		.await
		.unwrap();
}

#[test]
fn a_blank_name_en_falls_back_to_name() {
	let svc = |name_en: Option<&str>| mintworks_invoice::store::Service {
		id: 1,
		uid: mintworks_core::ids::ServiceId::generate(),
		org_id: 1,
		code: None,
		name: "Ora".to_owned(),
		description: None,
		unit: "ora".to_owned(),
		unit_price: Money(100),
		vat_code: VatCode::Std27,
		active: true,
		created_at: Timestamp(0),
		updated_at: Timestamp(0),
		name_en: name_en.map(str::to_owned),
		prices: Vec::new(),
	};
	for blank in ["", "  "] {
		assert_eq!(draft::service_name(&svc(Some(blank)), "en"), "Ora");
		assert_eq!(draft::service_name(&svc(Some(blank)), "hu-en"), "Ora");
	}
	assert_eq!(draft::service_name(&svc(Some("Hour")), "hu-en"), "Ora / Hour");
}

/// Each name passes `MAX_DESCRIPTION` alone, so a join past it must not refuse every draft.
#[test]
fn a_hu_en_join_past_the_description_limit_keeps_the_hungarian_name() {
	let (hu, en) = ("á".repeat(400), "e".repeat(400));
	let svc = |name: &str, name_en: &str| mintworks_invoice::store::Service {
		id: 1,
		uid: mintworks_core::ids::ServiceId::generate(),
		org_id: 1,
		code: None,
		name: name.to_owned(),
		description: None,
		unit: "ora".to_owned(),
		unit_price: Money(100),
		vat_code: VatCode::Std27,
		active: true,
		created_at: Timestamp(0),
		updated_at: Timestamp(0),
		name_en: Some(name_en.to_owned()),
		prices: Vec::new(),
	};
	assert_eq!(draft::service_name(&svc(&hu, &en), "hu-en"), hu);
	// Counted in characters, as `bounded_text` does: 512 fits though it is over 512 bytes.
	let (hu, en) = ("á".repeat(254), "e".repeat(255));
	assert_eq!(draft::service_name(&svc(&hu, &en), "hu-en"), format!("{hu} / {en}"));
}

// vim: ts=4
