// SPDX-License-Identifier: MPL-2.0
//! The `Invoices` **service handle** against a real `SqliteStore`: VAT and HUF reconciliation,
//! discounts, currency re-denomination and fulfilment-date repricing, org isolation, the
//! NAV text and shape refusals, step-up gating, the PDF data document, and storno semantics.
//!
//! These assert `mintworks-invoice` arithmetic, which no second store adapter can be expected to
//! reproduce, so they are not conformance tests — the raw-trait half is in
//! `adapters/store-sqlite/tests/invoice.rs`.
//!
//! Every test opens a real file database. `sqlite::memory:` gives each *connection* its own
//! database, so two stores over one in-memory URL would never contend for the write lock.
//!
//! Sibling to `invoice_units.rs`, which has its own harness; the two are deliberately not
//! merged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_core::{App, AppBuilder, config::Config, ctx::Ctx, ids::SellerId, prelude::*};
use mintworks_invoice::{
	draft::{Line, NewDraft, Party},
	money::{Discount, DraftLine},
	pricing::PricingHook,
	service_api::Invoices,
	store::{
		BuyerSnapshot, Invoice, InvoiceFilter, InvoiceKind, InvoicePatch, InvoiceStatus,
		InvoiceStore, InvoiceVatGroup, IssueInvoice, NewInvoice, NewInvoiceLine, PartyKind,
		PartyPatch, PaymentMethod, Seller, SellerVersionPatch, SellerVersionStatus,
	},
	vat::VatCode,
};
use mintworks_store_sqlite::SqliteStore;

const ORG: i64 = 1;

/// The platform root, moved off its natural id 1 so the fixture's own org can have it. The
/// framework finds the root by `kind = 'ROOT'` and never by its value.
const ROOT: i64 = 0;

/// The fixture's one seller. `put_seller` does not autoincrement, so the id is chosen here.
const SELLER: i64 = 1;

/// The version `seed_seller` publishes — the first row `seller_versions` ever gets.
const SELLER_VER: i64 = 1;

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-invoice-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn path(&self) -> String {
		self.0.join("test.db").to_string_lossy().into_owned()
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn open(db: &TmpDb) -> SqliteStore {
	SqliteStore::open(&Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	})
	.await
	.unwrap()
}

/// Migrations plus the minimum the foreign keys demand: one account, one org, seller 1.
/// `HUF` is already seeded by the framework module's migration.
async fn setup(db: &TmpDb) -> SqliteStore {
	let store = open(db).await;
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();

	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	// A fresh install seeds the root org at id 1, which this fixture wants for its own;
	// the framework finds the root by `kind = 'ROOT'`, never by its value.
	sqlx::query("UPDATE orgs SET id = ? WHERE kind = 'ROOT'")
		.bind(ROOT)
		.execute(store.write_pool())
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (?, 'org_t', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();

	seed_seller(&store).await;
	store
}

/// `put_seller` plus one published version — `issue` refuses a seller with no `CURRENT` one.
async fn seed_seller(store: &SqliteStore) {
	store.put_seller(&seller()).await.unwrap();
	store.save_seller_version_draft(SELLER, &seller_version()).await.unwrap();
	store
		.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();
}

fn seller() -> Seller {
	Seller {
		id: SELLER,
		uid: SellerId::generate(),
		org_id: ORG,
		nav_base_url: "https://api-test.onlineszamla.nav.gov.hu".into(),
		nav_login: None,
		series_code: "A".into(),
		closed_at: None,
		payment_days: None,
		created_at: Timestamp::now(),
	}
}

/// The seller row as stored, `uid` included: `put_seller` matches on the `uid` as well as the
/// `id`, so a second call for the same seller has to carry the one the first call wrote.
async fn stored_seller(store: &SqliteStore) -> Seller {
	store.seller_by_id(SELLER).await.unwrap().unwrap()
}

/// The statutory half, as the draft `seed_seller` publishes. Split from [`seller`] because it
/// is versioned: an invoice freezes the version, `sellers` keeps only what stays live.
fn seller_version() -> SellerVersionPatch {
	SellerVersionPatch {
		name: Some("Teszt Kft.".into()),
		country: Some("HU".into()),
		tax_number: Some("12345678242".into()),
		postcode: Some("1011".into()),
		city: Some("Budapest".into()),
		street: Some("Fo utca 1.".into()),
		vat_scheme: Some("NORMAL".into()),
		..Default::default()
	}
}

fn new_invoice(request_id: Option<&str>, kind: InvoiceKind, original: Option<i64>) -> NewInvoice {
	NewInvoice {
		org_id: ORG,
		seller_id: SELLER,
		billing_party_id: None,
		request_id: request_id.map(str::to_string),
		kind,
		original_invoice_id: original,
		currency: CurrencyCode::parse("HUF").unwrap(),
		rate_e6: 1_000_000,
		payment_method: PaymentMethod::Transfer,
		notes: None,
		discount_kind: None,
		discount_value: None,
	}
}

async fn draft(store: &SqliteStore, request_id: Option<&str>) -> Invoice {
	store
		.create_draft(&new_invoice(request_id, InvoiceKind::Normal, None))
		.await
		.unwrap()
}

/// One 27% group of `net` fillér. HUF, so `huf_rate_e6` and the `*_huf` trio stay NULL.
fn issue_input(invoice_id: i64, net: i64) -> IssueInvoice {
	let vat = net * 2700 / 10000;
	IssueInvoice {
		series_code: "A".into(),
		series_year: 2026,
		issued_at: Timestamp::now(),
		fulfilment_date: "2026-01-31".into(),
		due_date: Some("2026-02-08".into()),
		rate_date: None,
		rate_source: None,
		huf_rate_e6: None,
		rate_e6: None,
		net: Money(net),
		vat: Money(vat),
		gross: Money(net + vat),
		vat_note: None,
		seller_ver: SELLER_VER,
		buyer: BuyerSnapshot {
			kind: PartyKind::Company,
			name: "Vevo Zrt.".into(),
			country: "HU".into(),
			tax_number: Some("87654321242".into()),
			eu_vat_id: None,
			group_tax_no: None,
			postcode: Some("1052".into()),
			city: Some("Budapest".into()),
			street: Some("Deak ter 2.".into()),
			vies_request_id: None,
			vies_checked_at: None,
		},
		lines: vec![NewInvoiceLine {
			service_id: None,
			description: "Tanacsadas".into(),
			unit: "ora".into(),
			qty: Qty(1_000_000),
			unit_price: Money(net),
			discount_kind: None,
			discount_value: None,
			discount_amount: Money(0),
			discount_description: None,
			net: Money(net),
			vat_code: VatCode::Std27,
			vat_rate_bp: 2700,
			vat: Money(vat),
			gross: Money(net + vat),
			note: None,
		}],
		groups: vec![InvoiceVatGroup {
			invoice_id,
			vat_code: VatCode::Std27,
			vat_rate_bp: 2700,
			net: Money(net),
			vat: Money(vat),
			gross: Money(net + vat),
			net_huf: None,
			vat_huf: None,
			gross_huf: None,
		}],
		period_start: None,
		period_end: None,
		paid: false,
	}
}

/// The store plus an `App` and the `Invoices` service over it, and a default billing party
/// for the org so `Party::OrgDefault` resolves.
async fn service(db: &TmpDb) -> (App, Invoices, SqliteStore) {
	service_with(db, None).await
}

/// As [`service`], with a [`PricingHook`] registered.
async fn service_with(
	db: &TmpDb,
	hook: Option<std::sync::Arc<dyn PricingHook>>,
) -> (App, Invoices, SqliteStore) {
	let store = open(db).await;
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let mut builder = AppBuilder::new()
		.config(Config {
			master_key: [0; 32],
			db_path: db.path(),
			data_dir: db.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: String::new(),
			jobs_workers: None,
		})
		.store(std::sync::Arc::new(store.clone())
			as std::sync::Arc<dyn mintworks_core::store::CoreStore>)
		.settings(mintworks_invoice::SETTINGS)
		.extension(std::sync::Arc::new(store.clone()) as std::sync::Arc<dyn InvoiceStore>);
	if let Some(hook) = hook {
		builder = builder.extension(hook);
	}
	let app = builder.build().await.unwrap();

	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query("UPDATE orgs SET id = ? WHERE kind = 'ROOT'")
		.bind(ROOT)
		.execute(store.write_pool())
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (?, 'org_t', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', ?, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 1, 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	// 1 EUR = 400 HUF, fixed for pricing and published for the HUF figures the VAT groups
	// of a foreign-currency invoice must carry.
	sqlx::query(
		"INSERT INTO currencies (code, price_round_step, mode, fixed_rate_e6, fee_bp)
		 VALUES ('EUR', 1, 'FIXED', 400000000, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO currency_rates (pair, date, source, rate_e6, fetched_at)
		 VALUES ('EURHUF', '2020-01-01', 'BANK', 400000000, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	seed_seller(&store).await;
	let invoices = Invoices::new(app.clone());
	(app, invoices, store)
}

/// `draft::price` ran the VAT engine over **all** the lines but zipped the loop against a
/// parallel `service_ids` slice built *before* `PricingHook` ran. A hook that pushes a line
/// left `invoice_lines` summing to less than `invoices.net`, with an `invoice_vat_groups` row
/// describing a line that does not exist — which fails NAV's `summaryByVatRate` cross-check
/// on every such invoice. A hook that removes one shifted every later line onto its
/// predecessor's `service_id`.
struct Rewriting(&'static str);

#[async_trait::async_trait]
impl PricingHook for Rewriting {
	async fn price(&self, _ctx: &Ctx, lines: &mut Vec<DraftLine>) -> ClResult<()> {
		match self.0 {
			"add" => lines.push(DraftLine {
				service_id: None,
				description: "Belepteti dij".into(),
				unit: "db".into(),
				qty: Qty(1_000_000),
				unit_price: Money(500_000),
				vat_code: VatCode::Std27,
				discount: None,
				discount_description: None,
				note: None,
			}),
			_ => {
				lines.remove(0);
			}
		}
		Ok(())
	}
}

fn adhoc(qty: i64, unit_price: i64, discount: Option<Discount>) -> Line {
	Line {
		code: None,
		description: "Tanacsadas".into(),
		unit: "ora".into(),
		qty: Qty(qty),
		unit_price: Some(Money(unit_price)),
		vat_code: Some(VatCode::Std27),
		discount,
		discount_description: None,
		note: None,
	}
}

/// A line whose `code` resolves to no catalogue service.
fn unresolvable_line() -> Line {
	Line::code("no-such-service", Qty(1_000_000))
}

fn new_draft(request_id: Option<&str>, lines: Vec<Line>) -> NewDraft {
	NewDraft {
		request_id: request_id.map(str::to_string),
		billing_party: Party::OrgDefault,
		lines,
		..NewDraft::default()
	}
}

/// A second org to own the rows a cross-org test must not reach.
async fn second_org(store: &SqliteStore) -> i64 {
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (2, 'org_other', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Masik', 1, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	2
}

/// A draft with one ordinary line, through the service handle.
async fn service_draft(invoices: &Invoices, ctx: &Ctx) -> Invoice {
	invoices
		.draft(
			ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 100_000, None)],
				..NewDraft::default()
			},
		)
		.await
		.unwrap()
}

/// `to_base` was bounded by `i64`, not by `MAX_MINOR` — but `Money`'s `sqlx::Decode` bounds
/// by `MAX_MINOR`. So a large enough FX invoice wrote a `net_huf` nothing could read back,
/// and hydration, the PDF and the NAV filing were all a permanent 500 on that row. The
/// rejection has to happen here, at creation, while there is still nothing to repair.
#[tokio::test]
async fn an_fx_draft_past_the_huf_envelope_is_rejected_at_creation() {
	let db = TmpDb::new("huf-envelope");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// Half of `MAX_MINOR` in EUR cents is a figure `Money::parse` accepts, and 400x it is not.
	let err = invoices
		.draft(
			&ctx,
			&NewDraft {
				lines: vec![adhoc(1_000_000, mintworks_core::money::MAX_MINOR / 2, None)],
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				..NewDraft::default()
			},
		)
		.await
		.unwrap_err();
	assert!(matches!(err, Error::Validation(_)), "{err:?}");

	let left: i64 = sqlx::query_scalar("SELECT count(*) FROM invoice_vat_groups")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(left, 0, "nothing unreadable was persisted");
}

/// `fulfilmentDate` and `dueDate` went from the wire straight into their columns with
/// nothing parsing them. `"tomorrow"` froze onto an issued invoice and, because
/// `effective_rate_e6` compares dates *lexically*, sorted above every published rate — the
/// invoice took the newest rate rather than the one on the fulfilment date. `"9999-12-31"`
/// panicked in `add_days` at issue. The handle is the trust boundary, not the router.
#[tokio::test]
async fn an_unparseable_fulfilment_date_never_reaches_the_column() {
	let db = TmpDb::new("date-boundary");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let with_date = |date: &'static str| NewDraft {
		lines: vec![adhoc(1_000_000, 100_000, None)],
		fulfilment_date: Some(date.to_owned()),
		..NewDraft::default()
	};
	for date in ["tomorrow", "2026-1-1", "9999-12-31T00:00:00Z"] {
		let err = invoices.draft(&ctx, &with_date(date)).await.unwrap_err();
		assert!(matches!(err, Error::Validation(_)), "{date}: {err:?}");
	}

	// The end of the calendar is a real date, but `issue` adds `invoice.default_payment_days` to
	// it and overflows, so the range check refuses it before a row exists. `numbering`'s own
	// `add_days("9999-12-31", 8)` test pins the overflow guard underneath.
	assert_eq!(
		invoices.draft(&ctx, &with_date("9999-12-31")).await.unwrap_err().parts().1,
		"E-INV-DATE-RANGE"
	);

	let dates: Vec<Option<String>> = sqlx::query_scalar(
		"SELECT fulfilment_date FROM invoices WHERE fulfilment_date IS NOT NULL",
	)
	.fetch_all(store.read_pool())
	.await
	.unwrap();
	assert!(dates.is_empty(), "no unparseable or out-of-range date was stored: {dates:?}");
}

/// A discounted invoice must be cancellable at all, and its counter-invoice must
/// keep NAV's `lineNetAmount == quantity × unitPrice − discount` identity with every
/// component negative.
#[tokio::test]
async fn storno_of_a_discounted_invoice_reconciles() {
	let db = TmpDb::new("storno-discount");
	let (app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let issued = invoices
		.issue_now(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(2_000_000, 50_000, Some(Discount::Amount(Money(500))))],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();

	let st = mintworks_invoice::storno::run(&app, &store, &issued, "teves szamlazas")
		.await
		.unwrap();

	let lines = store.invoice_lines(st.id).await.unwrap();
	let l = &lines[0];
	assert!(l.qty.0 < 0 && l.unit_price.0 > 0, "the quantity is negated, not the unit price");
	assert!(l.discount_amount.0 < 0 && l.net.0 < 0);
	let raw = i128::from(l.qty.0) * i128::from(l.unit_price.0) / 1_000_000;
	assert_eq!(i128::from(l.net.0), raw - i128::from(l.discount_amount.0), "NAV line identity");
	assert_eq!(st.gross, Money(-issued.gross.0));
}

/// An invoice-level discount is apportioned into the lines, where it is indistinguishable
/// from a line's own discount — so it has to be stored and re-applied, or ISSUE re-prices
/// without it and bills more than the draft showed.
#[tokio::test]
async fn an_invoice_level_discount_survives_issue() {
	let db = TmpDb::new("invoice-discount");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// Both discounts on the same line: the line's own one used to win and the invoice-level
	// share was dropped at ISSUE.
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 100_000, Some(Discount::Amount(Money(1_000))))],
				discount: Some(Discount::Percent(1_000)), // 10%
				..NewDraft::default()
			},
		)
		.await
		.unwrap();

	assert_eq!(draft.net, Money(89_100), "100 000 − 1 000, less 10% of the rest");
	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
	assert_eq!((issued.net, issued.vat, issued.gross), (draft.net, draft.vat, draft.gross));
}

/// `invoice_lines.unit_price` is in the invoice currency, so a currency change has to
/// move every stored magnitude with it — otherwise the fillér figures are relabelled cents.
///
/// Both entry points, because `Invoices::patch` used to divert to `rewrite` only for a new
/// billing party: a bare `currency` went to `update_draft`'s `currency = COALESCE(?, currency)`
/// and relabelled an HUF draft as EUR with every `unit_price` still in fillér.
#[tokio::test]
async fn changing_the_currency_rescales_every_line() {
	let db = TmpDb::new("currency-change");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	for through_patch in [false, true] {
		// 40 000 HUF at 1 EUR = 400 HUF is 100.00 EUR, and the 4 000 HUF discount is 10.00 EUR.
		let draft = invoices
			.draft(
				&ctx,
				&NewDraft {
					billing_party: Party::OrgDefault,
					lines: vec![adhoc(1_000_000, 4_000_000, None)],
					discount: Some(Discount::Amount(Money(400_000))),
					..NewDraft::default()
				},
			)
			.await
			.unwrap();
		assert_eq!(draft.net, Money(3_600_000));

		let eur = if through_patch {
			invoices
				.patch(
					&ctx,
					draft.uid.as_str(),
					&InvoicePatch {
						currency: Some(CurrencyCode::parse("EUR").unwrap()),
						..InvoicePatch::default()
					},
				)
				.await
				.unwrap()
		} else {
			invoices
				.patch_by_uid(
					&ctx,
					draft.uid.as_str(),
					None,
					Some(&CurrencyCode::parse("EUR").unwrap()),
					&InvoicePatch::default(),
				)
				.await
				.unwrap()
		};

		assert_eq!(eur.currency, "EUR", "through_patch={through_patch}");
		assert_eq!(eur.discount_value, Some(1_000), "the invoice-level discount moves too");
		let lines = store.invoice_lines(eur.id).await.unwrap();
		assert_eq!(lines[0].unit_price, Money(10_000), "through_patch={through_patch}");
		assert_eq!(lines[0].discount_amount, Money(1_000));
		assert_eq!(eur.net, Money(9_000));
		assert_eq!(eur.vat, Money(2_430));
		assert_eq!(eur.gross, Money(11_430));
	}
}

/// `net_huf`, `vat_huf` and `gross_huf` are what Áfa tv. 172. § makes mandatory and NAV
/// cross-validates, so they have to sum. Three independent roundings do not: at 400.000044
/// the separately rounded gross is one fillér above the sum of the other two.
#[tokio::test]
async fn the_huf_figures_of_a_group_reconcile_at_a_non_round_rate() {
	let db = TmpDb::new("huf-reconcile");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// EUR is `FIXED` in the harness, so the rate is its `fixed_rate_e6`, not a published row.
	sqlx::query("UPDATE currencies SET fixed_rate_e6 = 400000044 WHERE code = 'EUR'")
		.execute(store.write_pool())
		.await
		.unwrap();

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 4_000_000, None)],
				discount: Some(Discount::Amount(Money(400_000))),
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	let eur = invoices
		.patch_by_uid(
			&ctx,
			draft.uid.as_str(),
			None,
			Some(&CurrencyCode::parse("EUR").unwrap()),
			&InvoicePatch::default(),
		)
		.await
		.unwrap();

	let groups = store.invoice_vat_groups(eur.id).await.unwrap();
	assert!(!groups.is_empty());
	for g in &groups {
		let (n, v, gr) = (g.net_huf.unwrap(), g.vat_huf.unwrap(), g.gross_huf.unwrap());
		assert_eq!(n + v, gr, "the HUF trio must reconcile");
	}
}

