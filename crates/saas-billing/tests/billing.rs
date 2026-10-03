//! The service half of payments against a real `SqliteStore`: the public callback, the
//! transitions it drives, and the operator's manual entry and allocation.
//!
//! The gateway is a stub [`PaymentProvider`] registered through the `Extensions` type-map, the
//! same way a consumer wires `adapters/payment-adapter-barion` up — nothing here names a
//! gateway, and no HTTP call leaves the test.
//!
//! Every test opens a real file database. `sqlite::memory:` gives each *connection* its own
//! database, so two stores over one in-memory URL would never contend for the write lock.
//!
//! The raw-trait half lives in `adapters/store-adapter-sqlite/tests/billing.rs`: a test that
//! drives `BillingStore` goes there, a test that drives `allocate`/`webhook` goes here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use saas_billing::provider::{
	CallbackRef, PaymentProvider, PaymentProviders, PaymentState, ProviderCaps, RefundResult,
	StartPayment, StartedPayment,
};
use saas_billing::store::{BillingStore, PaymentFilter, RefundRecord, store as billing_store};
use saas_billing::{Allocation, ManualPayment, StartRequest, allocate, webhook};
use saas_core::error::{Retry, StatusCode};
use saas_core::{App, AppBuilder, config::Config, ctx::Ctx, ids::SellerId, prelude::*};
use saas_invoice::{
	draft::{Line, NewDraft, Party},
	service_api::Invoices,
	store::{
		Invoice, InvoicePatch, InvoiceStatus, InvoiceStore, PaymentMethod, Seller,
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
const ORG_UID: &str = "org_t";

/// One line of `NET` at 27%, so an invoice's `gross` is [`GROSS`].
const NET: i64 = 100_000;
const GROSS: i64 = NET + NET * 2700 / 10000;

/// What [`Stub::start`] hands back as `payments.provider_ref`.
const PROVIDER_REF: &str = "prv-stub";

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-billing-test-{}-{name}", std::process::id()));
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

/// A gateway that answers from a fixed script. `fetch_state` is the only thing that may move a
/// payment, so what it returns is the whole of what a callback can do.
struct Stub {
	state: PaymentState,
	/// `start` fails, which is the case that used to leave a `PENDING` row owning the
	/// `request_id` forever.
	fail_start: bool,
	partial_refund: bool,
	/// The last [`StartPayment`] handed to `start`. A real gateway runs 3DS risk scoring on the
	/// item and billing block, and a stub that ignores it makes the first real call the test.
	started: Arc<Mutex<Option<StartPayment>>>,
	/// Every `provider_ref` `fetch_state` was asked about — the public callback must not reach
	/// the gateway on a reference we do not hold.
	fetched: Arc<Mutex<Vec<String>>>,
	/// Every idempotency key `refund` was handed: a retry of the same logical refund must
	/// reuse one, or the gateway pays out twice.
	refund_keys: Arc<Mutex<Vec<String>>>,
	/// A real gateway mints a distinct id per payment, and `idx_payment_provider_ref` is unique
	/// over `(provider, provider_ref)`. The first is [`PROVIDER_REF`], so `ping` still names it.
	started_count: Arc<AtomicUsize>,
	/// What `start` itself reports. A gateway that captures synchronously answers `Start` with
	/// SUCCEEDED, which is a different thing from what `fetch_state` says later.
	start_state: PaymentState,
	/// Moves the payment out of `REFUNDABLE` *inside* the gateway call, which is the window two
	/// concurrent partial refunds put each other in: the payout lands and `record_refund` then
	/// refuses. Filled in after `service_with`, which is what mints the store.
	sabotage: Arc<Mutex<Option<SqliteStore>>>,
	/// Armed with an amount, [`Racy`] adds it to `refunded_amount` the moment `payment_by_uid`
	/// has answered — the concurrent refund that wins the compare-and-set while this one is
	/// still holding the row it read. Lives on the stub only so `service_with` can reach it.
	race: Arc<Mutex<Option<i64>>>,
	/// Armed, every `fetch_state` fails with this class: `Backoff` is a gateway that is not
	/// answering — what the sweep must stop a batch on — and `Never` one row the gateway refuses,
	/// which it must keep sweeping past.
	fetch_failure: Arc<Mutex<Option<Retry>>>,
}

/// A `BillingStore` that hands out one stale `payment_by_uid` answer on demand, so the refund
/// race is a test rather than a timing accident. Every other method delegates.
struct Racy {
	inner: SqliteStore,
	bump: Arc<Mutex<Option<i64>>>,
}

#[async_trait]
impl BillingStore for Racy {
	async fn payment_by_uid(
		&self,
		org_id: Option<i64>,
		uid: &PaymentId,
	) -> ClResult<Option<saas_billing::Payment>> {
		let found = self.inner.payment_by_uid(org_id, uid).await?;
		// Taken, not peeked: one stale answer, or `alerts` and the re-read below race too. Out
		// of the mutex before the await — a `MutexGuard` must not be held across one.
		let bump = self.bump.lock().unwrap().take();
		if let (Some(p), Some(by)) = (&found, bump) {
			sqlx::query("UPDATE payments SET refunded_amount = refunded_amount + ? WHERE id = ?")
				.bind(by)
				.bind(p.id)
				.execute(self.inner.write_pool())
				.await
				.unwrap();
		}
		Ok(found)
	}

	async fn create_payment(
		&self,
		new: &saas_billing::store::NewPayment,
	) -> ClResult<saas_billing::Payment> {
		self.inner.create_payment(new).await
	}
	async fn payment(&self, id: i64) -> ClResult<Option<saas_billing::Payment>> {
		self.inner.payment(id).await
	}
	async fn payment_by_provider_ref(
		&self,
		provider: &str,
		provider_ref: &str,
	) -> ClResult<Option<saas_billing::Payment>> {
		self.inner.payment_by_provider_ref(provider, provider_ref).await
	}
	async fn payment_by_request_id(
		&self,
		org_id: i64,
		request_id: &str,
	) -> ClResult<Option<saas_billing::Payment>> {
		self.inner.payment_by_request_id(org_id, request_id).await
	}
	async fn set_started(
		&self,
		id: i64,
		provider_ref: &str,
		redirect_url: Option<&str>,
		expires_at: Option<Timestamp>,
	) -> ClResult<bool> {
		self.inner.set_started(id, provider_ref, redirect_url, expires_at).await
	}
	async fn advance_status(
		&self,
		id: i64,
		to: PaymentState,
		from: &[PaymentState],
	) -> ClResult<bool> {
		self.inner.advance_status(id, to, from).await
	}
	async fn settle(&self, s: &saas_billing::store::Settlement) -> ClResult<bool> {
		self.inner.settle(s).await
	}
	async fn list_payments(
		&self,
		org_id: i64,
		filter: &PaymentFilter<'_>,
	) -> ClResult<Vec<saas_billing::Payment>> {
		self.inner.list_payments(org_id, filter).await
	}
	async fn allocations(
		&self,
		payment_id: i64,
	) -> ClResult<Vec<saas_billing::store::PaymentAllocation>> {
		self.inner.allocations(payment_id).await
	}
	async fn allocations_for(
		&self,
		payment_ids: &[i64],
	) -> ClResult<Vec<saas_billing::store::PaymentAllocation>> {
		self.inner.allocations_for(payment_ids).await
	}
	async fn payments_by_invoice(
		&self,
		org_id: i64,
		invoice_id: i64,
	) -> ClResult<Vec<saas_billing::Payment>> {
		self.inner.payments_by_invoice(org_id, invoice_id).await
	}
	async fn org_id_by_uid(&self, uid: &OrgId) -> ClResult<Option<i64>> {
		self.inner.org_id_by_uid(uid).await
	}
	async fn record_refund(&self, r: &RefundRecord) -> ClResult<bool> {
		self.inner.record_refund(r).await
	}
	async fn overdue_invoices(
		&self,
		org_id: Option<i64>,
		after_invoice_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<saas_billing::store::OverdueInvoice>> {
		self.inner.overdue_invoices(org_id, after_invoice_id, limit).await
	}
	async fn unallocated_payments(
		&self,
		received_before: Timestamp,
	) -> ClResult<(i64, Option<Timestamp>)> {
		self.inner.unallocated_payments(received_before).await
	}
	async fn refund_discrepancies(&self) -> ClResult<(i64, Option<Timestamp>)> {
		self.inner.refund_discrepancies().await
	}
	async fn live_payments(
		&self,
		updated_before: Timestamp,
		after_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<saas_billing::Payment>> {
		self.inner.live_payments(updated_before, after_id, limit).await
	}
}

impl Stub {
	fn new(state: PaymentState) -> Self {
		Self {
			state,
			fail_start: false,
			partial_refund: true,
			started: Arc::default(),
			fetched: Arc::default(),
			refund_keys: Arc::default(),
			started_count: Arc::default(),
			start_state: PaymentState::Pending,
			sabotage: Arc::default(),
			race: Arc::default(),
			fetch_failure: Arc::default(),
		}
	}
}

#[async_trait]
impl PaymentProvider for Stub {
	#[allow(clippy::unnecessary_literal_bound)] // the trait ties the lifetime to `&self`
	fn id(&self) -> &str {
		"stub"
	}

	fn capabilities(&self) -> ProviderCaps {
		ProviderCaps {
			partial_refund: self.partial_refund,
			recurring: true,
			..ProviderCaps::default()
		}
	}

	async fn start(&self, req: &StartPayment) -> ClResult<StartedPayment> {
		*self.started.lock().unwrap() = Some(req.clone());
		if self.fail_start {
			return Err(Error::internal("the gateway is down"));
		}
		let n = self.started_count.fetch_add(1, Ordering::Relaxed);
		Ok(StartedPayment {
			provider_ref: if n == 0 {
				PROVIDER_REF.to_string()
			} else {
				format!("{PROVIDER_REF}-{n}")
			},
			redirect_url: Some("https://stub.invalid/pay".to_string()),
			state: self.start_state,
		})
	}

	async fn fetch_state(&self, provider_ref: &str) -> ClResult<PaymentState> {
		self.fetched.lock().unwrap().push(provider_ref.to_string());
		match *self.fetch_failure.lock().unwrap() {
			Some(Retry::Backoff) => Err(Error::coded_retry(
				StatusCode::BAD_GATEWAY,
				"E-PAY-PROVIDER-DOWN",
				"stub: throttled for 60s after HTTP 429",
			)),
			Some(Retry::Never) => Err(Error::coded(
				StatusCode::BAD_GATEWAY,
				"E-PAY-PROVIDER-DOWN",
				"stub: HTTP 400 with no readable error document",
			)),
			None => Ok(self.state),
		}
	}

	async fn refund(
		&self,
		_provider_ref: &str,
		amount: Money,
		request_id: &str,
	) -> ClResult<RefundResult> {
		self.refund_keys.lock().unwrap().push(request_id.to_string());
		// Cloned out before the await: a `MutexGuard` must not be held across one.
		let sabotage = self.sabotage.lock().unwrap().clone();
		if let Some(store) = sabotage {
			sqlx::query("UPDATE payments SET status = 'EXPIRED'")
				.execute(store.write_pool())
				.await
				.unwrap();
		}
		Ok(RefundResult { refunded: amount, state: PaymentState::Refunded })
	}

	async fn charge_recurring(
		&self,
		_token: &str,
		_req: &StartPayment,
	) -> ClResult<StartedPayment> {
		Ok(StartedPayment { provider_ref: "rec-1".into(), redirect_url: None, state: self.state })
	}

	/// The body is the reference, verbatim — everything a real adapter does here is parsing,
	/// and the truth still comes from `fetch_state`.
	fn parse_callback(&self, _headers: &HeaderMap, body: &[u8]) -> ClResult<CallbackRef> {
		let provider_ref = std::str::from_utf8(body)
			.map_err(|_| Error::Validation("callback body is not utf-8".into()))?;
		if provider_ref.is_empty() {
			return Err(Error::Validation("callback body is empty".into()));
		}
		Ok(CallbackRef { provider_ref: provider_ref.to_string() })
	}
}

async fn service(db: &TmpDb, state: PaymentState) -> (App, Invoices, SqliteStore) {
	service_with(db, Stub::new(state)).await
}

async fn service_with(db: &TmpDb, stub: Stub) -> (App, Invoices, SqliteStore) {
	let store = SqliteStore::open(&Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: "https://app.invalid".into(),
		jobs_workers: None,
	})
	.await
	.unwrap();
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	let app = AppBuilder::new()
		.config(Config {
			master_key: [0; 32],
			db_path: db.path(),
			data_dir: db.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: "https://app.invalid".into(),
			jobs_workers: None,
		})
		.store(Arc::new(store.clone()) as Arc<dyn saas_core::store::CoreStore>)
		.settings(saas_invoice::SETTINGS)
		.settings(saas_billing::SETTINGS)
		.extension(Arc::new(store.clone()) as Arc<dyn InvoiceStore>)
		.extension(Arc::new(Racy { inner: store.clone(), bump: Arc::clone(&stub.race) })
			as Arc<dyn BillingStore>)
		.extension(Arc::new(PaymentProviders::new().with(Arc::new(stub))))
		.build()
		.await
		.unwrap();

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
		 VALUES (?, ?, (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)",
	)
	.bind(ORG)
	.bind(ORG_UID)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  email, is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', ?, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 'vevo@e.st', 1, 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();

	store
		.put_seller(&Seller {
			id: SELLER,
			uid: SellerId::generate(),
			org_id: ORG,
			nav_base_url: "https://api-test.onlineszamla.nav.gov.hu".into(),
			nav_login: None,
			series_code: "A".into(),
			closed_at: None,
			payment_days: None,
			created_at: Timestamp::now(),
		})
		.await
		.unwrap();
	store
		.save_seller_version_draft(
			SELLER,
			&SellerVersionPatch {
				name: Some("Teszt Kft.".into()),
				country: Some("HU".into()),
				tax_number: Some("12345678242".into()),
				postcode: Some("1011".into()),
				city: Some("Budapest".into()),
				street: Some("Fo utca 1.".into()),
				vat_scheme: Some("NORMAL".into()),
				..Default::default()
			},
		)
		.await
		.unwrap();
	store
		.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();

	let invoices = Invoices::new(app.clone());
	(app, invoices, store)
}

fn ctx() -> Ctx {
	Ctx::system("test").with_org(ORG)
}

async fn draft(invoices: &Invoices) -> Invoice {
	invoices
		.draft(
			&ctx(),
			&NewDraft {
				request_id: None,
				billing_party: Party::OrgDefault,
				lines: vec![Line {
					code: None,
					description: "Tanacsadas".into(),
					unit: "ora".into(),
					qty: Qty(1_000_000),
					unit_price: Some(Money(NET)),
					vat_code: Some(VatCode::Std27),
					discount: None,
					discount_description: None,
					note: None,
				}],
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
		.unwrap()
}

async fn issued(app: &App, invoices: &Invoices, store: &SqliteStore) -> Invoice {
	let d = draft(invoices).await;
	saas_invoice::issue::run(app, store, d).await.unwrap()
}

/// The webhook as the gateway calls it: a path segment, a body, and nothing else.
async fn ping(app: &App, body: &'static str) -> StatusCode {
	webhook::callback(
		State(app.clone()),
		Path("stub".to_string()),
		RawQuery(None),
		HeaderMap::new(),
		Bytes::from_static(body.as_bytes()),
	)
	.await
}

async fn start_payment(app: &App, invoice: &Invoice) -> saas_billing::Payment {
	allocate::start(
		app,
		&ctx(),
		&invoice.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: None,
			return_url: "https://app.invalid/done".into(),
			locale: None,
		},
	)
	.await
	.unwrap()
	.0
}

/// The gateway retries, so two copies of the same ping may be in flight at once. The guard
/// lives in `settle`'s `WHERE`, so the second one moves nothing rather than settling twice.
#[tokio::test]
async fn a_duplicated_callback_settles_once() {
	let db = TmpDb::new("replay");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;

	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	let bstore = billing_store(&app).unwrap();
	let rows = bstore.allocations(payment.id).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].amount, Money(GROSS), "the second ping allocated nothing");
	assert_eq!(bstore.payment(payment.id).await.unwrap().unwrap().status, PaymentState::Succeeded);

	let paid = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(paid.paid_amount, Money(GROSS));
	assert!(paid.paid_at.is_some());
}

/// A reference we have never issued is still a 200: an error makes the gateway retry forever,
/// and a distinguishable one would leak which references exist.
#[tokio::test]
async fn a_callback_for_an_unknown_reference_changes_nothing() {
	let db = TmpDb::new("unknown-ref");
	let stub = Stub::new(PaymentState::Succeeded);
	let asked = Arc::clone(&stub.fetched);
	let (app, invoices, store) = service_with(&db, stub).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;

	assert_eq!(ping(&app, "prv-nobody").await, StatusCode::OK);
	// The public callback must find the payment before it calls out: at 600/min/ip an unknown
	// reference would otherwise be an unauthenticated amplifier against the gateway's API.
	assert!(
		asked.lock().unwrap().is_empty(),
		"the gateway was asked about a reference we do not hold"
	);
	// A provider nobody registered, and a body that parses to nothing, are 200 as well.
	assert_eq!(
		webhook::callback(
			State(app.clone()),
			Path("no-such-gateway".to_string()),
			RawQuery(None),
			HeaderMap::new(),
			Bytes::from_static(PROVIDER_REF.as_bytes()),
		)
		.await,
		StatusCode::OK
	);
	assert_eq!(ping(&app, "").await, StatusCode::OK);

	let bstore = billing_store(&app).unwrap();
	assert_eq!(bstore.payment(payment.id).await.unwrap().unwrap().status, PaymentState::Pending);
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().paid_amount, Money::ZERO);
}

/// A state that is not `SUCCEEDED` advances the status and allocates nothing — including
/// `PARTIALLY_SUCCEEDED`, because `fetch_state` answers with no amount.
#[tokio::test]
async fn a_partial_state_advances_without_allocating() {
	let db = TmpDb::new("partial-state");
	let (app, invoices, store) = service(&db, PaymentState::PartiallySucceeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;

	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	let bstore = billing_store(&app).unwrap();
	assert_eq!(
		bstore.payment(payment.id).await.unwrap().unwrap().status,
		PaymentState::PartiallySucceeded
	);
	let rows = bstore.allocations(payment.id).await.unwrap();
	assert_eq!(rows[0].amount, Money::ZERO, "still only the zero link row");
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().paid_amount, Money::ZERO);
}

/// Money that arrives short of the invoice leaves it unpaid: `paid_amount` moves, `paid_at`
/// does not, and the invoice stays `ISSUED`.
#[tokio::test]
async fn a_partial_payment_leaves_the_invoice_unpaid() {
	let db = TmpDb::new("partial-pay");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;

	let payment = allocate::manual(
		&app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "TRANSFER".into(),
			amount: Money(GROSS - 1),
			currency: inv.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: Some("NAV-2026-0001".into()),
			note: None,
			allocations: vec![Allocation {
				invoice_uid: inv.uid.clone(),
				amount: Money(GROSS - 1),
				currency: CurrencyCode::huf(),
			}],
		},
	)
	.await
	.unwrap();

	let part = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(part.paid_amount, Money(GROSS - 1));
	assert!(part.paid_at.is_none());
	assert_eq!(part.status, InvoiceStatus::Issued);

	// The remaining fillér cannot be allocated: it is more than the payment has left. The
	// ceiling is checked before the `(payment_id, invoice_id)` duplicate, so both hold here
	// and `E-PAY-ALLOC-EXCEEDS` is the one raised.
	let err = allocate::allocate(
		&app,
		&ctx(),
		&payment.uid,
		&Allocation {
			invoice_uid: inv.uid.clone(),
			amount: Money(1),
			currency: CurrencyCode::huf(),
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-ALLOC-EXCEEDS", .. }), "{err:?}");
}

/// One transfer covering two invoices is two `settle` calls against one `payments` row, and
/// neither is a special case in the store.
#[tokio::test]
async fn one_payment_settles_two_invoices() {
	let db = TmpDb::new("two-invoices");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let first = issued(&app, &invoices, &store).await;
	let second = issued(&app, &invoices, &store).await;

	let payment = allocate::manual(
		&app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "TRANSFER".into(),
			amount: Money(GROSS * 2),
			currency: first.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: None,
			note: None,
			allocations: vec![
				Allocation {
					invoice_uid: first.uid.clone(),
					amount: Money(GROSS),
					currency: CurrencyCode::huf(),
				},
				Allocation {
					invoice_uid: second.uid.clone(),
					amount: Money(GROSS),
					currency: CurrencyCode::huf(),
				},
			],
		},
	)
	.await
	.unwrap();

	let bstore = billing_store(&app).unwrap();
	assert_eq!(bstore.allocations(payment.id).await.unwrap().len(), 2);
	for inv in [&first, &second] {
		let paid = store.invoice_by_id(inv.id).await.unwrap().unwrap();
		assert_eq!(paid.paid_amount, Money(GROSS));
		assert!(paid.paid_at.is_some());
	}

	// Allocating past what arrived is refused before anything is written.
	let err = allocate::allocate(
		&app,
		&ctx(),
		&payment.uid,
		&Allocation {
			invoice_uid: first.uid.clone(),
			amount: Money(1),
			currency: CurrencyCode::huf(),
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-ALLOC-EXCEEDS", .. }), "{err:?}");
}

/// A draft has no final `gross` and `settle` refuses it outright, so the money arriving is what
/// issues it — before the settlement, not by a job after it.
#[tokio::test]
async fn paying_a_draft_issues_it_first() {
	let db = TmpDb::new("draft-pay");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let d = draft(&invoices).await;
	assert_eq!(d.status, InvoiceStatus::Draft);
	assert!(d.number.is_none());

	let payment = start_payment(&app, &d).await;
	// Locked while the gateway holds the charge, and still unnumbered: the number is allocated
	// in the issue transaction alone, so a payment that fails leaves no gap.
	let locked = store.invoice_by_id(d.id).await.unwrap().unwrap();
	assert_eq!(locked.status, InvoiceStatus::Pending);
	assert!(locked.number.is_none());
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	let inv = store.invoice_by_id(d.id).await.unwrap().unwrap();
	assert!(inv.number.is_some(), "it was issued on the way to being settled");
	assert_eq!(inv.paid_amount, inv.gross);
	assert!(inv.paid_at.is_some());
	// The whole point of the settlement: the row reads PAID, not ISSUED with a full
	// `paid_amount`, and a card sale does not file `TRANSFER` at NAV.
	assert_eq!(inv.status, InvoiceStatus::Paid);
	assert_eq!(inv.payment_method, PaymentMethod::Card);
	assert_eq!(
		billing_store(&app).unwrap().payment(payment.id).await.unwrap().unwrap().status,
		PaymentState::Succeeded
	);

	// `Invoices::issue` writes it; `issue::run`, which this used to call, does not.
	let issues: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM audit_logs WHERE action = 'ISSUE' AND entity_id = ?",
	)
	.bind(d.uid.as_str())
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(issues, 1);
}

/// `manual`, `allocate` and `refund` record money against any org the body names. Without
/// the gate any authenticated user could mark any org's invoice paid.
#[tokio::test]
async fn recording_a_payment_is_operator_only() {
	let db = TmpDb::new("operator-gate");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;

	let entry = || ManualPayment {
		org_uid: OrgId::from_trusted(ORG_UID.to_string()),
		kind: "TRANSFER".into(),
		amount: Money(GROSS),
		currency: inv.currency.clone(),
		received_at: Timestamp::now(),
		ext_ref: None,
		note: None,
		allocations: vec![Allocation {
			invoice_uid: inv.uid.clone(),
			amount: Money(GROSS),
			currency: CurrencyCode::huf(),
		}],
	};

	let user = Ctx { actor: saas_core::ctx::Actor::User { account_id: 1 }, ..ctx() };
	let err = allocate::manual(&app, &user, entry()).await.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-AUTH-FORBIDDEN", .. }), "{err:?}");
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().paid_amount, Money::ZERO);

	// `require_operator` grants `System` unconditionally, so the webhook path is unaffected.
	allocate::manual(&app, &ctx(), entry()).await.unwrap();
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().status, InvoiceStatus::Paid);
}

