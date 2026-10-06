//! `InvoiceStore` conformance: gapless numbering under contention, a rolled-back issue consuming
//! no number, immutability after ISSUE, the terminal-status guards, the storno chain at row
//! level, `request_id` idempotency, seller versioning, the summary and list query semantics, and
//! the `NavStore` invariants `adapter-contract.md` names, since this module owns the only fixture
//! that can issue an invoice. The index plans the sweeps and the audit export are selected by are
//! backend-specific and stay in each adapter's own tests.
//!
//! The service-handle half lives in `crates/invoice/tests/invoice_service.rs`: a test that
//! drives `Invoices` goes there, a test that drives `InvoiceStore` goes here.

use mintworks_core::{ids::SellerId, prelude::*};
use mintworks_invoice::{
	draft::Priced,
	store::{
		BuyerSnapshot, Invoice, InvoiceFilter, InvoiceKind, InvoicePatch, InvoiceStatus,
		InvoiceStore, InvoiceVatGroup, IssueInvoice, NewInvoice, NewInvoiceLine, PartyKind,
		PartyPatch, PaymentMethod, Seller, SellerVersionPatch, SellerVersionStatus,
	},
	vat::VatCode,
};
use mintworks_nav::{NavOp, NavVerdict, store::NavStore};
use serde_json::json;

use crate::{Harness, fresh};

const ORG: i64 = 1;

/// The platform root, moved off its natural id 1 so the fixture's own org can have it. The
/// framework finds the root by `kind = 'ROOT'` and never by its value.
const ROOT: i64 = 0;

/// The fixture's one seller. `put_seller` does not autoincrement, so the id is chosen here.
const SELLER: i64 = 1;

/// The version `seed_seller` publishes. It is the first row `seller_versions` ever gets, so it
/// is `1`; a test that publishes a second one names the id `publish_seller_version` returned.
const SELLER_VER: i64 = 1;

/// The minimum the foreign keys demand on top of a fresh database: one account, one org,
/// seller 1. `HUF` is already seeded. Public for the adapters' backend-specific invoice tests.
#[doc(hidden)]
pub async fn setup<H: Harness>(h: &H)
where
	H::Store: InvoiceStore,
{
	h.exec(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
		&[],
	)
	.await;
	h.exec("UPDATE orgs SET id = ? WHERE kind = 'ROOT'", &[json!(ROOT)]).await;
	h.exec(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (?, 'org_t', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
		&[json!(ORG)],
	)
	.await;
	seed_seller(h.store()).await;
}

/// `put_seller` plus one published version — the least a suite needs before an invoice can be
/// issued, since `issue` refuses a seller with no `CURRENT` version.
async fn seed_seller<S: InvoiceStore>(store: &S) {
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

async fn draft<S: InvoiceStore>(store: &S, request_id: Option<&str>) -> Invoice {
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

/// An issued invoice — `number IS NOT NULL`. Public for the adapters' index-plan tests.
#[doc(hidden)]
pub async fn issued<S: InvoiceStore>(store: &S) -> Invoice {
	let inv = draft(store, None).await;
	store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();
	inv
}

/// One rejected `CREATE` filing, so the live-submission index has a row to serve. Public for the
/// adapters' index-plan tests.
#[doc(hidden)]
pub async fn fail_submission<S: NavStore>(store: &S, invoice_id: i64) {
	let id = store
		.create_submission(invoice_id, NavOp::Create, "<InvoiceData/>")
		.await
		.unwrap()
		.unwrap();
	store
		.finish(
			id,
			Some(NavVerdict::Rejected),
			Some(("INVOICE_NUMBER_NOT_UNIQUE", "dup")),
			Timestamp::now(),
		)
		.await
		.unwrap();
}

pub async fn concurrent_issue_is_unique_and_gapless<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	const N: i64 = 8;

	let h = fresh!(H, "invoice-gapless");
	setup(&h).await;
	let store = h.store();

	let mut ids = Vec::new();
	for _ in 0..N {
		let d = draft(store, None).await;
		ids.push((d.id, d.version));
	}

	// A second store over the same database. A store's writer pool may be one connection, so two
	// tasks on a single store could queue in the pool instead of racing for the write lock.
	let other = h.reopen().await;

	let mut tasks = Vec::new();
	for (i, (id, version)) in ids.into_iter().enumerate() {
		let s = if i % 2 == 0 { store.clone() } else { other.clone() };
		tasks.push(tokio::spawn(
			async move { s.issue(id, &issue_input(id, 100_000), version).await },
		));
	}

	let mut numbers = Vec::new();
	for t in tasks {
		numbers.push(t.await.unwrap().unwrap().number.unwrap());
	}
	numbers.sort();

	let expected: Vec<String> = (1..=N).map(|n| format!("A2026/{n:06}")).collect();
	assert_eq!(numbers, expected, "numbers must be unique and gapless");
}

pub async fn rolled_back_issue_consumes_no_number<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-rollback");
	setup(&h).await;
	let store = h.store();

	let first = draft(store, None).await;
	store
		.issue(first.id, &issue_input(first.id, 100_000), first.version)
		.await
		.unwrap();

	// Rewind the series so the next allocation renders a number that already exists:
	// `idx_invoice_number` rejects the freeze, and the whole issue transaction rolls back.
	h.exec("UPDATE doc_series SET next_no = 1", &[]).await;

	let second = draft(store, None).await;
	assert!(
		store
			.issue(second.id, &issue_input(second.id, 100_000), second.version)
			.await
			.is_err()
	);

	let next_no = h.scalar_i64("SELECT next_no FROM doc_series", &[]).await;
	assert_eq!(next_no, 1, "a rolled-back issue must not consume a number");

	let again = store.invoice_by_id(second.id).await.unwrap().unwrap();
	assert!(matches!(again.status, InvoiceStatus::Draft));
	assert!(again.number.is_none());
}

pub async fn issued_invoice_cannot_be_mutated<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-immutable");
	setup(&h).await;
	let store = h.store();

	let inv = draft(store, None).await;
	let issued = store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();

	assert!(store.update_draft(inv.id, &InvoicePatch::default()).await.unwrap().is_none());
	assert!(
		!store
			.replace_draft_lines(
				inv.id,
				None,
				&Priced {
					lines: vec![],
					groups: vec![],
					net: Money(1),
					vat: Money(1),
					gross: Money(2),
				},
				issued.version,
			)
			.await
			.unwrap()
	);
	assert!(!store.delete_draft(inv.id).await.unwrap());

	let after = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(after.gross, issued.gross);
	assert_eq!(after.number, issued.number);
}

