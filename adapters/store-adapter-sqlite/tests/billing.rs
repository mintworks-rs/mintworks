//! `BillingStore` conformance tests — what a second store adapter must pass: the zero link row
//! `create_payment` writes, the status guard that makes a replayed callback a no-op, the
//! all-or-nothing rollback of `settle` against an unpayable invoice, the allocation upsert that
//! sums onto the existing pair rather than inserting a second, and `request_id` uniqueness.
//!
//! Every test opens a real file database. `sqlite::memory:` gives each *connection* its own
//! database, so two stores over one in-memory URL would never contend for the write lock.
//!
//! The service-handle half lives in `crates/saas-billing/tests/billing.rs`: a test that drives
//! `allocate`/`webhook` goes there, a test that drives `BillingStore` goes here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_billing::provider::PaymentState;
use saas_billing::store::{BillingStore, NewPayment, PaymentFilter, RefundRecord, Settlement};
use saas_core::{config::Config, ids::SellerId, prelude::*};
use saas_invoice::{
	store::{
		BuyerSnapshot, Invoice, InvoiceKind, InvoiceStatus, InvoiceStore, InvoiceVatGroup,
		IssueInvoice, NewInvoice, NewInvoiceLine, PartyKind, PaymentMethod, Seller,
		SellerVersionPatch,
	},
	vat::VatCode,
};
use store_adapter_sqlite::SqliteStore;

const ORG: i64 = 1;

/// The platform root, moved off its natural id 1 so the fixture's own org can have it. The
/// framework finds the root by `kind = 'ROOT'` and never by its value.
const ROOT: i64 = 0;

/// The fixture's one seller. `put_seller` does not autoincrement, so the id is chosen here.
const SELLER: i64 = 1;

/// The version `seed_seller` publishes — the first row `seller_versions` ever gets.
const SELLER_VER: i64 = 1;

/// `issue_input`'s net, so `gross` is `NET + NET * 27%`.
const NET: i64 = 100_000;
const GROSS: i64 = NET + NET * 2700 / 10000;

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("saas-billing-store-test-{}-{name}", std::process::id()));
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
/// `HUF` is already seeded by the framework module.
async fn setup(db: &TmpDb) -> SqliteStore {
	let store = open(db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

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

	store.put_seller(&seller()).await.unwrap();
	store.save_seller_version_draft(SELLER, &seller_version()).await.unwrap();
	store
		.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();
	store
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

async fn draft(store: &SqliteStore) -> Invoice {
	store
		.create_draft(&NewInvoice {
			org_id: ORG,
			seller_id: SELLER,
			billing_party_id: None,
			request_id: None,
			kind: InvoiceKind::Normal,
			original_invoice_id: None,
			currency: CurrencyCode::parse("HUF").unwrap(),
			rate_e6: 1_000_000,
			payment_method: PaymentMethod::Transfer,
			notes: None,
			discount_kind: None,
			discount_value: None,
		})
		.await
		.unwrap()
}

/// One 27% group of `NET` fillér. HUF, so `huf_rate_e6` and the `*_huf` trio stay NULL.
fn issue_input(invoice_id: i64) -> IssueInvoice {
	let vat = NET * 2700 / 10000;
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
		net: Money(NET),
		vat: Money(vat),
		gross: Money(NET + vat),
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
			unit_price: Money(NET),
			discount_kind: None,
			discount_value: None,
			discount_amount: Money(0),
			discount_description: None,
			net: Money(NET),
			vat_code: VatCode::Std27,
			vat_rate_bp: 2700,
			vat: Money(vat),
			gross: Money(NET + vat),
			note: None,
		}],
		groups: vec![InvoiceVatGroup {
			invoice_id,
			vat_code: VatCode::Std27,
			vat_rate_bp: 2700,
			net: Money(NET),
			vat: Money(vat),
			gross: Money(NET + vat),
			net_huf: None,
			vat_huf: None,
			gross_huf: None,
		}],
		period_start: None,
		period_end: None,
		paid: false,
	}
}

async fn issued(store: &SqliteStore) -> Invoice {
	let inv = draft(store).await;
	store.issue(inv.id, &issue_input(inv.id), inv.version).await.unwrap()
}

fn new_payment(invoice_id: Option<i64>, request_id: Option<&str>) -> NewPayment {
	NewPayment {
		org_id: ORG,
		kind: "STUB".into(),
		provider: Some("stub".into()),
		provider_ref: Some("prv-1".into()),
		request_id: request_id.map(str::to_string),
		status: PaymentState::Pending,
		amount: Money(GROSS),
		currency: CurrencyCode::parse("HUF").unwrap(),
		ext_ref: None,
		note: None,
		created_by: None,
		invoice_id,
	}
}