/// The buyer decides the VAT treatment, so a party change has to re-price —
/// and the verdict must not be persisted onto the draft line, or changing the buyer back
/// could never undo it. At ISSUE the winning code is frozen onto the lines, so the lines,
/// the groups and the NAV filing all say the same thing.
#[tokio::test]
async fn changing_the_billing_party_reprices_and_issue_freezes_the_lines() {
	let db = TmpDb::new("party-change");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// A third-country company: `BuyerZone::Third` + company is `Verdict::Override(Ho)`.
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (2, 'prt_us', ?, 'C', 'Acme Inc.', 'US', '99-1234567',
		  '10001', 'New York', '5th Ave 1.', 0, 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 100_000, None)],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(draft.vat, Money(27_000));

	let to_party = |id: i64| InvoicePatch { billing_party_id: Some(id), ..Default::default() };

	// -> US. The groups collapse to one 0% `HO` group, but the line keeps its own `STD27`.
	let exported = invoices.patch(&ctx, draft.uid.as_str(), &to_party(2)).await.unwrap();
	assert_eq!(exported.vat, Money(0), "reverse charge / export: no VAT is charged");
	let groups = store.invoice_vat_groups(exported.id).await.unwrap();
	assert_eq!(groups.len(), 1);
	assert_eq!((groups[0].vat_code, groups[0].vat_rate_bp), (VatCode::Ho, 0));
	let lines = store.invoice_lines(exported.id).await.unwrap();
	assert_eq!(lines[0].vat_code, VatCode::Std27, "a draft line keeps the product's own code");

	// -> back to HU. This is the case the persisted verdict made unrecoverable.
	let domestic = invoices.patch(&ctx, draft.uid.as_str(), &to_party(1)).await.unwrap();
	assert_eq!(domestic.vat, Money(27_000), "the domestic rate must come back");
	let groups = store.invoice_vat_groups(domestic.id).await.unwrap();
	assert_eq!((groups[0].vat_code, groups[0].vat_rate_bp), (VatCode::Std27, 2700));

	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
	let lines = store.invoice_lines(issued.id).await.unwrap();
	assert_eq!((lines[0].vat_code, lines[0].vat_rate_bp), (VatCode::Std27, 2700));

	// The lines and the summary must agree.
	let groups = store.invoice_vat_groups(issued.id).await.unwrap();
	let line_net: i64 = lines.iter().map(|l| l.net.0).sum();
	let group_net: i64 = groups.iter().map(|g| g.net.0).sum();
	assert_eq!(line_net, group_net);
	assert_eq!(line_net, issued.net.0);
	for l in &lines {
		assert!(groups.iter().any(|g| g.vat_code == l.vat_code), "every line has a group");
	}
}

/// `Invoices::draft` was `create_draft` then `huf_rate_e6` (which can 409) then
/// `replace_draft_lines` then `update_draft`, with no transaction. Anything failing after the
/// insert left an `invoices` row with the `request_id` consumed and **zero lines** — and the
/// retry then got that empty invoice back off the idempotency branch, where `issue::run`
/// refused it with `E-INV-EMPTY` forever. The checkout stayed wedged until `SWEEP_DRAFTS`.
#[tokio::test]
async fn a_failed_draft_leaves_no_row_behind() {
	let db = TmpDb::new("draft-atomic");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	assert!(
		invoices
			.draft(&ctx, &new_draft(Some("order-9182"), vec![unresolvable_line()]))
			.await
			.is_err()
	);

	let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(rows, 0, "a half-built draft survived and consumed the request_id");

	// So the retry builds a real draft, not an empty invoice it can never issue.
	let good = invoices
		.draft(&ctx, &new_draft(Some("order-9182"), vec![adhoc(1_000_000, 100_000, None)]))
		.await
		.unwrap();
	assert_eq!(good.net, Money(100_000));
	invoices.issue(&ctx, good.uid.as_str()).await.expect("the retry's draft issues");
}

/// The idempotency read and the insert are two statements, so two concurrent callers with the
/// same key both used to miss the read and both reach the insert; the loser got a raw unique
/// conflict instead of the invoice the winner made.
#[tokio::test]
async fn a_concurrent_duplicate_request_id_returns_one_shared_invoice() {
	let db = TmpDb::new("draft-race");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let one = invoices.clone();
	let two = invoices.clone();
	let (a, b) = tokio::join!(
		{
			let ctx = ctx.clone();
			async move {
				one.draft(
					&ctx,
					&new_draft(Some("checkout-1"), vec![adhoc(1_000_000, 100_000, None)]),
				)
				.await
			}
		},
		async move {
			two.draft(&ctx, &new_draft(Some("checkout-1"), vec![adhoc(1_000_000, 100_000, None)]))
				.await
		}
	);
	assert_eq!(
		a.expect("caller A").id,
		b.expect("caller B got a conflict instead of the existing invoice").id
	);
}

/// `huf_rate_e6` called `rate_on` directly, which demands a published `currency_rates` row. So
/// a currency configured `mode = 'FIXED'` — the documented admin-set mode, and how the seeded
/// HUF row itself is written — priced correctly as a draft and then `E-INV-NO-RATE` on the
/// HUF figure, and no invoice in it could ever be created or issued.
#[tokio::test]
async fn a_fixed_rate_currency_drafts_and_issues_with_no_published_rates() {
	let db = TmpDb::new("fixed-rate");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// `service` seeds EUR as FIXED *and* publishes a rate for it. Take the published rate
	// away: the fixed one is the whole point of the mode.
	sqlx::query("DELETE FROM currency_rates")
		.execute(store.write_pool())
		.await
		.unwrap();

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				..new_draft(None, vec![adhoc(1_000_000, 10_000, None)])
			},
		)
		.await
		.expect("a FIXED-mode currency must be draftable with no published rate");

	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.expect("…and issuable");
	assert_eq!(issued.currency, "EUR");
	assert_eq!(issued.huf_rate_e6, Some(400_000_000), "the fixed rate is what was frozen");
}

/// `update_party` clears the org's current default *before* the scoped `UPDATE`, so a
/// miss — another org's uid, or a typo — used to commit the clear anyway and leave the
/// org with no default at all. Every later `Party::OrgDefault` draft then fails.
#[tokio::test]
async fn a_missed_party_update_does_not_clear_the_default() {
	let db = TmpDb::new("party-update-miss");
	let (_app, _invoices, store) = service(&db).await;

	// `service` seeds party id 1 as this org's default.
	assert!(store.party_by_id(1).await.unwrap().unwrap().is_default);

	let stranger = store
		.create_party(
			second_org(&store).await,
			&PartyPatch {
				kind: Some(PartyKind::Company),
				name: Some("Masik org".into()),
				country: Some("HU".into()),
				..PartyPatch::default()
			},
		)
		.await
		.unwrap();

	let miss = store
		.update_party(
			ORG,
			&stranger.uid,
			&PartyPatch { is_default: Some(true), ..PartyPatch::default() },
		)
		.await
		.unwrap();

	assert!(miss.is_none(), "another org's party reads as absent");
	assert!(
		store.party_by_id(1).await.unwrap().unwrap().is_default,
		"a miss must roll back the clear, not commit it"
	);
}

/// `party_by_id` is not org-scoped and `InvoicePatch.billing_party_id` is a raw,
/// `Deserialize`d id — so `rewrite` has to confine it, or a caller can attach another
/// org's billing party to their draft and freeze it into the buyer snapshot at issue.
#[tokio::test]
async fn another_orgs_billing_party_cannot_be_patched_in() {
	let db = TmpDb::new("party-idor");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let stranger = store
		.create_party(
			second_org(&store).await,
			&PartyPatch {
				kind: Some(PartyKind::Company),
				name: Some("Masik org".into()),
				country: Some("HU".into()),
				..PartyPatch::default()
			},
		)
		.await
		.unwrap();

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 100_000, None)],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();

	let err = invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch { billing_party_id: Some(stranger.id), ..InvoicePatch::default() },
		)
		.await
		.expect_err("another org's party must not be reachable");
	assert_eq!(err.parts().1, "E-CORE-NOTFOUND", "absent, never 403");
}

/// `Verdict::Product` carries no note key of its own, but a product-level AAM line still
/// needs its statutory note on the PDF — and a NULL `vat_note` used to make `mintworks_nav::xml`
/// fail outright, stranding the invoice in `unfiled_invoices` forever.
#[tokio::test]
async fn a_product_level_exempt_line_still_freezes_a_vat_note() {
	let db = TmpDb::new("product-exempt-note");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// A domestic company buyer, so the verdict is `Product` and the line's own code stands.
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![Line {
					vat_code: Some(VatCode::Aam),
					..adhoc(1_000_000, 100_000, None)
				}],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();

	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
	assert_eq!(issued.vat, Money::ZERO);
	assert_eq!(issued.vat_note.as_deref(), Some("vat.aam"));
}

/// Line validation lived only in the HTTP layer (`routes::check_line` and
/// `LineBody::into_line`), but `Invoices::issue_now` builds a draft with no consumer in the
/// loop. NAV's `lineDescription` is `SimpleText512NotBlankType`, so a blank one is filed,
/// rejected, retried eight times and then re-driven hourly by `NAV_SWEEP` forever.
#[tokio::test]
async fn the_service_handle_rejects_a_line_the_router_would_have_caught() {
	let db = TmpDb::new("service-line-guard");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);
	let draft = service_draft(&invoices, &ctx).await;

	for bad in [
		Line { description: "   ".into(), ..adhoc(1_000_000, 100_000, None) },
		Line { unit: String::new(), ..adhoc(1_000_000, 100_000, None) },
		adhoc(0, 100_000, None),
		adhoc(-1_000_000, 100_000, None),
		adhoc(1_000_000, -1, None),
	] {
		let err = invoices
			.add_line(&ctx, draft.uid.as_str(), bad)
			.await
			.expect_err("the service handle is the trust boundary, not the router");
		assert_eq!(err.parts().1, "E-INV-LINE", "{err:?}");
	}
}

/// The other side: `storno::run` negates the *stored* lines straight into an
/// `IssueInvoice` and never calls `draft::price`, so the positive-quantity rule must not
/// touch a counter-invoice.
#[tokio::test]
async fn the_positive_quantity_rule_does_not_reach_a_storno() {
	let db = TmpDb::new("storno-negative-qty");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let draft = service_draft(&invoices, &ctx).await;
	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
	let storno = invoices.storno(&ctx, issued.uid.as_str(), "test").await.unwrap();

	let lines = store.invoice_lines(storno.id).await.unwrap();
	assert_eq!(lines[0].qty, Qty(-1_000_000), "the counter-invoice keeps its negative qty");
	assert_eq!(storno.gross, -issued.gross);
}

/// `rewrite` read the line set on the reader pool, re-priced in memory and handed the
/// whole set to `replace_draft_lines`. Two concurrent `POST /lines` both read N and both
/// wrote N+1, and the second commit discarded the first caller's line.
///
/// Both calls happen inside one second, which is the point: the guard used to be
/// `AND updated_at = ?` and `Timestamp` is unix seconds, so both matched.
#[tokio::test]
async fn a_draft_edit_priced_against_a_stale_snapshot_is_refused() {
	let db = TmpDb::new("stale-draft");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);
	let draft = service_draft(&invoices, &ctx).await;

	// The snapshot both callers read, and the price both computed from it.
	let stale = draft.version;
	let priced = mintworks_invoice::draft::price(
		draft.id,
		&[
			mintworks_invoice::draft::to_draft(&store.invoice_lines(draft.id).await.unwrap()[0])
				.unwrap(),
		],
		None,
		&mintworks_invoice::taxrule::Verdict::Product,
		None,
		false,
		// HUF, so the group VAT lands on whole forints exactly as the service path does.
		100,
	)
	.unwrap();

	assert!(store.replace_draft_lines(draft.id, None, &priced, stale).await.unwrap());
	assert!(
		!store.replace_draft_lines(draft.id, None, &priced, stale).await.unwrap(),
		"the second commit over the same snapshot must be refused"
	);

	// A fresh snapshot still commits.
	let fresh = store.invoice_by_id(draft.id).await.unwrap().unwrap();
	assert_eq!(fresh.version, stale + 1, "one commit, one bump");
	assert!(store.replace_draft_lines(draft.id, None, &priced, fresh.version).await.unwrap());

	// With a patch — the `change_currency` path — the row is written twice in the one
	// transaction, so the token jumps by two. The guard only needs it to have moved.
	let stale = store.invoice_by_id(draft.id).await.unwrap().unwrap().version;
	let patch = InvoicePatch { notes: Patch::Value("patched".into()), ..Default::default() };
	assert!(store.replace_draft_lines(draft.id, Some(&patch), &priced, stale).await.unwrap());
	let fresh = store.invoice_by_id(draft.id).await.unwrap().unwrap();
	assert!(fresh.version > stale, "a patched commit still advances the token");
	assert!(
		!store.replace_draft_lines(draft.id, Some(&patch), &priced, stale).await.unwrap(),
		"the stale token is refused on the patched path too"
	);
}

/// `issue` read the lines off the reader pool, then spent up to 15 s in a VIES lookup,
/// then handed that stale set to the store — which guarded only `status = 'DRAFT'`. A
/// `POST /lines` committing in that window was silently erased from an invoice that then got
/// a number and was filed to NAV. `replace_draft_lines` had the guard; `issue` did not.
#[tokio::test]
async fn an_edit_between_the_read_and_the_issue_is_refused() {
	let db = TmpDb::new("stale-issue");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);
	let draft = service_draft(&invoices, &ctx).await;

	// The snapshot the losing caller is holding, taken before the concurrent edit lands.
	let stale = draft.version;
	// The same state a `POST /lines` that commits during the VIES window leaves behind.
	sqlx::query("UPDATE invoices SET version = version + 1 WHERE id = ?")
		.bind(draft.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let lines = store.invoice_lines(draft.id).await.unwrap();

	let err = store
		.issue(draft.id, &issue_input(draft.id, 100_000), stale)
		.await
		.expect_err("issuing over a concurrent edit must be refused");
	assert_eq!(err.parts().1, "E-INV-CHANGED", "{err:?}");

	let again = store.invoice_by_id(draft.id).await.unwrap().unwrap();
	assert!(matches!(again.status, InvoiceStatus::Draft), "and it is still an unnumbered draft");
	assert!(again.number.is_none());
	assert_eq!(store.invoice_lines(draft.id).await.unwrap().len(), lines.len());

	// The retry, on a fresh read, issues.
	assert!(
		invoices.issue(&ctx, draft.uid.as_str()).await.unwrap().number.is_some(),
		"a losing caller retries and wins"
	);
}

/// `change_currency` reads the invoice to capture `old_cur` and `old_rate_e6`, and `rewrite`
/// then re-read it and used **that** row's `updated_at` as the optimistic guard — so a
/// currency change committing between the two reads was invisible to the guard and its lines
/// were converted a second time as if still in the old currency: a 10 000.00 HUF line priced
/// as 0.07 USD instead of 28.57, silently, on a draft that can then be issued.
///
/// The concurrent change is held open in a write transaction so it commits after the caller's
/// read and before its write. That pins the invariant — the guard refuses, and nothing is
/// converted twice — but not the regression itself: the window this closes is one reader query
/// wide (`currency::get` between the two reads), which no external caller can time into.
#[tokio::test]
async fn a_currency_change_under_a_second_one_is_refused_not_double_converted() {
	let db = TmpDb::new("currency-race");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);
	let draft = service_draft(&invoices, &ctx).await;

	// Holds the single write connection, so the caller below blocks on its write.
	let mut tx = store.write_pool().begin().await.unwrap();
	sqlx::query("UPDATE invoices SET version = version + 1 WHERE id = ?")
		.bind(draft.id)
		.execute(&mut *tx)
		.await
		.unwrap();

	let racer = {
		let (invoices, ctx, uid) = (invoices.clone(), ctx.clone(), draft.uid.as_str().to_owned());
		tokio::spawn(async move {
			invoices
				.patch(
					&ctx,
					&uid,
					&InvoicePatch {
						currency: Some(CurrencyCode::parse("EUR").unwrap()),
						..InvoicePatch::default()
					},
				)
				.await
		})
	};
	// Long enough for the read half to have happened and the write half to be waiting.
	tokio::time::sleep(std::time::Duration::from_millis(300)).await;
	tx.commit().await.unwrap();

	let err = racer
		.await
		.unwrap()
		.expect_err("a write over a concurrent change must be refused");
	assert_eq!(err.parts().1, "E-INV-STALE", "{err:?}");
	// And the draft is untouched, rather than carrying twice-converted lines.
	let after = store.invoice_by_id(draft.id).await.unwrap().unwrap();
	assert_eq!(after.currency, "HUF");
}

