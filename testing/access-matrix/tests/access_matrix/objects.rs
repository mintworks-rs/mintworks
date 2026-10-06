// SPDX-License-Identifier: MPL-2.0
//! The org topology and one seeder per object kind a route path names. Everything is written
//! straight to the store (or through a service handle as `System`): routes are under test.

use std::sync::atomic::{AtomicU32, Ordering};

use mintworks_auth::store::{
	Account, AccountStatus, AuthStore, LegalKind, NewAccount, NewLegalDoc, OrgKind,
};
use mintworks_billing::provider::PaymentState;
use mintworks_billing::store::{BillingStore, NewPayment};
use mintworks_core::ids::{DocId, OfferId, SellerId, SubscriptionId};
use mintworks_core::prelude::*;
use mintworks_core::refs::{CreateRef, Refs};
use mintworks_core::store::{CoreStore, NewApiKey, Role};
use mintworks_core::{App, Ctx};
use mintworks_invoice::store::{PartyPatch, ServiceDef};
use mintworks_invoice::{
	InvoiceStore, Invoices, Line, NewDraft, Party, PartyKind, Seller, SellerVersionPatch, VatCode,
};
use mintworks_pdf::DocumentStore;
use mintworks_plans::PlanStore;
use mintworks_plans::store::{
	Interval, NewOffer, OfferKind, OfferPrice, PayMethod, SubStatus, Subscription,
};
use mintworks_store_sqlite::SqliteStore;

use crate::fixture::{Fixture, on_rt};

/// Every seeded account's password; its argon2 hash is computed once, in `Fixture.pwd_hash`.
pub const PASSWORD: &str = "matrix-password-1";

/// The content every seeded document points at (`doc_path` is content-addressed).
const DOC_SHA: &str = "00000000000000000000000000000000000000000000000000000000000000d0";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OrgTag {
	Root,
	/// SHARED, has a seller.
	A,
	/// SHARED child of A, no seller: the customer org.
	A1,
	/// SHARED, has a seller: the cross-tenant twin.
	B,
	/// SHARED, suspended after its member's token is minted.
	S,
}

pub struct OrgRef {
	pub id: i64,
	pub uid: String,
	/// The accepted OWNER `create_org` inserts; `None` on the root, which has none.
	pub owner: Option<Account>,
}

pub struct Orgs {
	pub root: OrgRef,
	pub a: OrgRef,
	pub a1: OrgRef,
	pub b: OrgRef,
	pub s: OrgRef,
}

impl Orgs {
	pub fn of(&self, tag: OrgTag) -> &OrgRef {
		match tag {
			OrgTag::Root => &self.root,
			OrgTag::A => &self.a,
			OrgTag::A1 => &self.a1,
			OrgTag::B => &self.b,
			OrgTag::S => &self.s,
		}
	}
}

/// One per path parameter value a route needs. `{sessionId}` (QR login) and `{credentialId}`
/// (WebAuthn) have no store seam and are not kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjKind {
	/// `/api/invoices/{uid}` (+ `/lines`, `/lines/{lineNo}` with line `1`, `/issue`).
	DraftInvoice,
	/// `/api/invoices/{uid}` (+ `/pdf`, `/storno`, `/pay`, `/payments`). Needs a seller.
	IssuedInvoice,
	/// `/api/billing-parties/{uid}`.
	Party,
	/// `/api/services/{uid}`.
	Service,
	/// `/api/payments/{uid}`, `/api/admin/payments/{uid}/…`.
	Payment,
	/// `/api/refs/{code}` (type `promo`).
	Ref,
	/// A promo ref keyed by its uid: what `DELETE /api/refs/{code}` and `…/reactivate` take.
	RefUid,
	/// `/api/org/invites/{code}/accept` (type `org_invite`, role MEMBER).
	Invite,
	/// `/api/admin/offers/{code}`. Global: always sold by the root org.
	Offer,
	/// `/api/plans/subscriptions/{uid}/…`, `/api/admin/subscriptions/{uid}/cancel`.
	Subscription,
	/// `/api/documents/{uid}` (+ `/pdf`).
	Document,
	/// `/api/api-keys/{uid}`, owned by the org's owner.
	ApiKey,
	/// `/api/orgs/{uid}`, `/api/admin/orgs/{uid}/grants`. A fresh one is a child of `org`.
	Org,
	/// `/api/org/members/{accountUid}`: an accepted MEMBER of `org`.
	Member,
	/// `/api/admin/accounts/{uid}/…`: a victim account. Global.
	Account,
	/// `/api/legal/{kind}`, `/api/consents/{kind}`: `TOS`. Global.
	Legal,
	/// `/api/webhook/{provider}`: `stub`. Global.
	Provider,
	/// `/api/agent/runs/{uid}/…`.
	#[cfg(feature = "ai")]
	Run,
}