/// The row already owns the UNIQUE `request_id`, so a `PENDING` one left behind by a failed
/// `start` made every retry look like a payment in flight.
#[tokio::test]
async fn a_gateway_that_refuses_leaves_no_zombie() {
	let db = TmpDb::new("start-fails");
	let (app, invoices, store) =
		service_with(&db, Stub { fail_start: true, ..Stub::new(PaymentState::Pending) }).await;
	let inv = issued(&app, &invoices, &store).await;

	allocate::start(
		&app,
		&ctx(),
		&inv.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: Some("req-dead".into()),
			return_url: "https://app.invalid/done".into(),
			locale: None,
		},
	)
	.await
	.unwrap_err();

	let payment = billing_store(&app)
		.unwrap()
		.payment_by_request_id(ORG, "req-dead")
		.await
		.unwrap()
		.expect("the row was committed before the gateway was called");
	assert_eq!(payment.status, PaymentState::Failed);
}

#[tokio::test]
async fn an_off_date_draft_is_refused_before_the_gateway() {
	let db = TmpDb::new("card-dates");
	let stub = Stub::new(PaymentState::Pending);
	let seen = Arc::clone(&stub.started);
	let (app, invoices, store) = service_with(&db, stub).await;
	let tomorrow =
		saas_invoice::numbering::date_of(Timestamp(Timestamp::now().0 + 86_400)).unwrap();
	let d = draft(&invoices).await;
	let patch = InvoicePatch { due_date: Patch::Value(tomorrow), ..Default::default() };
	invoices.patch(&ctx(), d.uid.as_str(), &patch).await.unwrap();

	let err = allocate::start(
		&app,
		&ctx(),
		&d.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: Some("req-dates".into()),
			return_url: "https://app.invalid/done".into(),
			locale: None,
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-INV-CARD-DATES", .. }), "{err:?}");

	assert!(seen.lock().unwrap().is_none(), "the gateway was called");
	let payment = billing_store(&app).unwrap().payment_by_request_id(ORG, "req-dates").await;
	assert!(payment.unwrap().is_none());
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Draft);
}

