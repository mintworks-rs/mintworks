// SPDX-License-Identifier: MPL-2.0
//! One composed application shared by every test in the matrix.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use mintworks_auth::store::AuthStore;
use mintworks_billing::provider::{
	CallbackRef, PaymentProvider, PaymentProviders, PaymentState, ProviderCaps, RefundResult,
	StartPayment, StartedPayment,
};
use mintworks_billing::store::BillingStore;
use mintworks_core::auth_mw::{Claims, JWT_SECRET_KEY};
use mintworks_core::store::CoreStore;
use mintworks_core::{App, AppBuilder, config::Config, prelude::*};
use mintworks_invoice::store::InvoiceStore;
use mintworks_nav::store::NavStore;
use mintworks_store_sqlite::{FRAMEWORK, SqliteStore};
use tower::ServiceExt;

use crate::objects::{self, Obj, ObjKind, OrgTag, Orgs, PASSWORD};
use crate::subjects::{self, Subject};

pub struct Fixture {
	pub app: App,
	pub router: axum::Router,
	pub mock: wiremock::MockServer,
	pub store: Arc<SqliteStore>,
	pub orgs: Orgs,
	/// The canonical object of every kind in A, then its twin in B.
	pub objs: Vec<Obj>,
	/// argon2 of [`crate::objects::PASSWORD`], computed once: argon2 is slow in debug builds.
	pub pwd_hash: String,
	pub subjects: Vec<Subject>,
	_tmp: TmpDb,
}

impl Fixture {
	pub fn subject(&self, name: &str) -> &Subject {
		self.subjects.iter().find(|s| s.name == name).unwrap()
	}

	/// The canonical object (`OrgTag::A`) or its twin (`OrgTag::B`). By order, not by tag: a
	/// global kind is tagged Root in both.
	pub fn obj(&self, kind: ObjKind, org: OrgTag) -> &Obj {
		assert!(matches!(org, OrgTag::A | OrgTag::B), "canonical objects live in A and B");
		let nth = usize::from(org == OrgTag::B);
		self.objs.iter().filter(|o| o.kind == kind).nth(nth).unwrap()
	}
}

/// The fixture's pools and the wiremock server live on this runtime, not on a test's: each
/// `#[tokio::test]` has its own runtime, and the first one to drop would take them along.
fn rt() -> &'static tokio::runtime::Runtime {
	static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
	RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap())
}

/// Runs `fut` on the fixture's runtime. Anything touching the store, the router or the mock
/// server goes through here.
pub async fn on_rt<F>(fut: F) -> F::Output
where
	F: Future + Send + 'static,
	F::Output: Send + 'static,
{
	rt().spawn(fut).await.unwrap()
}

static FX: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

pub async fn fixture() -> &'static Fixture {
	FX.get_or_init(|| on_rt(build())).await
}