fn settlement(payment_id: i64, invoice_id: i64, amount: i64, from: PaymentState) -> Settlement {
	Settlement {
		payment_id,
		from: vec![from],
		to: PaymentState::Succeeded,
		invoice_id,
		amount: Money(amount),
		at: Timestamp::now(),
		allocated_by: None,
		ceiling: None,
	}
}

/// `payments` has no invoice column, so the zero-amount `payment_allocations` row is the only
/// link a callback carrying a bare `provider_ref` can follow back to the invoice.
#[tokio::test]
async fn create_payment_writes_the_zero_link_row() {
	let db = TmpDb::new("link-row");
	let store = setup(&db).await;
	let inv = issued(&store).await;

	let with = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();
	let rows = store.allocations(with.id).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].invoice_id, inv.id);
	assert_eq!(rows[0].amount, Money::ZERO);

	// No invoice named, no link row — a manual entry is allocated by hand afterwards.
	let without = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-2".into()),
			..new_payment(None, None)
		})
		.await
		.unwrap();
	assert!(store.allocations(without.id).await.unwrap().is_empty());
	assert_eq!(store.payment_by_provider_ref("stub", "prv-1").await.unwrap().unwrap().id, with.id);
}

/// `payments.request_id` is UNIQUE, which is what makes `POST /api/invoices/{uid}/pay`
/// idempotent without a read-then-write race.
#[tokio::test]
async fn a_spent_request_id_conflicts() {
	let db = TmpDb::new("request-id");
	let store = setup(&db).await;
	let inv = issued(&store).await;

	let first = store.create_payment(&new_payment(Some(inv.id), Some("req-1"))).await.unwrap();
	let err = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-2".into()),
			..new_payment(Some(inv.id), Some("req-1"))
		})
		.await
		.unwrap_err();
	assert!(matches!(err, Error::Conflict(_)), "{err:?}");
	assert_eq!(store.payment_by_request_id(ORG, "req-1").await.unwrap().unwrap().id, first.id);
	// Org-scoped: `request_id` is client text, so a global lookup answered one org's start
	// with another org's payment.
	assert!(store.payment_by_request_id(ORG + 1, "req-1").await.unwrap().is_none());

	// And the uniqueness is per org too: a global one let org B's own `"sub-2026-01"`
	// collide with A's — a permanent conflict on a key B had never used, and a probe for A's.
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (?, 'org_two', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Masik', 1, 0)",
	)
	.bind(ORG + 1)
	.execute(store.write_pool())
	.await
	.unwrap();
	let theirs = store
		.create_payment(&NewPayment {
			org_id: ORG + 1,
			provider_ref: Some("prv-2".into()),
			..new_payment(None, Some("req-1"))
		})
		.await
		.unwrap();
	assert_ne!(theirs.id, first.id);
	assert_eq!(store.payment_by_request_id(ORG + 1, "req-1").await.unwrap().unwrap().id, theirs.id);
	assert_eq!(store.payment_by_request_id(ORG, "req-1").await.unwrap().unwrap().id, first.id);
}

/// **The allocation ceiling is enforced inside `settle`'s transaction**, which a second adapter
/// must reproduce: checked in the caller off a reader connection it is a read-then-write, and
/// two concurrent allocations of one payment both saw zero allocated and both committed.
///
/// Two stores over one **file** database, because `sqlite::memory:` gives each connection its
/// own and they would never contend for the write lock.
#[tokio::test]
async fn the_allocation_ceiling_holds_under_two_writers() {
	let db = TmpDb::new("ceiling-race");
	let store = setup(&db).await;
	let first = issued(&store).await;
	let second = issued(&store).await;
	let other = open(&db).await;

	// One payment, and two allocations of the whole of it against different invoices.
	let payment = store.create_payment(&new_payment(None, None)).await.unwrap();
	assert!(
		store
			.advance_status(payment.id, PaymentState::Succeeded, &[PaymentState::Pending])
			.await
			.unwrap()
	);

	let ceiling = Some(Money(GROSS));
	let one =
		Settlement { ceiling, ..settlement(payment.id, first.id, GROSS, PaymentState::Succeeded) };
	let two =
		Settlement { ceiling, ..settlement(payment.id, second.id, GROSS, PaymentState::Succeeded) };
	let (a, b) = tokio::join!(store.settle(&one), other.settle(&two));
	let applied = usize::from(a.unwrap()) + usize::from(b.unwrap());
	assert_eq!(applied, 1, "exactly one may take the headroom");

	let allocated: i64 =
		store.allocations(payment.id).await.unwrap().iter().map(|x| x.amount.0).sum();
	assert_eq!(allocated, GROSS, "never more than the payment");
}