#[tokio::test]
async fn a_card_payment_on_a_closed_sellers_draft_is_refused_before_the_gateway() {
	let db = TmpDb::new("seller-closed");
	let stub = Stub::new(PaymentState::Pending);
	let seen = Arc::clone(&stub.started);
	let (app, invoices, store) = service_with(&db, stub).await;
	let d = draft(&invoices).await;
	assert!(store.set_seller_closed(d.seller_id, Some(Timestamp::now())).await.unwrap());

	let err = allocate::start(
		&app,
		&ctx(),
		&d.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: Some("req-closed".into()),
			return_url: "https://app.invalid/done".into(),
			locale: None,
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-INV-SELLER-CLOSED", .. }), "{err:?}");

	assert!(seen.lock().unwrap().is_none(), "the gateway was called");
	let payment = billing_store(&app).unwrap().payment_by_request_id(ORG, "req-closed").await;
	assert!(payment.unwrap().is_none());
}

/// A failed issue must not leave a TRANSFER draft `PENDING`: no payment would ever unlock it.
#[tokio::test]
async fn hand_allocating_a_transfer_draft_does_not_lock_it() {
	let db = TmpDb::new("transfer-lock");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let d = draft(&invoices).await;
	// No lines: `issue` answers `E-INV-EMPTY`.
	sqlx::query("DELETE FROM invoice_lines WHERE invoice_id = ?")
		.bind(d.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	let entry = ManualPayment {
		org_uid: OrgId::from_trusted(ORG_UID.to_string()),
		kind: "TRANSFER".into(),
		amount: Money(GROSS),
		currency: d.currency.clone(),
		received_at: Timestamp::now(),
		ext_ref: None,
		note: None,
		allocations: vec![Allocation {
			invoice_uid: d.uid.clone(),
			amount: Money(GROSS),
			currency: CurrencyCode::huf(),
		}],
	};
	let err = allocate::manual(&app, &ctx(), entry).await.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-INV-EMPTY", .. }), "{err:?}");
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Draft);
}

/// The invoice page's cold read: the gateway's return URL carries no query string, so the
/// payment has to be found from the invoice alone — through the zero-amount link row.
#[tokio::test]
async fn payments_are_readable_back_from_the_invoice() {
	let db = TmpDb::new("by-invoice");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;

	let found = allocate::for_invoice(&app, &ctx(), &inv.uid).await.unwrap();
	assert_eq!(found.len(), 1);
	assert_eq!(found[0].id, payment.id);
	assert_eq!(found[0].redirect_url.as_deref(), Some("https://stub.invalid/pay"));

	// Another org's invoice is absent, never forbidden.
	let other = Ctx::system("test").with_org(2);
	let err = allocate::for_invoice(&app, &other, &inv.uid).await.unwrap_err();
	assert!(matches!(err, Error::NotFound), "{err:?}");
}

/// `start`'s only gate used to be `Invoices::patch`'s seller-admin check, which the system
/// escalation inside it now bypasses — leaving a stranger able to open a payment on any invoice
/// whose uid they guessed.
#[tokio::test]
async fn a_non_member_cannot_start_a_payment() {
	let db = TmpDb::new("start-gate");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (2, 'acc_u', 'u@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	let req = || StartRequest {
		provider: "stub".into(),
		request_id: None,
		return_url: "https://app.invalid/done".into(),
		locale: None,
	};
	let as_account = |id| Ctx { actor: saas_core::ctx::Actor::User { account_id: id }, ..ctx() };

	let err = allocate::start(&app, &as_account(2), &inv.uid, req()).await.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-AUTH-FORBIDDEN", .. }), "{err:?}");

	// A plain MEMBER of the buyer org is enough: paying is not an administrative act.
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 2, 'MEMBER', 0, 0)",
	)
	.bind(ORG)
	.execute(store.write_pool())
	.await
	.unwrap();
	allocate::start(&app, &as_account(2), &inv.uid, req()).await.unwrap();
}

/// `payments.request_id` is UNIQUE, so a retried start answers with the payment it already
/// made instead of opening a second one at the gateway.
#[tokio::test]
async fn a_retried_start_reuses_the_payment() {
	let db = TmpDb::new("start-idempotent");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;

	let req = || StartRequest {
		provider: "stub".into(),
		request_id: Some("req-1".into()),
		return_url: "https://app.invalid/done".into(),
		locale: None,
	};
	let (first, url) = allocate::start(&app, &ctx(), &inv.uid, req()).await.unwrap();
	assert!(url.is_some());
	let (again, retry_url) = allocate::start(&app, &ctx(), &inv.uid, req()).await.unwrap();
	assert_eq!(again.id, first.id);
	// A retry answering with no URL leaves a customer who went back on a draft with nothing
	// to click.
	assert_eq!(retry_url, url, "the stored redirect, so the payment can be resumed");

	// An unregistered gateway is the caller's mistake, not a 404 on the invoice.
	let err =
		allocate::start(&app, &ctx(), &inv.uid, StartRequest { provider: "nope".into(), ..req() })
			.await
			.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-PROVIDER", .. }), "{err:?}");
}

/// A stored card recurrence pays a draft payer-absent: it issues and settles it, and a
/// retried charge under the same key answers the first payment, never another invoice's.
#[tokio::test]
async fn a_recurring_charge_settles_a_draft_and_is_idempotent() {
	let db = TmpDb::new("recurring");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = draft(&invoices).await;
	let charge = |uid: InvoiceId| {
		let app = app.clone();
		async move { allocate::charge_recurring(&app, &ctx(), &uid, "stub", "tok", "rec-a").await }
	};

	let first = charge(inv.uid.clone()).await.unwrap();
	assert_eq!(first.status, PaymentState::Succeeded);
	assert!(first.expires_at.is_some());
	let paid = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!((paid.status, paid.paid_amount), (InvoiceStatus::Paid, Money(GROSS)));
	assert_eq!(charge(inv.uid.clone()).await.unwrap().id, first.id);

	let other = draft(&invoices).await;
	let err = charge(other.uid.clone()).await.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-NOT-PAYABLE", .. }), "{err:?}");
}

#[tokio::test]
async fn a_recurring_charge_refuses_a_user_caller() {
	let db = TmpDb::new("recurring-user");
	let (app, invoices, _store) = service(&db, PaymentState::Succeeded).await;
	let inv = draft(&invoices).await;
	let user = ctx().as_user(1);
	let err = allocate::charge_recurring(&app, &user, &inv.uid, "stub", "tok", "rec-u")
		.await
		.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-AUTH-FORBIDDEN", .. }), "{err:?}");
}

/// A sandbox gateway cannot POST to a developer's `localhost`, so no callback ever arrives.
/// The return leg, with no callback at all: the payer's own landing is what settles the draft.
#[tokio::test]
async fn a_returning_payer_settles_a_draft_with_no_callback() {
	let db = TmpDb::new("no-callback");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let d = draft(&invoices).await;
	let payment = start_payment(&app, &d).await;

	// No `ping`: the gateway never reached us.
	let rows = allocate::refresh_invoice(&app, &ctx(), &d.uid).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].status, PaymentState::Succeeded);

	let inv = store.invoice_by_id(d.id).await.unwrap().unwrap();
	assert!(inv.number.is_some(), "it was issued on the way to being settled");
	assert_eq!(inv.paid_amount, inv.gross);
	assert!(inv.paid_at.is_some());
	assert_eq!(inv.status, InvoiceStatus::Paid);
	assert_eq!(inv.payment_method, PaymentMethod::Card);
	assert_eq!(
		billing_store(&app).unwrap().payment(payment.id).await.unwrap().unwrap().status,
		PaymentState::Succeeded
	);
}

/// The invoice page's two-second poll must not spend the gateway's quota: it is a read of our
/// own tables, and the gateway is asked once, by the return leg.
#[tokio::test]
async fn reading_the_invoice_does_not_ask_the_gateway() {
	let db = TmpDb::new("polled-read");
	let stub = Stub::new(PaymentState::Succeeded);
	let asked = Arc::clone(&stub.fetched);
	let (app, invoices, _store) = service_with(&db, stub).await;
	let d = draft(&invoices).await;
	start_payment(&app, &d).await;

	let rows = allocate::for_invoice(&app, &ctx(), &d.uid).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].status, PaymentState::Pending, "the read settled the payment");
	assert!(asked.lock().unwrap().is_empty(), "a poll called the gateway");

	let rows = allocate::refresh_invoice(&app, &ctx(), &d.uid).await.unwrap();
	assert_eq!(rows[0].status, PaymentState::Succeeded);
	assert_eq!(asked.lock().unwrap().len(), 1, "the return leg asks once per live row");
}

/// A throttled gateway must not turn the payment read into a 502 — the stored row is what the
/// page renders, and the sweep is what asks again.
#[tokio::test]
async fn a_throttled_gateway_leaves_the_payment_read_answering_the_stored_row() {
	let db = TmpDb::new("payment-read-throttled");
	let stub = Stub::new(PaymentState::Pending);
	let failure = Arc::clone(&stub.fetch_failure);
	let (app, invoices, _store) = service_with(&db, stub).await;
	let d = draft(&invoices).await;
	let payment = start_payment(&app, &d).await;
	*failure.lock().unwrap() = Some(Retry::Backoff);

	let read = allocate::payment(&app, &ctx(), &payment.uid).await.unwrap();
	assert_eq!(read.status, PaymentState::Pending, "the stored row is what renders");
}

/// The lock round trip. `start` freezes the draft at the total the gateway is charging, so a
/// patch landing after it cannot bill one figure and invoice another; a dead payment hands the
/// cart back; and an invoice that was already `ISSUED` when the payment opened keeps its status,
/// because a card payment against a numbered transfer invoice changes nothing about it.
#[tokio::test]
async fn a_started_payment_locks_the_draft_and_a_dead_one_releases_it() {
	for (dead, label) in [
		(PaymentState::Failed, "failed"),
		(PaymentState::Canceled, "canceled"),
		(PaymentState::Expired, "expired"),
	] {
		let db = TmpDb::new(&format!("lock-{label}"));
		let (app, invoices, store) = service(&db, dead).await;
		let d = draft(&invoices).await;
		let payment = start_payment(&app, &d).await;
		assert_eq!(
			store.invoice_by_id(d.id).await.unwrap().unwrap().status,
			InvoiceStatus::Pending,
			"{label}"
		);

		allocate::apply_state(&app, &payment, dead).await.unwrap();
		let inv = store.invoice_by_id(d.id).await.unwrap().unwrap();
		assert_eq!(inv.status, InvoiceStatus::Draft, "{label}");
		assert!(inv.number.is_none(), "{label}: no number was spent");
	}

	// An issued invoice paid by card: nothing to lock, and nothing to unlock afterwards.
	let db = TmpDb::new("lock-issued");
	let (app, invoices, store) = service(&db, PaymentState::Failed).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().status, InvoiceStatus::Issued);
	allocate::apply_state(&app, &payment, PaymentState::Failed).await.unwrap();
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().status, InvoiceStatus::Issued);
}

/// A terminal payment is never re-asked, so the return leg cannot walk a settled one.
#[tokio::test]
async fn a_terminal_payment_is_not_re_asked() {
	let db = TmpDb::new("terminal-read");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	let before = billing_store(&app).unwrap().payment(payment.id).await.unwrap().unwrap();
	allocate::refresh_invoice(&app, &ctx(), &inv.uid).await.unwrap();

	let after = billing_store(&app).unwrap().payment(payment.id).await.unwrap().unwrap();
	assert_eq!(after.status, PaymentState::Succeeded);
	assert_eq!(after.updated_at, before.updated_at, "nothing was written");
	assert_eq!(billing_store(&app).unwrap().allocations(payment.id).await.unwrap().len(), 1);
}