/// `set_invoice_status` took any `InvoiceStatus` and the adapter rejected the illegal ones in
/// SQL — the state machine living in the adapter. `mark_paid` and `mark_stornoed` replace it:
/// both hardcode `ISSUED` as the predecessor, so `-> DRAFT` and `-> ISSUED` are not writable
/// at all. The `trg_invoice_status_transition` trigger this replaces existed because
/// `set_invoice_status` let a caller name `STORNOED -> ISSUED` and only SQL refused it. With
/// the method split no trait call moves a terminal invoice anywhere — pinned here, because a
/// future `mark_*` with a looser predicate would resurrect one silently. Raw SQL against the
/// tables is the consumer's own problem, by contract.
pub async fn no_trait_path_leads_out_of_a_terminal_status<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-transitions");
	setup(&h).await;
	let store = h.store();

	for (terminal, set_paid_ok) in [(InvoiceStatus::Paid, true), (InvoiceStatus::Stornoed, false)] {
		let inv = draft(store, None).await;
		let issued = store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();
		// The one legal move in, from ISSUED.
		assert!(if terminal == InvoiceStatus::Paid {
			store.mark_paid(inv.id).await.unwrap()
		} else {
			store.mark_stornoed(inv.id).await.unwrap()
		});

		// Every trait path out, all of which must miss.
		assert!(!store.mark_paid(inv.id).await.unwrap(), "{terminal:?}");
		assert!(!store.mark_stornoed(inv.id).await.unwrap(), "{terminal:?}");
		assert!(
			store.update_draft(inv.id, &InvoicePatch::default()).await.unwrap().is_none(),
			"{terminal:?}"
		);
		assert!(!store.delete_draft(inv.id).await.unwrap(), "{terminal:?}");
		// `set_paid` is the exception: recording an amount against a paid invoice is a
		// permitted post-issue write, against a cancelled one it is not.
		assert_eq!(
			store.set_paid(inv.id, issued.gross, Some(Timestamp::now())).await.unwrap(),
			set_paid_ok,
			"{terminal:?}"
		);

		let after = store.invoice_by_id(inv.id).await.unwrap().unwrap();
		assert_eq!(after.status, terminal);
		assert_eq!(after.number, issued.number);
	}

	// A miss is reported, not swallowed — `set_paid` used to discard `rows_affected`, so a
	// payment allocated against a wrong id reported success while the amount landed nowhere.
	assert!(!store.mark_paid(9_999).await.unwrap());
	assert!(!store.set_paid(9_999, Money(1), None).await.unwrap());
}

/// The gateway lock. `set_status` is a compare-and-set so that two callbacks arriving together
/// cannot both act on one transition, and `PENDING` is a draft in every way but mutability:
/// every draft write refuses it, `update_notes` still goes through, and `issue` numbers it
/// exactly once — straight from `PENDING`, with no unlocked window in between.
pub async fn a_pending_invoice_is_frozen_but_still_issues<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-pending-lock");
	setup(&h).await;
	let store = h.store();

	let inv = draft(store, None).await;
	assert!(
		store
			.set_status(inv.id, InvoiceStatus::Draft, InvoiceStatus::Pending)
			.await
			.unwrap()
	);
	// The same move again finds no row: the first caller already made it.
	assert!(
		!store
			.set_status(inv.id, InvoiceStatus::Draft, InvoiceStatus::Pending)
			.await
			.unwrap()
	);
	assert!(
		!store
			.set_status(9_999, InvoiceStatus::Draft, InvoiceStatus::Pending)
			.await
			.unwrap()
	);
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().status, InvoiceStatus::Pending);

	assert!(store.update_draft(inv.id, &InvoicePatch::default()).await.unwrap().is_none());
	assert!(
		!store
			.replace_draft_lines(
				inv.id,
				None,
				&Priced {
					lines: vec![],
					groups: vec![],
					net: Money(1),
					vat: Money(1),
					gross: Money(2),
				},
				inv.version,
			)
			.await
			.unwrap()
	);
	assert!(!store.delete_draft(inv.id).await.unwrap(), "the zero link row must not be dropped");
	// The one write with no status predicate, locked or not.
	assert!(store.update_notes(inv.id, Some("varakozik")).await.unwrap().is_some());

	let locked = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	let issued = store.issue(locked.id, &issue_input(locked.id, 100_000), locked.version).await;
	let issued = issued.unwrap();
	assert_eq!(issued.status, InvoiceStatus::Issued);
	assert_eq!(issued.number.as_deref(), Some("A2026/000001"));

	// And back the other way, for a payment that died.
	let other = draft(store, Some("unwound")).await;
	assert!(
		store
			.set_status(other.id, InvoiceStatus::Draft, InvoiceStatus::Pending)
			.await
			.unwrap()
	);
	assert!(
		store
			.set_status(other.id, InvoiceStatus::Pending, InvoiceStatus::Draft)
			.await
			.unwrap()
	);
	assert!(store.update_draft(other.id, &InvoicePatch::default()).await.unwrap().is_some());
}

/// `sweep_drafts` deletes *abandoned* drafts, and it keyed on `created_at` — so a cart
/// opened a month ago and edited this morning was destroyed under the customer.
pub async fn the_sweep_spares_a_draft_that_is_still_being_edited<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-sweep-age");
	setup(&h).await;
	let store = h.store();

	let now = Timestamp::now().0;
	let old = now - 40 * 86_400;
	let cutoff = Timestamp(now - 30 * 86_400);

	let live = draft(store, Some("live")).await;
	let abandoned = draft(store, Some("abandoned")).await;
	// Both created well before the cutoff; only one has been touched since.
	h.exec(
		"UPDATE invoices SET created_at = ?, updated_at = ? WHERE id = ?",
		&[json!(old), json!(now), json!(live.id)],
	)
	.await;
	h.exec(
		"UPDATE invoices SET created_at = ?, updated_at = ? WHERE id = ?",
		&[json!(old), json!(old), json!(abandoned.id)],
	)
	.await;

	assert_eq!(store.sweep_drafts(cutoff).await.unwrap(), 1);
	assert!(store.invoice_by_id(live.id).await.unwrap().is_some(), "an edited cart survives");
	assert!(store.invoice_by_id(abandoned.id).await.unwrap().is_none());
}

/// A `PENDING` invoice past the sweep's horizon is an abandoned cart again: nothing re-asks its
/// gateway any more, so the lock would otherwise be permanent. One whose payment is still live
/// is spared — dropping its zero link row sends a payment that later succeeds into
/// `settle_full`'s "no invoice" branch, charged and unallocated.
pub async fn the_sweep_collects_a_dead_lock_and_spares_a_live_one<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-sweep-pending");
	setup(&h).await;
	let store = h.store();

	let now = Timestamp::now().0;
	let old = now - 40 * 86_400;
	let cutoff = Timestamp(now - 30 * 86_400);

	let dead = draft(store, Some("dead")).await;
	let held = draft(store, Some("held")).await;
	for inv in [&dead, &held] {
		assert!(
			store
				.set_status(inv.id, InvoiceStatus::Draft, InvoiceStatus::Pending)
				.await
				.unwrap()
		);
		h.exec(
			"UPDATE invoices SET created_at = ?, updated_at = ? WHERE id = ?",
			&[json!(old), json!(old), json!(inv.id)],
		)
		.await;
		// `held`'s gateway payment is still open; `dead`'s gave up long ago.
		h.exec(
			"INSERT INTO payments
			   (id, uid, org_id, kind, status, amount, currency, created_at, updated_at)
			   VALUES (?, ?, ?, 'STUB', ?, 1000, 'HUF', ?, ?)",
			&[
				json!(inv.id),
				json!(format!("pay_{}", inv.id)),
				json!(ORG),
				json!(if inv.id == held.id { "AWAITING_USER" } else { "EXPIRED" }),
				json!(old),
				json!(old),
			],
		)
		.await;
		h.exec(
			"INSERT INTO payment_allocations (payment_id, invoice_id, amount, allocated_at)
			   VALUES (?, ?, 0, ?)",
			&[json!(inv.id), json!(inv.id), json!(old)],
		)
		.await;
	}

	assert_eq!(store.sweep_drafts(cutoff).await.unwrap(), 1);
	assert!(store.invoice_by_id(dead.id).await.unwrap().is_none(), "a dead lock is a cart again");
	assert!(store.invoice_by_id(held.id).await.unwrap().is_some(), "a live payment holds it");
}