/// `overdue_invoices` is not filtered by "already reminded" — that record is the job's
/// `dedup_key` — so a flat `LIMIT` handed the sweep the same oldest page every day and nothing
/// past it was ever dunned. The cursor is what pages it.
#[tokio::test]
async fn overdue_invoices_page_past_the_first_batch() {
	let db = TmpDb::new("overdue-paging");
	let store = setup(&db).await;

	let mut ids = Vec::new();
	for _ in 0..5 {
		let inv = issued(&store).await;
		sqlx::query("UPDATE invoices SET due_date = date('now', '-30 day') WHERE id = ?")
			.bind(inv.id)
			.execute(store.write_pool())
			.await
			.unwrap();
		ids.push(inv.id);
	}

	// Two at a time, exactly as `dunning::sweep` walks it, until a short page ends the loop.
	let mut seen = Vec::new();
	let mut after = None;
	loop {
		let page = store.overdue_invoices(None, after, 2).await.unwrap();
		let short = page.len() < 2;
		after = page.last().map(|i| i.invoice_id);
		seen.extend(page.iter().map(|i| i.invoice_id));
		if short {
			break;
		}
	}
	assert_eq!(seen, ids, "every overdue invoice, once, in due-date order");
}

/// The guard is the whole replay defence: a status write that does not match `from` writes
/// nothing and answers `false`, rather than moving a payment twice.
#[tokio::test]
async fn advance_status_is_guarded_by_the_current_status() {
	let db = TmpDb::new("advance-guard");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();

	assert!(
		!store
			.advance_status(p.id, PaymentState::Succeeded, &[PaymentState::Authorized])
			.await
			.unwrap()
	);
	assert_eq!(store.payment(p.id).await.unwrap().unwrap().status, PaymentState::Pending);

	assert!(
		store
			.advance_status(p.id, PaymentState::AwaitingUser, &[PaymentState::Pending])
			.await
			.unwrap()
	);
	// The replay: the row has left `Pending`, so the second copy of the same ping is a no-op.
	assert!(
		!store
			.advance_status(p.id, PaymentState::AwaitingUser, &[PaymentState::Pending])
			.await
			.unwrap()
	);
	assert_eq!(store.payment(p.id).await.unwrap().unwrap().status, PaymentState::AwaitingUser);
}

/// `settle` is one transaction, so a refused invoice must take the payment row and the
/// allocation back with it — an allocation against a draft lands nowhere at all.
#[tokio::test]
async fn settle_against_a_draft_rolls_everything_back() {
	let db = TmpDb::new("settle-draft");
	let store = setup(&db).await;
	let inv = draft(&store).await;
	let p = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();

	assert!(
		!store
			.settle(&settlement(p.id, inv.id, GROSS, PaymentState::Pending))
			.await
			.unwrap()
	);
	assert_eq!(store.payment(p.id).await.unwrap().unwrap().status, PaymentState::Pending);
	let rows = store.allocations(p.id).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].amount, Money::ZERO);
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().paid_amount, Money::ZERO);
}

/// `PRIMARY KEY (payment_id, invoice_id)` admits one row per pair, so a second allocation adds
/// to the row already there — including the zero link row — and `invoices.paid_amount` is
/// recomputed from the sum rather than incremented.
#[tokio::test]
async fn a_second_allocation_sums_onto_the_same_row() {
	let db = TmpDb::new("alloc-sum");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();

	assert!(
		store
			.settle(&settlement(p.id, inv.id, 40_000, PaymentState::Pending))
			.await
			.unwrap()
	);
	let part = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(part.paid_amount, Money(40_000));
	assert!(part.paid_at.is_none(), "a partial payment does not mark the invoice paid");
	assert_eq!(part.status, InvoiceStatus::Issued);

	assert!(
		store
			.settle(&settlement(p.id, inv.id, GROSS - 40_000, PaymentState::Succeeded))
			.await
			.unwrap()
	);
	let rows = store.allocations(p.id).await.unwrap();
	assert_eq!(rows.len(), 1, "one row per (payment, invoice) pair, never a second");
	assert_eq!(rows[0].amount, Money(GROSS));

	let paid = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(paid.paid_amount, Money(GROSS));
	assert!(paid.paid_at.is_some());
	// `settle` is the only writer of `PAID` on the gateway path: without it the invoice reached
	// `paid_amount == gross` while still reading `ISSUED`.
	assert_eq!(paid.status, InvoiceStatus::Paid);
	assert_eq!(store.payment(p.id).await.unwrap().unwrap().status, PaymentState::Succeeded);

	// A reversal is the negative on the same pair, and drops `paid_at` again with the total.
	assert!(
		store
			.settle(&settlement(p.id, inv.id, -GROSS, PaymentState::Succeeded))
			.await
			.unwrap()
	);
	let reversed = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(reversed.paid_amount, Money::ZERO);
	assert!(reversed.paid_at.is_none());
	assert_eq!(reversed.status, InvoiceStatus::Issued, "the status walks back with the sum");
}