/// The payer who closed the tab: nobody reads the invoice page, so the sweep is what asks.
#[tokio::test]
async fn a_forgotten_payment_is_swept() {
	let db = TmpDb::new("sweep");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let d = draft(&invoices).await;
	let payment = start_payment(&app, &d).await;

	// Older than `STALE_AFTER_SECS`, so the sweep considers it; `created_at` stays fresh, so it
	// is not yet abandoned.
	sqlx::query("UPDATE payments SET updated_at = ? WHERE id = ?")
		.bind(Timestamp::now().0 - 600)
		.bind(payment.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	saas_billing::sweep::tick(&app).await.unwrap();

	let inv = store.invoice_by_id(d.id).await.unwrap().unwrap();
	assert_eq!(inv.status, InvoiceStatus::Paid);
	assert_eq!(
		billing_store(&app).unwrap().payment(payment.id).await.unwrap().unwrap().status,
		PaymentState::Succeeded
	);
}

/// A draft with an explicit `due_date`, for the dunning sweep: `issue::plan` otherwise derives
/// one from `invoice.default_payment_days` and nothing is ever overdue.
async fn overdue_draft(invoices: &Invoices) -> Invoice {
	invoices
		.draft(
			&ctx(),
			&NewDraft {
				fulfilment_date: Some("2026-02-01".into()),
				due_date: Some("2026-02-08".into()),
				billing_party: Party::OrgDefault,
				lines: vec![Line {
					code: None,
					description: "Tanacsadas".into(),
					unit: "ora".into(),
					qty: Qty(1_000_000),
					unit_price: Some(Money(NET)),
					vat_code: Some(VatCode::Std27),
					discount: None,
					discount_description: None,
					note: None,
				}],
				..Default::default()
			},
		)
		.await
		.unwrap()
}

/// `payments.request_id` is client text, and the lookup that answers a retried start used to be
/// global: posting another org's key handed back *their* payment — uid, amount, providerRef
/// and redirect — with a 201, while the poster's own invoice stayed unpaid.
#[tokio::test]
async fn another_orgs_request_id_discloses_nothing() {
	let db = TmpDb::new("cross-org-key");
	let (app, invoices, store) = service(&db, PaymentState::Pending).await;
	let mine = issued(&app, &invoices, &store).await;

	let (first, _) = allocate::start(
		&app,
		&ctx(),
		&mine.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: Some("req-shared".into()),
			return_url: "https://app.invalid/done".into(),
			locale: None,
		},
	)
	.await
	.unwrap();

	// A second org, with its own default billing party and its own issued invoice.
	sqlx::query(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES (2, 'org_other', 1, 'SHARED', 'Masik', 1, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street,
		  is_default, created_at, updated_at)
		 VALUES (2, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0C', 2, 'C', 'Masik Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 3.', 1, 0, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	let other_ctx = Ctx::system("test").with_org(2);
	let theirs = invoices
		.draft(
			&other_ctx,
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![Line {
					code: None,
					description: "Tanacsadas".into(),
					unit: "ora".into(),
					qty: Qty(1_000_000),
					unit_price: Some(Money(NET)),
					vat_code: Some(VatCode::Std27),
					discount: None,
					discount_description: None,
					note: None,
				}],
				..Default::default()
			},
		)
		.await
		.unwrap();
	let theirs = saas_invoice::issue::run(&app, &store, theirs).await.unwrap();

	// `UNIQUE (org_id, request_id)`, so org B's own `"req-shared"` is its own payment —
	// a global key gave B a permanent conflict on a key it had never used, and a probe for A's.
	let (theirs_payment, _) = allocate::start(
		&app,
		&other_ctx,
		&theirs.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: Some("req-shared".into()),
			return_url: "https://app.invalid/done".into(),
			locale: None,
		},
	)
	.await
	.unwrap();
	assert_ne!(theirs_payment.id, first.id, "a separate payment, not org 1's");
	assert_eq!(theirs_payment.org_id, 2);

	// And nothing of org 1's is reachable from org 2.
	assert_eq!(
		billing_store(&app)
			.unwrap()
			.payments_by_invoice(2, theirs.id)
			.await
			.unwrap()
			.len(),
		1,
		"org 1's payment {} was never disclosed",
		first.uid.as_str()
	);
	assert!(
		billing_store(&app)
			.unwrap()
			.payment_by_uid(Some(2), &first.uid)
			.await
			.unwrap()
			.is_none()
	);

	// The same org twice is still a conflict: that is what makes the start idempotent.
	let err = allocate::start(
		&app,
		&other_ctx,
		&theirs.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: Some("req-shared".into()),
			return_url: "https://app.invalid/done".into(),
			locale: None,
		},
	)
	.await
	.unwrap();
	assert_eq!(err.0.id, theirs_payment.id, "the retry finds the payment it already made");
}

/// Same org, same key, *other* invoice: answering with the stored payment redirected the
/// payer to pay invoice A off invoice B's page.
#[tokio::test]
async fn a_request_id_belongs_to_one_invoice() {
	let db = TmpDb::new("key-other-invoice");
	let (app, invoices, store) = service(&db, PaymentState::Pending).await;
	let first = issued(&app, &invoices, &store).await;
	let second = issued(&app, &invoices, &store).await;

	let req = |uid: &InvoiceId| {
		(
			uid.clone(),
			StartRequest {
				provider: "stub".into(),
				request_id: Some("req-2".into()),
				return_url: "https://app.invalid/done".into(),
				locale: None,
			},
		)
	};
	let (uid, r) = req(&first.uid);
	allocate::start(&app, &ctx(), &uid, r).await.unwrap();

	let (uid, r) = req(&second.uid);
	let err = allocate::start(&app, &ctx(), &uid, r).await.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-NOT-PAYABLE", .. }), "{err:?}");
}