pub async fn storno_negates_and_happens_once<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-storno");
	setup(&h).await;
	let store = h.store();

	let inv = draft(store, None).await;
	store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();

	let new = new_invoice(None, InvoiceKind::Storno, Some(inv.id));
	let st = store.storno(inv.id, &new, &issue_input(0, -100_000)).await.unwrap();

	assert!(matches!(st.kind, InvoiceKind::Storno));
	assert_eq!(st.original_invoice_id, Some(inv.id));
	assert_eq!(st.number.as_deref(), Some("A2026/000002"), "same series as the original");
	assert_eq!(st.gross, Money(-127_000));
	assert!(matches!(
		store.invoice_by_id(inv.id).await.unwrap().unwrap().status,
		InvoiceStatus::Stornoed
	));

	// At most once, whichever guard gets there first.
	assert!(store.storno(inv.id, &new, &issue_input(0, -100_000)).await.is_err());
}

/// `insert_draft` mapped *every* unique violation to `duplicate_request_id`, so a caller that
/// sent no `request_id` got a 409 naming a key it never sent. Here the collision is
/// `idx_invoice_storno_once`.
pub async fn a_conflict_without_a_request_id_does_not_name_one<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-requestid-none");
	setup(&h).await;
	let store = h.store();

	let original = draft(store, None).await;
	let cancel = new_invoice(None, InvoiceKind::Storno, Some(original.id));
	store.create_draft(&cancel).await.unwrap();

	let err = store.create_draft(&cancel).await.unwrap_err().to_string();
	assert!(!err.contains("request_id"), "{err}");

	// And the idempotency-replay answer is unchanged where a key really was sent.
	let keyed = draft(store, Some("req-1")).await;
	let err = store
		.create_draft(&new_invoice(Some("req-1"), InvoiceKind::Normal, None))
		.await
		.unwrap_err()
		.to_string();
	assert!(err.contains("request_id"), "{err}");
	assert_eq!(
		store.invoice_by_request_id(ORG, "req-1").await.unwrap().map(|i| i.id),
		Some(keyed.id)
	);
}

/// The uniqueness used to be global, so one org taking `"sub-2026-01"` made every other
/// org's subscription job answer `404` on a *create*, permanently, for that key. Any org
/// could squat any other org's natural idempotency keys.
pub async fn two_orgs_can_hold_the_same_request_id<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-requestid-orgs");
	setup(&h).await;
	let store = h.store();
	h.exec(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (2, 'org_u', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Masik', 1, 0)",
		&[],
	)
	.await;

	let mine = draft(store, Some("sub-2026-01")).await;
	let theirs = store
		.create_draft(&NewInvoice {
			org_id: 2,
			..new_invoice(Some("sub-2026-01"), InvoiceKind::Normal, None)
		})
		.await
		.expect("another org's key is not this org's key");

	assert_ne!(mine.id, theirs.id);
	// And each lookup stays inside its own org.
	assert_eq!(
		store.invoice_by_request_id(ORG, "sub-2026-01").await.unwrap().map(|i| i.id),
		Some(mine.id)
	);
	assert_eq!(
		store.invoice_by_request_id(2, "sub-2026-01").await.unwrap().map(|i| i.id),
		Some(theirs.id)
	);
}

/// `doc_series`' primary key includes `year`, so the first invoice of every January took
/// the INSERT path and picked up the column default. Nothing anywhere writes `format`, so an
/// operator's hand-set custom format silently reverted at each year boundary — on numbers that
/// are statutorily immutable once issued. It is carried forward from the previous year now.
pub async fn a_custom_series_format_survives_the_year_boundary<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-series-format-year");
	setup(&h).await;
	let store = h.store();

	let first = draft(store, None).await;
	let issued = store
		.issue(first.id, &issue_input(first.id, 100_000), first.version)
		.await
		.unwrap();
	assert_eq!(issued.number.as_deref(), Some("A2026/000001"));

	// What an operator does by hand: there is no endpoint for it.
	h.exec("UPDATE doc_series SET format = 'SZLA-{year}-{no:04}'", &[]).await;

	let second = draft(store, None).await;
	let mut input = issue_input(second.id, 100_000);
	input.series_year = 2027;
	let next_year = store.issue(second.id, &input, second.version).await.unwrap();
	assert_eq!(
		next_year.number.as_deref(),
		Some("SZLA-2027-0001"),
		"the new year's series row reverted to the schema default"
	);

	// The carried format is on the row, not only in the rendering.
	let format = h.scalar_text("SELECT format FROM doc_series WHERE year = 2027", &[]).await;
	assert_eq!(format, "SZLA-{year}-{no:04}");

	// A series that has never been used still starts from the schema default.
	let third = draft(store, None).await;
	let mut input = issue_input(third.id, 100_000);
	input.series_code = "B".into();
	let fresh_series = store.issue(third.id, &input, third.version).await.unwrap();
	assert_eq!(fresh_series.number.as_deref(), Some("B2026/000001"));
}

/// The PDF sweep took the fifty lowest ids, so fifty issued invoices whose `RENDER_PDF`
/// fails permanently held the whole `invoice.pdf_sweep_batch` cap and every newer invoice with
/// a missing PDF was never re-enqueued. Ordering by the last render attempt is what breaks the
/// fixed set — the same property `NavStore::unfiled_invoices` needs.
pub async fn the_pdf_sweep_returns_the_least_recently_attempted_first<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-pdf-sweep-order");
	setup(&h).await;
	let store = h.store();

	// A draft is not numbered, so it is not the sweep's business.
	let never_issued = draft(store, None).await;

	let mut issued = Vec::new();
	for net in [100_000, 200_000, 300_000] {
		let d = draft(store, None).await;
		store.issue(d.id, &issue_input(d.id, net), d.version).await.unwrap();
		issued.push(d.id);
	}

	// The one with a document is done and drops out.
	let done = store.invoice_by_id(issued[0]).await.unwrap().unwrap();
	store
		.put_invoice_document(
			&mintworks_invoice::store::InvoiceDocument {
				invoice_id: issued[0],
				kind: "PDF".into(),
				sha256: "deadbeef".into(),
				bytes: 1,
				template_version: "v1".into(),
				rendered_at: Timestamp::now(),
			},
			done.version,
		)
		.await
		.unwrap();

	let found = store.issued_without_document(10).await.unwrap();
	assert_eq!(found, vec![issued[1], issued[2]], "numbered, no document, oldest attempt first");
	assert!(!found.contains(&never_issued.id));

	// A permanently failing render leaves a FAILED job row behind, and the ordering finds it by
	// payload rather than by `dedup_key` — this row has none. That attempt must push the
	// invoice behind the one nothing has tried yet, whatever their ids say.
	h.exec(
		"INSERT INTO jobs (kind, payload, status, run_at, created_at, done_at, last_error)
		   VALUES ('RENDER_PDF', ?, 'FAILED', 0, ?, ?, 'typst blew up')",
		&[
			json!(json!({ "invoiceId": issued[1] }).to_string()),
			json!(Timestamp::now().0),
			json!(Timestamp::now().0),
		],
	)
	.await;

	assert_eq!(
		store.issued_without_document(10).await.unwrap(),
		vec![issued[2], issued[1]],
		"the just-retried invoice goes to the back, so a full batch of failures cannot starve it"
	);

	// And the cap still applies: one slot, and it goes to the least recently attempted.
	assert_eq!(store.issued_without_document(1).await.unwrap(), vec![issued[2]]);
}