/// The redirect and the deadline are stored, not handed back once: a retry under a spent
/// `request_id` opens no second payment and so has no URL of its own to answer with, and the
/// deadline is what the SPA counts down.
#[tokio::test]
async fn set_started_records_the_gateway_reference_and_its_redirect() {
	let db = TmpDb::new("set-started");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = store
		.create_payment(&NewPayment { provider_ref: None, ..new_payment(Some(inv.id), None) })
		.await
		.unwrap();
	let expires = Timestamp(Timestamp::now().0 + 600);

	assert!(
		store
			.set_started(p.id, "prv-9", Some("https://gw.invalid/p/9"), Some(expires))
			.await
			.unwrap()
	);
	let back = store.payment(p.id).await.unwrap().unwrap();
	assert_eq!(back.provider_ref.as_deref(), Some("prv-9"));
	assert_eq!(back.redirect_url.as_deref(), Some("https://gw.invalid/p/9"));
	assert_eq!(back.expires_at, Some(expires));

	// First write wins, so two concurrent starts cannot repoint the row at the abandoned one.
	assert!(!store.set_started(p.id, "prv-10", None, None).await.unwrap());
	let back = store.payment(p.id).await.unwrap().unwrap();
	assert_eq!(back.redirect_url.as_deref(), Some("https://gw.invalid/p/9"));
	assert_eq!(back.expires_at, Some(expires), "and not its deadline either");
}

/// `payment_allocations.invoice_id` does not cascade, so an abandoned gateway attempt's zero
/// link row used to make its own draft undeletable — and unsweepable.
#[tokio::test]
async fn a_draft_with_an_abandoned_payment_is_still_deletable() {
	let db = TmpDb::new("discard-draft");
	let store = setup(&db).await;
	let inv = draft(&store).await;
	store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();

	assert!(store.delete_draft(inv.id).await.unwrap());
	assert!(store.invoice_by_id(inv.id).await.unwrap().is_none());
}

/// The invoice page's cold read: found through the zero link row, so a payment that has not
/// settled yet is in the answer, and another org's is not.
#[tokio::test]
async fn payments_by_invoice_finds_the_unsettled_one() {
	let db = TmpDb::new("by-invoice");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();

	let rows = store.payments_by_invoice(ORG, inv.id).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].id, p.id);
	assert!(store.payments_by_invoice(ORG + 1, inv.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn org_id_by_uid_resolves_the_public_id() {
	let db = TmpDb::new("org-uid");
	let store = setup(&db).await;

	let uid = OrgId::from_trusted("org_t".to_string());
	assert_eq!(store.org_id_by_uid(&uid).await.unwrap(), Some(ORG));
	let missing = OrgId::from_trusted("org_nope".to_string());
	assert_eq!(store.org_id_by_uid(&missing).await.unwrap(), None);
}

#[tokio::test]
async fn list_payments_is_org_scoped_and_newest_first() {
	let db = TmpDb::new("list");
	let store = setup(&db).await;
	let inv = issued(&store).await;

	let first = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();
	let second = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-2".into()),
			..new_payment(Some(inv.id), None)
		})
		.await
		.unwrap();

	let page = store
		.list_payments(ORG, &PaymentFilter { limit: 10, ..Default::default() })
		.await
		.unwrap();
	assert_eq!(page.iter().map(|p| p.id).collect::<Vec<_>>(), vec![second.id, first.id]);
	// The cursor is a **uid**: `payments.id` is global, so putting it on the wire handed any
	// org a cross-org row-volume oracle.
	let next = store
		.list_payments(
			ORG,
			&PaymentFilter { before: Some(&second.uid), limit: 10, ..Default::default() },
		)
		.await
		.unwrap();
	assert_eq!(next.iter().map(|p| p.id).collect::<Vec<_>>(), vec![first.id]);
	assert!(
		store
			.list_payments(ORG + 1, &PaymentFilter { limit: 10, ..Default::default() })
			.await
			.unwrap()
			.is_empty()
	);

	// Another org's uid is `None`, which the caller turns into `E-CORE-NOTFOUND`.
	assert!(store.payment_by_uid(Some(ORG + 1), &first.uid).await.unwrap().is_none());
	assert!(store.payment_by_uid(Some(ORG), &first.uid).await.unwrap().is_some());
}