/// The gateway sends the payer wherever `returnUrl` points, so a checkout naming a foreign one
/// would land them on somebody else's page off a genuine success.
#[tokio::test]
async fn a_foreign_return_url_is_refused() {
	let db = TmpDb::new("return-url");
	let (app, invoices, store) = service(&db, PaymentState::Pending).await;
	let inv = issued(&app, &invoices, &store).await;

	let err = allocate::start(
		&app,
		&ctx(),
		&inv.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: None,
			return_url: "https://evil.example/".into(),
			locale: None,
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-RETURN-URL", .. }), "{err:?}");

	// A bare `starts_with` passes this, and the gateway sends the payer there after they
	// have paid.
	let err = allocate::start(
		&app,
		&ctx(),
		&inv.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: None,
			return_url: "https://app.invalid.evil.example/".into(),
			locale: None,
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-RETURN-URL", .. }), "{err:?}");

	// Refused before the row, so no `payments` row owns a `request_id` on its way out.
	assert!(
		billing_store(&app)
			.unwrap()
			.list_payments(ORG, &PaymentFilter { limit: 10, ..Default::default() })
			.await
			.unwrap()
			.is_empty()
	);
}

/// Gateways run 3DS risk scoring on the items and the billing address, and the block used to go
/// out empty — which only the first real gateway call would have shown.
#[tokio::test]
async fn a_start_carries_the_invoice_lines_and_the_buyer() {
	let db = TmpDb::new("start-items");
	let stub = Stub::new(PaymentState::Pending);
	let seen = Arc::clone(&stub.started);
	let (app, invoices, store) = service_with(&db, stub).await;
	let inv = issued(&app, &invoices, &store).await;

	start_payment(&app, &inv).await;

	let sent = seen.lock().unwrap().clone().expect("the gateway was called");
	assert_eq!(sent.items.len(), 1);
	// One unit at the line's gross, with the real quantity folded into the name: a gateway
	// cross-checks `ItemTotal == Quantity × UnitPrice`, and the invoice's own pair is a *net*
	// unit price against a *gross* total, which contradicts itself on the wire.
	assert_eq!(sent.items[0].name, "1.000000 × ora Tanacsadas");
	assert_eq!(sent.items[0].unit, "db");
	assert_eq!(sent.items[0].qty, Qty(1_000_000));
	assert_eq!(sent.items[0].unit_price, Money(GROSS));
	assert_eq!(sent.items[0].total, Money(GROSS));
	// And the items sum to the transaction total, which is the other thing a gateway rejects.
	assert_eq!(sent.items.iter().map(|i| i.total.0).sum::<i64>(), sent.amount.0);
	let billing = sent.billing.expect("the frozen buyer snapshot");
	assert_eq!(billing.country.as_deref(), Some("HU"));
	assert_eq!(billing.city.as_deref(), Some("Budapest"));
	assert_eq!(sent.payer_email.as_deref(), Some("vevo@e.st"));
	assert_eq!(sent.amount, Money(GROSS));
}

/// `Settlement { from: vec![payment.status], to: payment.status }` makes every status
/// self-legal, so the store's own guard cannot refuse this: an operator could allocate a FAILED
/// or EXPIRED payment and drive the invoice to PAID with money that never arrived.
#[tokio::test]
async fn a_payment_that_never_arrived_cannot_be_allocated() {
	let db = TmpDb::new("allocate-state");
	let (app, invoices, store) = service(&db, PaymentState::Expired).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	let err = allocate::allocate(
		&app,
		&ctx(),
		&payment.uid,
		&Allocation {
			invoice_uid: inv.uid.clone(),
			amount: Money(GROSS),
			currency: CurrencyCode::huf(),
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-STATE", .. }), "{err:?}");
	let untouched = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(untouched.status, InvoiceStatus::Issued);
	assert_eq!(untouched.paid_amount, Money::ZERO);
}

/// The correction `E-PAY-ALREADY-ALLOCATED`'s own message instructs an operator to make. There
/// is no delete route, so without this a mis-allocation was uncorrectable — and the block had no
/// floor, so a negative against an invoice with no allocation drove `paid_amount` below zero.
#[tokio::test]
async fn a_negative_allocation_reverses_a_settlement() {
	let db = TmpDb::new("reverse-alloc");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;

	let payment = allocate::manual(
		&app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "TRANSFER".into(),
			// Twice the invoice, so the `E-PAY-ALLOC-EXCEEDS` ceiling is never what answers a
			// second positive allocation — the duplicate check is what does.
			amount: Money(GROSS * 2),
			currency: inv.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: None,
			note: None,
			allocations: vec![Allocation {
				invoice_uid: inv.uid.clone(),
				amount: Money(GROSS),
				currency: CurrencyCode::huf(),
			}],
		},
	)
	.await
	.unwrap();
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().status, InvoiceStatus::Paid);

	// The conflict the reversal exists to let an operator correct.
	let err = allocate::allocate(
		&app,
		&ctx(),
		&payment.uid,
		&Allocation {
			invoice_uid: inv.uid.clone(),
			amount: Money(1),
			currency: CurrencyCode::huf(),
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-ALREADY-ALLOCATED", .. }), "{err:?}");

	let alloc = |amount: i64| Allocation {
		invoice_uid: inv.uid.clone(),
		amount: Money(amount),
		currency: CurrencyCode::huf(),
	};

	// Zero is the link row's own amount, so it is never a correction anybody meant to make.
	let err = allocate::allocate(&app, &ctx(), &payment.uid, &alloc(0)).await.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-AMOUNT", .. }), "{err:?}");

	// More than this payment put on the invoice is refused, so the sum cannot go negative.
	let err = allocate::allocate(&app, &ctx(), &payment.uid, &alloc(-GROSS - 1))
		.await
		.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-ALLOC-EXCEEDS", .. }), "{err:?}");

	allocate::allocate(&app, &ctx(), &payment.uid, &alloc(-GROSS)).await.unwrap();
	let back = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(back.paid_amount, Money::ZERO);
	assert!(back.paid_at.is_none());
	assert_eq!(back.status, InvoiceStatus::Issued, "the status walks back with the sum");

	// And now it is unallocated again, so a positive one is legal a second time.
	allocate::allocate(&app, &ctx(), &payment.uid, &alloc(GROSS)).await.unwrap();
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().status, InvoiceStatus::Paid);
}

/// A settled payment given back in full: the invoice walks `PAID -> ISSUED`, `paid_amount` drops
/// to zero, and the payment reads `REFUNDED`.
#[tokio::test]
async fn a_full_refund_walks_the_invoice_back() {
	let db = TmpDb::new("refund-full");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	let back = saas_billing::refund(&app, &ctx(), &payment.uid, None, Some("goodwill".into()))
		.await
		.unwrap();
	assert_eq!(back.status, PaymentState::Refunded);
	assert_eq!(back.refunded_amount, Money(GROSS));

	let reversed = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(reversed.paid_amount, Money::ZERO);
	assert!(reversed.paid_at.is_none());
	assert_eq!(reversed.status, InvoiceStatus::Issued);
}

/// A refund eats the payment's unallocated part first, so giving back an overpayment leaves
/// the invoice it settled alone — reversing the whole refund drove `paid_amount` negative and
/// made `overdue_invoices` chase for more than the invoice.
#[tokio::test]
async fn refunding_an_overpayment_leaves_the_invoice_settled() {
	let db = TmpDb::new("refund-overpay");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;

	let payment = allocate::manual(
		&app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "TRANSFER".into(),
			amount: Money(GROSS * 2),
			currency: inv.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: None,
			note: None,
			allocations: vec![Allocation {
				invoice_uid: inv.uid.clone(),
				amount: Money(GROSS),
				currency: CurrencyCode::huf(),
			}],
		},
	)
	.await
	.unwrap();

	let back = saas_billing::refund(
		&app,
		&ctx(),
		&payment.uid,
		Some((Money(GROSS), CurrencyCode::huf())),
		None,
	)
	.await
	.unwrap();
	assert_eq!(back.refunded_amount, Money(GROSS));
	let still = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(still.status, InvoiceStatus::Paid);
	assert_eq!(still.paid_amount, Money(GROSS), "only the overpayment went back");

	// The remainder has nowhere else to come from, so now the allocation reverses.
	saas_billing::refund(
		&app,
		&ctx(),
		&payment.uid,
		Some((Money(GROSS), CurrencyCode::huf())),
		None,
	)
	.await
	.unwrap();
	let walked = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(walked.status, InvoiceStatus::Issued);
	assert_eq!(walked.paid_amount, Money::ZERO, "never negative");
}

/// A partial refund lowers the allocation sum but not `payments.amount`, so the ceiling has to
/// subtract `refunded_amount` — otherwise the freed headroom settles a second invoice off
/// money the payer already got back.
#[tokio::test]
async fn refunded_money_cannot_be_allocated_again() {
	let db = TmpDb::new("refund-realloc");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let a = issued(&app, &invoices, &store).await;
	let b = issued(&app, &invoices, &store).await;

	let payment = allocate::manual(
		&app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "TRANSFER".into(),
			amount: Money(GROSS),
			currency: a.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: None,
			note: None,
			allocations: vec![Allocation {
				invoice_uid: a.uid.clone(),
				amount: Money(GROSS),
				currency: CurrencyCode::huf(),
			}],
		},
	)
	.await
	.unwrap();

	saas_billing::refund(
		&app,
		&ctx(),
		&payment.uid,
		Some((Money(GROSS / 2), CurrencyCode::huf())),
		None,
	)
	.await
	.unwrap();

	let err = allocate::allocate(
		&app,
		&ctx(),
		&payment.uid,
		&Allocation {
			invoice_uid: b.uid.clone(),
			amount: Money(GROSS / 2),
			currency: CurrencyCode::huf(),
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-ALLOC-EXCEEDS", .. }), "{err:?}");
	assert_eq!(store.invoice_by_id(b.id).await.unwrap().unwrap().paid_amount, Money::ZERO);
}

/// A partial refund moves `refunded_amount` and nothing else: `REFUNDED` is derived from the
/// whole amount being back, and no gateway status maps to it.
#[tokio::test]
async fn a_partial_refund_leaves_the_status_alone() {
	let db = TmpDb::new("refund-partial");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	let back = saas_billing::refund(
		&app,
		&ctx(),
		&payment.uid,
		Some((Money(1_000), CurrencyCode::huf())),
		None,
	)
	.await
	.unwrap();
	assert_eq!(back.status, PaymentState::Succeeded);
	assert_eq!(back.refunded_amount, Money(1_000));
	assert_eq!(
		store.invoice_by_id(inv.id).await.unwrap().unwrap().paid_amount,
		Money(GROSS - 1_000)
	);
}

/// The three ways a refund is refused before the gateway is asked: more than is left, a gateway
/// that cannot do partials, and a payment the money never arrived on.
#[tokio::test]
async fn a_refund_that_cannot_be_made_is_refused() {
	let db = TmpDb::new("refund-refused");
	let stub = Stub { partial_refund: false, ..Stub::new(PaymentState::Succeeded) };
	let (app, invoices, store) = service_with(&db, stub).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;

	// Still `PENDING`: no callback has landed, so there is nothing to give back.
	let err = saas_billing::refund(&app, &ctx(), &payment.uid, None, None).await.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-STATE", .. }), "{err:?}");

	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	let err = saas_billing::refund(
		&app,
		&ctx(),
		&payment.uid,
		Some((Money(GROSS + 1), CurrencyCode::huf())),
		None,
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-AMOUNT", .. }), "{err:?}");

	let err = saas_billing::refund(
		&app,
		&ctx(),
		&payment.uid,
		Some((Money(1_000), CurrencyCode::huf())),
		None,
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-CAPABILITY", .. }), "{err:?}");

	// Nothing was written by any of the three.
	assert_eq!(
		billing_store(&app)
			.unwrap()
			.payment(payment.id)
			.await
			.unwrap()
			.unwrap()
			.refunded_amount,
		Money::ZERO
	);
}

/// The `dunning:{invoice_id}:{step}` `dedup_key` *is* the "already sent" record, so a second
/// sweep on the same day enqueues nothing.
#[tokio::test]
async fn the_dunning_sweep_mails_each_step_once() {
	let db = TmpDb::new("dunning");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let d = overdue_draft(&invoices).await;
	let inv = saas_invoice::issue::run(&app, &store, d).await.unwrap();

	let queued = || async {
		sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = 'SEND_EMAIL'")
			.fetch_one(store.read_pool())
			.await
			.unwrap()
	};

	saas_billing::dunning::sweep(&app).await.unwrap();
	assert_eq!(queued().await, 1);
	saas_billing::dunning::sweep(&app).await.unwrap();
	assert_eq!(queued().await, 1, "the dedup_key is the record that this step went out");

	let key: String = sqlx::query_scalar("SELECT dedup_key FROM jobs WHERE kind = 'SEND_EMAIL'")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert!(key.starts_with(&format!("dunning:{}:", inv.id)), "{key}");

	// The payload is hand-built here because this crate cannot depend on `saas-email`, so
	// nothing but this keeps it and the template in step.
	let payload: String = sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'SEND_EMAIL'")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	let mail: saas_email::SendEmail = serde_json::from_str(&payload).unwrap();
	let body = std::fs::read_to_string(
		std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
			.join("../../templates/email")
			.join(format!("{}.txt.hbs", mail.template)),
	)
	.unwrap();
	for var in mail.vars.as_object().unwrap().keys() {
		assert!(
			body.contains(&format!("{{{{{var}}}}}")),
			"{var} is in the payload, not the template"
		);
	}

	// An empty schedule disables dunning outright, which is what `settings.rs` documents.
	app.settings.set("dunning.schedule_days", "", None).await.unwrap();
	saas_billing::dunning::sweep(&app).await.unwrap();
	assert_eq!(queued().await, 1);
}

/// `settle_full`'s "SUCCEEDED with no invoice to settle" branch writes a `tracing::warn` and
/// nothing else; the payer has been charged and no invoice reads paid, so this is the standing
/// record an operator can act on.
#[tokio::test]
async fn unallocated_money_raises_an_alert() {
	let db = TmpDb::new("unallocated");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;

	let entry = |allocations| ManualPayment {
		org_uid: OrgId::from_trusted(ORG_UID.to_string()),
		kind: "TRANSFER".into(),
		amount: Money(GROSS),
		currency: inv.currency.clone(),
		received_at: Timestamp::now(),
		ext_ref: None,
		note: None,
		allocations,
	};

	let settled = allocate::manual(
		&app,
		&ctx(),
		entry(vec![Allocation {
			invoice_uid: inv.uid.clone(),
			amount: Money(GROSS),
			currency: CurrencyCode::huf(),
		}]),
	)
	.await
	.unwrap();
	let loose = allocate::manual(&app, &ctx(), entry(Vec::new())).await.unwrap();

	// Inside the 48 h window, so neither is old enough yet.
	assert!(saas_billing::alerts(app.clone()).await.unwrap().is_empty());

	for id in [settled.id, loose.id] {
		sqlx::query("UPDATE payments SET received_at = ?, updated_at = ? WHERE id = ?")
			.bind(Timestamp::now().0 - 3 * 86_400)
			.bind(Timestamp::now().0 - 3 * 86_400)
			.bind(id)
			.execute(store.write_pool())
			.await
			.unwrap();
	}

	let alerts = saas_billing::alerts(app.clone()).await.unwrap();
	assert_eq!(alerts.len(), 1);
	assert_eq!(alerts[0].code, "A-PAY-UNALLOCATED");
	assert_eq!(alerts[0].count, 1, "the settled one is covered by its allocation");
	assert!(alerts[0].since.is_some());
}

/// Áfa tv. 172. § and Hungarian practice put the áthárított adó in whole forints, so a HUF
/// invoice's gross carries no fillér and a gateway that takes whole forints can be handed it.
/// 4 990 Ft net at 27% is 1 347.30 Ft exactly — the case that used to be unpayable by card.
#[tokio::test]
async fn a_huf_invoice_is_payable_in_whole_forints() {
	let db = TmpDb::new("huf-whole");
	let stub = Stub::new(PaymentState::Succeeded);
	let seen = Arc::clone(&stub.started);
	let (app, invoices, store) = service_with(&db, stub).await;

	let d = invoices
		.draft(
			&ctx(),
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![Line {
					code: None,
					description: "Tanacsadas".into(),
					unit: "ora".into(),
					qty: Qty(1_000_000),
					unit_price: Some(Money(499_000)),
					vat_code: Some(VatCode::Std27),
					discount: None,
					discount_description: None,
					note: None,
				}],
				..Default::default()
			},
		)
		.await
		.unwrap();
	let inv = saas_invoice::issue::run(&app, &store, d).await.unwrap();

	// The VAT engine rounded the group VAT, so the invoice itself is whole forints.
	assert_eq!(inv.net, Money(499_000));
	assert_eq!(inv.vat, Money(134_700), "1 347.30 -> 1 347 Ft");
	assert_eq!(inv.gross, Money(633_700));

	let payment = start_payment(&app, &inv).await;
	assert_eq!(payment.amount, Money(633_700), "charged exactly the gross, no surplus");
	let sent = seen.lock().unwrap().clone().expect("the gateway was called");
	assert_eq!(sent.amount.0 % 100, 0, "a whole forint reaches the gateway");
	assert_eq!(sent.items.iter().map(|i| i.total.0).sum::<i64>(), sent.amount.0);

	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);
	let paid = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(paid.status, InvoiceStatus::Paid);
	assert_eq!(paid.paid_amount, Money(633_700));
}

/// A legacy invoice issued before the VAT engine rounded is immutable, so the charge rounding
/// at `start` is what keeps it payable at all: up to the currency's step, never down, or the
/// invoice stays a few fillér short of `gross` forever and is dunned for good.
#[tokio::test]
async fn a_legacy_filler_gross_is_still_payable() {
	let db = TmpDb::new("huf-legacy");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;

	// Straight to the row, which is the only way to get the shape the old engine produced.
	sqlx::query("UPDATE invoices SET vat = ?, gross = ? WHERE id = ?")
		.bind(134_730_i64)
		.bind(633_730_i64)
		.bind(inv.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	let payment = start_payment(&app, &store.invoice_by_id(inv.id).await.unwrap().unwrap()).await;
	assert_eq!(payment.amount, Money(633_800), "rounded up to the whole forint");

	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);
	let paid = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	// The 70 fillér surplus is allocated to the invoice, which is why it reaches PAID.
	assert_eq!(paid.paid_amount, Money(633_800));
	assert_eq!(paid.status, InvoiceStatus::Paid);
}

/// `from_states` only ever names LIVE statuses and `PartiallySucceeded` is terminal, so the
/// gateway's later full capture settled nothing at all and one `tracing::debug` was the trace.
#[tokio::test]
async fn a_partial_that_later_succeeds_settles() {
	let db = TmpDb::new("partial-then-full");
	let (app, invoices, store) = service(&db, PaymentState::PartiallySucceeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;

	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);
	let bstore = billing_store(&app).unwrap();
	let row = bstore.payment(payment.id).await.unwrap().unwrap();
	assert_eq!(row.status, PaymentState::PartiallySucceeded);

	saas_billing::apply_state(&app, &row, PaymentState::Succeeded).await.unwrap();

	assert_eq!(bstore.payment(payment.id).await.unwrap().unwrap().status, PaymentState::Succeeded);
	let paid = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(paid.paid_amount, Money(GROSS));
	assert_eq!(paid.status, InvoiceStatus::Paid);
}

/// A gateway that captures synchronously answers `Start` with SUCCEEDED. Moving only the status
/// left the payer charged against an invoice still reading ISSUED, carrying the zero link row.
#[tokio::test]
async fn a_synchronous_capture_settles_at_start() {
	let db = TmpDb::new("sync-capture");
	let (app, invoices, store) = service_with(
		&db,
		Stub { start_state: PaymentState::Succeeded, ..Stub::new(PaymentState::Succeeded) },
	)
	.await;
	let inv = issued(&app, &invoices, &store).await;

	let payment = start_payment(&app, &inv).await;

	let bstore = billing_store(&app).unwrap();
	assert_eq!(bstore.payment(payment.id).await.unwrap().unwrap().status, PaymentState::Succeeded);
	let rows = bstore.allocations(payment.id).await.unwrap();
	assert_eq!(rows[0].amount, Money(GROSS), "not the zero link row");
	let paid = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(paid.paid_amount, Money(GROSS));
	assert_eq!(paid.status, InvoiceStatus::Paid);
}

/// A part-paid invoice charges less than its lines come to, so the item block needs a balancing
/// line: Barion rejects a `Start` whose items and `Total` disagree, and the remainder could then
/// never be paid by card at all.
#[tokio::test]
async fn a_part_paid_invoice_sends_a_balancing_item() {
	let db = TmpDb::new("part-paid-items");
	let stub = Stub::new(PaymentState::Pending);
	let seen = Arc::clone(&stub.started);
	let (app, invoices, store) = service_with(&db, stub).await;
	let inv = issued(&app, &invoices, &store).await;

	let part = Money(GROSS / 4);
	allocate::manual(
		&app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "TRANSFER".into(),
			amount: part,
			currency: inv.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: None,
			note: None,
			allocations: vec![Allocation {
				invoice_uid: inv.uid.clone(),
				amount: part,
				currency: CurrencyCode::huf(),
			}],
		},
	)
	.await
	.unwrap();

	// Rounded up to the whole forint, as every HUF charge is.
	let remainder = Money(GROSS - part.0);
	let charge = Money((remainder.0 + 99) / 100 * 100);
	let payment = start_payment(&app, &store.invoice_by_id(inv.id).await.unwrap().unwrap()).await;
	assert_eq!(payment.amount, charge);

	let sent = seen.lock().unwrap().clone().expect("the gateway was called");
	assert_eq!(sent.amount, charge);
	assert_eq!(
		sent.items.iter().map(|i| i.total.0).sum::<i64>(),
		charge.0,
		"the items sum to the transaction total"
	);
	assert_eq!(sent.items.len(), 2, "the line plus one balancing item");
	assert!(sent.items[1].total.0 < 0, "the earlier part payment comes off");
}

/// An operator entering a large transfer could drive one invoice past its own `gross`:
/// `paid_amount > gross` reads PAID and makes `OverdueInvoice::outstanding` negative.
#[tokio::test]
async fn an_allocation_cannot_exceed_the_invoice() {
	let db = TmpDb::new("over-invoice");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;

	let payment = allocate::manual(
		&app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "TRANSFER".into(),
			amount: Money(GROSS * 2),
			currency: inv.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: None,
			note: None,
			allocations: Vec::new(),
		},
	)
	.await
	.unwrap();

	let err = allocate::allocate(
		&app,
		&ctx(),
		&payment.uid,
		&Allocation {
			invoice_uid: inv.uid.clone(),
			amount: Money(GROSS * 2),
			currency: CurrencyCode::huf(),
		},
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-ALLOC-EXCEEDS", .. }), "{err:?}");
	assert_eq!(store.invoice_by_id(inv.id).await.unwrap().unwrap().paid_amount, Money::ZERO);

	// One `price_round_step` of tolerance, which is what the HUF charge round-up needs: the
	// surplus a card payment deliberately carries still has to be allocatable.
	allocate::allocate(
		&app,
		&ctx(),
		&payment.uid,
		&Allocation {
			invoice_uid: inv.uid.clone(),
			amount: Money(GROSS + 70),
			currency: CurrencyCode::huf(),
		},
	)
	.await
	.unwrap();
	let paid = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(paid.paid_amount, Money(GROSS + 70));
	assert_eq!(paid.status, InvoiceStatus::Paid);
}

/// `amount_in` parsed the declared currency and threw it away, so a refund posted as EUR
/// against a HUF payment gave back HUF and answered 200.
#[tokio::test]
async fn a_mismatched_currency_is_refused() {
	let db = TmpDb::new("currency-mismatch");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	let eur = CurrencyCode::parse("EUR").unwrap();
	let err =
		saas_billing::refund(&app, &ctx(), &payment.uid, Some((Money(1_000), eur.clone())), None)
			.await
			.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-CURRENCY", .. }), "{err:?}");

	let err = allocate::allocate(
		&app,
		&ctx(),
		&payment.uid,
		&Allocation { invoice_uid: inv.uid.clone(), amount: Money(1_000), currency: eur },
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-CURRENCY", .. }), "{err:?}");

	assert_eq!(
		billing_store(&app)
			.unwrap()
			.payment(payment.id)
			.await
			.unwrap()
			.unwrap()
			.refunded_amount,
		Money::ZERO
	);
}

/// The gateway pays out before anything records it. When `record_refund` then answers `false`
/// the money is gone and nothing says so — `A-PAY-REFUND-UNRECORDED` is what reaches a human.
#[tokio::test]
async fn an_unrecorded_payout_raises_an_alert() {
	let db = TmpDb::new("refund-unrecorded");
	let stub = Stub::new(PaymentState::Succeeded);
	let sabotage = Arc::clone(&stub.sabotage);
	let (app, invoices, store) = service_with(&db, stub).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	// The payment leaves REFUNDABLE *during* the gateway call, so the payout lands and
	// `record_refund` then refuses it — the window two concurrent partial refunds create.
	*sabotage.lock().unwrap() = Some(store.clone());

	let err = saas_billing::refund(
		&app,
		&ctx(),
		&payment.uid,
		Some((Money(1_000), CurrencyCode::huf())),
		None,
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-STATE", .. }), "{err:?}");

	let alerts = saas_billing::alerts(app.clone()).await.unwrap();
	assert!(
		alerts.iter().any(|a| a.code == "A-PAY-REFUND-UNRECORDED"),
		"the discrepancy has to reach an operator: {alerts:?}"
	);
}

/// A retry of the *same* logical refund must be the same refund at the gateway: the payout
/// happens before anything records it, so a fresh key pays out twice.
#[tokio::test]
async fn a_retried_refund_reuses_its_idempotency_key() {
	let db = TmpDb::new("refund-key");
	let stub = Stub::new(PaymentState::Succeeded);
	let keys = Arc::clone(&stub.refund_keys);
	let sabotage = Arc::clone(&stub.sabotage);
	let (app, invoices, store) = service_with(&db, stub).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);
	*sabotage.lock().unwrap() = Some(store.clone());

	// Twice, the second an operator's retry of a refund the first never recorded. Both run from
	// `refunded_amount = 0`, so both are the *same* logical refund.
	for _ in 0..2 {
		sqlx::query("UPDATE payments SET status = 'SUCCEEDED' WHERE id = ?")
			.bind(payment.id)
			.execute(store.write_pool())
			.await
			.unwrap();
		let err = saas_billing::refund(
			&app,
			&ctx(),
			&payment.uid,
			Some((Money(1_000), CurrencyCode::huf())),
			None,
		)
		.await
		.unwrap_err();
		assert!(matches!(err, Error::Coded { code: "E-PAY-STATE", .. }), "{err:?}");
	}

	let keys = keys.lock().unwrap().clone();
	assert_eq!(keys.len(), 2);
	assert_eq!(keys[0], keys[1], "the same logical refund, so the same key");
	assert!(keys[0].starts_with(payment.uid.as_str()), "{}", keys[0]);
}

/// One sweep reminds **every** overdue invoice, not just the ones the first page held. The
/// cursor itself is exercised at page boundaries in
/// `adapters/store-adapter-sqlite/tests/billing.rs`, which can seed past `BATCH` cheaply; this
/// asserts the service loop drains what the store gives it.
#[tokio::test]
async fn the_dunning_sweep_reminds_every_overdue_invoice() {
	let db = TmpDb::new("dunning-paging");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;

	// One more than a page would hold, had the page been the whole sweep.
	let mut ids = Vec::new();
	for _ in 0..3 {
		let d = overdue_draft(&invoices).await;
		ids.push(saas_invoice::issue::run(&app, &store, d).await.unwrap().id);
	}

	saas_billing::dunning::sweep(&app).await.unwrap();

	for id in ids {
		let n: i64 = sqlx::query_scalar(
			"SELECT count(*) FROM jobs WHERE kind = 'SEND_EMAIL' AND dedup_key LIKE ?",
		)
		.bind(format!("dunning:{id}:%"))
		.fetch_one(store.read_pool())
		.await
		.unwrap();
		assert_eq!(n, 1, "invoice {id} was never dunned");
	}
}

/// The sweep's own `EXPIRED`, not the gateway's: `settle_full` accepts it as a `from`, so a
/// gateway that was merely late and captured anyway is still the truth and still settles.
#[tokio::test]
async fn a_locally_expired_payment_the_gateway_captured_still_settles() {
	let db = TmpDb::new("expired-captured");
	let (app, invoices, store) = service(&db, PaymentState::Pending).await;
	let d = draft(&invoices).await;
	let payment = start_payment(&app, &d).await;
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Pending);

	// The sweep gave up on it an hour past its own deadline, and the invoice unlocked.
	let bstore = billing_store(&app).unwrap();
	let row = bstore.payment(payment.id).await.unwrap().unwrap();
	allocate::apply_state(&app, &row, PaymentState::Expired).await.unwrap();
	assert_eq!(
		store.invoice_by_id(d.id).await.unwrap().unwrap().status,
		InvoiceStatus::Draft,
		"the cart is editable again"
	);

	// The gateway answers late, with the money actually taken.
	let row = bstore.payment(payment.id).await.unwrap().unwrap();
	allocate::apply_state(&app, &row, PaymentState::Succeeded).await.unwrap();

	assert_eq!(bstore.payment(payment.id).await.unwrap().unwrap().status, PaymentState::Succeeded);
	let paid = store.invoice_by_id(d.id).await.unwrap().unwrap();
	assert_eq!(paid.status, InvoiceStatus::Paid);
	assert_eq!(paid.paid_amount, Money(GROSS));
}

/// The backstop: without it, deleting the local give-up would leave a payment a broken gateway
/// never resolves — and its invoice — locked at `PENDING` forever. `MAX_AGE_SECS` only stops the
/// asking; it does not unlock anything.
#[tokio::test]
async fn a_payment_the_gateway_never_resolves_is_expired_by_the_sweep() {
	let db = TmpDb::new("sweep-backstop");
	let (app, invoices, store) = service_with(&db, Stub::new(PaymentState::Pending)).await;
	// A draft, so the lock and the unlock are both observable: joining `PENDING` is what
	// `start` does to a draft, and `DRAFT` is what a dead payment gives back.
	let d = draft(&invoices).await;
	let payment = start_payment(&app, &d).await;
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Pending);

	// Past its own deadline by more than `GIVE_UP_AFTER_SECS`, and stale enough to be selected.
	let stale = Timestamp::now().0 - 600;
	sqlx::query("UPDATE payments SET expires_at = ?, updated_at = ? WHERE id = ?")
		.bind(stale - 3_600)
		.bind(stale)
		.bind(payment.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	saas_billing::sweep::tick(&app).await.unwrap();

	assert_eq!(
		billing_store(&app).unwrap().payment(payment.id).await.unwrap().unwrap().status,
		PaymentState::Expired,
		"a gateway that never resolves a payment does not lock its invoice forever"
	);
	assert_eq!(
		store.invoice_by_id(d.id).await.unwrap().unwrap().status,
		InvoiceStatus::Draft,
		"the dead payment handed the cart back"
	);
}

/// A row written before `expires_at` existed carries no deadline of its own. Without the
/// sweep's own `created_at + window` fallback it is never expired locally, so its invoice stays
/// locked at `PENDING` until the seven-day backstop — which only stops the asking.
#[tokio::test]
async fn a_live_payment_without_an_expiry_is_expired_by_the_sweep() {
	let db = TmpDb::new("sweep-legacy-null-expiry");
	let (app, invoices, store) = service_with(&db, Stub::new(PaymentState::Pending)).await;
	let d = draft(&invoices).await;
	let payment = start_payment(&app, &d).await;
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Pending);

	// The legacy shape: live, gateway-backed, and no deadline of its own. `created_at + window`
	// is therefore the deadline, and it is already past the give-up grace.
	let window = app.settings.int("payment.window_minutes").await.unwrap() * 60;
	let stale = Timestamp::now().0 - 600;
	sqlx::query(
		"UPDATE payments SET expires_at = NULL, created_at = ?, updated_at = ? WHERE id = ?",
	)
	.bind(stale - window - 3_600 - 1)
	.bind(stale)
	.bind(payment.id)
	.execute(store.write_pool())
	.await
	.unwrap();

	saas_billing::sweep::tick(&app).await.unwrap();

	assert_eq!(
		billing_store(&app).unwrap().payment(payment.id).await.unwrap().unwrap().status,
		PaymentState::Expired,
		"a legacy row with NULL `expires_at` was never given up on"
	);
	assert_eq!(
		store.invoice_by_id(d.id).await.unwrap().unwrap().status,
		InvoiceStatus::Draft,
		"the dead payment handed the cart back"
	);
}

/// A row still LIVE past `MAX_AGE_SECS` is exactly what the give-up backstop exists for.
#[tokio::test]
async fn the_sweep_expires_a_payment_older_than_the_gateway_horizon() {
	let db = TmpDb::new("sweep-beyond-max-age");
	let stub = Stub::new(PaymentState::Pending);
	let fetched = Arc::clone(&stub.fetched);
	let (app, invoices, store) = service_with(&db, stub).await;
	let d = draft(&invoices).await;
	let payment = start_payment(&app, &d).await;
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Pending);

	// Past `MAX_AGE_SECS` on both stamps: the sweep has to rule on it without asking the gateway.
	let old = Timestamp::now().0 - 8 * 86_400;
	sqlx::query("UPDATE payments SET created_at = ?, updated_at = ? WHERE id = ?")
		.bind(old)
		.bind(old)
		.bind(payment.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	saas_billing::sweep::tick(&app).await.unwrap();

	assert_eq!(
		billing_store(&app).unwrap().payment(payment.id).await.unwrap().unwrap().status,
		PaymentState::Expired,
		"a payment past the gateway horizon was never given up on"
	);
	assert_eq!(
		store.invoice_by_id(d.id).await.unwrap().unwrap().status,
		InvoiceStatus::Draft,
		"the dead payment handed the cart back"
	);
	assert!(fetched.lock().unwrap().is_empty(), "an over-age row still cost a gateway call");
}

/// A resurrected cancellation must not unlock an invoice a *newer* gateway payment holds: the
/// draft would become editable while a card charge for the old total is still in flight.
#[tokio::test]
async fn a_stale_cancellation_does_not_unlock_an_invoice_a_newer_payment_holds() {
	let db = TmpDb::new("stale-cancel-unlock");
	let (app, invoices, store) = service_with(&db, Stub::new(PaymentState::Pending)).await;
	let d = draft(&invoices).await;
	let first = start_payment(&app, &d).await;

	// The old local give-up: cancelled and the cart handed back.
	let bstore = billing_store(&app).unwrap();
	let row = bstore.payment(first.id).await.unwrap().unwrap();
	allocate::apply_state(&app, &row, PaymentState::Canceled).await.unwrap();
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Draft);

	// A second attempt re-locks the same draft, and this one is still live.
	let _second = start_payment(&app, &d).await;
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Pending);

	// v12 puts the abandoned row back to `PENDING`; the gateway still says `Canceled`.
	sqlx::query("UPDATE payments SET status = 'PENDING' WHERE id = ?")
		.bind(first.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let row = bstore.payment(first.id).await.unwrap().unwrap();
	allocate::apply_state(&app, &row, PaymentState::Canceled).await.unwrap();

	assert_eq!(
		store.invoice_by_id(d.id).await.unwrap().unwrap().status,
		InvoiceStatus::Pending,
		"a stale cancellation unlocked an invoice a newer live payment holds"
	);
}