/// `#[sqlx(transparent)]` derived a `Decode` that turned **any** `i64` in the database
/// into a `Money`, bypassing the `MAX_MINOR` bound that makes `Money`'s unchecked `Add`/`Sub`/
/// `Sum` safe. A row written outside `Money::parse` — a consumer app's own SQL, a data import
/// — then trapped under `overflow-checks` on the next addition, which is a dropped connection
/// rather than a `400`. The derive is gone; the bound now lives in the adapter's
/// `util::read_money`, which every `Money` read goes through, so the read is still a trust
/// boundary of its own.
#[tokio::test]
async fn an_out_of_range_amount_in_the_database_fails_to_decode() {
	let db = TmpDb::new("money-decode-bound");
	let store = setup(&db).await;
	let inv = draft(&store, None).await;

	sqlx::query("UPDATE invoices SET net = ? WHERE id = ?")
		.bind(mintworks_core::money::MAX_MINOR + 1)
		.bind(inv.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let err = store
		.invoice_by_id(inv.id)
		.await
		.expect_err("an amount past MAX_MINOR must not be read back");
	assert!(format!("{err}").contains("out of range"), "{err}");

	// The bound itself still reads, so nothing legitimate is refused.
	sqlx::query("UPDATE invoices SET net = ? WHERE id = ?")
		.bind(mintworks_core::money::MAX_MINOR)
		.bind(inv.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let ok = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(ok.net, Money(mintworks_core::money::MAX_MINOR));
}

#[tokio::test]
async fn a_pricing_hook_that_adds_a_line_keeps_the_totals_and_the_groups_honest() {
	let db = TmpDb::new("hook-add");
	let (_app, invoices, store) =
		service_with(&db, Some(std::sync::Arc::new(Rewriting("add")))).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft { lines: vec![adhoc(1_000_000, 4_000_000, None)], ..NewDraft::default() },
		)
		.await
		.unwrap();

	let lines = store.invoice_lines(draft.id).await.unwrap();
	assert_eq!(lines.len(), 2, "the hook's line was dropped on the floor");
	let summed: i64 = lines.iter().map(|l| l.net.0).sum();
	assert_eq!(summed, draft.net.0, "the stored lines must sum to invoices.net");

	// Every VAT group has to describe lines that actually exist.
	for g in store.invoice_vat_groups(draft.id).await.unwrap() {
		let of_group: i64 =
			lines.iter().filter(|l| l.vat_code == g.vat_code).map(|l| l.net.0).sum();
		assert_eq!(of_group, g.net.0, "group {:?} describes lines that are not there", g.vat_code);
	}
}

#[tokio::test]
async fn a_pricing_hook_that_removes_a_line_does_not_shift_the_service_ids() {
	let db = TmpDb::new("hook-remove");
	let (_app, invoices, store) =
		service_with(&db, Some(std::sync::Arc::new(Rewriting("remove")))).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// Two catalogue lines, so each carries a distinct `services.id`. The hook drops the first.
	for (code, name) in [("SETUP", "Belepteti dij"), ("HOUR", "Tanacsadas")] {
		invoices
			.create_service(
				&ctx,
				&mintworks_invoice::store::ServiceDef {
					code: code.to_owned(),
					name: name.to_owned(),
					description: None,
					unit: "db".to_owned(),
					unit_price: Money(1_000_000),
					vat_code: VatCode::Std27,
				},
			)
			.await
			.unwrap();
	}
	let kept = invoices.service_by_code(&ctx, "HOUR").await.unwrap();

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				lines: vec![
					mintworks_invoice::draft::Line::code("SETUP", Qty(1_000_000)),
					mintworks_invoice::draft::Line::code("HOUR", Qty(1_000_000)),
				],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();

	let lines = store.invoice_lines(draft.id).await.unwrap();
	assert_eq!(lines.len(), 1);
	assert_eq!(lines[0].service_id, Some(kept.id), "the surviving line kept its own service");
	assert_eq!(lines[0].description, "Tanacsadas");
	assert_eq!(lines[0].net.0, draft.net.0);
}

/// `note` is the caller's text on both branches of `draft::resolve`, and the catalogue
/// overwrite set is exactly description/unit/price/vat_code — the reason the field exists.
#[tokio::test]
async fn a_catalogue_line_keeps_its_note_while_its_description_is_overwritten() {
	let db = TmpDb::new("line-note");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	invoices
		.create_service(
			&ctx,
			&mintworks_invoice::store::ServiceDef {
				code: "HOUR".to_owned(),
				name: "Tanacsadas".to_owned(),
				description: None,
				unit: "ora".to_owned(),
				unit_price: Money(1_000_000),
				vat_code: VatCode::Std27,
			},
		)
		.await
		.unwrap();

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				lines: vec![mintworks_invoice::draft::Line {
					description: "eldobando".into(),
					note: Some("2026-10-03, ablak melletti".into()),
					..mintworks_invoice::draft::Line::code("HOUR", Qty(1_000_000))
				}],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();

	let lines = store.invoice_lines(draft.id).await.unwrap();
	assert_eq!(lines[0].description, "Tanacsadas", "the catalogue name wins");
	assert_eq!(lines[0].note.as_deref(), Some("2026-10-03, ablak melletti"));
}

/// `country` went from the wire straight into the column. `BuyerZone::of` compares
/// against `EU_COUNTRIES`, so `"Magyarország"` classified as `Third` and a domestic
/// Hungarian sale issued at **0% VAT**; `"hu"` got the right VAT but NAV's
/// `base:countryCode` is `[A-Z]{2}`, so the filing failed the schema instead.
#[tokio::test]
async fn a_party_country_is_normalised_to_iso_alpha2() {
	let db = TmpDb::new("party-country");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// No tax number: `billing_parties` holds one unique per org, and it is only demanded
	// at issue. This is about `country` alone.
	let party = |country: &str| PartyPatch {
		kind: Some(PartyKind::Company),
		name: Some("Vevo Zrt.".into()),
		country: Some(country.into()),
		..PartyPatch::default()
	};

	for input in ["hu", " Hu ", "HU"] {
		let created = invoices.create_party(&ctx, &party(input)).await.unwrap();
		assert_eq!(created.country, "HU", "{input} must be stored normalised");
	}

	for bad in ["Magyarország", "HUN", "", "H1"] {
		let err = invoices.create_party(&ctx, &party(bad)).await.unwrap_err();
		assert_eq!(err.parts().1, "E-INV-COUNTRY", "{bad} must be refused");
	}

	// The update path is the same trust boundary.
	let created = invoices.create_party(&ctx, &party("DE")).await.unwrap();
	let patched = invoices
		.update_party(
			&ctx,
			created.uid.as_str(),
			&PartyPatch { country: Some("at".into()), ..PartyPatch::default() },
		)
		.await
		.unwrap();
	assert_eq!(patched.country, "AT");
	let err = invoices
		.update_party(
			&ctx,
			created.uid.as_str(),
			&PartyPatch { country: Some("Austria".into()), ..PartyPatch::default() },
		)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-COUNTRY");
}

/// Every buyer defect NAV would reject, refused at `issue` — **before a number is allocated**.
/// A number is the one thing an invoice can never give back: once allocated, an invoice that
/// fails the XSD or NAV's cross-checks on every attempt cannot be corrected, only stornoed.
///
/// `customerAddress` is mandatory for a company and absent for a private person (§4.3);
/// `base:TaxNumberType` takes 8, 9 or 11 digits with the separators stripped, as
/// `customer_info` does; and `thirdStateTaxId` is `SimpleText50NotBlankType`, so a blank one
/// is not an absent one.
#[tokio::test]
async fn a_buyer_nav_would_reject_is_refused_before_a_number_is_allocated() {
	let db = TmpDb::new("buyer-refusals");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// `doc_series` has no row until the first allocation, so sum rather than fetch: a refused
	// issue must not even create the series.
	let allocated = || async {
		sqlx::query_scalar::<_, i64>("SELECT COALESCE(SUM(next_no), 0) FROM doc_series")
			.fetch_one(store.read_pool())
			.await
			.unwrap()
	};
	let company = |name: &str, country: &str| PartyPatch {
		kind: Some(PartyKind::Company),
		name: Some(name.to_owned()),
		country: Some(country.to_owned()),
		postcode: Patch::Value("1052".to_owned()),
		city: Patch::Value("Budapest".to_owned()),
		street: Patch::Value("Deak ter 2.".to_owned()),
		..PartyPatch::default()
	};
	let hu = |tax: &str| PartyPatch {
		tax_number: Patch::Value(tax.to_owned()),
		..company("Vevo Zrt.", "HU")
	};

	// (label, the buyer, the errCode — or None when the invoice must issue)
	let addressless = PartyPatch {
		postcode: Patch::Null,
		city: Patch::Null,
		street: Patch::Null,
		..hu("11223344142")
	};
	for (label, buyer, want) in [
		("a company with no address", addressless, Some("E-INV-BUYER-ADDRESS")),
		// A private person carries no `customerAddress` in the filing at all, so the rule does
		// not touch them.
		(
			"a private person with no address",
			PartyPatch {
				kind: Some(PartyKind::Person),
				name: Some("Kiss Anna".into()),
				country: Some("HU".into()),
				..PartyPatch::default()
			},
			None,
		),
		(
			"a blank third-state tax number",
			PartyPatch { tax_number: Patch::Value("   ".to_owned()), ..company("Acme Inc.", "US") },
			Some("E-INV-BUYER-TAXNUMBER"),
		),
		(
			"a third-state tax number",
			PartyPatch {
				tax_number: Patch::Value("US-99-1234567".to_owned()),
				..company("Acme GmbH", "US")
			},
			None,
		),
	] {
		let before = allocated().await;
		let party = invoices
			.create_party(&ctx, &buyer)
			.await
			.unwrap_or_else(|e| panic!("{label} is a legal party to hold: {e}"));
		let draft = invoices
			.draft(
				&ctx,
				&NewDraft {
					billing_party: Party::Uid(party.uid),
					lines: vec![adhoc(1_000_000, 100_000, None)],
					..NewDraft::default()
				},
			)
			.await
			.unwrap();

		if let Some(code) = want {
			let err = invoices.issue(&ctx, draft.uid.as_str()).await.expect_err(label);
			// The code names the field, so the caller knows what to fix.
			assert_eq!(err.parts().1, code, "{label}");
			assert_eq!(allocated().await, before, "{label} consumed an invoice number");
		} else {
			let issued = invoices
				.issue(&ctx, draft.uid.as_str())
				.await
				.unwrap_or_else(|e| panic!("{label} is filable and must issue: {e}"));
			assert!(issued.number.is_some(), "{label}");
		}
	}

	// The Hungarian tax number is patched onto one party rather than carried by many:
	// `billing_parties.tax_number` is unique per org, so each shape needs the same row.
	let uid = invoices.list_parties(&ctx).await.unwrap()[0].uid.as_str().to_owned();
	let hu_cases = ["12-34", "1234567890", "876543216", "876543210", "876543216242"]
		.map(|bad| (bad, false))
		.into_iter()
		// 8, 9 and 11 digits all file, and the separators are stripped, as `customer_info` does.
		.chain(["12345678", "123456781", "87654321242", "8765-4321-2-42"].map(|good| (good, true)));

	for (number, filable) in hu_cases {
		let before = allocated().await;
		invoices
			.update_party(
				&ctx,
				&uid,
				&PartyPatch {
					tax_number: Patch::Value(number.to_owned()),
					..PartyPatch::default()
				},
			)
			.await
			.unwrap();
		let issue = invoices
			.issue_now(
				&ctx,
				&NewDraft { lines: vec![adhoc(1_000_000, 4_000_000, None)], ..NewDraft::default() },
			)
			.await;
		if filable {
			issue.unwrap_or_else(|e| panic!("`{number}` should issue: {e}"));
		} else {
			let err = issue.expect_err(&format!("`{number}` is not a filable tax number"));
			assert!(err.parts().0.is_client_error(), "`{number}`: {err}");
			assert_eq!(allocated().await, before, "`{number}` consumed an invoice number");
		}
	}
}

/// `LineBody::into_line` refuses `serviceCode` together with `unitPrice`/`vatCode` because
/// the catalogue is operator-owned — but `edit_line` applied both unconditionally, so PATCH
/// was the way around it: issue an operator's catalogue item at your own price and your own
/// tax code, with `invoice_lines.service_id` still pointing at the catalogue row.
#[tokio::test]
async fn a_catalogue_lines_price_and_vat_code_cannot_be_patched() {
	let db = TmpDb::new("catalogue-line-patch");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	invoices
		.create_service(
			&ctx,
			&mintworks_invoice::store::ServiceDef {
				code: "PLAN-PRO".to_owned(),
				name: "Pro csomag".to_owned(),
				description: None,
				unit: "db".to_owned(),
				unit_price: Money(100_000),
				vat_code: VatCode::Std27,
			},
		)
		.await
		.unwrap();
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				lines: vec![Line::code("PLAN-PRO", Qty(1_000_000))],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();

	let patch = async |p: mintworks_invoice::service_api::LinePatch| {
		invoices.edit_line(&ctx, draft.uid.as_str(), 1, p).await
	};
	let err = patch(mintworks_invoice::service_api::LinePatch {
		unit_price: Some(Money(100)),
		..Default::default()
	})
	.await
	.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-LINE");
	let err = patch(mintworks_invoice::service_api::LinePatch {
		vat_code: Some(VatCode::Aam),
		..Default::default()
	})
	.await
	.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-LINE");

	// Everything else on a catalogue line still patches.
	patch(mintworks_invoice::service_api::LinePatch {
		qty: Some(Qty(2_000_000)),
		..Default::default()
	})
	.await
	.unwrap();
	let lines = store.invoice_lines(draft.id).await.unwrap();
	assert_eq!(lines[0].qty, Qty(2_000_000));
	assert_eq!(lines[0].unit_price, Money(100_000), "the catalogue price stands");
}

/// `currencies.price_round_step` is how a currency that displays no decimals is
/// expressed (`Money` is fixed at two), but it was applied only inside `currency::price_in` —
/// i.e. only to a *catalogue* line's converted price. A catalogue HUF line was stepped to
/// whole forints and an ad-hoc line at the same price was not, purely by which field the
/// caller filled. Group VAT and totals are deliberately left unstepped: rounding those breaks
/// `net + vat = gross` and NAV's `summaryByVatRate` cross-validation.
///
/// A *catalogue* price is derived and still rounds; a caller-supplied one is an assertion and
/// is refused rather than rewritten.
#[tokio::test]
async fn an_adhoc_line_gets_the_same_price_round_step_as_a_catalogue_one() {
	let db = TmpDb::new("adhoc-step");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// 1234.56 HUF — a price the step has something to do.
	let raw = Money(123_456);
	invoices
		.create_service(
			&ctx,
			&mintworks_invoice::store::ServiceDef {
				code: "HOUR".to_owned(),
				name: "Tanacsadas".to_owned(),
				description: None,
				unit: "ora".to_owned(),
				unit_price: raw,
				vat_code: VatCode::Std27,
			},
		)
		.await
		.unwrap();

	let stepped = Money(123_500);
	let line = |price| {
		vec![
			mintworks_invoice::draft::Line::code("HOUR", Qty(1_000_000)),
			mintworks_invoice::draft::Line::adhoc(
				"Kiszallas",
				"ora",
				Qty(1_000_000),
				price,
				VatCode::Std27,
			),
		]
	};

	// The caller's `raw` is off the step, so it is refused rather than quietly billed at
	// `stepped` — a price the client never sent.
	assert_eq!(
		invoices
			.draft(&ctx, &NewDraft { lines: line(raw), ..NewDraft::default() })
			.await
			.unwrap_err()
			.parts()
			.1,
		"E-INV-LINE"
	);

	let draft = invoices
		.draft(&ctx, &NewDraft { lines: line(stepped), ..NewDraft::default() })
		.await
		.unwrap();

	let lines = store.invoice_lines(draft.id).await.unwrap();
	assert_eq!(lines.len(), 2);
	// HUF seeds `price_round_step = 100`, so both land on whole forints — the catalogue line
	// by rounding, the ad-hoc one because nothing else is accepted.
	assert_eq!(lines[0].unit_price, stepped, "the catalogue line was already stepped");
	assert_eq!(lines[1].unit_price, lines[0].unit_price, "the ad-hoc line must match it");

	// And `edit_line` is the one path that writes a caller price straight into the set, so
	// `rewrite` has to catch it there too.
	assert_eq!(
		invoices
			.edit_line(
				&ctx,
				draft.uid.as_str(),
				2,
				mintworks_invoice::service_api::LinePatch {
					unit_price: Some(raw),
					..Default::default()
				},
			)
			.await
			.unwrap_err()
			.parts()
			.1,
		"E-INV-LINE",
		"edit_line bypassed the step"
	);

	// The bug in the small: an ad-hoc HUF line at `1.50` was silently stored and billed as
	// `2.00` — a 33% overcharge, answered `201` with no indication.
	assert_eq!(
		invoices
			.draft(
				&ctx,
				&NewDraft { lines: vec![adhoc(1_000_000, 150, None)], ..NewDraft::default() }
			)
			.await
			.expect_err("1.50 HUF is not on the step and must not be billed as 2.00")
			.parts()
			.1,
		"E-INV-LINE"
	);
	let ok = invoices
		.draft(&ctx, &NewDraft { lines: vec![adhoc(1_000_000, 200, None)], ..NewDraft::default() })
		.await
		.unwrap();
	assert_eq!(store.invoice_lines(ok.id).await.unwrap()[0].unit_price, Money(200));

	// The same price off the catalogue is derived, not asserted, so it still rounds — and
	// `issue_now` is the other entry point that has to agree.
	invoices
		.create_service(
			&ctx,
			&mintworks_invoice::store::ServiceDef {
				code: "ODD".to_owned(),
				name: "Tanacsadas".to_owned(),
				description: None,
				unit: "ora".to_owned(),
				unit_price: Money(150),
				vat_code: VatCode::Std27,
			},
		)
		.await
		.unwrap();
	let catalogue = invoices
		.issue_now(
			&ctx,
			&NewDraft { lines: vec![Line::code("ODD", Qty(1_000_000))], ..NewDraft::default() },
		)
		.await
		.unwrap();
	assert_eq!(
		store.invoice_lines(catalogue.id).await.unwrap()[0].unit_price,
		Money(200),
		"a catalogue price still rounds to the step"
	);
}

/// `issue::run` read the stored lines back and ran `pricing::apply` over them a **second**
/// time. `Invoices::draft` had already applied the hook and persisted its output, so a hook
/// that appends a line appended it twice — and the doubled amount landed on a numbered,
/// immutable, NAV-filed invoice. Nothing in `PricingHook`'s contract asked it to be
/// idempotent, and `Invoices::rewrite` never called it at all, so a draft priced differently
/// depending on which path touched it last.
#[tokio::test]
async fn the_pricing_hook_runs_once_per_draft_and_not_again_at_issue() {
	let db = TmpDb::new("hook-once");
	let (_app, invoices, store) =
		service_with(&db, Some(std::sync::Arc::new(Rewriting("add")))).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let issued = invoices
		.issue_now(
			&ctx,
			&NewDraft { lines: vec![adhoc(1_000_000, 4_000_000, None)], ..NewDraft::default() },
		)
		.await
		.unwrap();

	let lines = store.invoice_lines(issued.id).await.unwrap();
	let hook_lines = lines.iter().filter(|l| l.description == "Belepteti dij").count();
	assert_eq!(hook_lines, 1, "the hook was applied to its own persisted output");
	assert_eq!(lines.len(), 2);
	let summed: i64 = lines.iter().map(|l| l.net.0).sum();
	assert_eq!(summed, issued.net.0);
}

/// The address fields were bounded at `MAX_PARTY_NAME` (512) while NAV's
/// `SimpleText255NotBlankType` caps them at 255, and the postcode was bounded by length alone
/// where NAV's `PostalCodeType` is a pattern. A 300-character street or a one-character
/// postcode passed issue and then failed the schema on every attempt — the same immutable,
/// permanently-unfilable invoice a `DRAFT` filing produces.
#[tokio::test]
async fn address_fields_are_held_to_navs_limits_at_the_party() {
	let db = TmpDb::new("address-bounds");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let uid = invoices.list_parties(&ctx).await.unwrap()[0].uid.as_str().to_owned();
	let patch = |p: PartyPatch| {
		let ctx = ctx.clone();
		let invoices = invoices.clone();
		let uid = uid.clone();
		async move { invoices.update_party(&ctx, &uid, &p).await }
	};
	let street = |v: &str| PartyPatch {
		street: mintworks_core::types::Patch::Value(v.to_owned()),
		..PartyPatch::default()
	};
	let postcode = |v: &str| PartyPatch {
		postcode: mintworks_core::types::Patch::Value(v.to_owned()),
		..PartyPatch::default()
	};

	assert!(
		patch(street(&"a".repeat(300))).await.unwrap_err().parts().0.is_client_error(),
		"a 300-character street is longer than SimpleText255NotBlankType allows"
	);
	patch(street(&"a".repeat(255))).await.unwrap();

	for bad in ["1", "12", "sw1a  1aaa1", "-1051", "1051-"] {
		assert!(
			patch(postcode(bad)).await.unwrap_err().parts().0.is_client_error(),
			"`{bad}` is not a PostalCodeType"
		);
	}
	// Uppercased on the way in rather than refused: a postcode is not case-bearing data.
	let party = patch(postcode("sw1a 1aa")).await.unwrap();
	assert_eq!(party.postcode.as_deref(), Some("SW1A 1AA"));
	patch(postcode("1051")).await.unwrap();

	// The three tax identifiers reach NAV through the same frozen snapshot: an over-long
	// `euVatId` fails the XSD on every attempt, on an invoice already numbered and immutable.
	let field = |f: fn(&mut PartyPatch, mintworks_core::types::Patch<String>), v: &str| {
		let mut p = PartyPatch::default();
		f(&mut p, mintworks_core::types::Patch::Value(v.to_owned()));
		p
	};
	let eu = |p: &mut PartyPatch, v| p.eu_vat_id = v;
	let tax = |p: &mut PartyPatch, v| p.tax_number = v;
	let group = |p: &mut PartyPatch, v| p.group_tax_no = v;

	// `CommunityVatNumberType` is `AtomicStringType15`, `minLength 4`.
	assert!(patch(field(eu, &format!("DE{}", "1".repeat(14)))).await.is_err(), "16 characters");
	assert!(patch(field(eu, "DE1")).await.is_err(), "3 characters");
	patch(field(eu, &format!("DE{}", "1".repeat(13)))).await.unwrap();
	// The pattern too, not only the length — `[A-Z]{2}[0-9A-Z]{2,13}`. A separator or a
	// lowercase prefix is normalised in rather than refused; an unknown prefix is refused.
	let party = patch(field(eu, "de 811.569.869")).await.unwrap();
	assert_eq!(party.eu_vat_id.as_deref(), Some("DE811569869"));
	for bad in ["XX1234", "US123456789", "1234567"] {
		assert!(
			patch(field(eu, bad)).await.unwrap_err().parts().0.is_client_error(),
			"`{bad}` is not a communityVatNumber"
		);
	}
	// `thirdStateTaxId` is `SimpleText50NotBlankType`.
	assert!(patch(field(tax, &"9".repeat(51))).await.is_err(), "51 characters");
	patch(field(tax, &"9".repeat(50))).await.unwrap();
	// `base:TaxNumberType` takes the first 8 digits as `base:taxpayerId`.
	assert!(patch(field(group, "1234567")).await.is_err(), "7 digits");
	patch(field(group, "12345678")).await.unwrap();
}

/// Storno gates on step-up in the service, not the handler; issuing does not.
#[tokio::test]
async fn storno_gates_on_step_up_and_issue_does_not() {
	use mintworks_core::ctx::Actor;

	let db = TmpDb::new("service-stepup");
	let (_app, invoices, store) = service(&db).await;
	let system = Ctx::system("test").with_org(ORG);
	let draft = service_draft(&invoices, &system).await;
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 1, 'ADMIN', 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();

	// A seller admin whose credential was presented well outside `auth.stepup_window`.
	let stale = Ctx {
		actor: Actor::User { account_id: 1 },
		auth_at: Some(Timestamp::now().0 - 1_000_000),
		..Ctx::system("test").with_org(ORG)
	};
	let issued = invoices.issue(&stale, draft.uid.as_str()).await.unwrap();
	assert_eq!(issued.status, InvoiceStatus::Issued);

	assert_eq!(
		invoices.storno(&stale, issued.uid.as_str(), "t").await.unwrap_err().parts().1,
		"E-AUTH-STEPUP"
	);
	invoices.storno(&system, issued.uid.as_str(), "t").await.unwrap();

	let new_draft = NewDraft {
		billing_party: Party::OrgDefault,
		lines: vec![adhoc(1_000_000, 100_000, None)],
		..NewDraft::default()
	};
	assert_eq!(invoices.issue_now(&stale, &new_draft).await.unwrap().status, InvoiceStatus::Issued);
}

/// Only storno is step-up gated (rust-api.md §4.4); recording a payment is routine bookkeeping.
#[tokio::test]
async fn mark_paid_and_set_paid_need_no_step_up() {
	use mintworks_core::ctx::Actor;

	let db = TmpDb::new("paid-no-stepup");
	let (_app, invoices, store) = service(&db).await;
	let system = Ctx::system("test").with_org(ORG);
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 1, 'ADMIN', 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	let stale = Ctx {
		actor: Actor::User { account_id: 1 },
		auth_at: Some(Timestamp::now().0 - 1_000_000),
		..Ctx::system("test").with_org(ORG)
	};
	let issued = invoices.issue_now(&system, &plain_draft()).await.unwrap();
	invoices.set_paid(&stale, issued.uid.as_str(), Money(1), None).await.unwrap();
	assert_eq!(
		invoices.mark_paid(&stale, issued.uid.as_str()).await.unwrap().status,
		InvoiceStatus::Paid
	);
}

/// `insert_draft` mapped every unique violation to the `request_id` conflict before
/// returning, so the storno path's outer unique-violation match could never match
/// and an `idx_invoice_storno_once` collision answered `E-CORE-CONFLICT` with the wrong
/// message. The guard held either way — this was a wrong-answer defect — but
/// `storno::run` documents `E-INV-ALREADY-STORNOED`, and now that is what comes back.
///
/// A real file database, so the two stores genuinely contend for the single write lock.
#[tokio::test]
async fn a_second_storno_is_refused_as_already_cancelled() {
	let db = TmpDb::new("storno-race");
	let (app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);
	let draft = service_draft(&invoices, &ctx).await;
	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();

	// Concurrently: exactly one may land, whichever guard catches the other.
	let one = {
		let (invoices, ctx, uid) = (invoices.clone(), ctx.clone(), issued.uid.clone());
		tokio::spawn(async move { invoices.storno(&ctx, uid.as_str(), "race").await })
	};
	let two = {
		let invoices = Invoices::new(app.clone());
		let (ctx, uid) = (ctx.clone(), issued.uid.clone());
		tokio::spawn(async move { invoices.storno(&ctx, uid.as_str(), "race").await })
	};
	let outcomes = [one.await.unwrap(), two.await.unwrap()];
	assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1, "exactly one storno may land");

	// The index path specifically. The race above usually stops at the in-transaction status
	// read, which is the *other* guard; putting the original back to ISSUED with its
	// counter-invoice already written is the only way to reach `idx_invoice_storno_once`
	// deterministically. No trait method can do it, which
	// `a_cancelled_invoice_cannot_be_revived_through_the_trait` pins.
	sqlx::query("UPDATE invoices SET status = 'ISSUED' WHERE id = ?")
		.bind(issued.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	let err = invoices.storno(&ctx, issued.uid.as_str(), "again").await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-ALREADY-STORNOED", "{err:?}");
	assert!(
		!err.to_string().contains("request_id"),
		"the storno path answered with the draft path's conflict: {err:?}"
	);
}

/// `patch` routes `currency` to `change_currency` and `billing_party_id` to `rewrite`,
/// the two fields that move a draft's monetary basis — but `rate_e6` fell straight through to
/// `update_draft`'s bare `rate_e6 = COALESCE(?, rate_e6)`. The stored line prices kept the old
/// rate, `add_line` priced new catalogue lines at the new one, and `issue::freeze` copied
/// whatever was on the row, so the invoice issued at a rate that did not describe its own unit
/// prices. Refused now, while the pair a currency change sets together still works.
#[tokio::test]
async fn a_rate_cannot_be_patched_without_the_currency_that_re_prices() {
	let db = TmpDb::new("patch-rate");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 4_000_000, None)],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();

	let err = invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch { rate_e6: Some(400_000_000), ..Default::default() },
		)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-RATE", "{err:?}");

	// Untouched: the refusal is in front of the write.
	let same = store.invoice_by_id(draft.id).await.unwrap().unwrap();
	assert_eq!(same.rate_e6, draft.rate_e6);

	// And the legitimate pair — what `patch_by_uid` builds for a currency change — still
	// re-prices through `change_currency` rather than tripping the guard.
	let eur = invoices
		.patch_by_uid(
			&ctx,
			draft.uid.as_str(),
			None,
			Some(&CurrencyCode::parse("EUR").unwrap()),
			&InvoicePatch::default(),
		)
		.await
		.unwrap();
	assert_eq!(eur.currency, "EUR");
	let lines = store.invoice_lines(eur.id).await.unwrap();
	assert_eq!(lines[0].unit_price, Money(10_000), "the lines were not re-priced with the rate");
}