/// What the payment sweep asks for: only a gateway-backed row, only a live status, and only one
/// nothing has touched recently — a fresh row belongs to a payer who is still on the page. Age is
/// not a filter, because an ancient live row is what the sweep's local expiry exists for.
#[tokio::test]
async fn live_payments_finds_stale_gateway_backed_rows_at_any_age() {
	let db = TmpDb::new("live-payments");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let now = Timestamp::now();
	let stale = now.0 - 600;

	let fresh = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();
	let backdated = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-2".into()),
			..new_payment(Some(inv.id), None)
		})
		.await
		.unwrap();
	let settled = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-3".into()),
			status: PaymentState::Succeeded,
			..new_payment(Some(inv.id), None)
		})
		.await
		.unwrap();
	let no_ref = store
		.create_payment(&NewPayment { provider_ref: None, ..new_payment(Some(inv.id), None) })
		.await
		.unwrap();
	// Older than `MAX_AGE_SECS` and still selected: it is the row the sweep has to expire locally.
	let ancient = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-4".into()),
			..new_payment(Some(inv.id), None)
		})
		.await
		.unwrap();

	for id in [backdated.id, settled.id, no_ref.id, ancient.id] {
		sqlx::query("UPDATE payments SET updated_at = ? WHERE id = ?")
			.bind(stale)
			.bind(id)
			.execute(store.write_pool())
			.await
			.unwrap();
	}
	sqlx::query("UPDATE payments SET created_at = ? WHERE id = ?")
		.bind(now.0 - 8 * 86_400)
		.bind(ancient.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	let rows = store.live_payments(Timestamp(now.0 - 120), None, 10).await.unwrap();
	assert_eq!(rows.iter().map(|p| p.id).collect::<Vec<_>>(), vec![backdated.id, ancient.id]);
	assert!(!rows.iter().any(|p| p.id == fresh.id), "a fresh row is still the payer's");
}

async fn backdate(store: &SqliteStore, id: i64, at: i64) {
	sqlx::query("UPDATE payments SET updated_at = ? WHERE id = ?")
		.bind(at)
		.bind(id)
		.execute(store.write_pool())
		.await
		.unwrap();
}
async fn ask(store: &SqliteStore, now: Timestamp) -> Vec<i64> {
	store
		.live_payments(Timestamp(now.0 - 120), None, 10)
		.await
		.unwrap()
		.iter()
		.map(|p| p.id)
		.collect()
}

/// The store-level half: `CANCELED` is the gateway's own verdict, so the sweep never selects it.
#[tokio::test]
async fn a_canceled_payment_is_never_selected() {
	let db = TmpDb::new("live-payments-canceled");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let now = Timestamp::now();

	let live = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();
	let canceled = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-2".into()),
			status: PaymentState::Canceled,
			..new_payment(Some(inv.id), None)
		})
		.await
		.unwrap();
	backdate(&store, live.id, now.0 - 600).await;
	backdate(&store, canceled.id, now.0 - 600).await;
	assert_eq!(ask(&store, now).await, vec![live.id], "ten minutes old is not stale enough");
	// However old it gets: a gateway cancellation is final and is not re-asked at all.
	backdate(&store, canceled.id, now.0 - 2 * 3_600).await;
	assert_eq!(ask(&store, now).await, vec![live.id], "a canceled row is never swept");
}

fn refund_record(payment_id: i64, invoice_id: Option<i64>, amount: i64) -> RefundRecord {
	RefundRecord {
		payment_id,
		from: vec![PaymentState::Succeeded],
		to: PaymentState::Refunded,
		amount: Money(amount),
		expect_refunded: Money::ZERO,
		reverse: Money(amount),
		invoice_id,
		at: Timestamp::now(),
		by: None,
	}
}

/// Settles `p` in full against `inv`, which is what every refund case below starts from.
async fn settled(store: &SqliteStore, invoice_id: i64) -> i64 {
	let p = store.create_payment(&new_payment(Some(invoice_id), None)).await.unwrap();
	assert!(
		store
			.settle(&settlement(p.id, invoice_id, GROSS, PaymentState::Pending))
			.await
			.unwrap()
	);
	p.id
}

/// The `from` guard is the replay defence `settle` has, and `false` must mean *nothing* was
/// written — not the status alone, and not the allocation alone.
#[tokio::test]
async fn record_refund_is_guarded_by_the_current_status() {
	let db = TmpDb::new("refund-guard");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = settled(&store, inv.id).await;

	let mut r = refund_record(p, Some(inv.id), GROSS);
	r.from = vec![PaymentState::Pending];
	assert!(!store.record_refund(&r).await.unwrap());

	assert_eq!(store.payment(p).await.unwrap().unwrap().refunded_amount, Money::ZERO);
	assert_eq!(store.allocations(p).await.unwrap()[0].amount, Money(GROSS));
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().paid_amount, Money(GROSS));
}