/// The previous release's local give-up was `CANCELED`, which no `from` list accepted, so a
/// gateway that captured the money afterwards was never recorded. v12 resurrects the row, and
/// this is what it is resurrected for.
#[tokio::test]
async fn a_canceled_payment_the_gateway_now_captures_still_settles() {
	let db = TmpDb::new("canceled-captured-settles");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let d = draft(&invoices).await;
	let payment = start_payment(&app, &d).await;
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Pending);

	// A local give-up: cancelled locally, nothing told the gateway, cart editable again.
	let bstore = billing_store(&app).unwrap();
	let row = bstore.payment(payment.id).await.unwrap().unwrap();
	allocate::apply_state(&app, &row, PaymentState::Canceled).await.unwrap();
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Draft);

	// The v12 upgrade, applied to the one row.
	sqlx::query("UPDATE payments SET status = 'PENDING' WHERE id = ?")
		.bind(payment.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let row = bstore.payment(payment.id).await.unwrap().unwrap();
	assert_eq!(row.status, PaymentState::Pending);

	// The gateway is asked again and says the money was taken after all.
	allocate::apply_state(&app, &row, PaymentState::Succeeded).await.unwrap();

	assert_eq!(bstore.payment(payment.id).await.unwrap().unwrap().status, PaymentState::Succeeded);
	let paid = store.invoice_by_id(d.id).await.unwrap().unwrap();
	assert_eq!(paid.status, InvoiceStatus::Paid);
	assert_eq!(paid.paid_amount, Money(GROSS));
}