/// `PartyPatch` makes every field optional so one type can drive PATCH, and `checked_party`
/// validates only what is present — so `POST /api/billing-parties` with `{}` bound NULL into
/// three `NOT NULL` columns and came back `500 E-CORE-INTERNAL`. Update must keep taking a
/// partial, so the requirement belongs on the create path only.
#[tokio::test]
async fn creating_a_billing_party_without_its_mandatory_fields_is_a_400() {
	let db = TmpDb::new("party-required");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let err = invoices.create_party(&ctx, &PartyPatch::default()).await.unwrap_err();
	let (status, code) = err.parts();
	assert_eq!(status, mintworks_core::error::StatusCode::BAD_REQUEST, "{err:?}");
	assert_eq!(code, "E-INV-BAD-TEXT");

	// Each of the three, one at a time.
	let full = PartyPatch {
		kind: Some(PartyKind::Company),
		name: Some("Vevo Zrt.".into()),
		country: Some("HU".into()),
		..Default::default()
	};
	for missing in ["kind", "name", "country"] {
		let mut p = full.clone();
		match missing {
			"kind" => p.kind = None,
			"name" => p.name = None,
			_ => p.country = None,
		}
		let err = invoices.create_party(&ctx, &p).await.unwrap_err();
		assert_eq!(err.parts().1, "E-INV-BAD-TEXT", "missing {missing}: {err:?}");
		assert!(err.to_string().contains(missing), "{err:?}");
	}

	// And the complete patch still creates.
	assert_eq!(invoices.create_party(&ctx, &full).await.unwrap().name, "Vevo Zrt.");
}

/// The PDF printed one statutory VAT note, taken by `find_map` from the *first* zero-rated
/// group — so a domestic invoice with one `AAM` line and one `TAM` line rendered only
/// `vat.aam` and the Áfa tv. 169. § m) reference for the other exempt supply was missing from
/// a numbered, immutable document. Meanwhile `mintworks_nav::xml` filed a correct per-code `reason`
/// for both, so the PDF and the filing disagreed and nothing raised.
///
/// The template is compiled here too: `#for k in notes` is the loop that replaced the single
/// lookup, and a typst error in it would only ever surface at render time.
#[tokio::test]
async fn a_mixed_exempt_invoice_prints_every_statutory_note() {
	let db = TmpDb::new("pdf-vat-notes");
	let store = setup(&db).await;

	let inv = draft(&store, None).await;
	let mut input = issue_input(inv.id, 100_000);
	// One exempt group per code, and the joined column `issue::prepare` now writes.
	input.vat = Money(0);
	input.gross = input.net;
	input.vat_note = Some("vat.aam\nvat.tam".to_owned());
	input.lines[0].vat_code = VatCode::Aam;
	input.lines[0].vat_rate_bp = 0;
	input.lines[0].vat = Money(0);
	input.lines[0].gross = input.lines[0].net;
	input.groups = [VatCode::Aam, VatCode::Tam]
		.into_iter()
		.map(|code| InvoiceVatGroup {
			invoice_id: inv.id,
			vat_code: code,
			vat_rate_bp: 0,
			net: Money(50_000),
			vat: Money(0),
			gross: Money(50_000),
			net_huf: None,
			vat_huf: None,
			gross_huf: None,
		})
		.collect();

	let issued = store.issue(inv.id, &input, inv.version).await.unwrap();
	let seller = store.current_seller_version(SELLER).await.unwrap().unwrap();
	let lines = store.invoice_lines(issued.id).await.unwrap();
	let groups = store.invoice_vat_groups(issued.id).await.unwrap();

	let data =
		mintworks_invoice::pdf::document(&seller, &issued, &lines, &groups, None, None).unwrap();
	let doc: serde_json::Value = serde_json::from_str(&data).unwrap();
	assert_eq!(
		doc["invoice"]["vatNotes"],
		serde_json::json!(["vat.aam", "vat.tam"]),
		"both notes have to reach the template: {data}"
	);

	// And the template renders them. A `#for` over a missing key or a bad type fails here.
	let pdf = mintworks_invoice::pdf::render(&data).unwrap();
	assert!(pdf.starts_with(b"%PDF"), "not a PDF");

	// The page content is compressed, so the proof that both notes were *drawn* is that the
	// document shrinks as they are taken away, one at a time.
	let with = |notes: serde_json::Value| {
		let mut doc = doc.clone();
		doc["invoice"]["vatNotes"] = notes;
		mintworks_invoice::pdf::render(&doc.to_string()).unwrap().len()
	};
	let one = with(serde_json::json!(["vat.aam"]));
	let none = with(serde_json::json!([]));
	assert!(pdf.len() > one && one > none, "two notes {} > one {one} > none {none}", pdf.len());
}

/// `pdf::document` emitted every amount as a bare grouped decimal and never emitted the
/// currency at all. The only currency token on the page was `d.rate.quote` in the optional
/// exchange-rate footnote, which is `null` for a HUF invoice — so a HUF invoice carried no
/// currency indication whatsoever, and a EUR one was distinguished only by that footnote.
#[tokio::test]
async fn the_pdf_data_document_names_the_currency() {
	let db = TmpDb::new("pdf-currency");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);
	let seller = store.current_seller_version(SELLER).await.unwrap().unwrap();

	for (currency, expected) in [(None, "HUF"), (Some(CurrencyCode::parse("EUR").unwrap()), "EUR")]
	{
		let issued = invoices
			.issue_now(
				&ctx,
				&NewDraft { currency, ..new_draft(None, vec![adhoc(1_000_000, 100_000, None)]) },
			)
			.await
			.unwrap();
		let lines = store.invoice_lines(issued.id).await.unwrap();
		let groups = store.invoice_vat_groups(issued.id).await.unwrap();

		let data = mintworks_invoice::pdf::document(&seller, &issued, &lines, &groups, None, None)
			.unwrap();
		let doc: serde_json::Value = serde_json::from_str(&data).unwrap();
		assert_eq!(doc["invoice"]["currency"], expected, "{data}");

		// And the template reads it. A missing field would fail here, not at render time in
		// production.
		assert!(mintworks_invoice::pdf::render(&data).unwrap().starts_with(b"%PDF"));
	}
}

/// `Invoices::seller` took `_ctx`, did no authorization and returned the raw `Seller` row
/// — `nav_login` and `nav_base_url` included. The HTTP route was safe only because
/// `catalog::seller` happened to wrap it; a Rust consumer rendering the handle directly (the
/// documented primary integration path) published the operator's NAV technical user.
#[tokio::test]
async fn the_seller_handle_never_hands_out_the_nav_credentials() {
	let db = TmpDb::new("seller-redaction");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	store
		.put_seller(&Seller {
			nav_login: Some("tech-user-1".into()),
			..stored_seller(&store).await
		})
		.await
		.unwrap();

	let json = serde_json::to_string(&invoices.seller(&ctx).await.unwrap()).unwrap();
	assert!(!json.contains("navLogin"), "{json}");
	assert!(!json.contains("navBaseUrl"), "{json}");
	assert!(!json.contains("tech-user-1"), "{json}");
	assert!(!json.contains("onlineszamla"), "{json}");
	// Still the seller, not an empty shell.
	assert!(json.contains("12345678242"), "{json}");
}

/// `draft::price` stored `stored_code.rate_bp()` beside money computed over the
/// *verdict-overridden* code. On a draft (`freeze_vat = false`) `stored_code` is the
/// product's, so a third-country buyer produced a line reading `STD27` / `2700` next to
/// `vat: 0.00`, in an invoice whose only VAT group was `HO` at 0%.
#[tokio::test]
async fn a_draft_lines_vat_rate_follows_the_money_not_the_product_code() {
	let db = TmpDb::new("draft-rate-follows-money");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// A third-country company: `BuyerZone::Third` + company is `Verdict::Override(Ho)`.
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (2, 'prt_us2', ?, 'C', 'Acme Inc.', 'US', '99-1234567',
		  '10001', 'New York', '5th Ave 1.', 0, 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();

	let draft = invoices
		.draft(&ctx, &new_draft(None, vec![adhoc(1_000_000, 100_000, None)]))
		.await
		.unwrap();
	let draft = invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch { billing_party_id: Some(2), ..Default::default() },
		)
		.await
		.unwrap();
	assert_eq!(draft.vat, Money(0), "reverse charge / export: no VAT is charged");

	let line = &store.invoice_lines(draft.id).await.unwrap()[0];
	assert_eq!(line.vat, Money(0));
	assert_eq!(line.vat_rate_bp, 0, "the rate has to match the money beside it");
	// The one-way door stays open: the draft still remembers what the caller asked for, so a
	// later change back to a domestic buyer can undo the override.
	assert_eq!(line.vat_code, VatCode::Std27, "a draft line keeps the product's own code");
}