/// For the `RouteSpec.body` fn pointers, which run only after a test awaited [`fixture`].
pub fn fixture_now() -> &'static Fixture {
	FX.get().unwrap()
}

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new() -> Self {
		let dir =
			std::env::temp_dir().join(format!("mintworks-access-matrix-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [7; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: self.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: "https://app.invalid".into(),
			// No workers and no reclaim: a job only queues, so nothing reaches the network.
			jobs_workers: Some(0),
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// Composes what `bin/mintworks/src/app.rs` `compose` + `feature_crates` would for an app
/// mounting every bundle — minus the `/api/{*rest}` catch-all, so an unmounted route is axum's
/// empty-body 404/405.
async fn build() -> Fixture {
	let tmp = TmpDb::new();
	let mock = wiremock::MockServer::builder().start().await;
	let store = SqliteStore::open(&tmp.config()).await.unwrap();
	store.migrate(&[FRAMEWORK]).await.unwrap();

	let gate = mintworks_auth::routes::consent_gate();
	let routes = mintworks_script::routes::MOUNTS
		.iter()
		.fold(mintworks_core::app::Scoped::from(axum::Router::new()), |acc, name| {
			acc.merge(mintworks_script::routes::resolve(name, &gate).unwrap())
		});

	let docs: Arc<dyn mintworks_pdf::DocumentStore> = Arc::new(store.clone());
	let b = AppBuilder::new()
		.config(tmp.config())
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		.settings(mintworks_auth::SETTINGS)
		.settings(mintworks_email::SETTINGS)
		.settings(mintworks_invoice::SETTINGS)
		.settings(mintworks_nav::SETTINGS)
		.settings(mintworks_billing::SETTINGS)
		.settings(mintworks_plans::SETTINGS)
		.secrets(mintworks_auth::SECRETS)
		.secrets(mintworks_email::SECRETS)
		.secrets(mintworks_nav::SECRETS)
		.secrets(mintworks_plans::SECRETS)
		// The boot refuses a blank required key; jobs never run, so none of these is used.
		.setting_default("email.from", "matrix@app.invalid")
		.setting_default("email.smtp.host", "127.0.0.1")
		.setting_default("nav.software_id", "MINTWORKSEXAMPLE01")
		.setting_default("nav.software_name", "matrix")
		.setting_default("nav.software_main_version", "1")
		.setting_default("nav.software_dev_name", "matrix")
		.setting_default("nav.software_dev_contact", "matrix@app.invalid")
		// Thousands of cells share a handful of accounts; the per-account buckets would 429 them.
		.setting_default("ratelimit.authenticated", "1000000/min/account")
		.setting_default("ratelimit.account_export", "1000000/min/account")
		.extension(Arc::new(store.clone()) as Arc<dyn AuthStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_core::refs::RefStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn InvoiceStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn NavStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn BillingStore>)
		.extension(Arc::new(PaymentProviders::new().with(Arc::new(Stub("stub")))))
		.account_data_hook(Arc::new(mintworks_pdf::DocumentHook {
			docs: Arc::clone(&docs),
			data_dir: tmp.0.to_string_lossy().into_owned(),
		}))
		.extension(docs)
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_entitle::EntitleStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_plans::PlanStore>)
		.extension(Arc::new(mintworks_plans::events::Recurrence)
			as Arc<dyn mintworks_billing::provider::RecurrenceHook>);
	let b = mintworks_entitle::install(b, []);
	let b = mintworks_plans::install(b, []);
	#[cfg(feature = "ai")]
	let b = ai(b, &store, &tmp, &mock.uri()).await;

	let (app, router) = b.routes(routes).into_service().await.unwrap();
	let pwd_hash = mintworks_auth::register::hash_password(PASSWORD.to_owned()).await.unwrap();
	let orgs = objects::topology(&store, &pwd_hash, &mock.uri()).await;
	let mut fx = Fixture {
		app,
		router,
		mock,
		store: Arc::new(store),
		orgs,
		objs: vec![],
		pwd_hash,
		subjects: vec![],
		_tmp: tmp,
	};
	let mut objs = Vec::new();
	for org in [OrgTag::A, OrgTag::B] {
		for &kind in ObjKind::ALL {
			objs.push(match kind {
				// The canonical org object is the org itself; `make` mints a child.
				ObjKind::Org => {
					let o = fx.orgs.of(org);
					Obj {
						name: format!("Org@{org:?}"),
						kind,
						org,
						org_id: o.id,
						key: o.uid.clone(),
					}
				}
				_ => objects::make(&fx, kind, org).await,
			});
		}
	}
	fx.objs = objs;
	fx.subjects = subjects::roster(&fx).await;
	fx
}

/// `feature_crates`' `llm`, `memory`, `agent` and `search` blocks. Every provider points at
/// the wiremock server; a profile's `llm.kind` is the fake.
#[cfg(feature = "ai")]
async fn ai(b: AppBuilder, store: &SqliteStore, tmp: &TmpDb, mock: &str) -> AppBuilder {
	let app_db = Arc::new(mintworks_appdb_sqlite::SqliteAppDb::new(tmp.0.join("app.db")));
	app_db
		.migrate(&[mintworks_appdb_sqlite::MEMORY, mintworks_appdb_sqlite::AGENT])
		.await
		.unwrap();
	let auth = || Arc::new(store.clone()) as Arc<dyn AuthStore>;
	let memory = Arc::new(mintworks_memory::Memory::new(
		Arc::clone(&app_db) as Arc<dyn mintworks_memory::MemoryStore>,
		auth(),
	));
	let threads = Arc::clone(&app_db) as Arc<dyn mintworks_agent::ThreadStore>;
	let base: &'static str = Box::leak(mock.to_owned().into_boxed_str());
	b.settings(mintworks_llm::SETTINGS)
		.secrets(mintworks_llm::SECRETS)
		.settings(mintworks_agent::SETTINGS)
		.settings(mintworks_search::SETTINGS)
		.secrets(mintworks_search::SECRETS)
		.setting_default("llm.kind.matrix", "fake")
		.setting_default("llm.base_url.matrix", base)
		.setting_default("search.provider", "fake")
		.setting_default("search.fetcher", "fake")
		.setting_default("search.base_url.fake", base)
		.extension(mintworks_llm::LlmState::default())
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_llm::LlmStore>)
		.extension(Arc::new(mintworks_llm::Prompts::load(&tmp.0).unwrap()))
		.extension(Arc::clone(&memory))
		.account_data_hook(memory)
		.extension(mintworks_agent::RunPool::default())
		.extension(Arc::new(mintworks_agent::Skills::load(&tmp.0).unwrap()))
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_agent::AgentRunStore>)
		.extension(Arc::clone(&threads))
		.account_data_hook(Arc::new(mintworks_agent::AgentHook { threads, orgs: auth() }))
		.extension(mintworks_search::Fixtures::default())
		.extension(Arc::new(
			mintworks_search::SearchBackends::new()
				.with_provider(Arc::new(mintworks_search::Fixtures::default()))
				.with_fetcher(Arc::new(mintworks_search::Fixtures::default())),
		))
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_search::SearchStore>)
}