/// `currency::price_in` is `base * 1e6 * (10_000 + fee_bp) / (rate_e6 * 10_000)`, so a seeded
/// `fee_bp = -10000` zeroes the numerator and a 40 000.00 HUF item issues, files and prints at
/// 0.00. A `price_round_step` of 0 makes `round_to_step` a 500 on every catalogue line while
/// `ensure_price_on_step` waves ad-hoc prices through.
pub async fn a_currency_refuses_a_markup_or_a_step_that_would_misprice<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-currency-checks");
	setup(&h).await;

	for (code, step, fee_bp) in [
		("AAA", 1, -10000),    // zeroes every converted price
		("BBB", 1, -250),      // a silent 2.5% undercharge
		("CCC", 1, 1_000_001), // past the 100x ceiling the i128 product is sized for
		("DDD", 0, 0),         // the two halves of the rounding rule disagree
	] {
		h.try_exec(
			"INSERT INTO currencies (code, price_round_step, mode, fee_bp) VALUES (?, ?, 'OFFICIAL', ?)",
			&[json!(code), json!(step), json!(fee_bp)],
		)
		.await
		.expect_err("the CHECK has to refuse this");
	}

	// The bounds themselves are legal: no markup, and a 100x ceiling.
	h.exec(
		"INSERT INTO currencies (code, price_round_step, mode, fee_bp)
		 VALUES ('EEE', 1, 'OFFICIAL', 0), ('FFF', 100, 'OFFICIAL', 1000000)",
		&[],
	)
	.await;
}

/// The two CHECKs that make `mintworks_nav::export::original_number`'s refusals unreachable: an
/// audit export names a storno's original, and a blank `originalInvoiceNumber` is a file the
/// tax authority reads as a plain invoice. A second adapter owes both.
pub async fn a_storno_always_has_a_numbered_original_to_name<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-storno-original-checks");
	setup(&h).await;
	let store = h.store();

	let original = issued(store).await;
	let cancel = new_invoice(None, InvoiceKind::Storno, Some(original.id));
	let storno = store.create_draft(&cancel).await.unwrap();

	for (label, sql, id) in [
		(
			"a storno with no original",
			"UPDATE invoices SET original_invoice_id = NULL WHERE id = ?",
			storno.id,
		),
		(
			"an issued invoice with no number",
			"UPDATE invoices SET number = NULL WHERE id = ?",
			original.id,
		),
	] {
		let err = h.try_exec(sql, &[json!(id)]).await.unwrap_err();
		// Each backend words it differently; both name the CHECK.
		assert!(err.to_lowercase().contains("check constraint"), "{label}: {err}");
	}
}

// ---------------------------------------------------------------- seller versions

/// An edit is **not** a version. Repeated saves rewrite the one DRAFT row, so an operator who
/// corrects a typo four times still publishes once — and the live version the invoices freeze
/// never moves while they are typing.
pub async fn repeated_draft_saves_rewrite_one_row_and_publish_nothing<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-draft-rewrite");
	setup(&h).await;
	let store = h.store();

	for name in ["Elso", "Masodik", "Harmadik"] {
		store
			.save_seller_version_draft(
				SELLER,
				&SellerVersionPatch { name: Some(name.into()), ..Default::default() },
			)
			.await
			.unwrap();
	}

	let drafts = h
		.scalar_i64("SELECT COUNT(*) FROM seller_versions WHERE status = 'DRAFT'", &[])
		.await;
	assert_eq!(drafts, 1, "each edit made its own draft");

	let draft = store.draft_seller_version(SELLER).await.unwrap().unwrap();
	assert_eq!(draft.name, "Harmadik");
	// Seeded from the CURRENT row, so an untouched field is not blanked.
	assert_eq!(draft.tax_number, "12345678242");
	assert!(draft.valid_from.is_none(), "a draft is not in force");

	let current = store.current_seller_version(SELLER).await.unwrap().unwrap();
	assert_eq!(current.name, "Teszt Kft.", "an unpublished edit changed the live version");
	assert_eq!(store.seller_version_history(SELLER).await.unwrap().len(), 1);
}

/// Archive-then-promote, in one transaction: the old CURRENT row is stamped `superseded_at`
/// and the draft takes its place, and `idx_seller_version_current` means there is never a
/// moment with two.
pub async fn publishing_archives_the_live_version_and_promotes_the_draft<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-publish");
	setup(&h).await;
	let store = h.store();
	let before = store.current_seller_version(SELLER).await.unwrap().unwrap();

	store
		.save_seller_version_draft(
			SELLER,
			&SellerVersionPatch { name: Some("Uj Nev Kft.".into()), ..Default::default() },
		)
		.await
		.unwrap();
	let ver = store
		.publish_seller_version(SELLER, Timestamp(1_800_000_000), &|_| Ok(()))
		.await
		.unwrap();
	let ver = ver.expect("the draft was promoted");

	let current = store.current_seller_version(SELLER).await.unwrap().unwrap();
	assert_eq!(current.seller_ver, ver);
	assert_eq!(current.name, "Uj Nev Kft.");
	assert_eq!(current.valid_from, Some(Timestamp(1_800_000_000)));
	assert!(store.draft_seller_version(SELLER).await.unwrap().is_none(), "the draft survived");

	let archived = store.seller_version(before.seller_ver).await.unwrap().unwrap();
	assert_eq!(archived.status, SellerVersionStatus::Archived);
	assert_eq!(archived.superseded_at, Some(Timestamp(1_800_000_000)));

	// Newest first, and contiguous: the archived row's `superseded_at` is the live one's
	// `valid_from`, which is what answers "which version was in force on this date".
	let history = store.seller_version_history(SELLER).await.unwrap();
	assert_eq!(history.len(), 2);
	assert_eq!(history[0].seller_ver, ver);
	assert_eq!(history[1].superseded_at, history[0].valid_from);
}

/// One transaction leaves only two orders: the sync wins and publishes its own patch, or the
/// operator's draft wins and the sync writes nothing — never a blend.
pub async fn sync_seller_version_never_publishes_a_concurrent_draft<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-sync-race");
	setup(&h).await;
	let store = h.store();
	let before = store.current_seller_version(SELLER).await.unwrap().unwrap();

	let synced = SellerVersionPatch { name: Some("Sync Kft.".into()), ..seller_version() };
	let interloper =
		SellerVersionPatch { name: Some("Operator Kft.".into()), ..Default::default() };
	// A second pooled handle, so the two contend for the writer connection rather than
	// re-entering one transaction.
	let other = h.reopen().await;
	let (sync, saved) = tokio::join!(
		store.sync_seller_version(SELLER, Timestamp(1_800_000_000), &synced, &|_| Ok(())),
		other.save_seller_version_draft(SELLER, &interloper),
	);
	saved.unwrap();

	let current = store.current_seller_version(SELLER).await.unwrap().unwrap();
	match sync.unwrap() {
		Some(ver) => {
			assert_eq!(current.seller_ver, ver);
			assert_eq!(current.name, "Sync Kft.");
		}
		// The draft was already open when the transaction probed: nothing was written at all.
		None => assert_eq!(current.seller_ver, before.seller_ver),
	}
	assert_ne!(current.name, "Operator Kft.", "a concurrent draft was published");
}

/// Publishing with nothing to publish must leave the live version alone. The archive runs
/// first, so a naive implementation ends with no CURRENT row at all.
pub async fn publishing_with_no_draft_changes_nothing<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-publish-empty");
	setup(&h).await;
	let store = h.store();
	let before = store.current_seller_version(SELLER).await.unwrap().unwrap();

	assert!(
		store
			.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(()))
			.await
			.unwrap()
			.is_none()
	);

	let after = store.current_seller_version(SELLER).await.unwrap().unwrap();
	assert_eq!(after.seller_ver, before.seller_ver);
	assert_eq!(after.status, SellerVersionStatus::Current);
	assert!(
		after.superseded_at.is_none(),
		"the live version was archived with nothing to replace it"
	);
}