impl ObjKind {
	pub const ALL: &[ObjKind] = &[
		ObjKind::DraftInvoice,
		ObjKind::IssuedInvoice,
		ObjKind::Party,
		ObjKind::Service,
		ObjKind::Payment,
		ObjKind::Ref,
		ObjKind::RefUid,
		ObjKind::Invite,
		ObjKind::Offer,
		ObjKind::Subscription,
		ObjKind::Document,
		ObjKind::ApiKey,
		ObjKind::Org,
		ObjKind::Member,
		ObjKind::Account,
		ObjKind::Legal,
		ObjKind::Provider,
		#[cfg(feature = "ai")]
		ObjKind::Run,
	];

	/// Kinds that belong to no tenant: seeded under the root whatever org is asked for.
	pub fn global(self) -> bool {
		matches!(self, ObjKind::Offer | ObjKind::Account | ObjKind::Legal | ObjKind::Provider)
	}
}

#[derive(Clone, Debug)]
pub struct Obj {
	pub name: String,
	pub kind: ObjKind,
	pub org: OrgTag,
	pub org_id: i64,
	/// What is substituted into the path: a uid, a code, a legal kind or a provider id.
	pub key: String,
}

/// A fresh lowercase ULID, borrowed from an id generator (no `ulid` dependency here).
fn rand() -> String {
	PaymentId::generate().into_string()["pay_".len()..].to_lowercase()
}