/// The ceiling is in the `WHERE`, not left to the table's `CHECK`: an over-refund comes back as
/// `false` the service turns into `E-PAY-AMOUNT`, never as a driver error.
#[tokio::test]
async fn a_refund_past_the_payment_is_refused() {
	let db = TmpDb::new("refund-ceiling");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = settled(&store, inv.id).await;

	assert!(!store.record_refund(&refund_record(p, Some(inv.id), GROSS + 1)).await.unwrap());
	assert_eq!(store.payment(p).await.unwrap().unwrap().refunded_amount, Money::ZERO);

	// Two partials that together fit are both legal; the third one over the line is not. Each
	// carries the `refunded_amount` it was computed against, which is what makes it a *distinct*
	// refund rather than the first one recorded twice.
	let mut half = refund_record(p, Some(inv.id), GROSS / 2);
	half.to = PaymentState::Succeeded;
	assert!(store.record_refund(&half).await.unwrap());
	half.expect_refunded = Money(GROSS / 2);
	assert!(store.record_refund(&half).await.unwrap());

	let mut over = refund_record(p, Some(inv.id), 1);
	over.expect_refunded = Money(GROSS / 2 * 2);
	assert!(!store.record_refund(&over).await.unwrap());
}

/// A refund reverses the allocation and the invoice walks back with the sum — the same
/// recompute `settle` does, so `PAID` becomes `ISSUED` and `paid_at` clears.
#[tokio::test]
async fn a_refund_reverses_the_allocation_and_the_invoice_cache() {
	let db = TmpDb::new("refund-reverses");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = settled(&store, inv.id).await;
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().status, InvoiceStatus::Paid);

	assert!(store.record_refund(&refund_record(p, Some(inv.id), GROSS)).await.unwrap());

	let back = store.payment(p).await.unwrap().unwrap();
	assert_eq!(back.refunded_amount, Money(GROSS));
	assert_eq!(back.status, PaymentState::Refunded);
	let rows = store.allocations(p).await.unwrap();
	assert_eq!(rows.len(), 1, "the negative lands on the pair, never as a second row");
	assert_eq!(rows[0].amount, Money::ZERO);

	let reversed = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(reversed.paid_amount, Money::ZERO);
	assert!(reversed.paid_at.is_none());
	assert_eq!(reversed.status, InvoiceStatus::Issued);
}

/// The deliberate difference from `settle`, which rolls the whole transaction back when the
/// invoice refuses the recompute: the gateway has already given the money back, so a refund
/// against an invoice stornoed since must still be recorded on the payment.
#[tokio::test]
async fn a_refund_against_a_stornoed_invoice_still_records() {
	let db = TmpDb::new("refund-stornoed");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	// Part-paid, so the invoice is still `ISSUED` and `mark_stornoed` will take it.
	let p = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();
	assert!(
		store
			.settle(&settlement(p.id, inv.id, GROSS - 1, PaymentState::Pending))
			.await
			.unwrap()
	);
	assert!(store.mark_stornoed(inv.id).await.unwrap());

	assert!(
		store
			.record_refund(&refund_record(p.id, Some(inv.id), GROSS - 1))
			.await
			.unwrap()
	);
	assert_eq!(store.payment(p.id).await.unwrap().unwrap().refunded_amount, Money(GROSS - 1));
	// The invoice is out of `('ISSUED','PAID')`, so its cache is untouched — and unchecked.
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().status, InvoiceStatus::Stornoed);
}

/// What the dunning sweep and the admin aging list both read. `PAID` is excluded by
/// `paid_amount < gross` as well as by status: a partially paid invoice is still overdue for
/// the remainder.
#[tokio::test]
async fn overdue_invoices_is_the_aging_list() {
	let db = TmpDb::new("overdue");
	let store = setup(&db).await;

	let overdue = issued(&store).await;
	let part = issued(&store).await;
	let paid = issued(&store).await;
	let draft = draft(&store).await;

	let part_p = store.create_payment(&new_payment(Some(part.id), None)).await.unwrap();
	assert!(
		store
			.settle(&settlement(part_p.id, part.id, GROSS - 1, PaymentState::Pending))
			.await
			.unwrap()
	);
	let paid_p = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-9".into()),
			..new_payment(Some(paid.id), None)
		})
		.await
		.unwrap();
	assert!(
		store
			.settle(&settlement(paid_p.id, paid.id, GROSS, PaymentState::Pending))
			.await
			.unwrap()
	);

	let rows = store.overdue_invoices(None, None, 50).await.unwrap();
	let ids: Vec<i64> = rows.iter().map(|r| r.invoice_id).collect();
	assert_eq!(ids, vec![overdue.id, part.id], "oldest first, and only what is still owed");
	assert!(!ids.contains(&paid.id) && !ids.contains(&draft.id));

	// `issue_input`'s `due_date` is 2026-02-08, so both figures are computed, not stored.
	assert_eq!(rows[0].due_date, "2026-02-08");
	assert!(rows[0].days_overdue > 0);
	assert_eq!(rows[0].outstanding, Money(GROSS));
	assert_eq!(rows[1].outstanding, Money(1));
	assert_eq!(rows[0].org_id, ORG);

	// Org-filtered for the aging list; the sweep passes `None` and gets every org's.
	assert_eq!(store.overdue_invoices(Some(ORG), None, 50).await.unwrap().len(), 2);
	assert!(store.overdue_invoices(Some(ORG + 1), None, 50).await.unwrap().is_empty());
}