/// Discarding throws away the edit and nothing else.
pub async fn discarding_a_draft_leaves_the_live_version_untouched<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-discard");
	setup(&h).await;
	let store = h.store();

	assert!(!store.discard_seller_version_draft(SELLER).await.unwrap(), "there was no draft");
	store
		.save_seller_version_draft(
			SELLER,
			&SellerVersionPatch { name: Some("Elvetve".into()), ..Default::default() },
		)
		.await
		.unwrap();
	assert!(store.discard_seller_version_draft(SELLER).await.unwrap());

	assert!(store.draft_seller_version(SELLER).await.unwrap().is_none());
	let current = store.current_seller_version(SELLER).await.unwrap().unwrap();
	assert_eq!(current.name, "Teszt Kft.");
	assert_eq!(store.seller_version_history(SELLER).await.unwrap().len(), 1);
}

/// `idx_seller_version_current` is integrity, not performance: two publishes racing on one
/// draft must not leave two live versions, whichever order they land in.
pub async fn two_racing_publishes_cannot_leave_two_live_versions<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-publish-race");
	setup(&h).await;
	let store = h.store();
	store
		.save_seller_version_draft(
			SELLER,
			&SellerVersionPatch { name: Some("Verseny Kft.".into()), ..Default::default() },
		)
		.await
		.unwrap();

	let (a, b) = tokio::join!(
		{
			let store = store.clone();
			async move { store.publish_seller_version(SELLER, Timestamp(1), &|_| Ok(())).await }
		},
		{
			let store = store.clone();
			async move { store.publish_seller_version(SELLER, Timestamp(2), &|_| Ok(())).await }
		},
	);
	// One promoted the draft; the other found none and said so. Neither may have inserted a
	// second live row.
	let promoted = [a.unwrap(), b.unwrap()].into_iter().flatten().count();
	assert_eq!(promoted, 1, "both publishes claimed the same draft");

	let live = h
		.scalar_i64("SELECT COUNT(*) FROM seller_versions WHERE status = 'CURRENT'", &[])
		.await;
	assert_eq!(live, 1);
}

/// What an invoice's `seller_ver` buys: the frozen row still reads back after the seller has
/// been edited and published over.
pub async fn an_issued_invoice_resolves_the_version_it_froze<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-frozen");
	setup(&h).await;
	let store = h.store();
	let invoice = store.create_draft(&new_invoice(None, InvoiceKind::Normal, None)).await.unwrap();
	let issued = store
		.issue(invoice.id, &issue_input(invoice.id, 100_000), invoice.version)
		.await
		.unwrap();
	assert_eq!(issued.seller_ver, Some(SELLER_VER));

	store
		.save_seller_version_draft(
			SELLER,
			&SellerVersionPatch { name: Some("Utana Kft.".into()), ..Default::default() },
		)
		.await
		.unwrap();
	store
		.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();

	let frozen = store.seller_version(issued.seller_ver.unwrap()).await.unwrap().unwrap();
	assert_eq!(frozen.name, "Teszt Kft.");
	assert_eq!(frozen.status, SellerVersionStatus::Archived);
	// And the bulk read the audit export uses answers the same.
	let bulk = store.seller_versions(&[issued.seller_ver.unwrap()]).await.unwrap();
	assert_eq!(bulk.len(), 1);
	assert_eq!(bulk[0].name, "Teszt Kft.");
}

/// `NavStore::request_archived` is what `job::report` freezes batch membership on. It must
/// answer exactly what `submission_archive` shows.
pub async fn request_archived_tracks_the_archived_request<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-nav-archive-flag-agrees");
	setup(&h).await;
	let store = h.store();

	let direct = issued(store).await;
	let direct_id = store
		.create_submission(direct.id, NavOp::Create, "<direct/>")
		.await
		.unwrap()
		.unwrap();
	assert_archive(store, direct_id, Some("<direct/>")).await;

	let leader = issued(store).await;
	let claimed = store
		.claim_batch(leader.id, NavOp::Create, leader.uid.as_str(), &[])
		.await
		.unwrap()[0]
		.0;
	assert_archive(store, claimed, None).await;

	store.archive_request(claimed, "<first/>").await.unwrap();
	assert_archive(store, claimed, Some("<first/>")).await;

	// First write wins: the second attempt's text is refused and the first stands.
	store.archive_request(claimed, "<second/>").await.unwrap();
	assert_archive(store, claimed, Some("<first/>")).await;
}

/// `request_archived` says exactly what `nav_submission_xml.request_xml IS NOT NULL` says.
async fn assert_archive<S: NavStore>(store: &S, id: i64, request_xml: Option<&str>) {
	let flag = store.request_archived(id).await.unwrap();
	let archived = store.submission_archive(id).await.unwrap().and_then(|a| a.request_xml);
	assert_eq!(archived.as_deref(), request_xml);
	assert_eq!(flag, archived.is_some(), "the flag and the archive row disagree");
}

/// `release_batch` deletes a pristine member's row. Without the `ON DELETE CASCADE` on
/// `nav_submission_xml.submission_id` its archive row silently orphans.
pub async fn releasing_a_pristine_member_takes_its_archive_with_it<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-nav-release-cascades-archive");
	setup(&h).await;
	let store = h.store();

	let leader = issued(store).await;
	let member = issued(store).await;
	let claimed = store
		.claim_batch(leader.id, NavOp::Create, leader.uid.as_str(), &[member.id])
		.await
		.unwrap();
	let leader_id = claimed[0].0;
	let member_id = claimed[1].0;
	store.archive_request(member_id, "<slice/>").await.unwrap();

	store.release_batch(leader.uid.as_str(), leader_id).await.unwrap();

	assert!(store.submission(member_id).await.unwrap().is_none(), "the member row is gone");
	assert!(store.submission_archive(member_id).await.unwrap().is_none(), "and so is its archive");
}

/// `release_batch` deletes a pristine member, and both archive writes used to be an
/// `UPDATE nav_submissions` that simply matched nothing. Through the FK child they must
/// still be a no-op rather than `FOREIGN KEY constraint failed`.
pub async fn archiving_for_a_deleted_submission_is_a_no_op<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-nav-archive-deleted-submission");
	setup(&h).await;
	let store = h.store();

	let leader = issued(store).await;
	let member = issued(store).await;
	let claimed = store
		.claim_batch(leader.id, NavOp::Create, leader.uid.as_str(), &[member.id])
		.await
		.unwrap();
	let member_id = claimed[1].0;

	store.release_batch(leader.uid.as_str(), claimed[0].0).await.unwrap();

	store.archive_request(member_id, "<x/>").await.unwrap();
	store.archive_response(member_id, "<y/>").await.unwrap();
	assert!(store.submission_archive(member_id).await.unwrap().is_none());
}

/// The batch read never joins `nav_submission_xml`: it returns up to `nav.batch_max` rows on the
/// report path, and a join would read every member's archived slice back.
pub async fn a_batch_read_does_not_carry_the_archive<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-nav-batch-read-no-archive");
	setup(&h).await;
	let store = h.store();

	let leader = issued(store).await;
	let claimed = store
		.claim_batch(leader.id, NavOp::Create, leader.uid.as_str(), &[])
		.await
		.unwrap()[0]
		.0;
	store.archive_request(claimed, "<envelope/>").await.unwrap();

	let rows = store.submissions_by_batch(leader.uid.as_str()).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert!(store.request_archived(claimed).await.unwrap());
	assert_eq!(
		store.submission_archive(claimed).await.unwrap().unwrap().request_xml.as_deref(),
		Some("<envelope/>"),
		"and the text is one separate read away"
	);
}