/// The deadline is our own clock, written by `set_started` once the gateway accepted — so a
/// start the gateway refused has none, and is left for the 7-day backstop.
#[tokio::test]
async fn a_started_payment_carries_its_expiry() {
	let db = TmpDb::new("start-expiry");
	let (app, invoices, store) = service(&db, PaymentState::Pending).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;

	let window = app.settings.int("payment.window_minutes").await.unwrap() * 60;
	let row = billing_store(&app).unwrap().payment(payment.id).await.unwrap().unwrap();
	let expires = row.expires_at.expect("a started payment carries a deadline");
	// `<= 1`, not equality: `created_at` is the row's own `now` and the deadline the gateway's
	// `set_started`, and the clock may tick between them.
	assert!(
		(expires.0 - row.created_at.0 - window).abs() <= 1,
		"{} is not {} + {window}",
		expires.0,
		row.created_at.0
	);

	// The gateway refused, so nothing was stamped and nothing was locked.
	let db = TmpDb::new("start-refused-expiry");
	let (app, invoices, store) =
		service_with(&db, Stub { fail_start: true, ..Stub::new(PaymentState::Pending) }).await;
	let inv = issued(&app, &invoices, &store).await;
	allocate::start(
		&app,
		&ctx(),
		&inv.uid,
		StartRequest {
			provider: "stub".into(),
			request_id: Some("req-refused".into()),
			return_url: "https://app.invalid/done".into(),
			locale: None,
		},
	)
	.await
	.unwrap_err();
	let refused = billing_store(&app)
		.unwrap()
		.payment_by_request_id(ORG, "req-refused")
		.await
		.unwrap()
		.expect("the row was committed before the gateway was called");
	assert!(refused.expires_at.is_none(), "a refused start has no window to honour");
}

/// Two refunds in flight both read `refunded_amount = 0`, so both derive the *same* gateway
/// idempotency key: Barion dedupes and pays out once. Without the compare-and-set both recorded
/// it, doubling `refunded_amount` and dunning the customer for money they do not owe.
#[tokio::test]
async fn two_concurrent_refunds_record_once() {
	let db = TmpDb::new("refund-cas");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	// Both built from the one pre-refund snapshot, which is what makes them one gateway refund.
	let record = || RefundRecord {
		payment_id: payment.id,
		from: vec![PaymentState::Succeeded],
		to: PaymentState::Succeeded,
		amount: Money(1_000),
		expect_refunded: Money::ZERO,
		reverse: Money(1_000),
		invoice_id: Some(inv.id),
		at: Timestamp::now(),
		by: None,
	};
	let bstore = billing_store(&app).unwrap();
	assert!(bstore.record_refund(&record()).await.unwrap());
	assert!(!bstore.record_refund(&record()).await.unwrap(), "one payout, one record");

	assert_eq!(bstore.payment(payment.id).await.unwrap().unwrap().refunded_amount, Money(1_000));
	let back = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(back.paid_amount, Money(GROSS - 1_000), "the allocation dropped once");
	assert_eq!(back.status, InvoiceStatus::Issued);
}

/// A `PARTIALLY_SUCCEEDED` payment allocates nothing automatically, so an operator allocates it
/// by hand. `allocations` orders by `invoice_id`, so the full capture that followed settled
/// whichever invoice sorted first — the hand-allocated one, not the one the payer opened.
#[tokio::test]
async fn a_hand_allocated_partial_does_not_settle_the_wrong_invoice() {
	let db = TmpDb::new("partial-hand-alloc");
	let (app, invoices, store) = service(&db, PaymentState::PartiallySucceeded).await;
	let a = issued(&app, &invoices, &store).await;
	let b = issued(&app, &invoices, &store).await;
	assert!(a.id < b.id, "A has to sort first for this to be the wrong pick");

	let payment = start_payment(&app, &b).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	allocate::allocate(
		&app,
		&ctx(),
		&payment.uid,
		&Allocation {
			invoice_uid: a.uid.clone(),
			amount: Money(300),
			currency: CurrencyCode::huf(),
		},
	)
	.await
	.unwrap();

	let bstore = billing_store(&app).unwrap();
	let row = bstore.payment(payment.id).await.unwrap().unwrap();
	allocate::apply_state(&app, &row, PaymentState::Succeeded).await.unwrap();

	assert_eq!(bstore.payment(payment.id).await.unwrap().unwrap().status, PaymentState::Succeeded);
	let allocated: i64 =
		bstore.allocations(payment.id).await.unwrap().iter().map(|x| x.amount.0).sum();
	assert!(allocated <= payment.amount.0, "{allocated} allocated of {}", payment.amount.0);
	assert_eq!(store.invoice_by_id(a.id).await.unwrap().unwrap().paid_amount, Money(300));
	assert_eq!(
		store.invoice_by_id(b.id).await.unwrap().unwrap().paid_amount,
		Money::ZERO,
		"the invoice the payment was opened for was not settled off A's allocation"
	);
}

/// `apply_state` writes nothing when the fetched state is the stored one, so `updated_at` never
/// moves: a flat `LIMIT` handed the sweep the same oldest page every five minutes and payment
/// 201 was never re-asked until it aged out entirely.
#[tokio::test]
async fn the_sweep_reaches_past_its_first_batch() {
	let db = TmpDb::new("sweep-paging");
	let stub = Stub::new(PaymentState::Pending);
	let fetched = Arc::clone(&stub.fetched);
	let (app, _invoices, store) = service_with(&db, stub).await;

	// Straight into the table: what the sweep pages over is `payments`, and 201 gateway starts
	// would buy nothing. The stub answers PENDING — the state they already hold.
	let n_rows: usize = 201;
	let stale = Timestamp::now().0 - 600;
	for n in 0..n_rows {
		sqlx::query(
			"INSERT INTO payments (uid, org_id, kind, provider, provider_ref, status, amount,
			  currency, created_at, updated_at)
			 VALUES (?, ?, 'STUB', 'stub', ?, 'PENDING', 1000, 'HUF', ?, ?)",
		)
		.bind(format!("pay_sweep{n:04}"))
		.bind(ORG)
		.bind(format!("prv-sweep-{n}"))
		.bind(stale)
		.bind(stale)
		.execute(store.write_pool())
		.await
		.unwrap();
	}

	saas_billing::sweep::tick(&app).await.unwrap();

	assert_eq!(fetched.lock().unwrap().len(), n_rows, "the sweep stopped at its first batch");
}

/// The "vissza" case: a payer who presses Barion's own back button leaves a `CANCELED` row,
/// which is the *gateway's* verdict and final, so it is never re-asked or swept again.
#[tokio::test]
async fn a_gateway_cancellation_is_never_re_asked() {
	let db = TmpDb::new("cancel-final");
	let stub = Stub::new(PaymentState::Pending);
	let fetched = Arc::clone(&stub.fetched);
	let (app, _invoices, store) = service_with(&db, stub).await;
	stale_payment(&store, "pay_canceled", "CANCELED", Timestamp::now().0 - 600, None).await;

	let bstore = billing_store(&app).unwrap();
	let now = Timestamp::now();
	let rows = bstore.live_payments(Timestamp(now.0 - 120), None, 200).await.unwrap();
	assert!(rows.is_empty(), "a gateway cancellation is final, not swept");

	saas_billing::sweep::tick(&app).await.unwrap();
	assert!(fetched.lock().unwrap().is_empty(), "the sweep asked about a canceled payment");
}

/// One gateway payment straight into the table. `created_at`/`updated_at` are both backdated
/// past `STALE_AFTER_SECS` and inside `MAX_AGE_SECS`, so it is exactly what the sweep selects;
/// `expires_at` is the deadline the backstop gives up on.
async fn stale_payment(
	store: &SqliteStore,
	uid: &str,
	status: &str,
	updated_at: i64,
	expires_at: Option<i64>,
) {
	sqlx::query(
		"INSERT INTO payments (uid, org_id, kind, provider, provider_ref, status, amount,
		  currency, created_at, updated_at, expires_at)
		 VALUES (?, ?, 'STUB', 'stub', ?, ?, 1000, 'HUF', ?, ?, ?)",
	)
	.bind(uid)
	.bind(ORG)
	.bind(format!("prv-{uid}"))
	.bind(status)
	.bind(updated_at)
	.bind(updated_at)
	.bind(expires_at)
	.execute(store.write_pool())
	.await
	.unwrap();
}

/// `n` live gateway payments the sweep will select: `PENDING`, `updated_at` past
/// `STALE_AFTER_SECS` and `created_at` inside `MAX_AGE_SECS`.
async fn stale_rows(store: &SqliteStore, n: usize) {
	let stale = Timestamp::now().0 - 600;
	for i in 0..n {
		stale_payment(store, &format!("pay_stale{i:04}"), "PENDING", stale, None).await;
	}
}