/// `None` is every org, for the operator paths: they are gated by `require_operator`, and
/// scoping them by `ctx.org_id` refused the operator the payment it had just created.
#[tokio::test]
async fn payment_by_uid_none_crosses_orgs() {
	let db = TmpDb::new("by-uid-scope");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();

	assert_eq!(store.payment_by_uid(None, &p.uid).await.unwrap().unwrap().id, p.id);
	assert_eq!(store.payment_by_uid(Some(ORG), &p.uid).await.unwrap().unwrap().id, p.id);
	assert!(store.payment_by_uid(Some(ORG + 1), &p.uid).await.unwrap().is_none());
}

/// An empty `from` renders `status IN ()`, which only SQLite accepts. It matches nothing and
/// answers `false` rather than becoming an unguarded `UPDATE` on a second adapter.
#[tokio::test]
async fn an_empty_from_moves_nothing() {
	let db = TmpDb::new("empty-from");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();

	assert!(!store.advance_status(p.id, PaymentState::Succeeded, &[]).await.unwrap());
	assert_eq!(store.payment(p.id).await.unwrap().unwrap().status, PaymentState::Pending);
}

/// A partial refund lowers the allocation sum and leaves the status `SUCCEEDED`, so the alert
/// counted every refunded payment as money that settles nothing — and never stopped.
#[tokio::test]
async fn a_partially_refunded_payment_is_not_unallocated() {
	let db = TmpDb::new("unalloc-refund");
	let store = setup(&db).await;
	let inv = issued(&store).await;
	let p = settled(&store, inv.id).await;

	let mut half = refund_record(p, Some(inv.id), GROSS / 2);
	half.to = PaymentState::Succeeded;
	half.reverse = Money(GROSS / 2);
	assert!(store.record_refund(&half).await.unwrap());

	let (n, _) = store.unallocated_payments(Timestamp(Timestamp::now().0 + 60)).await.unwrap();
	assert_eq!(n, 0, "what went back is not money that settles nothing");
}

/// An ERROR alert nobody can clear is an alert nobody reads: retrying the refund route after
/// the cause is fixed writes a `PAYMENT_REFUND` for the same payment, and that is what resolves
/// the discrepancy.
#[tokio::test]
async fn a_retried_refund_clears_its_discrepancy() {
	let db = TmpDb::new("refund-discrepancy");
	let store = setup(&db).await;
	let audit = |uid: &'static str, action: &'static str, at: i64| {
		sqlx::query(
			"INSERT INTO audit_logs (at, entity, entity_id, action) VALUES (?, 'payment', ?, ?)",
		)
		.bind(at)
		.bind(uid)
		.bind(action)
		.execute(store.write_pool())
	};

	audit("pay_a", "PAYMENT_REFUND_UNRECORDED", 100).await.unwrap();
	assert_eq!(store.refund_discrepancies().await.unwrap(), (1, Some(Timestamp(100))));

	// Another payment's successful refund says nothing about this one.
	audit("pay_b", "PAYMENT_REFUND", 200).await.unwrap();
	assert_eq!(store.refund_discrepancies().await.unwrap().0, 1);

	audit("pay_a", "PAYMENT_REFUND", 300).await.unwrap();
	assert_eq!(store.refund_discrepancies().await.unwrap(), (0, None));
}

/// A page of payments costs one read, not one per row.
#[tokio::test]
async fn allocations_for_reads_a_page_at_once() {
	let db = TmpDb::new("alloc-batch");
	let store = setup(&db).await;
	let a = issued(&store).await;
	let b = issued(&store).await;

	let p1 = store.create_payment(&new_payment(Some(a.id), None)).await.unwrap();
	let p2 = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-2".into()),
			..new_payment(Some(b.id), None)
		})
		.await
		.unwrap();
	assert!(store.settle(&settlement(p1.id, b.id, 1, PaymentState::Pending)).await.unwrap());

	let rows = store.allocations_for(&[p2.id, p1.id]).await.unwrap();
	assert_eq!(
		rows.iter().map(|r| (r.payment_id, r.invoice_id)).collect::<Vec<_>>(),
		vec![(p1.id, a.id), (p1.id, b.id), (p2.id, b.id)],
		"grouped by payment, then invoice, whatever order was asked for"
	);
	assert!(store.allocations_for(&[]).await.unwrap().is_empty());
}