/// `set_status` takes both ends of the transition, and an unconstrained one is a way past
/// ISSUED-immutability: `InvoiceStore` is public, and `set_status(id, Issued, Draft)` walked a
/// numbered, NAV-filed invoice back to where `replace_draft_lines` and `issue` renumber it.
pub async fn set_status_refuses_any_pair_but_the_gateway_lock<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-set-status-guard");
	setup(&h).await;
	let store = h.store();

	let d = draft(store, None).await;
	assert!(
		store
			.set_status(d.id, InvoiceStatus::Draft, InvoiceStatus::Pending)
			.await
			.unwrap()
	);
	assert!(
		store
			.set_status(d.id, InvoiceStatus::Pending, InvoiceStatus::Draft)
			.await
			.unwrap()
	);

	let inv = issued(store).await;
	assert!(
		store
			.set_status(inv.id, InvoiceStatus::Issued, InvoiceStatus::Draft)
			.await
			.is_err(),
		"an issued invoice must not be walked back to DRAFT"
	);
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().status, InvoiceStatus::Issued);
}

/// `doc_series` is keyed on `seller_id`, so two orgs' sellers each start their own series at 1
/// and neither can consume the other's numbers — what scoping `sellers` to an org is for.
pub async fn two_sellers_number_independently<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-two-sellers");
	setup(&h).await;
	let store = h.store();

	h.exec(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (2, 'org_u', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Masik', 1, 0)",
		&[],
	)
	.await;
	store
		.put_seller(&Seller {
			id: 2,
			uid: SellerId::generate(),
			org_id: 2,
			series_code: "B".into(),
			..seller()
		})
		.await
		.unwrap();
	store.save_seller_version_draft(2, &seller_version()).await.unwrap();
	let ver2 = store
		.publish_seller_version(2, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap()
		.unwrap();

	let mut numbers = Vec::new();
	// `series_code` and `seller_ver` are the caller's input, not the store's: `mintworks-invoice`
	// takes them off the seller it resolved.
	for (org_id, seller_id, code, ver) in [(ORG, SELLER, "A", SELLER_VER), (2, 2, "B", ver2)] {
		let d = store
			.create_draft(&NewInvoice {
				org_id,
				seller_id,
				..new_invoice(None, InvoiceKind::Normal, None)
			})
			.await
			.unwrap();
		let input = IssueInvoice {
			series_code: code.into(),
			seller_ver: ver,
			..issue_input(d.id, 100_000)
		};
		let issued = store.issue(d.id, &input, d.version).await.unwrap();
		numbers.push(issued.number.unwrap());
	}
	assert_eq!(numbers, vec!["A2026/000001".to_owned(), "B2026/000001".to_owned()]);
}

/// `put_seller` is an upsert keyed on `id`. Unguarded, a second call moved a taxpayer id, its
/// NAV credentials and its `doc_series` counter to another org — a seller takeover. `org_id`
/// is now matched by the upsert, so a mismatched pair is refused rather than rewritten, and a
/// second store adapter must reimplement that.
pub async fn create_seller_only_mints_on_an_active_shared_org<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-create-kind");
	setup(&h).await;
	let store = h.store();
	h.exec(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at, status)
		 VALUES (2, 'org_p', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'PERSONAL', 'P', 1, 0, 'ACTIVE'),
		        (3, 'org_s', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'S', 1, 0, 'SUSPENDED'),
		        (4, 'org_a', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'A', 1, 0, 'ACTIVE')",
		&[],
	)
	.await;
	let mint = |id: i64| Seller { id, org_id: id, uid: SellerId::generate(), ..seller() };

	for org in [ROOT, 2, 3, 99] {
		assert!(!store.create_seller(&mint(org)).await.unwrap(), "minted on org {org}");
		assert!(store.seller_by_id(org).await.unwrap().is_none());
	}
	assert!(store.create_seller(&mint(4)).await.unwrap());
	assert_eq!(store.seller_by_id(4).await.unwrap().unwrap().org_id, 4);
}

pub async fn create_seller_never_overwrites_an_existing_row<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-create-overwrite");
	setup(&h).await;
	let store = h.store();
	let before = store.seller_by_id(SELLER).await.unwrap().unwrap();

	// `ORG` is an active SHARED org, and `SELLER` already sits on its id.
	let err = store
		.create_seller(&Seller { uid: SellerId::generate(), series_code: "Z".into(), ..seller() })
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-CONFLICT", "{err:?}");
	let after = store.seller_by_id(SELLER).await.unwrap().unwrap();
	assert_eq!((after.uid.as_str(), after.series_code), (before.uid.as_str(), before.series_code));
}

pub async fn put_seller_cannot_move_a_seller_to_another_org<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-immovable");
	setup(&h).await;
	let store = h.store();
	let before = store.seller_by_id(SELLER).await.unwrap().unwrap();

	h.exec(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (2, 'org_u', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Masik', 1, 0)",
		&[],
	)
	.await;
	// An org-matching upsert always affects one row, so zero rows is unambiguous.
	let err = store
		.put_seller(&Seller {
			id: SELLER,
			uid: SellerId::generate(),
			org_id: 2,
			series_code: "B".into(),
			nav_base_url: "https://evil.invalid".into(),
			..seller()
		})
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-CONFLICT", "{err:?}");

	let after = store.seller_by_id(SELLER).await.unwrap().unwrap();
	assert_eq!(after.org_id, ORG, "org_id must not follow the upsert");
	assert_eq!(after.uid.as_str(), before.uid.as_str(), "nor may the public id change");
	assert_eq!(after.series_code, before.series_code, "nor an operational column be rewritten");
	assert_eq!(after.nav_base_url, before.nav_base_url);
}

/// Same `id` and `org_id` with a different `uid` used to upsert successfully and leave a row
/// the caller never sent. `uid` is matched now, not just carried.
pub async fn put_seller_refuses_a_changed_uid_on_the_same_id<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-uid-change");
	setup(&h).await;
	let store = h.store();
	let before = store.seller_by_id(SELLER).await.unwrap().unwrap().uid;

	let err = store
		.put_seller(&Seller { id: SELLER, uid: SellerId::generate(), ..seller() })
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-CONFLICT", "{err:?}");

	let after = store.seller_by_id(SELLER).await.unwrap().unwrap();
	assert_eq!(after.uid.as_str(), before.as_str(), "the stored uid must not follow the upsert");
}

/// A fresh `id` carrying a `uid` already in the table violates the `uid` column's own `UNIQUE`
/// before `ON CONFLICT (id)` can apply, so it escaped as a raw driver error — `E-CORE-UNAVAILABLE`
/// on a path whose whole point is a stated failure.
pub async fn put_seller_refuses_a_reused_uid<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller-uid-reuse");
	setup(&h).await;
	let store = h.store();
	let taken = store.seller_by_id(SELLER).await.unwrap().unwrap().uid;

	let err = store.put_seller(&Seller { id: 2, uid: taken, ..seller() }).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-CONFLICT", "{err:?}");
}

/// Adding HUF and EUR minor units together is the one unforgivable bug in an invoice
/// aggregate, so every bucket carries its own currency and nothing is converted.
pub async fn summary_groups_by_status_and_currency<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-summary-status");
	setup(&h).await;
	let store = h.store();
	// `invoices.currency` references `currencies(code)`, so EUR has to exist first.
	h.exec(
		"INSERT INTO currencies (code, price_round_step, mode, fixed_rate_e6, fee_bp)
		 VALUES ('EUR', 1, 'FIXED', 400000000, 0)",
		&[],
	)
	.await;

	let inv = draft(store, None).await;
	store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();
	draft(store, None).await;
	store
		.create_draft(&NewInvoice {
			currency: CurrencyCode::parse("EUR").unwrap(),
			..new_invoice(None, InvoiceKind::Normal, None)
		})
		.await
		.unwrap();

	let sum = store.invoice_summary(ORG, "2000-01", "2026-06-01", NO_SPAN).await.unwrap();
	let find = |status: InvoiceStatus, code: &str| {
		sum.statuses
			.iter()
			.find(|b| b.status == status && b.currency.as_str() == code)
			.unwrap_or_else(|| panic!("no {status:?}/{code} bucket"))
	};

	assert_eq!(sum.statuses.len(), 3);
	assert_eq!(find(InvoiceStatus::Draft, "HUF").count, 1);
	assert_eq!(find(InvoiceStatus::Draft, "EUR").count, 1);
	let issued = find(InvoiceStatus::Issued, "HUF");
	assert_eq!((issued.count, issued.gross), (1, Money(127_000)));
}