/// One row is one gateway round trip and the sweep issued them back to back — a burst the edge
/// in front of the sandbox refuses with 429 (measured: about two requests per ten seconds). The
/// first refusal now ends the pass rather than being repeated once per row.
#[tokio::test]
async fn the_sweep_stops_at_a_gateway_that_is_not_answering() {
	let db = TmpDb::new("sweep-throttled");
	let stub = Stub::new(PaymentState::Pending);
	let fetched = Arc::clone(&stub.fetched);
	let failure = Arc::clone(&stub.fetch_failure);
	let (app, _invoices, store) = service_with(&db, stub).await;
	stale_rows(&store, 4).await;
	*failure.lock().unwrap() = Some(Retry::Backoff);

	saas_billing::sweep::tick(&app).await.unwrap();

	assert_eq!(fetched.lock().unwrap().len(), 1, "the sweep kept asking a gateway that refused");
}

/// The other half of the same branch: one row the gateway refuses on its own — a 400 naming that
/// payment id — is not a gateway-wide failure, so the rest of the batch is still swept.
#[tokio::test]
async fn the_sweep_keeps_going_past_a_row_the_gateway_refuses() {
	let db = TmpDb::new("sweep-one-bad-row");
	let stub = Stub::new(PaymentState::Pending);
	let fetched = Arc::clone(&stub.fetched);
	let failure = Arc::clone(&stub.fetch_failure);
	let (app, _invoices, store) = service_with(&db, stub).await;
	stale_rows(&store, 4).await;
	*failure.lock().unwrap() = Some(Retry::Never);

	saas_billing::sweep::tick(&app).await.unwrap();

	assert_eq!(fetched.lock().unwrap().len(), 4, "a row the gateway refused stopped the sweep");
}

/// The payout is irreversible, so every refusal has to come *before* it. The two-invoice
/// refusal sat after `provider.refund`, giving the money back and recording nothing — not even
/// the `PAYMENT_REFUND_UNRECORDED` row — and every retry re-ran the same refusal.
#[tokio::test]
async fn a_refund_refused_for_two_invoices_never_reaches_the_gateway() {
	let db = TmpDb::new("refund-two-invoices");
	let stub = Stub::new(PaymentState::Succeeded);
	let keys = Arc::clone(&stub.refund_keys);
	let (app, invoices, store) = service_with(&db, stub).await;
	let first = issued(&app, &invoices, &store).await;
	let second = issued(&app, &invoices, &store).await;

	let payment = start_payment(&app, &first).await;
	assert_eq!(ping(&app, PROVIDER_REF).await, StatusCode::OK);

	// The reachable shape: the gateway settled the first invoice, then an operator moved one
	// fillér of it onto the second by hand.
	let mv = |invoice: &Invoice, amount: i64| {
		let (app, ctx, uid, invoice_uid) =
			(app.clone(), ctx(), payment.uid.clone(), invoice.uid.clone());
		async move {
			allocate::allocate(
				&app,
				&ctx,
				&uid,
				&Allocation { invoice_uid, amount: Money(amount), currency: CurrencyCode::huf() },
			)
			.await
			.unwrap();
		}
	};
	mv(&first, -1).await;
	mv(&second, 1).await;

	let err = saas_billing::refund(
		&app,
		&ctx(),
		&payment.uid,
		Some((Money(10), CurrencyCode::huf())),
		None,
	)
	.await
	.unwrap_err();
	assert!(matches!(err, Error::Coded { code: "E-PAY-STATE", .. }), "{err:?}");
	assert!(keys.lock().unwrap().is_empty(), "the gateway must not have been asked to pay out");
}

/// `PARTIALLY_SUCCEEDED` means the gateway took less than the payment was opened for, and
/// `fetch_state` never says how much — so the ceiling is what an operator recorded as an
/// allocation, not `payments.amount`. With the opened amount as the ceiling an operator could
/// refund money that never arrived, and a fully-refunded partial never reached `REFUNDED`.
#[tokio::test]
async fn a_partial_capture_refunds_only_what_was_recorded() {
	let db = TmpDb::new("refund-partial-capture");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;

	let payment = allocate::manual(
		&app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "CARD".into(),
			amount: Money(1_000),
			currency: inv.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: None,
			note: None,
			allocations: vec![Allocation {
				invoice_uid: inv.uid.clone(),
				amount: Money(400),
				currency: CurrencyCode::huf(),
			}],
		},
	)
	.await
	.unwrap();
	sqlx::query("UPDATE payments SET status = 'PARTIALLY_SUCCEEDED' WHERE id = ?")
		.bind(payment.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	let refund = |amount: i64| {
		let (app, ctx, uid) = (app.clone(), ctx(), payment.uid.clone());
		async move {
			saas_billing::refund(&app, &ctx, &uid, Some((Money(amount), CurrencyCode::huf())), None)
				.await
		}
	};
	let err = refund(500).await.unwrap_err();
	assert_eq!(err.parts().1, "E-PAY-AMOUNT", "400 arrived, not 1 000");

	let back = refund(400).await.unwrap();
	assert_eq!(back.status, PaymentState::Refunded, "all of what arrived went back");
	assert_eq!(back.refunded_amount, Money(400));
}

/// The "a concurrent request already recorded this" fast path is only sound behind a gateway:
/// both requests derive one idempotency key from one `refunded_amount`, so the payout happened
/// once. A transfer has no gateway and no key, so two concurrent refunds of it are two payouts
/// — the loser must not answer 200 for money nobody gave back.
#[tokio::test]
async fn a_transfer_refund_that_lost_the_race_is_a_conflict() {
	let db = TmpDb::new("refund-transfer-race");
	let stub = Stub::new(PaymentState::Succeeded);
	let race = Arc::clone(&stub.race);
	let (app, invoices, store) = service_with(&db, stub).await;
	let inv = issued(&app, &invoices, &store).await;

	let payment = allocate::manual(
		&app,
		&ctx(),
		ManualPayment {
			org_uid: OrgId::from_trusted(ORG_UID.to_string()),
			kind: "TRANSFER".into(),
			amount: Money(GROSS),
			currency: inv.currency.clone(),
			received_at: Timestamp::now(),
			ext_ref: None,
			note: None,
			allocations: vec![Allocation {
				invoice_uid: inv.uid.clone(),
				amount: Money(GROSS),
				currency: CurrencyCode::huf(),
			}],
		},
	)
	.await
	.unwrap();
	assert!(payment.provider.is_none(), "a transfer has no gateway");

	// The other request books the whole refund while this one holds the row it read.
	*race.lock().unwrap() = Some(GROSS);
	let err = saas_billing::refund(
		&app,
		&ctx(),
		&payment.uid,
		Some((Money(GROSS), CurrencyCode::huf())),
		None,
	)
	.await
	.unwrap_err();
	assert_eq!(err.parts().1, "E-PAY-STATE");

	// And no alert: nothing paid out, so there is no discrepancy for an operator to clear.
	let alerts = saas_billing::alerts(app.clone()).await.unwrap();
	assert!(
		!alerts.iter().any(|a| a.code == "A-PAY-REFUND-UNRECORDED"),
		"a payout that never happened is not a discrepancy: {alerts:?}"
	);
	let _ = store;
}

/// `require_stepup` sat in the two operator **handlers** while `routes.rs` says handlers decide
/// nothing, so `saas_billing::refund` and `allocate::allocate` moved money for a token that had
/// presented no credential. `manual` had no step-up at all, although it creates a `SUCCEEDED`
/// payment and drives invoices to `PAID`. Both gates are service-side now, and this drives the
/// handles, never the router.
#[tokio::test]
async fn the_operator_and_step_up_gates_are_in_the_service() {
	use saas_core::ctx::Actor;

	let db = TmpDb::new("gates");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES ((SELECT id FROM orgs WHERE kind = 'ROOT'), 1, 'OWNER', 0, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	// The refused actor has to be a *different* account: `require_operator` reads a root
	// membership now, so account 1 is an operator whichever `Actor` variant wraps it.
	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (2, 'acc_u', 'u@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	let with = |actor: Actor, age: i64| Ctx {
		actor,
		auth_at: Some(Timestamp::now().0 - age),
		..Ctx::system("test").with_org(ORG)
	};
	let user = with(Actor::User { account_id: 2 }, 0);
	let stale = with(Actor::Operator { account_id: 1 }, 1_000_000);
	let fresh = with(Actor::Operator { account_id: 1 }, 0);

	let entry = || ManualPayment {
		org_uid: OrgId::from_trusted(ORG_UID.to_string()),
		kind: "TRANSFER".into(),
		amount: Money(GROSS),
		currency: inv.currency.clone(),
		received_at: Timestamp::now(),
		ext_ref: None,
		note: None,
		allocations: vec![],
	};
	assert_eq!(
		allocate::manual(&app, &user, entry()).await.unwrap_err().parts().1,
		"E-AUTH-FORBIDDEN"
	);
	assert_eq!(
		allocate::manual(&app, &stale, entry()).await.unwrap_err().parts().1,
		"E-AUTH-STEPUP"
	);
	let payment = allocate::manual(&app, &fresh, entry()).await.unwrap();

	let one = Allocation {
		invoice_uid: inv.uid.clone(),
		amount: Money(GROSS),
		currency: CurrencyCode::huf(),
	};
	let alloc = |ctx: &Ctx| {
		let (app, ctx, uid, req) = (app.clone(), ctx.clone(), payment.uid.clone(), one.clone());
		async move { allocate::allocate(&app, &ctx, &uid, &req).await }
	};
	assert_eq!(alloc(&user).await.unwrap_err().parts().1, "E-AUTH-FORBIDDEN");
	assert_eq!(alloc(&stale).await.unwrap_err().parts().1, "E-AUTH-STEPUP");
	alloc(&fresh).await.unwrap();

	let refund = |ctx: &Ctx| {
		let (app, ctx, uid) = (app.clone(), ctx.clone(), payment.uid.clone());
		async move { saas_billing::refund(&app, &ctx, &uid, None, None).await }
	};
	assert_eq!(refund(&user).await.unwrap_err().parts().1, "E-AUTH-FORBIDDEN");
	assert_eq!(refund(&stale).await.unwrap_err().parts().1, "E-AUTH-STEPUP");
	assert_eq!(refund(&fresh).await.unwrap().status, PaymentState::Refunded);
}

/// Barion has been documented to put `paymentId` in the callback body *and* in the callback
/// URL's query string, and which one arrives has changed between API versions. So a body the
/// provider cannot parse falls back to the query.
#[tokio::test]
async fn a_callback_reference_may_arrive_in_the_query_string() {
	let db = TmpDb::new("callback-query");
	let (app, invoices, store) = service(&db, PaymentState::Succeeded).await;
	let inv = issued(&app, &invoices, &store).await;
	let payment = start_payment(&app, &inv).await;

	// An empty body is what `Stub::parse_callback` refuses, exactly as a real adapter refuses
	// a shape it does not recognise.
	let status = webhook::callback(
		State(app.clone()),
		Path("stub".to_string()),
		RawQuery(Some(PROVIDER_REF.to_string())),
		HeaderMap::new(),
		Bytes::from_static(b""),
	)
	.await;
	assert_eq!(status, StatusCode::OK);

	let settled = store.invoice_by_id(inv.id).await.unwrap().unwrap();
	assert_eq!(settled.paid_amount, Money(GROSS), "the query carried the reference");
	let _ = payment;
}

/// A dead payment only unlocks. The draft stays — still `CARD`, re-payable, or movable to
/// `TRANSFER` — and only `invoice.draft_ttl_days`' sweep removes it.
#[tokio::test]
async fn an_expired_payment_leaves_the_draft_in_place() {
	let db = TmpDb::new("expired-keeps-draft");
	let (app, invoices, store) = service(&db, PaymentState::Expired).await;
	let d = draft(&invoices).await;
	let payment = start_payment(&app, &d).await;
	allocate::apply_state(&app, &payment, PaymentState::Expired).await.unwrap();

	let inv = store.invoice_by_id(d.id).await.unwrap().expect("the draft survives expiry");
	assert_eq!(inv.status, InvoiceStatus::Draft);
	assert_eq!(inv.payment_method, PaymentMethod::Card);

	start_payment(&app, &inv).await;
	assert_eq!(store.invoice_by_id(d.id).await.unwrap().unwrap().status, InvoiceStatus::Pending);
	assert!(invoices.unlock(&ctx(), d.uid.as_str()).await.unwrap());
	let patch = saas_invoice::store::InvoicePatch {
		payment_method: Some(PaymentMethod::Transfer),
		..Default::default()
	};
	let moved = invoices.patch(&ctx(), d.uid.as_str(), &patch).await.unwrap();
	assert_eq!(moved.payment_method, PaymentMethod::Transfer);
}

// vim: ts=4