/// `sweep_drafts` is unattended, so it makes the open-payment check `bookings::discard` makes
/// with `E-BOOK-PAYMENT-OPEN`: dropping the link row sends a payment that later succeeds into
/// `settle_full`'s "no invoice" branch — charged, and unallocated.
#[tokio::test]
async fn sweep_drafts_leaves_a_draft_with_a_live_payment() {
	let db = TmpDb::new("sweep-live");
	let store = setup(&db).await;
	let live = draft(&store).await;
	let dead = draft(&store).await;

	store.create_payment(&new_payment(Some(live.id), None)).await.unwrap();
	let failed = store
		.create_payment(&NewPayment {
			provider_ref: Some("prv-2".into()),
			status: PaymentState::Failed,
			..new_payment(Some(dead.id), None)
		})
		.await
		.unwrap();
	assert_eq!(store.payment(failed.id).await.unwrap().unwrap().status, PaymentState::Failed);

	let cutoff = Timestamp(Timestamp::now().0 + 60);
	assert_eq!(store.sweep_drafts(cutoff).await.unwrap(), 1);
	assert!(store.invoice_by_id(live.id).await.unwrap().is_some(), "its gateway payment is live");
	assert!(store.invoice_by_id(dead.id).await.unwrap().is_none());
}

/// The money has already arrived, which is worse than in flight. `settle_full` issues before it
/// settles, and when that issue fails — no exchange rate, no seller version, a numbering
/// conflict — the invoice stays `PENDING` with a `SUCCEEDED` payment against it and nothing
/// re-drives it. The sweep then deleted the invoice the customer had paid for, and its
/// allocation row with it.
#[tokio::test]
async fn sweep_drafts_leaves_an_unissued_invoice_that_was_paid() {
	let db = TmpDb::new("sweep-paid");
	let store = setup(&db).await;
	let paid = draft(&store).await;

	let p = store
		.create_payment(&NewPayment {
			status: PaymentState::Succeeded,
			..new_payment(Some(paid.id), None)
		})
		.await
		.unwrap();
	// What `settle_full` leaves behind when `issue_if_unissued` fails: still locked, never
	// numbered, and the link row is the only thing pointing at the money.
	sqlx::query("UPDATE invoices SET status = 'PENDING' WHERE id = ?")
		.bind(paid.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	let cutoff = Timestamp(Timestamp::now().0 + 60);
	assert_eq!(store.sweep_drafts(cutoff).await.unwrap(), 0);
	assert!(store.invoice_by_id(paid.id).await.unwrap().is_some());
	assert_eq!(store.allocations(p.id).await.unwrap().len(), 1, "the money trail survives too");
}

/// `idx_payment_provider_ref` is unique over `(provider, provider_ref)` — the webhook replay
/// defence: the callback carries only the gateway's id, and two rows under it would make
/// `payment_by_provider_ref` answer with either.
#[tokio::test]
async fn a_provider_reference_is_claimed_once() {
	let db = TmpDb::new("provider-ref");
	let store = setup(&db).await;
	let inv = issued(&store).await;

	store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap();
	let err = store.create_payment(&new_payment(Some(inv.id), None)).await.unwrap_err();
	assert!(matches!(err, Error::Conflict(_)), "{err:?}");

	// Per provider, not global: two gateways may mint the same id.
	store
		.create_payment(&NewPayment {
			provider: Some("other".into()),
			..new_payment(Some(inv.id), None)
		})
		.await
		.unwrap();
}

/// `apply_state` allocates nothing for a `PARTIALLY_SUCCEEDED` payment by design — `fetch_state`
/// answers with a status and no amount — so the operator is the only one who can place it. The
/// alert filtered on `SUCCEEDED`, and nothing told them the money had arrived at all.
#[tokio::test]
async fn a_partial_that_settles_nothing_is_unallocated_money() {
	let db = TmpDb::new("unalloc-partial");
	let store = setup(&db).await;
	let inv = issued(&store).await;

	let p = store
		.create_payment(&NewPayment {
			status: PaymentState::PartiallySucceeded,
			..new_payment(Some(inv.id), None)
		})
		.await
		.unwrap();
	// Only the zero link row, and nothing ever stamped `received_at` — the alert's clock falls
	// back to `updated_at` for exactly this row.
	assert_eq!(store.allocations(p.id).await.unwrap()[0].amount, Money::ZERO);

	let (n, _) = store.unallocated_payments(Timestamp(Timestamp::now().0 + 60)).await.unwrap();
	assert_eq!(n, 1, "money arrived and settles nothing");
}

// vim: ts=4