/// A gateway that never moves, copied from `crates/billing/tests/routes.rs`.
struct Stub(&'static str);

#[async_trait]
impl PaymentProvider for Stub {
	fn id(&self) -> &str {
		self.0
	}

	fn capabilities(&self) -> ProviderCaps {
		ProviderCaps { partial_refund: true, ..ProviderCaps::default() }
	}

	async fn start(&self, _req: &StartPayment) -> ClResult<StartedPayment> {
		Ok(StartedPayment {
			// `(provider, provider_ref)` is unique, so each payment needs its own.
			provider_ref: mintworks_core::ids::PaymentId::generate().into_string(),
			redirect_url: Some("https://stub.invalid/pay".to_string()),
			state: PaymentState::Pending,
		})
	}

	async fn fetch_state(&self, _provider_ref: &str) -> ClResult<PaymentState> {
		Ok(PaymentState::Pending)
	}

	async fn refund(
		&self,
		_provider_ref: &str,
		amount: Money,
		_request_id: &str,
	) -> ClResult<RefundResult> {
		Ok(RefundResult { refunded: amount, state: PaymentState::Refunded })
	}

	async fn charge_recurring(
		&self,
		_token: &str,
		_req: &StartPayment,
	) -> ClResult<StartedPayment> {
		Ok(StartedPayment {
			provider_ref: mintworks_core::ids::PaymentId::generate().into_string(),
			redirect_url: None,
			state: PaymentState::Pending,
		})
	}

	fn parse_callback(&self, _headers: &HeaderMap, body: &[u8]) -> ClResult<CallbackRef> {
		Ok(CallbackRef { provider_ref: String::from_utf8_lossy(body).into_owned() })
	}
}

/// A request from its own /64: a shared address would pile up rate-limit buckets and trip
/// proof-of-work, and without `ConnectInfo` the public auth routes answer 500.
pub fn req(
	method: Method,
	uri: &str,
	bearer: Option<&str>,
	body: Option<serde_json::Value>,
) -> Request<Body> {
	static NEXT: AtomicU32 = AtomicU32::new(1);
	let n = NEXT.fetch_add(1, Ordering::Relaxed);
	#[allow(clippy::cast_possible_truncation)]
	let ip = Ipv6Addr::new(0xfd00, 0, 0, n as u16, 0, 0, 0, 1);
	let mut b = Request::builder().method(method).uri(uri);
	if let Some(t) = bearer {
		b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
	}
	let body = match body {
		Some(v) => {
			b = b.header(header::CONTENT_TYPE, "application/json");
			Body::from(v.to_string())
		}
		None => Body::empty(),
	};
	let mut r = b.body(body).unwrap();
	r.extensions_mut().insert(ConnectInfo(SocketAddr::new(IpAddr::V6(ip), 40000)));
	r
}

pub struct Resp {
	pub status: StatusCode,
	pub body: Option<serde_json::Value>,
	/// No bytes at all: axum's own 404/405 for an unmounted route, never the error envelope.
	pub empty: bool,
}

pub async fn call(router: &axum::Router, req: Request<Body>) -> Resp {
	let router = router.clone();
	on_rt(async move {
		let res = router.oneshot(req).await.unwrap();
		let status = res.status();
		let bytes = res.into_body().collect().await.unwrap().to_bytes();
		Resp { status, body: serde_json::from_slice(&bytes).ok(), empty: bytes.is_empty() }
	})
	.await
}

/// An HS256 token under the app's own key, for the shapes no route mints.
pub async fn forge(app: &App, claims: &Claims) -> String {
	let (app, claims) = (app.clone(), claims.clone());
	on_rt(async move {
		let key = app.secrets.get_or_create(JWT_SECRET_KEY, 32).await.unwrap();
		jsonwebtoken::encode(
			&jsonwebtoken::Header::default(),
			&claims,
			&jsonwebtoken::EncodingKey::from_secret(&key),
		)
		.unwrap()
	})
	.await
}

// vim: ts=4