/// `storno::run` copied `discount_kind` and `discount_value` verbatim while negating
/// `discount_amount`. Correct for `PERCENT` — basis points are sign-neutral — but an `AMOUNT`
/// counter-line then claimed a +5.00 discount beside a −5.00 resolved one, on a numbered,
/// immutable document that `LineView` serves both fields from.
#[tokio::test]
async fn a_storno_line_keeps_its_discount_value_and_amount_in_the_same_sign() {
	let db = TmpDb::new("storno-discount-sign");
	let (app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	for (discount, negated) in
		[(Discount::Amount(Money(500)), true), (Discount::Percent(1_000), false)]
	{
		let issued = invoices
			.issue_now(&ctx, &new_draft(None, vec![adhoc(2_000_000, 50_000, Some(discount))]))
			.await
			.unwrap();
		let original = &store.invoice_lines(issued.id).await.unwrap()[0];
		let before = original.discount_value.unwrap();

		let st = mintworks_invoice::storno::run(&app, &store, &issued, "teves szamlazas")
			.await
			.unwrap();
		let line = &store.invoice_lines(st.id).await.unwrap()[0];
		let after = line.discount_value.unwrap();

		if negated {
			assert_eq!(after, -before, "an AMOUNT discount is money and follows the amount");
			assert!(
				after < 0 && line.discount_amount.0 < 0,
				"{after} / {}",
				line.discount_amount.0
			);
		} else {
			assert_eq!(after, before, "PERCENT is basis points and is sign-neutral");
		}
	}
}

/// `issue::prepare` mapped `currency.rate_source` with `_ => RateSource::Bank`, so
/// `MANUAL` — a real variant, and a value both `currency_rates.source` and
/// `invoices.rate_source` accept — was silently rewritten to `BANK` on an immutable row that
/// says which rate the seller may legally use. Garbage cannot reach the mapping at all:
/// `SettingDef::choice` refuses it in both `Settings::set` and `Settings::get`, and
/// `RateSource`'s generated `FromStr` is the belt under that.
#[tokio::test]
async fn a_manual_rate_source_is_frozen_as_manual_and_not_as_bank() {
	let db = TmpDb::new("rate-source-manual");
	let (app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	app.settings.set("currency.rate_source", "MANUAL", None).await.unwrap();
	sqlx::query(
		"INSERT INTO currency_rates (pair, date, source, rate_e6, fetched_at)
		 VALUES ('EURHUF', '2020-01-01', 'MANUAL', 400000000, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	let issued = invoices
		.issue_now(
			&ctx,
			&NewDraft {
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				..new_draft(None, vec![adhoc(1_000_000, 100_000, None)])
			},
		)
		.await
		.unwrap();
	assert_eq!(issued.rate_source, Some(mintworks_invoice::store::RateSource::Manual));

	assert!(
		app.settings.set("currency.rate_source", "FED", None).await.is_err(),
		"an unknown source is refused before it can be frozen onto an invoice"
	);
}

/// `change_currency` unfed *every* line, but only a catalogue line's stored price ever had
/// `fee_bp` applied — `draft::resolve` stores an ad-hoc price verbatim. So a currency change
/// divided a markup out of a price that never had one and undercharged by it, on a draft that
/// `issue` then froze onto a numbered, immutable, NAV-filed document.
#[tokio::test]
async fn changing_the_currency_of_an_adhoc_line_adds_no_fee_and_removes_none() {
	let db = TmpDb::new("currency-adhoc-fee");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	sqlx::query("UPDATE currencies SET fee_bp = 500 WHERE code = 'EUR'")
		.execute(store.write_pool())
		.await
		.unwrap();

	// 100.00 EUR, the caller's own figure, at 1 EUR = 400 HUF.
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				lines: vec![adhoc(1_000_000, 10_000, None)],
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(store.invoice_lines(draft.id).await.unwrap()[0].unit_price, Money(10_000));

	invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch {
				currency: Some(CurrencyCode::parse("HUF").unwrap()),
				..InvoicePatch::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(
		store.invoice_lines(draft.id).await.unwrap()[0].unit_price,
		Money(4_000_000),
		"40 000.00 HUF, not the 38 095.24 an unfeed of a fee that was never applied gave"
	);

	// And back: an ad-hoc price round-trips, with neither currency's fee stuck to it.
	invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch {
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				..InvoicePatch::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(store.invoice_lines(draft.id).await.unwrap()[0].unit_price, Money(10_000));
}

/// `patch` routed a fulfilment-date move through `rewrite`, which changed no line and
/// re-ran the VAT engine over the **existing** `unit_price` values while stamping the new
/// date's `rate_e6` on the row. The draft then claimed rate R2 with lines priced at R1 — and
/// `add_line` (`draft::resolve(store, &cur, invoice.rate_e6, …)`) and `change_currency`
/// (`to_base_unfeed(m, &old_cur, old_rate_e6)`) both read `invoice.rate_e6` as "the rate these
/// lines were priced at", so the error compounded and survived `issue` onto a numbered,
/// immutable, NAV-filed invoice.
#[tokio::test]
async fn a_moved_fulfilment_date_reprices_catalogue_lines_and_does_not_compound() {
	use mintworks_invoice::numbering::date_of;

	let db = TmpDb::new("fulfilment-date-reprice");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// `OFFICIAL`, so the rate depends on the date at all — the seeded EUR is `FIXED`.
	// 300 HUF/USD three days ago, 400 HUF/USD today.
	let today = date_of(Timestamp::now()).unwrap();
	let earlier = date_of(Timestamp(Timestamp::now().0 - 3 * 86_400)).unwrap();
	// And a quarter-percent move, the size an MNB fixing actually makes, for the tail below.
	let yesterday = date_of(Timestamp(Timestamp::now().0 - 86_400)).unwrap();
	sqlx::query(
		"INSERT INTO currencies (code, price_round_step, mode, fee_bp)
		 VALUES ('USD', 1, 'OFFICIAL', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	for (date, rate) in
		[(&earlier, 300_000_000_i64), (&today, 400_000_000), (&yesterday, 401_000_000)]
	{
		sqlx::query(
			"INSERT INTO currency_rates (pair, date, source, rate_e6, fetched_at)
			 VALUES ('USDHUF', ?, 'BANK', ?, 0)",
		)
		.bind(date)
		.bind(rate)
		.execute(store.write_pool())
		.await
		.unwrap();
	}
	invoices
		.create_service(
			&ctx,
			&mintworks_invoice::store::ServiceDef {
				code: "HOUR".to_owned(),
				name: "Tanacsadas".to_owned(),
				description: None,
				unit: "ora".to_owned(),
				// Catalogue prices are base-currency (HUF): 10 000.00 HUF an hour.
				unit_price: Money(1_000_000),
				vat_code: VatCode::Std27,
			},
		)
		.await
		.unwrap();

	// One catalogue line (priced through `price_in`, so it carries the fee) and one ad-hoc
	// line (stored verbatim). No fulfilment date, so both are priced on today's 400.
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				lines: vec![
					mintworks_invoice::draft::Line::code("HOUR", Qty(1_000_000)),
					adhoc(1_000_000, 10_000, None),
				],
				currency: Some(CurrencyCode::parse("USD").unwrap()),
				..NewDraft::default()
			},
		)
		.await
		.unwrap();

	// The HUF a line is worth: the invariant a rate change must preserve, up to one
	// `price_round_step` of the invoice currency (worth `rate_e6 / 1e6` base units).
	let base = |price: Money, rate_e6: i64| price.0 * rate_e6 / 1_000_000;
	let prices = async |id| {
		store
			.invoice_lines(id)
			.await
			.unwrap()
			.iter()
			.map(|l| l.unit_price)
			.collect::<Vec<_>>()
	};
	let before = prices(draft.id).await;
	assert_eq!(before, vec![Money(2_500), Money(10_000)], "10 000.00 HUF / 100.00 USD at 400");

	invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch {
				fulfilment_date: Patch::Value(earlier.clone()),
				..InvoicePatch::default()
			},
		)
		.await
		.unwrap();

	let row = store.invoice_by_id(draft.id).await.unwrap().unwrap();
	assert_eq!(row.rate_e6, 300_000_000, "the row took the patched date's rate");
	let after = prices(draft.id).await;
	// The catalogue line re-prices: its master price genuinely lives in the base currency, so
	// a new rate is a new USD figure for the same 10 000.00 HUF. 400/300 in, one step out —
	// the base value is what survives a rate change.
	assert_ne!(after[0], before[0], "a catalogue line left at the old rate is the bug");
	assert!(
		(base(before[0], 400_000_000) - base(after[0], 300_000_000)).abs() <= 300,
		"{:?} at 400 and {:?} at 300 are not the same money",
		before[0],
		after[0]
	);
	// The ad-hoc line does not, and this is the other half of the same rule: 100.00 USD is the
	// caller's own figure, already denominated in the invoice currency and never derived from a
	// base-currency master price. The currency did not change, so nothing here may.
	assert_eq!(after[1], before[1], "a date move rewrote the caller's own ad-hoc price");
	let groups = store.invoice_vat_groups(draft.id).await.unwrap();
	for g in &groups {
		assert_eq!(g.net + g.vat, g.gross);
	}

	// A line added afterwards is priced at the row's rate, which is now the rate every
	// existing line carries — so the same catalogue item costs the same on both.
	invoices
		.add_line(
			&ctx,
			draft.uid.as_str(),
			mintworks_invoice::draft::Line::code("HOUR", Qty(1_000_000)),
		)
		.await
		.unwrap();
	let with_new = prices(draft.id).await;
	assert_eq!(with_new[2], with_new[0], "the new catalogue line matches the re-priced one");

	// And a currency change on top of it does not compound. EUR is fixed at 400 HUF, which is
	// what USD was worth when the draft was created, so the catalogue line comes back where it
	// started apart from step rounding. The ad-hoc line *does* convert here — the currency
	// really changes — and 100.00 USD on a day USD is 300 HUF is 75.00 EUR at 400.
	invoices
		.patch(
			&ctx,
			draft.uid.as_str(),
			&InvoicePatch {
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				..InvoicePatch::default()
			},
		)
		.await
		.unwrap();
	let eur = prices(draft.id).await;
	assert!(
		(before[0].0 - eur[0].0).abs() <= 1,
		"{:?} -> {:?} compounded a rounding error",
		before[0],
		eur[0]
	);
	assert_eq!(eur[1], Money(7_500), "100.00 USD at 300 HUF is 75.00 EUR at 400");

	// The HUF figures Áfa tv. 172. § makes mandatory move with the date too:
	// `invoice_vat_groups.{net,vat,gross}_huf` re-resolve on the patched fulfilment date.
	let plain = invoices
		.draft(
			&ctx,
			&NewDraft {
				lines: vec![adhoc(1_000_000, 10_000, None)],
				currency: Some(CurrencyCode::parse("USD").unwrap()),
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	let net_huf = async |id| store.invoice_vat_groups(id).await.unwrap()[0].net_huf;
	assert_eq!(net_huf(plain.id).await, Some(Money(4_000_000)), "100.00 USD at today's 400");
	invoices
		.patch(
			&ctx,
			plain.uid.as_str(),
			&InvoicePatch {
				fulfilment_date: Patch::Value(earlier.clone()),
				..InvoicePatch::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(
		store.invoice_by_id(plain.id).await.unwrap().unwrap().fulfilment_date,
		Some(earlier.clone())
	);
	// The ad-hoc price itself does not move; what moves is what it is worth in HUF.
	assert_eq!(store.invoice_lines(plain.id).await.unwrap()[0].unit_price, Money(10_000));
	assert_eq!(net_huf(plain.id).await, Some(Money(3_000_000)), "re-resolved on the patched date");

	// A quarter-percent move is where a wrong rescale hides. Re-stamping `rate_e6` may re-price
	// catalogue lines only: an ad-hoc line and an invoice-level `AMOUNT` discount are already
	// denominated in the invoice currency, so `old_rate / new_rate` on them is silent theft.
	let small = invoices
		.draft(
			&ctx,
			&NewDraft {
				lines: vec![
					mintworks_invoice::draft::Line::code("HOUR", Qty(1_000_000)),
					adhoc(1_000_000, 10_000, None),
				],
				discount: Some(Discount::Amount(Money(500))),
				currency: Some(CurrencyCode::parse("USD").unwrap()),
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(small.discount_value, Some(500));
	let before = prices(small.id).await;
	invoices
		.patch(
			&ctx,
			small.uid.as_str(),
			&InvoicePatch {
				fulfilment_date: Patch::Value(yesterday.clone()),
				..InvoicePatch::default()
			},
		)
		.await
		.unwrap();

	let row = store.invoice_by_id(small.id).await.unwrap().unwrap();
	assert_eq!(row.rate_e6, 401_000_000, "the row took the patched date's rate");
	let after = prices(small.id).await;
	// Byte-identical, not "close": 10_000 * 400/401 truncates to 9_975, which is the defect.
	assert_eq!(after[1], before[1], "the ad-hoc price was rescaled by a rate it never used");
	assert_eq!(row.discount_value, Some(500), "the AMOUNT discount was rescaled");
	// …while the catalogue line, whose price really is a base-currency figure, did move.
	assert_ne!(after[0], before[0], "the catalogue line must re-price off the new rate");
}

/// `Invoice::original_invoice_id` is internal and `hydrate` dropped it, so nothing on the
/// wire paired a storno with what it cancelled — `notes` carries only the free-text reason.
/// The page cursor was the raw `invoices.id`, which is not a value a response may carry.
#[tokio::test]
async fn a_view_pairs_a_storno_with_its_original_and_pages_on_uids() {
	let db = TmpDb::new("storno-uids-and-cursor");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let mut issued = Vec::new();
	for _ in 0..3 {
		let d = invoices
			.draft(&ctx, &new_draft(None, vec![adhoc(1_000_000, 100_000, None)]))
			.await;
		issued.push(invoices.issue(&ctx, d.unwrap().uid.as_str()).await.unwrap());
	}
	let storno = invoices.storno(&ctx, issued[0].uid.as_str(), "test").await.unwrap();

	let of = async |uid: &str| invoices.full(&ctx, uid).await.unwrap();
	let counter = of(storno.uid.as_str()).await;
	assert_eq!(counter.original_invoice_uid.as_ref(), Some(&issued[0].uid));
	assert_eq!(counter.storno_invoice_uid, None);

	let cancelled = of(issued[0].uid.as_str()).await;
	assert_eq!(cancelled.storno_invoice_uid.as_ref(), Some(&storno.uid));
	assert_eq!(cancelled.original_invoice_uid, None);

	// An untouched invoice is paired with nothing.
	let plain = of(issued[1].uid.as_str()).await;
	assert!(plain.original_invoice_uid.is_none() && plain.storno_invoice_uid.is_none());

	// The listing resolves the same three uids by join rather than per row, so it is asserted
	// separately from `full` above.
	let listed = invoices.list_full(&ctx, &InvoiceFilter::default(), None, 10).await.unwrap();
	let find = |uid: &mintworks_core::prelude::InvoiceId| {
		listed
			.iter()
			.find(|f| f.invoice.uid == *uid)
			.expect("row is on the page")
			.clone()
	};
	assert_eq!(find(&storno.uid).original_invoice_uid.as_ref(), Some(&issued[0].uid));
	assert_eq!(find(&issued[0].uid).storno_invoice_uid.as_ref(), Some(&storno.uid));
	assert!(find(&issued[1].uid).storno_invoice_uid.is_none());
	assert!(listed.iter().all(|f| f.party_uid.is_some()), "the party uid comes off the join");

	// Paging: the cursor is the last item's uid, and it resumes after that row.
	let page = invoices.list_full(&ctx, &InvoiceFilter::default(), None, 2).await.unwrap();
	assert_eq!(page.len(), 2);
	let cursor = page.last().unwrap().invoice.uid.as_str().to_owned();
	let next: Vec<_> = invoices
		.list_full(&ctx, &InvoiceFilter::default(), Some(&cursor), 10)
		.await
		.unwrap()
		.into_iter()
		.map(|f| f.invoice.uid)
		.collect();
	assert!(!next.iter().any(|uid| uid.as_str() == cursor), "the cursor row is not repeated");
	assert!(next.iter().any(|uid| *uid == issued[0].uid), "and paging continues past it");

	// A row id — what the cursor used to be — is not a cursor.
	assert!(
		invoices
			.list_full(&ctx, &InvoiceFilter::default(), Some("1"), 10)
			.await
			.is_err()
	);
}

/// `storno::run` took its series from `numbering::series_for`, which reads
/// `sellers.series_code` — the seller's *current* one. `invoices.series_code`, frozen on the
/// row being cancelled, was never read, so changing the seller's code landed every later
/// storno in a different series from its original and broke the gapless run the two documents
/// are supposed to share (`storno.rs`'s own module doc claims they share it).
#[tokio::test]
async fn a_storno_draws_from_the_original_series_not_the_sellers_current_one() {
	let db = TmpDb::new("storno-series");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let draft = service_draft(&invoices, &ctx).await;
	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
	let original_series = issued.series_code.clone().expect("an issued invoice is numbered");

	// The seller moves to a new series — a new year's prefix, a rebrand, anything.
	store
		.put_seller(&Seller { series_code: "B".to_owned(), ..stored_seller(&store).await })
		.await
		.unwrap();

	let storno = invoices.storno(&ctx, issued.uid.as_str(), "teves szamlazas").await.unwrap();
	assert_eq!(
		storno.series_code,
		Some(original_series),
		"the counter-invoice left the series it is cancelling in"
	);
	// A fresh invoice does follow the seller, so this is not just pinning everything to 'A'.
	let next = invoices
		.issue(&ctx, service_draft(&invoices, &ctx).await.uid.as_str())
		.await
		.unwrap();
	assert_eq!(next.series_code, Some("B".to_owned()));
}

/// `Invoices::draft` resolved the FX rate on **today** — `numbering::date_of(now())` —
/// although `req.fulfilment_date` was in hand and format-checked two lines above.
/// `Invoices::rewrite` goes to explicit trouble to resolve on the patched or stored fulfilment
/// date, and `issue::plan` resolves on `fulfilment_date` and freezes it as `rate_date`, so the
/// draft and the issue disagreed: the customer-visible figures silently changed at issue.
///
/// Both the invoice's own `rate_e6`, which prices every line, and the `*_huf` trio on each VAT
/// group were wrong, so the test pins the stored figures *and* that `issue` does not move them.
#[tokio::test]
async fn a_draft_is_priced_on_its_fulfilment_date_not_on_today() {
	let db = TmpDb::new("draft-rate-date");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	// The harness ships EUR as `FIXED`, which ignores the date by construction. `OFFICIAL` is
	// what makes `effective_rate_e6` consult `currency_rates`.
	sqlx::query("UPDATE currencies SET mode = 'OFFICIAL', fixed_rate_e6 = NULL WHERE code = 'EUR'")
		.execute(store.write_pool())
		.await
		.unwrap();

	let today = mintworks_invoice::numbering::date_of(Timestamp::now()).unwrap();
	let fulfilment = mintworks_invoice::numbering::add_days(&today, -5).unwrap();
	for (date, rate_e6) in [(&fulfilment, 395_000_000_i64), (&today, 410_000_000)] {
		sqlx::query(
			"INSERT INTO currency_rates (pair, date, source, rate_e6, fetched_at)
			 VALUES ('EURHUF', ?, 'BANK', ?, 0)",
		)
		.bind(date)
		.bind(rate_e6)
		.execute(store.write_pool())
		.await
		.unwrap();
	}

	// 100.00 EUR net at 27%.
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				fulfilment_date: Some(fulfilment.clone()),
				..new_draft(None, vec![adhoc(1_000_000, 10_000, None)])
			},
		)
		.await
		.unwrap();
	assert_eq!(draft.rate_e6, 395_000_000, "the lines were priced on today's rate");

	// 27.00 EUR of VAT at 395.00 is 10 665.00 HUF; at today's 410.00 it would be 11 070.00.
	let vat_huf = |id: i64| {
		let store = store.clone();
		async move {
			sqlx::query_scalar::<_, i64>(
				"SELECT vat_huf FROM invoice_vat_groups WHERE invoice_id = ?",
			)
			.bind(id)
			.fetch_one(store.read_pool())
			.await
			.unwrap()
		}
	};
	assert_eq!(vat_huf(draft.id).await, 1_066_500, "the HUF figures took the wrong day's rate");

	// And issuing must not move them: `issue::plan` resolves on the same date.
	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
	assert_eq!(issued.rate_date.as_deref(), Some(fulfilment.as_str()));
	assert_eq!(issued.huf_rate_e6, Some(395_000_000));
	assert_eq!(vat_huf(issued.id).await, 1_066_500, "the draft silently re-priced at issue");
}

/// The PDF printed `huf_rate_e6.unwrap_or(rate_e6)` as the Áfa tv. 172. § exchange rate.
/// Those are two different quantities — `rate_e6` is base→invoice-currency — so on any
/// non-HUF-base deployment the page carried a fabricated statutory rate. `mintworks_nav::xml`
/// refuses the same `None`; failing the render is right, a wrong rate on an issued invoice is
/// worse than no PDF.
#[tokio::test]
async fn the_pdf_refuses_to_invent_a_statutory_exchange_rate() {
	let db = TmpDb::new("pdf-huf-rate");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);
	let seller = store.current_seller_version(SELLER).await.unwrap().unwrap();

	let issued = invoices
		.issue_now(
			&ctx,
			&NewDraft {
				currency: Some(CurrencyCode::parse("EUR").unwrap()),
				..new_draft(None, vec![adhoc(1_000_000, 100_000, None)])
			},
		)
		.await
		.unwrap();
	let lines = store.invoice_lines(issued.id).await.unwrap();
	let groups = store.invoice_vat_groups(issued.id).await.unwrap();
	// The real row carries one, and renders.
	assert!(issued.huf_rate_e6.is_some());
	assert!(
		mintworks_invoice::pdf::document(&seller, &issued, &lines, &groups, None, None).is_ok()
	);

	let no_rate = Invoice { huf_rate_e6: None, ..issued };
	assert!(
		mintworks_invoice::pdf::document(&seller, &no_rate, &lines, &groups, None, None).is_err(),
		"a foreign-currency invoice with no HUF rate must not render"
	);
}

/// `draft::to_draft` read the stored `(discount_kind, discount_value)` pair with
/// `u32::try_from(v).unwrap_or(0)` instead of `money::discount_of`, the documented single
/// funnel. `invoice_lines.discount_value` has no sign `CHECK`, so a negative stored percentage
/// silently re-priced the line with **no** discount: the customer was billed the undiscounted
/// net and the invoice issued and filed with no diagnostic anywhere.
#[tokio::test]
async fn an_out_of_range_stored_percent_discount_is_refused_not_ignored() {
	let db = TmpDb::new("stored-discount-percent");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let draft = invoices
		.draft(
			&ctx,
			&new_draft(None, vec![adhoc(1_000_000, 100_000, Some(Discount::Percent(1000)))]),
		)
		.await
		.unwrap();
	// What a data import or a consumer's own SQL can write, and the column permits.
	sqlx::query("UPDATE invoice_lines SET discount_value = -1 WHERE invoice_id = ?")
		.bind(draft.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	// Any re-price reads the pair back. Both paths refuse it rather than billing the full net.
	let err = invoices
		.add_line(&ctx, draft.uid.as_str(), adhoc(1_000_000, 1_000, None))
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-DISCOUNT");
	let err = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-DISCOUNT");
}

/// Both `discount_value` columns were read as a raw `i64`, bypassing the `bounded` check
/// every other money column goes through. `storno` negates the line's (`.map(|v| -v)`), and
/// negating `i64::MIN` traps under `[profile.release] overflow-checks` — a panic in the storno
/// path rather than a `400`.
#[tokio::test]
async fn an_unbounded_stored_discount_value_is_a_validation_error_not_a_trap() {
	let db = TmpDb::new("stored-discount-bounds");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let issued = invoices
		.issue_now(
			&ctx,
			&new_draft(None, vec![adhoc(1_000_000, 100_000, Some(Discount::Amount(Money(100))))]),
		)
		.await
		.unwrap();
	sqlx::query("UPDATE invoice_lines SET discount_value = ? WHERE invoice_id = ?")
		.bind(i64::MIN)
		.bind(issued.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	// The read itself is the guard, so every caller gets the `400` — including the storno
	// path, whose negation is what used to trap.
	assert_eq!(
		store.invoice_lines(issued.id).await.unwrap_err().parts().0,
		mintworks_core::error::StatusCode::BAD_REQUEST
	);
	let err = invoices.storno(&ctx, issued.uid.as_str(), "test").await.unwrap_err();
	assert_eq!(err.parts().0, mintworks_core::error::StatusCode::BAD_REQUEST);
}

/// Every NAV text rule a draft line carries, at the service handle rather than at the router:
/// the XSD caps `lineDescription` at `SimpleText512NotBlankType` and `unitOfMeasureOwnValue` at
/// 50, `invoice_lines.note` at `MAX_LINE_NOTE`, and a blank `discount_description` is the same
/// refusal as a blank line description — NAV takes neither, and an invoice that fails the XSD
/// after it has a number cannot be corrected.
///
/// The caps count **characters, not bytes**, so every over-long fixture here is non-ASCII.
#[tokio::test]
async fn nav_text_caps_are_enforced_at_the_service_handle() {
	let db = TmpDb::new("nav-text-caps");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let n = |c: char, len: usize| std::iter::repeat_n(c, len).collect::<String>();
	let draft_of = |line: Line| NewDraft { lines: vec![line], ..NewDraft::default() };
	let base = || adhoc(1_000_000, 100_000, None);
	let note = |len: usize| Line { note: Some(n('á', len)), ..base() };
	let discounted = |d: Option<&str>| Line {
		discount: Some(Discount::Percent(1000)),
		discount_description: d.map(str::to_owned),
		..base()
	};

	// (label, the line, the errCode it is refused with — or None when it must be accepted)
	for (label, line, want) in [
		(
			"a description past SimpleText512",
			Line { description: n('á', 513), ..base() },
			Some("E-INV-TOO-LONG"),
		),
		("a description at the cap", Line { description: n('á', 512), ..base() }, None),
		("a unit past 50", Line { unit: n('ó', 51), ..base() }, Some("E-INV-TOO-LONG")),
		("a unit at the cap", Line { unit: n('ó', 50), ..base() }, None),
		(
			"a note past MAX_LINE_NOTE",
			note(mintworks_invoice::store::MAX_LINE_NOTE + 1),
			Some("E-INV-TOO-LONG"),
		),
		("a note at the cap", note(mintworks_invoice::store::MAX_LINE_NOTE), None),
		("a blank discount description", discounted(Some(" ")), Some("E-INV-LINE")),
		// Absent stays legal — the element is optional.
		("no discount description", discounted(None), None),
		("a real discount description", discounted(Some("hűségkedvezmény")), None),
	] {
		let got = invoices.draft(&ctx, &draft_of(line)).await;
		if let Some(code) = want {
			let err = got.expect_err(label).parts();
			assert_eq!(err.1, code, "{label}");
			assert!(err.0.is_client_error(), "{label}: {:?}", err.0);
		} else {
			got.unwrap_or_else(|e| panic!("{label} is filable and must be accepted: {e}"));
		}
	}

	// The PATCH path funnels through the same `draft::price` check.
	let draft = invoices
		.draft(&ctx, &draft_of(discounted(Some("hűségkedvezmény"))))
		.await
		.unwrap();
	let err = invoices
		.edit_line(
			&ctx,
			draft.uid.as_str(),
			1,
			mintworks_invoice::service_api::LinePatch {
				discount_description: Some(Some("  ".to_owned())),
				..Default::default()
			},
		)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-LINE");

	// And `customerName` is `SimpleText512` on the party, by the same rule.
	let err = invoices
		.create_party(
			&ctx,
			&PartyPatch {
				kind: Some(PartyKind::Company),
				name: Some(n('á', 513)),
				country: Some("HU".into()),
				..PartyPatch::default()
			},
		)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-TOO-LONG");
}

/// `003_invoice.sql`'s immutability comment says `notes` is the one column an issued invoice
/// may still change, and no code path anywhere wrote it after issue: every
/// patch routed through `update_draft`, whose `WHERE... AND status = 'DRAFT'` answered
/// `E-INV-NOT-DRAFT`. Everything else on an issued invoice is `E-INV-IMMUTABLE`, which is a
/// different statement — the row is frozen, not on the wrong route.
#[tokio::test]
async fn an_issued_invoice_takes_a_note_and_nothing_else() {
	let db = TmpDb::new("issued-note");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 100_000, None)],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();

	let noted = invoices
		.patch(
			&ctx,
			issued.uid.as_str(),
			&InvoicePatch {
				notes: Patch::Value("Fizetve keszpenzzel.".into()),
				..InvoicePatch::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(noted.notes.as_deref(), Some("Fizetve keszpenzzel."));
	assert_eq!((noted.net, noted.vat, noted.gross), (issued.net, issued.vat, issued.gross));
	assert_eq!(noted.number, issued.number);

	let err = invoices
		.patch(
			&ctx,
			issued.uid.as_str(),
			&InvoicePatch { payment_method: Some(PaymentMethod::Cash), ..InvoicePatch::default() },
		)
		.await
		.expect_err("an issued invoice takes only a note");
	assert_eq!(err.parts().1, "E-INV-IMMUTABLE");
}

/// A locked draft answers `E-INV-LOCKED`, not `E-INV-NOT-DRAFT` or `E-INV-IMMUTABLE`: those two
/// say "wrong route" and "frozen forever", and neither is true here — the edit is legal again
/// the moment the payment settles or dies, and the caller's move is to wait or abandon it.
/// `notes` still goes through, and so does `issue`, which is what the settlement calls.
#[tokio::test]
async fn a_locked_draft_refuses_every_edit_with_its_own_code() {
	let db = TmpDb::new("locked-draft");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 100_000, None)],
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	let uid = draft.uid.to_string();
	assert!(invoices.lock(&ctx, &uid).await.unwrap());
	assert!(!invoices.lock(&ctx, &uid).await.unwrap(), "a compare-and-set, not a setter");

	for (label, err) in [
		(
			"patch",
			invoices
				.patch(
					&ctx,
					&uid,
					&InvoicePatch {
						payment_method: Some(PaymentMethod::Cash),
						..InvoicePatch::default()
					},
				)
				.await
				.unwrap_err(),
		),
		(
			"add_line",
			invoices
				.add_line(&ctx, &uid, adhoc(1_000_000, 100_000, None))
				.await
				.unwrap_err(),
		),
		("remove_line", invoices.remove_line(&ctx, &uid, 1).await.unwrap_err()),
		("delete_draft", invoices.delete_draft(&ctx, &uid).await.unwrap_err()),
	] {
		assert_eq!(err.parts().1, "E-INV-LOCKED", "{label}");
	}

	let noted = invoices
		.patch(
			&ctx,
			&uid,
			&InvoicePatch {
				notes: Patch::Value("Fizetes folyamatban.".into()),
				..Default::default()
			},
		)
		.await
		.unwrap();
	assert_eq!(noted.notes.as_deref(), Some("Fizetes folyamatban."));

	// The payment died: the cart is editable again.
	assert!(invoices.unlock(&ctx, &uid).await.unwrap());
	assert!(invoices.add_line(&ctx, &uid, adhoc(1_000_000, 100_000, None)).await.is_ok());

	// And from locked, the settlement's own path numbers it in one transaction.
	assert!(invoices.lock(&ctx, &uid).await.unwrap());
	let issued = invoices.issue(&ctx, &uid).await.unwrap();
	assert_eq!(issued.status, InvoiceStatus::Issued);
	assert!(issued.number.is_some());
}

/// `update_notes` is deliberately guard-free, so an all-absent `{}` body used to reach it as
/// `NULL` and erase a STORNO row's statutory cancellation reason. Absent leaves it; explicit
/// `null` still clears it.
#[tokio::test]
async fn an_absent_notes_patch_leaves_an_issued_note_alone() {
	let db = TmpDb::new("issued-note-absent");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 100_000, None)],
				notes: Some("Storno indoka.".into()),
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
	assert_eq!(issued.notes.as_deref(), Some("Storno indoka."));

	let untouched = invoices
		.patch(&ctx, issued.uid.as_str(), &InvoicePatch::default())
		.await
		.unwrap();
	assert_eq!(untouched.notes.as_deref(), Some("Storno indoka."));

	let cleared = invoices
		.patch(
			&ctx,
			issued.uid.as_str(),
			&InvoicePatch { notes: Patch::Null, ..InvoicePatch::default() },
		)
		.await
		.unwrap();
	assert_eq!(cleared.notes, None);
}

// ---------------------------------------------------------------- the frozen seller

/// Publish a new seller version onto the same `sellers` row and the PDF of an invoice issued
/// beforehand must not move. This is the gap the whole `seller_versions` table exists for:
/// `RENDER_PDF` runs *after* the issue transaction commits and is retried with backoff, so an
/// edit landing in that window used to rewrite the supplier block of a numbered, immutable
/// document.
#[tokio::test]
async fn a_published_seller_edit_never_moves_an_already_issued_invoice() {
	let db = TmpDb::new("seller-freeze");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let first = invoices
		.issue(&ctx, service_draft(&invoices, &ctx).await.uid.as_str())
		.await
		.unwrap();

	invoices
		.save_seller_draft(
			&ctx,
			&SellerVersionPatch {
				name: Some("Masik Kft.".into()),
				street: Some("Uj utca 9.".into()),
				..Default::default()
			},
		)
		.await
		.unwrap();
	invoices.publish_seller(&ctx).await.unwrap();

	assert_eq!(seller_block(&store, &first).await["name"], "Teszt Kft.");
	assert_eq!(seller_block(&store, &first).await["taxNumber"], "12345678242");
	assert!(
		seller_block(&store, &first).await["address"]
			.as_str()
			.unwrap()
			.contains("Fo utca 1."),
		"the frozen address moved"
	);

	// And an invoice issued afterwards carries the new one — this is a freeze, not a refusal
	// to ever change.
	let second = invoices
		.issue(&ctx, service_draft(&invoices, &ctx).await.uid.as_str())
		.await
		.unwrap();
	assert_ne!(second.seller_ver, first.seller_ver);
	assert_eq!(seller_block(&store, &second).await["name"], "Masik Kft.");
}

/// A *saved but unpublished* draft is not the seller. The whole reason the edit is staged: an
/// invoice issued while an operator is half way through typing an address must freeze the live
/// version, never the draft.
#[tokio::test]
async fn an_unpublished_seller_draft_is_never_frozen_onto_an_invoice() {
	let db = TmpDb::new("seller-draft-not-frozen");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	invoices
		.save_seller_draft(
			&ctx,
			&SellerVersionPatch { name: Some("Felig Beirt Kft.".into()), ..Default::default() },
		)
		.await
		.unwrap();

	let issued = invoices
		.issue(&ctx, service_draft(&invoices, &ctx).await.uid.as_str())
		.await
		.unwrap();
	assert_eq!(issued.seller_ver, Some(SELLER_VER));
	assert_eq!(seller_block(&store, &issued).await["name"], "Teszt Kft.");

	// The draft is still there to be finished.
	let draft = invoices
		.seller_draft(&ctx)
		.await
		.unwrap()
		.expect("the draft survived the issue");
	assert_eq!(draft.name, "Felig Beirt Kft.");
	assert_eq!(draft.status, SellerVersionStatus::Draft);
}

/// `taxrule.rs`'s bug in its mutation-over-time form. An `ALANYI_MENTES` seller issues AAM
/// lines; flipping the scheme to `NORMAL` afterwards must leave that invoice exempt in the
/// filing too, or the PDF says 0% while NAV is told 27% — on a document neither can correct.
#[tokio::test]
async fn flipping_the_vat_scheme_after_issue_leaves_the_invoice_exempt() {
	let db = TmpDb::new("seller-scheme-flip");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	invoices
		.save_seller_draft(
			&ctx,
			&SellerVersionPatch { vat_scheme: Some("ALANYI_MENTES".into()), ..Default::default() },
		)
		.await
		.unwrap();
	invoices.publish_seller(&ctx).await.unwrap();

	let issued = invoices
		.issue(&ctx, service_draft(&invoices, &ctx).await.uid.as_str())
		.await
		.unwrap();
	let groups = store.invoice_vat_groups(issued.id).await.unwrap();
	assert_eq!(groups[0].vat_code, VatCode::Aam, "an exempt seller issues AAM");

	invoices
		.save_seller_draft(
			&ctx,
			&SellerVersionPatch { vat_scheme: Some("NORMAL".into()), ..Default::default() },
		)
		.await
		.unwrap();
	invoices.publish_seller(&ctx).await.unwrap();

	let frozen = store.seller_version(issued.seller_ver.unwrap()).await.unwrap().unwrap();
	assert_eq!(
		frozen.vat_scheme, "ALANYI_MENTES",
		"the filing would have declared a taxable supply against AAM lines"
	);
}

/// A cancellation carries the supplier data of the invoice it cancels, the same way it carries
/// its `series_code` — not whatever the seller looks like on the day of the storno.
#[tokio::test]
async fn a_storno_carries_the_originals_seller_version() {
	let db = TmpDb::new("seller-storno");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let issued = invoices
		.issue(&ctx, service_draft(&invoices, &ctx).await.uid.as_str())
		.await
		.unwrap();

	invoices
		.save_seller_draft(
			&ctx,
			&SellerVersionPatch { name: Some("Azota Mas Kft.".into()), ..Default::default() },
		)
		.await
		.unwrap();
	let new_ver = invoices.publish_seller(&ctx).await.unwrap().seller_ver;

	let storno = invoices.storno(&ctx, issued.uid.as_str(), "teves szamlazas").await.unwrap();
	assert_eq!(storno.seller_ver, issued.seller_ver);
	assert_ne!(storno.seller_ver, Some(new_ver));
	assert_eq!(seller_block(&store, &storno).await["name"], "Teszt Kft.");
}

/// The `SellerView` the operator edits through, end to end — and that `GET /api/seller` keeps
/// answering the live version while a draft is open.
#[tokio::test]
async fn the_seller_draft_lifecycle_is_visible_through_the_handle() {
	let db = TmpDb::new("seller-lifecycle");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	assert!(invoices.seller_draft(&ctx).await.unwrap().is_none());
	// Nothing to publish is the caller's error, not a silent no-op that leaves no live version.
	let err = invoices.publish_seller(&ctx).await.unwrap_err();
	assert!(format!("{err:?}").contains("E-INV-SELLER-NO-DRAFT"), "{err:?}");

	invoices
		.save_seller_draft(
			&ctx,
			&SellerVersionPatch { city: Some("Debrecen".into()), ..Default::default() },
		)
		.await
		.unwrap();
	assert_eq!(invoices.seller(&ctx).await.unwrap().city, "Budapest", "the draft went live early");

	invoices.discard_seller_draft(&ctx).await.unwrap();
	assert!(invoices.seller_draft(&ctx).await.unwrap().is_none());
	assert_eq!(invoices.seller(&ctx).await.unwrap().city, "Budapest");
	assert_eq!(invoices.seller_history(&ctx).await.unwrap().len(), 1, "a discard made history");

	// A malformed value is refused on the way *in*, not at filing time, when the invoice it
	// would reach is already numbered and immutable.
	let err = invoices
		.save_seller_draft(
			&ctx,
			&SellerVersionPatch { tax_number: Some("12345678042".into()), ..Default::default() },
		)
		.await
		.unwrap_err();
	assert!(format!("{err:?}").contains("E-INV-SELLER-TAXNUMBER"), "{err:?}");
}

/// A config- or env-driven seed calls this on every boot, so it must publish only what
/// actually moved — otherwise one identical version stacks up per restart.
#[tokio::test]
async fn sync_seller_publishes_only_on_a_real_change() {
	let db = TmpDb::new("seller-sync");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let versions = async || invoices.seller_history(&ctx).await.unwrap().len();
	let before = versions().await;

	assert!(invoices.sync_seller(&ctx, &seller_version()).await.unwrap().is_none());
	assert_eq!(versions().await, before, "an unchanged seed stacked a version");

	let moved = SellerVersionPatch { city: Some("Debrecen".into()), ..seller_version() };
	assert!(invoices.sync_seller(&ctx, &moved).await.unwrap().is_some());
	assert_eq!(versions().await, before + 1);
	assert!(invoices.sync_seller(&ctx, &moved).await.unwrap().is_none());
	assert_eq!(versions().await, before + 1);

	// Compared *after* `checked_seller_version` normalises: on the raw patch a lower-case
	// country differs from the stored `HU` and republishes on every boot.
	let cased = SellerVersionPatch { country: Some("hu".into()), ..moved.clone() };
	assert!(invoices.sync_seller(&ctx, &cased).await.unwrap().is_none());
	assert_eq!(versions().await, before + 1);

	invoices.save_seller_draft(&ctx, &seller_version()).await.unwrap();
	let err = invoices.sync_seller(&ctx, &seller_version()).await.unwrap_err();
	assert!(format!("{err:?}").contains("E-INV-SELLER-DRAFT-OPEN"), "{err:?}");
	assert_eq!(versions().await, before + 1, "the sync published over an open draft");
}

/// The completeness check used to run on a draft read in an earlier statement, so a
/// `save_seller_draft` landing between the read and the promotion could make a blank version
/// `CURRENT` — and an invoice freezing it fails NAV's XSD when it is already immutable. The
/// `UPDATE` below is that concurrent write, played at the one moment the check has to cover.
#[tokio::test]
async fn a_draft_rewritten_blank_under_the_publish_is_still_refused() {
	let db = TmpDb::new("seller-publish-race");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	invoices
		.save_seller_draft(
			&ctx,
			&SellerVersionPatch { city: Some("Debrecen".into()), ..Default::default() },
		)
		.await
		.unwrap();
	sqlx::query("UPDATE seller_versions SET name = '' WHERE status = 'DRAFT'")
		.execute(store.write_pool())
		.await
		.unwrap();

	let err = invoices.publish_seller(&ctx).await.unwrap_err();
	assert!(format!("{err:?}").contains("E-INV-SELLER-INCOMPLETE"), "{err:?}");

	let current = store.current_seller_version(SELLER).await.unwrap().unwrap();
	assert_eq!(current.seller_ver, SELLER_VER, "the live version was archived anyway");
	assert_eq!(current.name, "Teszt Kft.");
}

/// The JSON `pdf::document` builds for one invoice's seller block, resolved through the
/// version the invoice froze — which is what `pdf::run` does.
async fn seller_block(store: &SqliteStore, invoice: &Invoice) -> serde_json::Value {
	let version = store.seller_version(invoice.seller_ver.unwrap()).await.unwrap().unwrap();
	let lines = store.invoice_lines(invoice.id).await.unwrap();
	let groups = store.invoice_vat_groups(invoice.id).await.unwrap();
	let data =
		mintworks_invoice::pdf::document(&version, invoice, &lines, &groups, None, None).unwrap();
	serde_json::from_str::<serde_json::Value>(&data).unwrap()["seller"].clone()
}

/// `issue::plan` passed a literal `100` as the group-VAT step whenever the invoice was in the
/// base currency, while the draft path passes the `currencies` row's own `price_round_step`.
/// That column is operator-writable, so a HUF row edited off 100 re-priced the invoice between
/// draft and issue — the same class of bug as re-pricing the lines at ISSUE, which billed the
/// customer more than the draft showed.
#[tokio::test]
async fn issuing_a_huf_invoice_keeps_the_draft_vat_round_step() {
	let db = TmpDb::new("issue-step");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	sqlx::query("UPDATE currencies SET price_round_step = 1 WHERE code = 'HUF'")
		.execute(store.write_pool())
		.await
		.unwrap();

	// 370.00 Ft at 27% is 99.90 Ft — off a whole forint, so the two steps disagree.
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft { lines: vec![adhoc(1_000_000, 37_000, None)], ..NewDraft::default() },
		)
		.await
		.unwrap();
	assert_eq!(draft.vat, Money(9_990));

	let issued = invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();
	assert_eq!(issued.vat, draft.vat, "the step must not change between draft and issue");
	assert_eq!(issued.gross, draft.gross);
}

/// `sellers` is scoped to an org, so an org whose whole chain up to the root owns none is not
/// a seller and cannot invoice. `NotFound`, not `403`: there is no seller to authorize against.
#[tokio::test]
async fn an_org_with_no_seller_up_the_chain_cannot_draft() {
	let db = TmpDb::new("no-seller-anywhere");
	let (_app, invoices, store) = service(&db).await;
	let other = second_org(&store).await;

	let err = invoices
		.draft(&Ctx::system("test").with_org(other), &plain_draft())
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-NOTFOUND");
}

/// The ERP business-unit case and the portal-customer case are the same case: a child org with
/// no `sellers` row of its own bills through its nearest seller-owning ancestor, and the gate
/// is the role on *that ancestor*. Role inheritance runs downward only, so the child's own
/// admin is a customer here, not a seller — without this it could issue in the seller's name.
#[tokio::test]
async fn a_child_org_bills_through_its_ancestors_seller() {
	use mintworks_core::ctx::Actor;

	let db = TmpDb::new("child-seller");
	let (_app, invoices, store) = service(&db).await;

	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (3, 'org_child', ?, 'SHARED', 'Egyseg', 1, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (2, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0C', 3, 'C', 'Vevo Kft.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 1, 0, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	// Account 2 administers the seller's org; account 3 only the child.
	for (account, org) in [(2i64, ORG), (3, 3)] {
		sqlx::query("INSERT INTO accounts (id, uid, email, created_at) VALUES (?, ?, ?, 0)")
			.bind(account)
			.bind(format!("acc_{account}"))
			.bind(format!("a{account}@e.st"))
			.execute(store.write_pool())
			.await
			.unwrap();
		sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES (?, ?, 'ADMIN', 0, 0)",
		)
		.bind(org)
		.bind(account)
		.execute(store.write_pool())
		.await
		.unwrap();
	}

	let ctx_for = |account_id| Ctx {
		actor: Actor::User { account_id },
		auth_at: Some(Timestamp::now().0),
		..Ctx::system("test").with_org(3)
	};

	let draft = invoices.draft(&ctx_for(2), &plain_draft()).await.unwrap();
	assert_eq!(
		invoices.issue(&ctx_for(2), draft.uid.as_str()).await.unwrap().status,
		InvoiceStatus::Issued
	);
	assert_eq!(
		invoices.draft(&ctx_for(3), &plain_draft()).await.unwrap_err().parts().1,
		"E-AUTH-FORBIDDEN"
	);
}

/// `accounts.is_operator` is gone. An operator is an `ADMIN` on the root org, and the same
/// ancestor walk that reaches every org reaches the seller — no special case anywhere.
#[tokio::test]
async fn a_root_org_admin_issues_by_inheritance() {
	use mintworks_core::ctx::Actor;

	let db = TmpDb::new("root-admin-issues");
	let (_app, invoices, store) = service(&db).await;
	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (2, 'acc_op', 'op@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES ((SELECT id FROM orgs WHERE kind = 'ROOT'), 2, 'ADMIN', 0, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	let ctx = Ctx {
		actor: Actor::User { account_id: 2 },
		auth_at: Some(Timestamp::now().0),
		..Ctx::system("test").with_org(ORG)
	};
	let draft = invoices.draft(&ctx, &plain_draft()).await.unwrap();
	assert_eq!(
		invoices.issue(&ctx, draft.uid.as_str()).await.unwrap().status,
		InvoiceStatus::Issued
	);
}

/// The seller-org role gate (`require_seller_role`) with real actors. Most of this suite drives
/// every mutating method as `Actor::System`, which `require_role_on` grants unconditionally, so
/// the gate itself had no coverage — a shipped example's checkout reached `E-AUTH-FORBIDDEN`.
#[tokio::test]
async fn the_seller_org_gate_refuses_a_member_and_admits_an_admin() {
	use mintworks_core::ctx::Actor;

	let db = TmpDb::new("seller-gate");
	let (_app, invoices, store) = service(&db).await;
	// 2 is a `MEMBER` of the seller's org and 3 its `ADMIN`: the same org, so the only
	// difference between the two actors is the role.
	for (id, role) in [(2i64, "MEMBER"), (3, "ADMIN")] {
		sqlx::query("INSERT INTO accounts (id, uid, email, created_at) VALUES (?, ?, ?, 0)")
			.bind(id)
			.bind(format!("acc_{id}"))
			.bind(format!("a{id}@e.st"))
			.execute(store.write_pool())
			.await
			.unwrap();
		sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES (?, ?, ?, 0, 0)",
		)
		.bind(ORG)
		.bind(id)
		.bind(role)
		.execute(store.write_pool())
		.await
		.unwrap();
	}
	let as_user = |account_id| Ctx {
		actor: Actor::User { account_id },
		auth_at: Some(Timestamp::now().0),
		..Ctx::system("test").with_org(ORG)
	};
	let member = as_user(2);
	let admin = as_user(3);
	let sys = Ctx::system("test").with_org(ORG);
	let forbidden = |e: Error| assert_eq!(e.parts().1, "E-AUTH-FORBIDDEN", "{e:?}");

	// The three methods that need no existing row.
	forbidden(invoices.draft(&member, &plain_draft()).await.unwrap_err());
	forbidden(invoices.sync_services(&member, &[]).await.unwrap_err());
	forbidden(invoices.list_currencies(&member, true).await.unwrap_err());
	invoices.draft(&admin, &plain_draft()).await.unwrap();
	invoices.sync_services(&admin, &[]).await.unwrap();
	invoices.list_currencies(&admin, true).await.unwrap();

	// The rest, on a fresh draft each so a successful admin edit cannot mask the next refusal.
	let d = invoices.draft(&sys, &plain_draft()).await.unwrap();
	forbidden(
		invoices
			.add_line(&member, d.uid.as_str(), adhoc(1_000_000, 100_000, None))
			.await
			.unwrap_err(),
	);
	invoices
		.add_line(&admin, d.uid.as_str(), adhoc(1_000_000, 100_000, None))
		.await
		.unwrap();

	let patch = InvoicePatch { notes: Patch::Value("patched".into()), ..Default::default() };
	let d = invoices.draft(&sys, &plain_draft()).await.unwrap();
	forbidden(invoices.patch(&member, d.uid.as_str(), &patch).await.unwrap_err());
	invoices.patch(&admin, d.uid.as_str(), &patch).await.unwrap();

	let d = invoices.draft(&sys, &plain_draft()).await.unwrap();
	forbidden(invoices.delete_draft(&member, d.uid.as_str()).await.unwrap_err());
	invoices.delete_draft(&admin, d.uid.as_str()).await.unwrap();

	let d = invoices.draft(&sys, &plain_draft()).await.unwrap();
	forbidden(invoices.issue(&member, d.uid.as_str()).await.unwrap_err());
	let issued = invoices.issue(&admin, d.uid.as_str()).await.unwrap();

	let d = invoices.draft(&sys, &plain_draft()).await.unwrap();
	let cancelled = invoices.issue(&sys, d.uid.as_str()).await.unwrap();
	assert_eq!(issued.status, InvoiceStatus::Issued, "the admin's issue went through");
	forbidden(invoices.storno(&member, cancelled.uid.as_str(), "hiba").await.unwrap_err());
	invoices.storno(&admin, cancelled.uid.as_str(), "hiba").await.unwrap();

	// The reads a `MEMBER` legitimately holds.
	invoices.seller(&member).await.unwrap();
	invoices.list_services(&member, true).await.unwrap();
	invoices.list_currencies(&member, false).await.unwrap();
}

/// A member of a *customer* org — one with no relation to the seller's — is refused every
/// mutation of their own org's invoice, not only `draft`: the gate is the seller's org.
#[tokio::test]
async fn a_customer_org_member_cannot_mutate_its_own_invoice() {
	use mintworks_core::ctx::Actor;

	let db = TmpDb::new("customer-org-gate");
	let (_app, invoices, store) = service(&db).await;
	// A customer org under the seller's: the seller resolves up the chain, the member holds no
	// role on it — exactly the shipped example's layout.
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (3, 'org_cust', ?, 'SHARED', 'Vevo', 1, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	let customer = 3;
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (2, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0C', ?, 'C', 'Vevo Kft.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 1, 0, 0)",
	)
	.bind(customer)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (2, 'acc_c', 'c@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 2, 'MEMBER', 0, 0)",
	)
	.bind(customer)
	.execute(store.write_pool())
	.await
	.unwrap();

	let member = Ctx {
		actor: Actor::User { account_id: 2 },
		auth_at: Some(Timestamp::now().0),
		..Ctx::system("test").with_org(customer)
	};
	let sys = Ctx::system("test").with_org(customer);
	let forbidden = |e: Error| assert_eq!(e.parts().1, "E-AUTH-FORBIDDEN", "{e:?}");

	forbidden(invoices.draft(&member, &plain_draft()).await.unwrap_err());
	let patch = InvoicePatch { notes: Patch::Value("patched".into()), ..Default::default() };
	let d = invoices.draft(&sys, &plain_draft()).await.unwrap();
	forbidden(invoices.patch(&member, d.uid.as_str(), &patch).await.unwrap_err());
	forbidden(invoices.issue(&member, d.uid.as_str()).await.unwrap_err());
	forbidden(invoices.delete_draft(&member, d.uid.as_str()).await.unwrap_err());
	let issued = invoices.issue(&sys, d.uid.as_str()).await.unwrap();
	forbidden(invoices.storno(&member, issued.uid.as_str(), "hiba").await.unwrap_err());

	// `lock`/`unlock` are the seller's too, though `ctx.org()` scopes the invoice to the buyer.
	let locked = invoices.draft(&sys, &plain_draft()).await.unwrap();
	forbidden(invoices.lock(&member, locked.uid.as_str()).await.unwrap_err());
	assert!(invoices.lock(&sys, locked.uid.as_str()).await.unwrap());
}

/// The ancestor walk filtered `status` on its starting org only, so suspending an org blocked
/// its own routes while the authority it delegated to its children stayed intact.
#[tokio::test]
async fn suspending_an_org_withdraws_the_authority_it_delegates() {
	use mintworks_core::ctx::Actor;

	let db = TmpDb::new("suspended-delegation");
	let (_app, invoices, store) = service(&db).await;
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (3, 'org_child3', ?, 'SHARED', 'Egyseg', 1, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	// The child's own seller, so resolution never needs the suspended ancestor.
	store
		.put_seller(&Seller {
			id: 2,
			uid: SellerId::generate(),
			org_id: 3,
			nav_base_url: String::new(),
			nav_login: None,
			series_code: "B".into(),
			closed_at: None,
			payment_days: None,
			created_at: Timestamp::now(),
		})
		.await
		.unwrap();
	// 2 administers the parent only; 3 administers the child directly.
	for (account, org) in [(2i64, ORG), (3, 3)] {
		sqlx::query("INSERT INTO accounts (id, uid, email, created_at) VALUES (?, ?, ?, 0)")
			.bind(account)
			.bind(format!("acc_{account}"))
			.bind(format!("a{account}@e.st"))
			.execute(store.write_pool())
			.await
			.unwrap();
		sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES (?, ?, 'ADMIN', 0, 0)",
		)
		.bind(org)
		.bind(account)
		.execute(store.write_pool())
		.await
		.unwrap();
	}
	let as_user = |account_id| Ctx {
		actor: Actor::User { account_id },
		auth_at: Some(Timestamp::now().0),
		..Ctx::system("test").with_org(3)
	};

	// Both hold it while the ancestor is active: 3 directly, 2 by inheritance.
	invoices.sync_services(&as_user(3), &[]).await.unwrap();
	invoices.sync_services(&as_user(2), &[]).await.unwrap();

	sqlx::query("UPDATE orgs SET status = 'SUSPENDED' WHERE id = ?")
		.bind(ORG)
		.execute(store.write_pool())
		.await
		.unwrap();

	// The direct membership survives; the inherited one does not.
	invoices.sync_services(&as_user(3), &[]).await.unwrap();
	assert_eq!(
		invoices.sync_services(&as_user(2), &[]).await.unwrap_err().parts().1,
		"E-AUTH-FORBIDDEN"
	);
}

/// The one-line draft the tests above share.
fn plain_draft() -> NewDraft {
	NewDraft {
		billing_party: Party::OrgDefault,
		lines: vec![adhoc(1_000_000, 100_000, None)],
		..NewDraft::default()
	}
}

/// The clamp is observable: unclamped, `months = 0` would put `from_month` a month ahead of
/// today and hide the current month, and a huge window would overflow the month arithmetic.
#[tokio::test]
async fn summary_clamps_months() {
	let db = TmpDb::new("summary-clamp");
	let (_app, invoices, _store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);

	let today = mintworks_invoice::date_of(Timestamp::now()).unwrap();
	let draft = invoices
		.draft(
			&ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![adhoc(1_000_000, 100_000, None)],
				fulfilment_date: Some(today.clone()),
				..NewDraft::default()
			},
		)
		.await
		.unwrap();
	invoices.issue(&ctx, draft.uid.as_str()).await.unwrap();

	let one = invoices.summary(&ctx, 0).await.unwrap();
	assert_eq!(one.months.len(), 1);
	assert_eq!(one.months[0].month, &today[..7], "months = 0 clamps up to the current month");
	assert_eq!(invoices.summary(&ctx, 10_000).await.unwrap().months.len(), 1);
}

#[tokio::test]
async fn summary_is_org_scoped() {
	let db = TmpDb::new("summary-org");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);
	service_draft(&invoices, &ctx).await;

	assert_eq!(invoices.summary(&ctx, 12).await.unwrap().statuses.len(), 1);
	let other = Ctx::system("test").with_org(second_org(&store).await);
	assert!(invoices.summary(&other, 12).await.unwrap().statuses.is_empty());
}

/// A status name is request text, so an unknown one is a 400 -- `str_enum!`'s `FromStr` answers
/// `Internal`, which is right for a column value and wrong here.
#[tokio::test]
async fn list_full_rejects_unknown_status() {
	let err = InvoiceFilter::parse(Some("DRAFT,NOPE"), None).unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-VALIDATION");

	let f = InvoiceFilter::parse(Some(" DRAFT , ISSUED ,"), Some("   ")).unwrap();
	assert_eq!(f.statuses, vec![InvoiceStatus::Draft, InvoiceStatus::Issued]);
	assert!(f.q.is_none(), "blank is absent");
}

/// A member of the seller's own org, holding no admin role there.
async fn seller_member(store: &SqliteStore) -> Ctx {
	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (2, 'acc_m', 'm@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 2, 'MEMBER', 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	Ctx {
		actor: mintworks_core::ctx::Actor::User { account_id: 2 },
		auth_at: Some(Timestamp::now().0),
		..Ctx::system("test").with_org(ORG)
	}
}

/// Money marked received by hand is a bank transfer, declared by the seller's admin. A card
/// invoice is paid by the gateway's settle alone.
#[tokio::test]
async fn marking_paid_by_hand_is_transfer_only_and_the_seller_admins() {
	let db = TmpDb::new("mark-paid-gate");
	let (_app, invoices, store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);

	let card = invoices.draft(&sys, &plain_draft()).await.unwrap();
	assert!(invoices.begin_card_payment(&sys, card.uid.as_str()).await.unwrap());
	let card = invoices.issue(&sys, card.uid.as_str()).await.unwrap();
	assert_eq!(card.payment_method, PaymentMethod::Card);
	for err in [
		invoices.mark_paid(&sys, card.uid.as_str()).await.unwrap_err(),
		invoices.set_paid(&sys, card.uid.as_str(), card.gross, None).await.unwrap_err(),
	] {
		assert_eq!(err.parts().1, "E-INV-NOT-TRANSFER", "{err:?}");
	}

	let transfer = invoices.draft(&sys, &plain_draft()).await.unwrap();
	let transfer = invoices.issue(&sys, transfer.uid.as_str()).await.unwrap();
	let member = seller_member(&store).await;
	for err in [
		invoices.mark_paid(&member, transfer.uid.as_str()).await.unwrap_err(),
		invoices
			.set_paid(&member, transfer.uid.as_str(), transfer.gross, None)
			.await
			.unwrap_err(),
	] {
		assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN", "{err:?}");
	}
	let paid = invoices.mark_paid(&sys, transfer.uid.as_str()).await.unwrap();
	assert_eq!(paid.status, InvoiceStatus::Paid);
}

/// A caller can choose TRANSFER or CASH; CARD is written by the payment start alone, and an
/// unlocked CARD invoice does not issue.
#[tokio::test]
async fn a_caller_cannot_set_or_issue_an_unpaid_card_invoice() {
	let db = TmpDb::new("card-reserved");
	let (_app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let with = |m| NewDraft { payment_method: Some(m), ..plain_draft() };
	let code = |e: Error| e.parts().1;

	assert_eq!(
		code(invoices.draft(&sys, &with(PaymentMethod::Card)).await.unwrap_err()),
		"E-INV-METHOD-RESERVED"
	);
	assert_eq!(
		code(invoices.draft(&sys, &with(PaymentMethod::Other)).await.unwrap_err()),
		"E-INV-METHOD-UNSUPPORTED"
	);
	let cash = invoices.draft(&sys, &with(PaymentMethod::Cash)).await.unwrap();
	assert_eq!(cash.payment_method, PaymentMethod::Cash);

	let d = invoices.draft(&sys, &plain_draft()).await.unwrap();
	let uid = d.uid.to_string();
	let set = |m| InvoicePatch { payment_method: Some(m), ..Default::default() };
	assert_eq!(
		code(invoices.patch(&sys, &uid, &set(PaymentMethod::Card)).await.unwrap_err()),
		"E-INV-METHOD-RESERVED"
	);
	assert_eq!(
		code(invoices.patch(&sys, &uid, &set(PaymentMethod::Other)).await.unwrap_err()),
		"E-INV-METHOD-UNSUPPORTED"
	);

	// A dead payment leaves an unlocked CARD draft: it does not issue, and moving it to
	// TRANSFER does.
	assert!(invoices.begin_card_payment(&sys, &uid).await.unwrap());
	assert!(invoices.unlock(&sys, &uid).await.unwrap());
	assert_eq!(code(invoices.issue(&sys, &uid).await.unwrap_err()), "E-INV-CARD-UNPAID");
	let moved = invoices.patch(&sys, &uid, &set(PaymentMethod::Transfer)).await.unwrap();
	assert_eq!(moved.payment_method, PaymentMethod::Transfer);
	assert_eq!(invoices.issue(&sys, &uid).await.unwrap().status, InvoiceStatus::Issued);
	assert!(!invoices.begin_card_payment(&sys, &uid).await.unwrap(), "an issued invoice is no-op");
}

#[tokio::test]
async fn card_payment_refuses_a_draft_dated_off_today() {
	use mintworks_invoice::numbering::{add_days, date_of};
	let db = TmpDb::new("card-dates");
	let (_app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let today = date_of(Timestamp::now()).unwrap();
	let later = add_days(&today, 10).unwrap();

	for d in [
		NewDraft { fulfilment_date: Some(later.clone()), ..plain_draft() },
		NewDraft { due_date: Some(later.clone()), ..plain_draft() },
	] {
		let uid = invoices.draft(&sys, &d).await.unwrap().uid.to_string();
		let e = invoices.begin_card_payment(&sys, &uid).await.unwrap_err();
		assert_eq!(e.parts().1, "E-INV-CARD-DATES");
	}
	let same = NewDraft { due_date: Some(today), ..plain_draft() };
	let uid = invoices.draft(&sys, &same).await.unwrap().uid.to_string();
	assert!(invoices.begin_card_payment(&sys, &uid).await.unwrap());
}

#[tokio::test]
async fn an_issued_card_invoice_is_fulfilled_and_due_on_its_issue_date() {
	use mintworks_invoice::numbering::date_of;
	let db = TmpDb::new("card-issue-dates");
	let (_app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let uid = invoices.draft(&sys, &plain_draft()).await.unwrap().uid.to_string();
	assert!(invoices.begin_card_payment(&sys, &uid).await.unwrap());
	let issued = invoices.issue(&sys, &uid).await.unwrap();
	let today = date_of(Timestamp::now()).unwrap();
	assert_eq!(issued.fulfilment_date.as_deref(), Some(today.as_str()));
	assert_eq!(issued.due_date.as_deref(), Some(today.as_str()), "no default payment days");
}

/// The default party of `service`'s fixture.
const DEFAULT_PARTY: &str = "prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B";

#[tokio::test]
async fn a_cash_invoice_is_dated_today_and_paid_on_issue() {
	use mintworks_invoice::numbering::{add_days, date_of};
	let db = TmpDb::new("cash-issue");
	let (_app, invoices, store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let today = date_of(Timestamp::now()).unwrap();
	let later = add_days(&today, 10).unwrap();
	let cash = |d: NewDraft| NewDraft { payment_method: Some(PaymentMethod::Cash), ..d };

	let e = invoices
		.draft(&sys, &cash(NewDraft { due_date: Some(later.clone()), ..plain_draft() }))
		.await
		.unwrap_err();
	assert_eq!(e.parts().1, "E-INV-CASH-DATES");
	let dated = invoices
		.draft(&sys, &NewDraft { fulfilment_date: Some(later), ..plain_draft() })
		.await
		.unwrap();
	let to_cash = InvoicePatch { payment_method: Some(PaymentMethod::Cash), ..Default::default() };
	let e = invoices.patch(&sys, dated.uid.as_str(), &to_cash).await.unwrap_err();
	assert_eq!(e.parts().1, "E-INV-CASH-DATES", "the stored date conflicts too");

	// 100 Ft + 27% = 127 Ft: the debt stays exact, only the payable rounds to 125.
	let d = NewDraft { lines: vec![adhoc(1_000_000, 10_000, None)], ..plain_draft() };
	let uid = invoices.draft(&sys, &cash(d)).await.unwrap().uid.to_string();
	let issued = invoices.issue(&sys, &uid).await.unwrap();
	assert_eq!(issued.status, InvoiceStatus::Paid);
	assert_eq!(issued.gross, Money(12_700));
	assert_eq!(issued.paid_amount, issued.gross);
	assert_eq!(issued.paid_at, issued.issued_at);
	assert_eq!(issued.fulfilment_date.as_deref(), Some(today.as_str()));
	assert_eq!(issued.due_date.as_deref(), Some(today.as_str()));

	let seller = store.current_seller_version(SELLER).await.unwrap().unwrap();
	let lines = store.invoice_lines(issued.id).await.unwrap();
	let groups = store.invoice_vat_groups(issued.id).await.unwrap();
	// The step is the HUF row's seeded `cash_round_step`, not a constant in the code.
	let step = store.currency_get("HUF").await.unwrap().unwrap().cash_round_step;
	assert_eq!(step, Some(500));
	let data =
		mintworks_invoice::pdf::document(&seller, &issued, &lines, &groups, None, step).unwrap();
	let doc: serde_json::Value = serde_json::from_str(&data).unwrap();
	assert_eq!(doc["totals"]["payable"], "125,00", "{data}");
	assert!(mintworks_invoice::pdf::render(&data).unwrap().starts_with(b"%PDF"));
}

#[tokio::test]
async fn the_due_date_defaults_to_party_then_seller_then_setting() {
	use mintworks_invoice::numbering::{add_days, date_of};
	let db = TmpDb::new("due-precedence");
	let (app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let today = date_of(Timestamp::now()).unwrap();
	let due = async || {
		let uid = invoices.draft(&sys, &plain_draft()).await.unwrap().uid.to_string();
		invoices.issue(&sys, &uid).await.unwrap().due_date.unwrap()
	};

	app.settings.set("invoice.default_payment_days", "3", None).await.unwrap();
	assert_eq!(due().await, add_days(&today, 3).unwrap(), "the setting");
	let view = invoices.set_seller_payment_days(&sys, Some(15)).await.unwrap();
	assert_eq!((view.payment_days, view.default_payment_days), (Some(15), 3));
	assert_eq!(due().await, add_days(&today, 15).unwrap(), "the seller's");
	let terms = PartyPatch { payment_days: Patch::Value(30), ..Default::default() };
	invoices.update_party(&sys, DEFAULT_PARTY, &terms).await.unwrap();
	assert_eq!(due().await, add_days(&today, 30).unwrap(), "the party's");
}

#[tokio::test]
async fn a_party_payment_method_is_the_draft_default_and_card_is_refused() {
	let db = TmpDb::new("party-method");
	let (_app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let method = |m| PartyPatch { payment_method: Patch::Value(m), ..Default::default() };

	let e = invoices.update_party(&sys, DEFAULT_PARTY, &method(PaymentMethod::Card)).await;
	assert_eq!(e.unwrap_err().parts().1, "E-INV-METHOD-RESERVED");
	let days = PartyPatch { payment_days: Patch::Value(36_501), ..Default::default() };
	let e = invoices.update_party(&sys, DEFAULT_PARTY, &days).await;
	assert_eq!(e.unwrap_err().parts().1, "E-INV-PAYMENT-DAYS");

	let party = invoices
		.update_party(&sys, DEFAULT_PARTY, &method(PaymentMethod::Cash))
		.await
		.unwrap();
	assert_eq!(party.payment_method, Some(PaymentMethod::Cash));
	let draft = invoices.draft(&sys, &plain_draft()).await.unwrap();
	assert_eq!(draft.payment_method, PaymentMethod::Cash);
	let explicit = NewDraft { payment_method: Some(PaymentMethod::Transfer), ..plain_draft() };
	assert_eq!(
		invoices.draft(&sys, &explicit).await.unwrap().payment_method,
		PaymentMethod::Transfer
	);
}

#[tokio::test]
async fn seller_payment_days_need_admin_and_a_bounded_value() {
	let db = TmpDb::new("seller-days");
	let (_app, invoices, store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let member = seller_member(&store).await;

	let e = invoices.set_seller_payment_days(&member, Some(10)).await.unwrap_err();
	assert_eq!(e.parts().1, "E-AUTH-FORBIDDEN");
	for bad in [-1, 36_501] {
		let e = invoices.set_seller_payment_days(&sys, Some(bad)).await.unwrap_err();
		assert_eq!(e.parts().1, "E-INV-PAYMENT-DAYS");
	}
	assert_eq!(
		invoices.set_seller_payment_days(&sys, Some(0)).await.unwrap().payment_days,
		Some(0)
	);
	assert_eq!(invoices.set_seller_payment_days(&sys, None).await.unwrap().payment_days, None);
	assert_eq!(invoices.seller(&sys).await.unwrap().payment_days, None);
}

#[tokio::test]
async fn a_due_date_before_the_fulfilment_date_is_accepted() {
	use mintworks_invoice::numbering::{add_days, date_of};
	let db = TmpDb::new("due-before-fulfilment");
	let (_app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let f = date_of(Timestamp::now()).unwrap();
	let before = add_days(&f, -5).unwrap();
	let d = NewDraft { fulfilment_date: Some(f), due_date: Some(before.clone()), ..plain_draft() };
	let uid = invoices.draft(&sys, &d).await.unwrap().uid.to_string();
	let issued = invoices.issue(&sys, &uid).await.unwrap();
	assert_eq!(issued.due_date, Some(before));
}

fn period_draft(start: &str, end: &str) -> NewDraft {
	NewDraft {
		period_start: Some(start.to_owned()),
		period_end: Some(end.to_owned()),
		..plain_draft()
	}
}

#[tokio::test]
async fn a_draft_period_is_refused_incomplete_reversed_too_long_or_with_a_fulfilment_date() {
	let db = TmpDb::new("period-rules");
	let (_app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let half = NewDraft { period_end: None, ..period_draft("2026-01-01", "2026-01-31") };
	let with_f = NewDraft {
		fulfilment_date: Some(mintworks_invoice::date_of(Timestamp::now()).unwrap()),
		..period_draft("2026-01-01", "2026-01-31")
	};
	for (d, code) in [
		(half, "E-INV-PERIOD-INCOMPLETE"),
		(period_draft("2026-02-01", "2026-01-31"), "E-CORE-VALIDATION"),
		(period_draft("2026-01-01", "2027-01-01"), "E-INV-PERIOD-TOO-LONG"),
		(with_f, "E-INV-PERIOD-FULFILMENT"),
	] {
		assert_eq!(invoices.draft(&sys, &d).await.unwrap_err().parts().1, code);
	}
}

#[tokio::test]
async fn a_patch_is_period_checked_against_the_stored_draft() {
	let db = TmpDb::new("period-patch");
	let (_app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let today = mintworks_invoice::date_of(Timestamp::now()).unwrap();
	let d = NewDraft { fulfilment_date: Some(today), ..plain_draft() };
	let uid = invoices.draft(&sys, &d).await.unwrap().uid.to_string();
	let period = InvoicePatch {
		period_start: Patch::Value("2026-01-01".to_owned()),
		period_end: Patch::Value("2026-01-31".to_owned()),
		..Default::default()
	};
	let e = invoices.patch(&sys, &uid, &period).await.unwrap_err();
	assert_eq!(e.parts().1, "E-INV-PERIOD-FULFILMENT");
	let cleared = InvoicePatch { fulfilment_date: Patch::Null, ..period.clone() };
	let patched = invoices.patch(&sys, &uid, &cleared).await.unwrap();
	assert_eq!(patched.period_end.as_deref(), Some("2026-01-31"));
	let half = InvoicePatch { period_start: Patch::Null, ..Default::default() };
	let e = invoices.patch(&sys, &uid, &half).await.unwrap_err();
	assert_eq!(e.parts().1, "E-INV-PERIOD-INCOMPLETE");
}

#[tokio::test]
async fn an_issued_period_invoice_derives_its_fulfilment_date_and_its_storno_keeps_the_period() {
	use mintworks_invoice::numbering::{add_days, date_of};
	let db = TmpDb::new("period-issue");
	let (_app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let today = date_of(Timestamp::now()).unwrap();
	let day = |n| add_days(&today, n).unwrap();

	// Issued and due within the period: the issue date.
	let d = NewDraft { due_date: Some(day(10)), ..period_draft(&day(-10), &day(20)) };
	let uid = invoices.draft(&sys, &d).await.unwrap().uid.to_string();
	let issued = invoices.issue(&sys, &uid).await.unwrap();
	assert_eq!(issued.fulfilment_date.as_deref(), Some(today.as_str()));

	// Due after the period: the due date.
	let d = NewDraft { due_date: Some(day(5)), ..period_draft(&day(-30), &day(-1)) };
	let uid = invoices.draft(&sys, &d).await.unwrap().uid.to_string();
	let issued = invoices.issue(&sys, &uid).await.unwrap();
	assert_eq!(issued.fulfilment_date, Some(day(5)));
	assert_eq!(issued.period_start, Some(day(-30)));

	let storno = invoices.storno(&sys, issued.uid.as_str(), "test").await.unwrap();
	assert_eq!((storno.period_start, storno.period_end), (Some(day(-30)), Some(day(-1))));
	assert_eq!(storno.fulfilment_date, Some(day(5)));
}

/// Áfa tv. 80. § (1) b): a 58. § invoice converts at the rate valid on its issue date, whether
/// the derived fulfilment date falls after it (no rate published yet) or before it.
#[tokio::test]
async fn a_period_invoice_converts_at_the_issue_date_rate() {
	use mintworks_invoice::numbering::{add_days, date_of};
	let db = TmpDb::new("period-rate-date");
	let (_app, invoices, store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let today = date_of(Timestamp::now()).unwrap();
	let day = |n| add_days(&today, n).unwrap();

	sqlx::query("UPDATE currencies SET mode = 'OFFICIAL', fixed_rate_e6 = NULL WHERE code = 'EUR'")
		.execute(store.write_pool())
		.await
		.unwrap();
	for (date, rate_e6) in [(day(-1), 395_000_000_i64), (today.clone(), 410_000_000)] {
		sqlx::query(
			"INSERT INTO currency_rates (pair, date, source, rate_e6, fetched_at)
			 VALUES ('EURHUF', ?, 'BANK', ?, 0)",
		)
		.bind(date)
		.bind(rate_e6)
		.execute(store.write_pool())
		.await
		.unwrap();
	}
	let eur = Some(CurrencyCode::parse("EUR").unwrap());

	// Fulfilment in the future: the due date, 30 days out.
	let d = NewDraft {
		currency: eur.clone(),
		due_date: Some(day(30)),
		..period_draft(&day(-30), &day(-1))
	};
	let uid = invoices.draft(&sys, &d).await.unwrap().uid.to_string();
	let issued = invoices.issue(&sys, &uid).await;
	// Midnight passed mid-test: every expectation below is keyed to the old `today`.
	if date_of(Timestamp::now()).unwrap() != today {
		return;
	}
	let issued = issued.unwrap();
	assert_eq!(issued.fulfilment_date, Some(day(30)));
	assert_eq!(issued.rate_date, Some(today.clone()));
	assert_eq!(issued.huf_rate_e6, Some(410_000_000));

	// Fulfilment in the past: issued and due inside a period ending yesterday is the period end.
	let d =
		NewDraft { currency: eur, due_date: Some(day(-2)), ..period_draft(&day(-30), &day(-1)) };
	let uid = invoices.draft(&sys, &d).await.unwrap().uid.to_string();
	let issued = invoices.issue(&sys, &uid).await;
	if date_of(Timestamp::now()).unwrap() != today {
		return;
	}
	let issued = issued.unwrap();
	assert_eq!(issued.fulfilment_date, Some(day(-1)));
	assert_eq!(issued.rate_date, Some(today));
	assert_eq!(issued.huf_rate_e6, Some(410_000_000));
}

// ---- self-service seller mint --------------------------------------------------------------

/// Account 1 with `role` on a fresh org `id` of `kind` under root, acting there.
async fn member_of(store: &SqliteStore, id: i64, kind: &str, role: &str) -> Ctx {
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (?, ?, (SELECT id FROM orgs WHERE kind = 'ROOT'), ?, 'Uj', 1, 0)",
	)
	.bind(id)
	.bind(format!("org_{id}"))
	.bind(kind)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 1, ?, 0, 0)",
	)
	.bind(id)
	.bind(role)
	.execute(store.write_pool())
	.await
	.unwrap();
	let mut ctx = Ctx::system("test").with_org(id);
	ctx.actor = mintworks_core::ctx::Actor::User { account_id: 1 };
	ctx
}

async fn seller_rows(store: &SqliteStore) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM sellers")
		.fetch_one(store.read_pool())
		.await
		.unwrap()
}

#[tokio::test]
async fn an_org_admin_mints_its_own_seller_once() {
	let db = TmpDb::new("mint-once");
	let (_app, invoices, store) = service(&db).await;
	let ctx = member_of(&store, 7, "SHARED", "ADMIN").await;

	let view = invoices.create_seller(&ctx, Some(" B "), &seller_version()).await.unwrap();
	assert!(!view.inherited);
	assert_eq!(view.series_code, "B");
	assert_eq!(view.status, SellerVersionStatus::Current);
	let row = store.seller_by_id(7).await.unwrap().unwrap();
	assert_eq!((row.org_id, row.nav_login), (7, None));
	assert_eq!(invoices.seller(&ctx).await.unwrap().uid, view.uid);

	let err = invoices.create_seller(&ctx, None, &seller_version()).await.unwrap_err();
	assert_eq!(err.parts(), (mintworks_core::error::StatusCode::CONFLICT, "E-INV-SELLER-EXISTS"));
}

#[tokio::test]
async fn a_member_cannot_mint_a_seller() {
	let db = TmpDb::new("mint-member");
	let (_app, invoices, store) = service(&db).await;
	let ctx = member_of(&store, 7, "SHARED", "MEMBER").await;
	let before = seller_rows(&store).await;

	let err = invoices.create_seller(&ctx, None, &seller_version()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");
	assert_eq!(seller_rows(&store).await, before);
}

#[tokio::test]
async fn minting_is_refused_on_a_personal_or_root_org() {
	let db = TmpDb::new("mint-kind");
	let (_app, invoices, store) = service(&db).await;
	let before = seller_rows(&store).await;

	let personal = member_of(&store, 7, "PERSONAL", "OWNER").await;
	let err = invoices.create_seller(&personal, None, &seller_version()).await.unwrap_err();
	assert_eq!(err.parts(), (mintworks_core::error::StatusCode::CONFLICT, "E-INV-SELLER-ORG-KIND"));

	let root = Ctx::system("test").with_org(ROOT);
	let err = invoices.create_seller(&root, None, &seller_version()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-SELLER-ORG-KIND");
	assert_eq!(seller_rows(&store).await, before);
}

#[tokio::test]
async fn a_malformed_seller_writes_no_row() {
	let db = TmpDb::new("mint-malformed");
	let (_app, invoices, store) = service(&db).await;
	let ctx = member_of(&store, 7, "SHARED", "ADMIN").await;
	let before = seller_rows(&store).await;

	for (what, series, patch) in [
		(
			"a bad tax number",
			None,
			SellerVersionPatch { tax_number: Some("1234".into()), ..seller_version() },
		),
		("no name", None, SellerVersionPatch { name: None, ..seller_version() }),
		("a blank series", Some("  "), seller_version()),
	] {
		assert!(invoices.create_seller(&ctx, series, &patch).await.is_err(), "{what} was accepted");
		assert_eq!(seller_rows(&store).await, before, "{what} wrote a row");
	}
}

#[tokio::test]
async fn a_half_minted_seller_is_completed_by_the_retry() {
	let db = TmpDb::new("mint-resume");
	let (_app, invoices, store) = service(&db).await;
	let ctx = member_of(&store, 7, "SHARED", "ADMIN").await;
	// The first write landed, the version did not.
	let half = Seller { id: 7, org_id: 7, series_code: "X".into(), ..seller() };
	assert!(store.create_seller(&half).await.unwrap());

	let view = invoices.create_seller(&ctx, Some("B"), &seller_version()).await.unwrap();
	assert_eq!(view.uid, half.uid, "the retry minted a second seller");
	assert_eq!(view.series_code, "B");
	assert_eq!(view.status, SellerVersionStatus::Current);
}

#[tokio::test]
async fn a_child_org_sees_its_parents_seller_as_inherited() {
	let db = TmpDb::new("mint-inherited");
	let (_app, invoices, store) = service(&db).await;
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (8, 'org_child', ?, 'SHARED', 'Egyseg', 1, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();

	let own = invoices.seller(&Ctx::system("test").with_org(ORG)).await.unwrap();
	let child = invoices.seller(&Ctx::system("test").with_org(8)).await.unwrap();
	assert!(!own.inherited);
	assert!(child.inherited);
	assert_eq!(child.uid, own.uid);
}

#[tokio::test]
async fn a_minted_seller_owns_its_catalogue() {
	let db = TmpDb::new("mint-catalogue");
	let (_app, invoices, store) = service(&db).await;
	let ctx = member_of(&store, 7, "SHARED", "ADMIN").await;
	invoices.create_seller(&ctx, None, &seller_version()).await.unwrap();

	let def = mintworks_invoice::store::ServiceDef {
		code: "RENTAL".into(),
		name: "Berles".into(),
		description: None,
		unit: "nap".into(),
		unit_price: Money(10_000),
		vat_code: VatCode::Std27,
	};
	invoices.create_service(&ctx, &def).await.unwrap();
	assert_eq!(invoices.list_services(&ctx, true).await.unwrap().len(), 1);
	let other = Ctx::system("test").with_org(ORG);
	assert!(invoices.list_services(&other, true).await.unwrap().is_empty());
}

/// Before the first numbered invoice the tax number is a typo to fix; after it, NAV knows the
/// company by it, so a change is a new company.
#[tokio::test]
async fn tax_number_changes_until_first_issue() {
	let db = TmpDb::new("taxnumber-lock");
	let (_app, invoices, store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	store
		.put_seller(&Seller { nav_login: Some("tech".into()), ..stored_seller(&store).await })
		.await
		.unwrap();
	assert!(!invoices.seller(&sys).await.unwrap().tax_number_locked);

	let retax = |t: &str| SellerVersionPatch { tax_number: Some(t.into()), ..Default::default() };
	invoices.sync_seller(&sys, &retax("23310795-2-08")).await.unwrap().unwrap();
	assert_eq!(stored_seller(&store).await.nav_login, None, "the old taxpayer's NAV user");

	let draft = service_draft(&invoices, &sys).await;
	invoices.issue(&sys, draft.uid.as_str()).await.unwrap();
	assert!(invoices.seller(&sys).await.unwrap().tax_number_locked);
	for err in [
		invoices.sync_seller(&sys, &retax("12345678242")).await.unwrap_err(),
		invoices.save_seller_draft(&sys, &retax("12345678242")).await.unwrap_err(),
	] {
		assert_eq!(err.parts().1, "E-INV-SELLER-TAXNUMBER-LOCKED");
	}
	let rename = SellerVersionPatch { name: Some("Uj Nev Kft.".into()), ..Default::default() };
	assert_eq!(invoices.sync_seller(&sys, &rename).await.unwrap().unwrap().name, "Uj Nev Kft.");
}

#[tokio::test]
async fn closed_seller_records_payments_only() {
	let db = TmpDb::new("seller-closed");
	let (_app, invoices, _store) = service(&db).await;
	let sys = Ctx::system("test").with_org(ORG);
	let issued = service_draft(&invoices, &sys).await;
	let issued = invoices.issue(&sys, issued.uid.as_str()).await.unwrap();
	let open_draft = service_draft(&invoices, &sys).await;

	assert!(invoices.set_seller_closed(&sys, true).await.unwrap().closed_at.is_some());
	let rename = SellerVersionPatch { name: Some("X Kft.".into()), ..Default::default() };
	for err in [
		invoices.draft(&sys, &plain_draft()).await.unwrap_err(),
		invoices.issue(&sys, open_draft.uid.as_str()).await.unwrap_err(),
		invoices.storno(&sys, issued.uid.as_str(), "t").await.unwrap_err(),
		invoices.sync_seller(&sys, &rename).await.unwrap_err(),
	] {
		assert_eq!(err.parts().1, "E-INV-SELLER-CLOSED");
	}
	assert_eq!(
		invoices.mark_paid(&sys, issued.uid.as_str()).await.unwrap().status,
		InvoiceStatus::Paid
	);

	assert!(invoices.set_seller_closed(&sys, false).await.unwrap().closed_at.is_none());
	invoices.draft(&sys, &plain_draft()).await.unwrap();
}

#[tokio::test]
async fn closing_needs_owner_and_stepup() {
	use mintworks_core::ctx::Actor;

	let db = TmpDb::new("seller-close-gate");
	let (_app, invoices, store) = service(&db).await;
	for (account, role) in [(2, "ADMIN"), (3, "OWNER")] {
		sqlx::query("INSERT INTO accounts (id, uid, email, created_at) VALUES (?, ?, ?, 0)")
			.bind(account)
			.bind(format!("acc_{account}"))
			.bind(format!("a{account}@e.st"))
			.execute(store.write_pool())
			.await
			.unwrap();
		sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES (?, ?, ?, 0, 0)",
		)
		.bind(ORG)
		.bind(account)
		.bind(role)
		.execute(store.write_pool())
		.await
		.unwrap();
	}
	let ctx = |account_id, auth_at| Ctx {
		actor: Actor::User { account_id },
		auth_at: Some(auth_at),
		..Ctx::system("test").with_org(ORG)
	};
	let now = Timestamp::now().0;
	assert_eq!(
		invoices.set_seller_closed(&ctx(2, now), true).await.unwrap_err().parts().1,
		"E-AUTH-FORBIDDEN"
	);
	assert_eq!(
		invoices
			.set_seller_closed(&ctx(3, now - 1_000_000), true)
			.await
			.unwrap_err()
			.parts()
			.1,
		"E-AUTH-STEPUP"
	);
	assert!(
		invoices
			.set_seller_closed(&ctx(3, now), true)
			.await
			.unwrap()
			.closed_at
			.is_some()
	);
}

/// The tax-limit facts: invoiced by fulfilment month, net, in HUF, place-of-supply-abroad
/// groups out and a storno netting its original; received by the month of `paid_at`, plus
/// CASH by fulfilment month.
#[tokio::test]
async fn revenue_buckets_invoiced_and_received_huf_by_month() {
	let db = TmpDb::new("revenue");
	let (_app, invoices, store) = service(&db).await;
	let ctx = Ctx::system("test").with_org(ORG);
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (2, 'prt_us', ?, 'C', 'Acme Inc.', 'US', '99-1234567',
		  '10001', 'New York', '5th Ave 1.', 0, 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	let issue = async |on: &str, patch: NewDraft| {
		let d = NewDraft { fulfilment_date: Some(on.into()), ..patch };
		let d = invoices.draft(&ctx, &d).await.unwrap();
		invoices.issue(&ctx, d.uid.as_str()).await.unwrap()
	};

	// Relative to today: a CASH invoice is fulfilled on its issue date.
	let today = mintworks_invoice::date_of(Timestamp::now()).unwrap();
	let (year, cash_month) = (&today[..4], today[5..7].parse::<usize>().unwrap());
	let huf = issue(&format!("{year}-02-10"), plain_draft()).await;
	issue(
		&format!("{year}-02-11"),
		NewDraft {
			currency: Some(CurrencyCode::parse("EUR").unwrap()),
			..new_draft(None, vec![adhoc(1_000_000, 10_000, None)])
		},
	)
	.await;
	issue(
		&format!("{year}-02-12"),
		NewDraft {
			billing_party: Party::Uid(mintworks_core::ids::PartyId::from_trusted("prt_us".into())),
			..plain_draft()
		},
	)
	.await;
	let cancelled = issue(&format!("{year}-01-20"), plain_draft()).await;
	invoices.storno(&ctx, cancelled.uid.as_str(), "Téves").await.unwrap();
	issue(&today, NewDraft { payment_method: Some(PaymentMethod::Cash), ..plain_draft() }).await;
	let march = Timestamp::parse_rfc3339(&format!("{year}-03-15T12:00:00Z")).unwrap();
	invoices.set_paid(&ctx, huf.uid.as_str(), huf.gross, Some(march)).await.unwrap();

	// HUF + EUR at 400 in February, HO left out; the January storno nets its original out.
	// Outstanding: the unpaid EUR and HO TRANSFER invoices, by fulfilment month.
	let mut want = vec![(Money(0), Money(0), Money(0)); 12];
	want[1].0 = Money(100_000 + 4_000_000);
	want[1].2 = Money(4_000_000 + 100_000);
	want[2].1 = Money(100_000);
	want[cash_month - 1].0 = Money(want[cash_month - 1].0.0 + 100_000);
	want[cash_month - 1].1 = Money(want[cash_month - 1].1.0 + 100_000);
	let got: Vec<_> = invoices
		.revenue(&ctx, year.parse().unwrap())
		.await
		.unwrap()
		.into_iter()
		.map(|m| (m.invoiced_huf, m.received_huf, m.outstanding_huf))
		.collect();
	assert_eq!(got, want);
	assert_eq!(invoices.revenue(&ctx, 1999).await.unwrap_err().parts().1, "E-CORE-VALIDATION");
}

// vim: ts=4