/// An ACTIVE, activated account (with its PERSONAL org) under the shared password hash.
pub async fn account(store: &SqliteStore, pwd_hash: &str, email: &str) -> Account {
	let (account, _) = store
		.create_account(
			&NewAccount {
				email: email.to_owned(),
				pwd_hash: Some(pwd_hash.to_owned()),
				name: None,
				locale: "hu".to_owned(),
				org_name: email.to_owned(),
			},
			&[],
		)
		.await
		.unwrap();
	store.set_account_status(account.id, AccountStatus::Active).await.unwrap();
	// `set_account_status` reads this as the evidence of activation.
	sqlx::query("UPDATE accounts SET activated_at = ? WHERE id = ?")
		.bind(Timestamp::now().0)
		.bind(account.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	store.account_by_id(account.id).await.unwrap().unwrap()
}

/// ROOT ─ A ─ A1, ROOT ─ B, ROOT ─ S, each with its own owner; sellers and a default party on
/// A and B; TOS + PRIVACY published (the consent gate fails closed without them).
pub async fn topology(store: &SqliteStore, pwd_hash: &str, nav_url: &str) -> Orgs {
	let root_id = store.root_org_id().await.unwrap();
	let root_uid: String = sqlx::query_scalar("SELECT uid FROM orgs WHERE id = ?")
		.bind(root_id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	let shared = async |name: &str, parent: i64| {
		let owner = account(store, pwd_hash, &format!("owner_{name}@matrix.invalid")).await;
		let org = store.create_org(OrgKind::Shared, parent, name, owner.id, None).await.unwrap();
		OrgRef { id: org.id, uid: org.uid.into_string(), owner: Some(owner) }
	};
	let a = shared("a", root_id).await;
	let a1 = shared("a1", a.id).await;
	let b = shared("b", root_id).await;
	let s = shared("s", root_id).await;
	for org in [&a, &b] {
		seller(store, org.id, nav_url).await;
		party(store, org.id, true).await;
	}
	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		store
			.insert_legal_doc(&NewLegalDoc {
				kind,
				locale: "hu".to_owned(),
				version: "1".to_owned(),
				title: format!("{kind:?}"),
				body: "…".to_owned(),
				sha256: format!("{:064x}", 0),
				effective_from: Timestamp(0),
			})
			.await
			.unwrap();
	}
	// What `Auth::publish_legal_document` does for itself; a raw insert has to.
	mintworks_auth::consent::invalidate_document_cache();
	Orgs { root: OrgRef { id: root_id, uid: root_uid, owner: None }, a, a1, b, s }
}

async fn seller(store: &SqliteStore, org_id: i64, nav_url: &str) {
	// The seller id is free; the org id is unique and keeps it readable.
	store
		.put_seller(&Seller {
			id: org_id,
			uid: SellerId::generate(),
			org_id,
			nav_base_url: nav_url.to_owned(),
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
			org_id,
			&SellerVersionPatch {
				name: Some(format!("Matrix {org_id} Kft.")),
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
		.publish_seller_version(org_id, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();
}

/// A fresh, checksum-valid HU tax number: a billing party's is unique within its org.
fn tax_number() -> String {
	static NEXT: AtomicU32 = AtomicU32::new(8_765_432);
	let base = format!("{:07}", NEXT.fetch_add(1, Ordering::Relaxed));
	let sum: u32 = base
		.bytes()
		.zip([9, 7, 3, 1, 9, 7, 3])
		.map(|(d, w)| u32::from(d - b'0') * w)
		.sum();
	format!("{base}{}242", (10 - sum % 10) % 10)
}

async fn party(store: &SqliteStore, org_id: i64, is_default: bool) -> String {
	let p = store
		.create_party(
			org_id,
			&PartyPatch {
				kind: Some(PartyKind::Company),
				name: Some("Vevo Zrt.".into()),
				country: Some("HU".into()),
				tax_number: Patch::Value(tax_number()),
				postcode: Patch::Value("1052".into()),
				city: Patch::Value("Budapest".into()),
				street: Patch::Value("Deak ter 2.".into()),
				email: Patch::Value("vevo@matrix.invalid".into()),
				is_default: Some(is_default),
				..Default::default()
			},
		)
		.await
		.unwrap();
	p.uid.into_string()
}

async fn service(store: &SqliteStore, org_id: i64) -> mintworks_invoice::Service {
	store
		.create_service(
			org_id,
			&ServiceDef {
				code: format!("svc-{}", rand()),
				name: "Matrix".into(),
				description: None,
				unit: "db".into(),
				unit_price: Money(100_000),
				vat_code: VatCode::Std27,
			},
		)
		.await
		.unwrap()
}

async fn offer(store: &SqliteStore, org_id: i64, code: &str) -> mintworks_plans::store::Offer {
	let svc = service(store, org_id).await;
	store
		.offer_upsert(&NewOffer {
			uid: OfferId::generate(),
			seller_org_id: org_id,
			code: code.to_owned(),
			name: "Matrix".into(),
			kind: OfferKind::Recurring,
			service_id: svc.id,
			family: None,
			rank: 1,
			interval: Some(Interval::Month),
			interval_count: Some(1),
			validity_days: None,
			trial_days: 0,
			prices: vec![OfferPrice { currency: CurrencyCode::huf(), amount: Money(499_000) }],
			// `install(b, [])`: the registry is empty, so an offer may grant nothing.
			entitlements: vec![],
		})
		.await
		.unwrap()
}

async fn draft(app: &App, org_id: i64) -> mintworks_invoice::Invoice {
	Invoices::new(app.clone())
		.draft(
			&Ctx::system("matrix").with_org(org_id),
			&NewDraft {
				billing_party: Party::OrgDefault,
				lines: vec![Line::adhoc(
					"Matrix",
					"db",
					Qty(1_000_000),
					Money(10_000),
					VatCode::Std27,
				)],
				..Default::default()
			},
		)
		.await
		.unwrap()
}

async fn mint_ref(app: &App, org_id: i64, req: &CreateRef) -> mintworks_core::refs::Ref {
	let ctx = Ctx::system("matrix").with_org(org_id);
	Refs::from_app(app).unwrap().mint(&ctx, req).await.unwrap()
}

/// A fresh object of `kind` in `org`, on the fixture's runtime. Each call is a new row, so a
/// mutating cell never sees another cell's leftovers.
pub async fn seed(fx: &'static Fixture, kind: ObjKind, org: OrgTag) -> Obj {
	on_rt(make(fx, kind, org)).await
}

/// [`seed`] for a caller already on the fixture's runtime — `build()` itself.
pub async fn make(fx: &Fixture, kind: ObjKind, org: OrgTag) -> Obj {
	let org = if kind.global() { OrgTag::Root } else { org };
	let o = fx.orgs.of(org);
	let store = &*fx.store;
	let owner_id = || o.owner.as_ref().map_or(0, |a| a.id);
	let key = match kind {
		ObjKind::DraftInvoice => draft(&fx.app, o.id).await.uid.into_string(),
		ObjKind::IssuedInvoice => {
			let d = draft(&fx.app, o.id).await;
			mintworks_invoice::issue::run(&fx.app, store, d)
				.await
				.unwrap()
				.uid
				.into_string()
		}
		ObjKind::Party => party(store, o.id, false).await,
		ObjKind::Service => service(store, o.id).await.uid.into_string(),
		ObjKind::Payment => store
			.create_payment(&NewPayment {
				org_id: o.id,
				kind: "manual".into(),
				provider: Some("stub".into()),
				provider_ref: Some(rand()),
				request_id: None,
				status: PaymentState::Pending,
				amount: Money(100_000),
				currency: CurrencyCode::huf(),
				ext_ref: None,
				note: None,
				created_by: None,
				invoice_id: None,
			})
			.await
			.unwrap()
			.uid
			.into_string(),
		ObjKind::Ref | ObjKind::RefUid => {
			let req = CreateRef { ref_type: "promo".into(), ..CreateRef::default() };
			let r = mint_ref(&fx.app, o.id, &req).await;
			if kind == ObjKind::Ref { r.code } else { r.uid.into_string() }
		}
		ObjKind::Invite => {
			let req = CreateRef {
				ref_type: "org_invite".into(),
				params: Some(serde_json::json!({ "role": Role::Member.as_str() })),
				uses_left: Some(1),
				..CreateRef::default()
			};
			mint_ref(&fx.app, o.id, &req).await.code
		}
		ObjKind::Offer => {
			// `quote` reads the buyer's own seller catalogue, so A and B sell the code as well.
			let code = format!("ofr-{}", rand());
			for org in [o.id, fx.orgs.a.id, fx.orgs.b.id] {
				offer(store, org, &code).await;
			}
			code
		}
		ObjKind::Subscription => {
			let ofr = offer(store, fx.orgs.root.id, &format!("ofr-{}", rand())).await;
			let now = Timestamp::now().0;
			store
				.sub_insert(&Subscription {
					id: 0,
					uid: SubscriptionId::generate(),
					org_id: o.id,
					offer_id: ofr.id,
					family: None,
					qty: 1,
					status: SubStatus::Active,
					currency: CurrencyCode::huf(),
					price: Money(499_000),
					period_start: Timestamp(now),
					period_end: Timestamp(now + 30 * 86_400),
					cancel_at_period_end: false,
					next_offer_id: None,
					next_qty: None,
					pay_method: PayMethod::Transfer,
					provider: None,
					recurrence_ref: None,
					coupon_ref_id: None,
					coupon_periods_left: None,
					created_at: Timestamp(0),
					updated_at: Timestamp(0),
					billing_anchor: Timestamp(now),
				})
				.await
				.unwrap()
				.uid
				.into_string()
		}
		ObjKind::Document => {
			let uid = DocId::generate();
			store.document_insert(o.id, &uid, "matrix", &rand()).await.unwrap();
			let path = mintworks_pdf::doc_path(&fx.app.config.data_dir, DOC_SHA).unwrap();
			std::fs::create_dir_all(path.parent().unwrap()).unwrap();
			std::fs::write(&path, b"%PDF-1.4 matrix").unwrap();
			store.document_rendered(&uid, DOC_SHA, 15).await.unwrap();
			uid.into_string()
		}
		ObjKind::ApiKey => {
			let r = rand();
			store
				.create_api_key(
					&NewApiKey {
						org_id: o.id,
						account_id: owner_id(),
						name: "matrix".into(),
						// Unique 8-char lookup handle; the key itself is never presented.
						prefix: r[r.len() - 8..].to_owned(),
						key_hash: format!("{:064x}", 0),
						scopes: "[]".into(),
						expires_at: None,
					},
					1_000,
				)
				.await
				.unwrap()
				.unwrap()
				.uid
				.into_string()
		}
		ObjKind::Org => store
			.create_org(OrgKind::Shared, o.id, &format!("child-{}", rand()), owner_id(), None)
			.await
			.unwrap()
			.uid
			.into_string(),
		ObjKind::Member => {
			let m =
				account(store, &fx.pwd_hash, &format!("member-{}@matrix.invalid", rand())).await;
			store.put_membership(o.id, m.id, Role::Member).await.unwrap();
			store.accept_membership(o.id, m.id, Timestamp::now()).await.unwrap();
			m.uid.into_string()
		}
		ObjKind::Account => {
			account(store, &fx.pwd_hash, &format!("victim-{}@matrix.invalid", rand()))
				.await
				.uid
				.into_string()
		}
		ObjKind::Legal => "TOS".into(),
		ObjKind::Provider => "stub".into(),
		#[cfg(feature = "ai")]
		ObjKind::Run => {
			use mintworks_agent::{AgentRunStore, NewRun, ThreadStore};
			let threads = fx.app.extensions.get::<std::sync::Arc<dyn ThreadStore>>().unwrap();
			let thread = threads.thread_create(&o.uid, None, None).await.unwrap();
			let uid = RunId::generate();
			store
				.run_insert(&NewRun {
					uid: &uid,
					thread: &thread.uid,
					org_id: o.id,
					account_id: o.owner.as_ref().map(|a| a.id),
					role: "matrix",
					spec: "{}",
				})
				.await
				.unwrap()
				.unwrap();
			uid.into_string()
		}
	};
	Obj { name: format!("{kind:?}@{org:?}"), kind, org, org_id: o.id, key }
}

// vim: ts=4