/// `fulfilment_date` is the statutory period; `issued_at` buckets in UTC.
pub async fn summary_months_use_fulfilment_date<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-summary-month");
	setup(&h).await;
	let store = h.store();

	let inv = draft(store, None).await;
	let mut input = issue_input(inv.id, 100_000);
	input.fulfilment_date = "2026-03-05".into();
	store.issue(inv.id, &input, inv.version).await.unwrap();

	let sum = store.invoice_summary(ORG, "2000-01", "2026-06-01", NO_SPAN).await.unwrap();
	assert_eq!(sum.months.len(), 1);
	assert_eq!(sum.months[0].month, "2026-03", "and not the month `issued_at` falls in");
	assert_eq!(sum.months[0].gross, Money(127_000));
}

pub async fn summary_excludes_drafts_from_months<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-summary-draft");
	setup(&h).await;
	let store = h.store();
	draft(store, None).await;

	let sum = store.invoice_summary(ORG, "2000-01", "2026-06-01", NO_SPAN).await.unwrap();
	assert_eq!(sum.statuses.len(), 1, "a draft is a status bucket");
	assert!(sum.months.is_empty(), "but never revenue: it carries no number");
}

/// The reason `months` does not filter on `kind`: the storno carries the negative amounts, so
/// a cancelled invoice and its cancellation leave the period at zero, which is the answer.
pub async fn summary_storno_nets_out<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-summary-storno");
	setup(&h).await;
	let store = h.store();

	let inv = draft(store, None).await;
	store.issue(inv.id, &issue_input(inv.id, 100_000), inv.version).await.unwrap();
	let new = new_invoice(None, InvoiceKind::Storno, Some(inv.id));
	store.storno(inv.id, &new, &issue_input(0, -100_000)).await.unwrap();

	let sum = store.invoice_summary(ORG, "2000-01", "2026-06-01", NO_SPAN).await.unwrap();
	assert_eq!(sum.months.len(), 1);
	assert_eq!(sum.months[0].count, 2, "both documents are in the month");
	assert_eq!(sum.months[0].gross, Money(0));

	// Not an unpaid `ISSUED` row lowering outstanding: the pair nets under `STORNOED`.
	assert!(sum.statuses.iter().all(|b| b.status != InvoiceStatus::Issued));
	let st = sum.statuses.iter().find(|b| b.status == InvoiceStatus::Stornoed).unwrap();
	assert_eq!((st.currency.as_str(), st.count, st.gross), ("HUF", 2, Money(0)));
}

pub async fn summary_overdue_ignores_paid_and_future<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-summary-overdue");
	setup(&h).await;
	let store = h.store();

	let late = draft(store, None).await;
	store
		.issue(late.id, &issue_input(late.id, 100_000), late.version)
		.await
		.unwrap();

	let settled = draft(store, None).await;
	let paid = store
		.issue(settled.id, &issue_input(settled.id, 200_000), settled.version)
		.await
		.unwrap();
	assert!(store.set_paid(settled.id, paid.gross, Some(Timestamp::now())).await.unwrap());

	let future = draft(store, None).await;
	let mut input = issue_input(future.id, 300_000);
	input.due_date = Some("2026-12-31".into());
	store.issue(future.id, &input, future.version).await.unwrap();

	let sum = store.invoice_summary(ORG, "2000-01", "2026-06-01", NO_SPAN).await.unwrap();
	assert_eq!(sum.overdue.len(), 1);
	assert_eq!(sum.overdue[0].count, 1, "only the unpaid one past its due date");
	assert_eq!(sum.overdue[0].outstanding, Money(127_000));
}

/// A `this_month` span no payment falls in.
const NO_SPAN: (Timestamp, Timestamp) = (Timestamp(0), Timestamp(0));

pub async fn summary_paid_this_month_is_by_payment_date<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-summary-paid-month");
	setup(&h).await;
	let store = h.store();

	let inv = draft(store, None).await;
	let mut input = issue_input(inv.id, 100_000);
	input.fulfilment_date = "2026-04-20".into();
	let issued = store.issue(inv.id, &input, inv.version).await.unwrap();
	let paid_at = Timestamp(1_780_000_000); // 2026-05-28: fulfilled in April, paid in May
	assert!(store.set_paid(inv.id, issued.gross, Some(paid_at)).await.unwrap());

	let may = (Timestamp(paid_at.0 - 100), Timestamp(paid_at.0 + 100));
	let sum = store.invoice_summary(ORG, "2000-01", "2026-06-01", may).await.unwrap();
	assert_eq!(sum.paid_this_month.len(), 1);
	assert_eq!(sum.paid_this_month[0].0.as_str(), "HUF");
	assert_eq!(sum.paid_this_month[0].1, Money(127_000));

	let sum = store.invoice_summary(ORG, "2000-01", "2026-06-01", NO_SPAN).await.unwrap();
	assert!(sum.paid_this_month.is_empty(), "paid outside the span");
}

/// A fresh org answers three empty vectors, not a decode error: there is nothing to sum.
pub async fn summary_empty_org_is_empty<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-summary-empty");
	setup(&h).await;
	let store = h.store();

	let sum = store.invoice_summary(ORG, "2000-01", "2026-06-01", NO_SPAN).await.unwrap();
	assert!(sum.statuses.is_empty() && sum.months.is_empty() && sum.overdue.is_empty());
}

/// A list filter narrows by status, and an empty `statuses` means every status rather than
/// none.
pub async fn list_filters_by_status<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-list-status");
	setup(&h).await;
	let store = h.store();
	draft(store, None).await;
	issued(store).await;

	let of = |statuses: Vec<InvoiceStatus>| InvoiceFilter { statuses, ..Default::default() };
	let all = store
		.list_invoices_page(ORG, &InvoiceFilter::default(), None, 50)
		.await
		.unwrap();
	assert_eq!(all.len(), 2);

	let rows = store
		.list_invoices_page(ORG, &of(vec![InvoiceStatus::Draft]), None, 50)
		.await
		.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].invoice.status, InvoiceStatus::Draft);

	let two = of(vec![InvoiceStatus::Draft, InvoiceStatus::Issued]);
	assert_eq!(store.list_invoices_page(ORG, &two, None, 50).await.unwrap().len(), 2);

	let none = of(vec![InvoiceStatus::Paid]);
	assert!(store.list_invoices_page(ORG, &none, None, 50).await.unwrap().is_empty());
}

pub async fn list_filters_by_number_and_buyer_name<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-list-q");
	setup(&h).await;
	let store = h.store();
	let inv = draft(store, None).await;
	let mut input = issue_input(inv.id, 100_000);
	input.buyer.name = "Kovacs Bt.".into();
	store.issue(inv.id, &input, inv.version).await.unwrap();
	draft(store, Some("other")).await;

	let q = |s: &str| InvoiceFilter { q: Some(s.to_owned()), ..Default::default() };
	// SQLite's `LIKE` folds ASCII, so the lowercase needle finds the capitalised snapshot.
	let rows = store.list_invoices_page(ORG, &q("kovacs"), None, 50).await.unwrap();
	assert_eq!(rows.len(), 1);

	let number = rows[0].invoice.number.clone().expect("an issued invoice has a number");
	let by_number = store.list_invoices_page(ORG, &q(&number), None, 50).await.unwrap();
	assert_eq!(by_number.len(), 1);
	assert_eq!(by_number[0].invoice.number, Some(number));
}

/// A DRAFT has no buyer snapshot yet -- it is frozen at ISSUE -- so the joined party name is
/// the only thing that finds it by who it is for.
pub async fn list_finds_draft_by_party_name<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-list-party");
	setup(&h).await;
	let store = h.store();
	let party = store
		.create_party(
			ORG,
			&PartyPatch {
				kind: Some(PartyKind::Company),
				name: Some("Nagy Kft.".into()),
				country: Some("HU".into()),
				..Default::default()
			},
		)
		.await
		.unwrap();
	store
		.create_draft(&NewInvoice {
			billing_party_id: Some(party.id),
			..new_invoice(None, InvoiceKind::Normal, None)
		})
		.await
		.unwrap();
	draft(store, Some("other")).await;

	let f = InvoiceFilter { q: Some("nagy".into()), ..Default::default() };
	let rows = store.list_invoices_page(ORG, &f, None, 50).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].party_uid, Some(party.uid));
}

/// Unescaped, a user typing `%` lists the whole table and one typing `_` gets near-random rows.
pub async fn list_q_escapes_like_wildcards<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-list-escape");
	setup(&h).await;
	let store = h.store();
	let party = store
		.create_party(
			ORG,
			&PartyPatch {
				kind: Some(PartyKind::Company),
				name: Some("100%".into()),
				country: Some("HU".into()),
				..Default::default()
			},
		)
		.await
		.unwrap();
	store
		.create_draft(&NewInvoice {
			billing_party_id: Some(party.id),
			..new_invoice(None, InvoiceKind::Normal, None)
		})
		.await
		.unwrap();
	draft(store, Some("plain")).await;

	let q = |s: &str| InvoiceFilter { q: Some(s.to_owned()), ..Default::default() };
	assert_eq!(store.list_invoices_page(ORG, &q("100%"), None, 50).await.unwrap().len(), 1);
	assert_eq!(store.list_invoices_page(ORG, &q("%"), None, 50).await.unwrap().len(), 1);
	assert!(store.list_invoices_page(ORG, &q("_"), None, 50).await.unwrap().is_empty());
}

/// The `before_id` cursor pages *within* the filtered set: a page that dropped the filter
/// would walk onto the unmatched draft.
pub async fn list_filter_pages_with_cursor<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-list-cursor");
	setup(&h).await;
	let store = h.store();
	for i in 0..3 {
		let rid = format!("m{i}");
		let inv = draft(store, Some(&rid)).await;
		let mut input = issue_input(inv.id, 100_000);
		input.buyer.name = "Match Kft.".into();
		store.issue(inv.id, &input, inv.version).await.unwrap();
	}
	draft(store, Some("nomatch")).await;

	let f = InvoiceFilter { q: Some("Match".into()), ..Default::default() };
	let mut cursor = None;
	let mut seen = Vec::new();
	for _ in 0..3 {
		let page = store.list_invoices_page(ORG, &f, cursor, 1).await.unwrap();
		assert_eq!(page.len(), 1);
		cursor = Some(page[0].invoice.id);
		seen.push(page[0].invoice.id);
	}
	assert!(seen.windows(2).all(|w| w[0] > w[1]), "id DESC");
	assert!(store.list_invoices_page(ORG, &f, cursor, 1).await.unwrap().is_empty());
}

pub async fn seller_has_issued_ignores_drafts<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-seller_has_issued");
	setup(&h).await;
	let store = h.store();
	let inv = draft(store, None).await;
	store
		.set_status(inv.id, InvoiceStatus::Draft, InvoiceStatus::Pending)
		.await
		.unwrap();
	assert!(!store.seller_has_issued(SELLER).await.unwrap(), "DRAFT and PENDING carry no number");
	issued(store).await;
	assert!(store.seller_has_issued(SELLER).await.unwrap());
}

pub async fn closing_is_refused_while_a_payment_is_pending<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-close_pending");
	setup(&h).await;
	let store = h.store();
	let inv = draft(store, None).await;
	store
		.set_status(inv.id, InvoiceStatus::Draft, InvoiceStatus::Pending)
		.await
		.unwrap();
	let now = Some(Timestamp::now());
	assert!(!store.set_seller_closed(SELLER, now).await.unwrap());
	assert_eq!(store.seller_by_id(SELLER).await.unwrap().unwrap().closed_at, None);

	store
		.set_status(inv.id, InvoiceStatus::Pending, InvoiceStatus::Draft)
		.await
		.unwrap();
	assert!(store.set_seller_closed(SELLER, now).await.unwrap());
	assert_eq!(store.seller_by_id(SELLER).await.unwrap().unwrap().closed_at, now);
	assert!(store.set_seller_closed(SELLER, None).await.unwrap());
	assert_eq!(store.seller_by_id(SELLER).await.unwrap().unwrap().closed_at, None);
}

pub async fn payment_terms_round_trip_and_put_seller_keeps_them<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-payment_terms");
	setup(&h).await;
	let store = h.store();
	assert!(store.set_seller_payment_days(SELLER, Some(15)).await.unwrap());
	let s = store.seller_by_id(SELLER).await.unwrap().unwrap();
	assert_eq!(s.payment_days, Some(15));
	store.put_seller(&Seller { payment_days: None, ..s }).await.unwrap();
	assert_eq!(store.seller_by_id(SELLER).await.unwrap().unwrap().payment_days, Some(15));
	assert!(!store.set_seller_payment_days(999, None).await.unwrap(), "no such seller");

	let party = store
		.create_party(
			ORG,
			&PartyPatch {
				kind: Some(PartyKind::Company),
				name: Some("Terms Kft.".into()),
				country: Some("HU".into()),
				payment_days: Patch::Value(30),
				payment_method: Patch::Value(PaymentMethod::Cash),
				..Default::default()
			},
		)
		.await
		.unwrap();
	assert_eq!((party.payment_days, party.payment_method), (Some(30), Some(PaymentMethod::Cash)));
	let untouched = PartyPatch { name: Some("Renamed Kft.".into()), ..Default::default() };
	let party = store.update_party(ORG, &party.uid, &untouched).await.unwrap().unwrap();
	assert_eq!((party.payment_days, party.payment_method), (Some(30), Some(PaymentMethod::Cash)));
	let cleared =
		PartyPatch { payment_days: Patch::Null, payment_method: Patch::Null, ..Default::default() };
	let party = store.update_party(ORG, &party.uid, &cleared).await.unwrap().unwrap();
	assert_eq!((party.payment_days, party.payment_method), (None, None));
}

/// Boot re-puts every seller; a `put_seller` that wrote `closed_at` would reopen the company.
pub async fn put_seller_does_not_reopen<H: Harness>()
where
	H::Store: InvoiceStore + NavStore,
{
	let h = fresh!(H, "invoice-put_seller_closed");
	setup(&h).await;
	let store = h.store();
	assert!(store.set_seller_closed(SELLER, Some(Timestamp::now())).await.unwrap());
	let mut s = store.seller_by_id(SELLER).await.unwrap().unwrap();
	s.closed_at = None;
	s.series_code = "B".into();
	store.put_seller(&s).await.unwrap();
	let after = store.seller_by_id(SELLER).await.unwrap().unwrap();
	assert_eq!(after.series_code, "B");
	assert!(after.closed_at.is_some());
}

// vim: ts=4
