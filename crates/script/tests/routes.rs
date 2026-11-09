// SPDX-License-Identifier: MPL-2.0
//! The runtime's authorization surface, driven through a real axum router: the `.public()` /
//! gated split, the org boundary, `dispatch`'s argument shapes and the `E-APP-*` raise.
//!
//! An integration test, not an inline `mod tests`: the `#[cfg(test)]` build of a crate is a
//! distinct crate from the one `mintworks-store-sqlite` links against, so the store impls would
//! not unify (`E0599`). The adapter is therefore a dev-dependency, a cycle Cargo permits.
//!
//! The harness is a real file database in a temp dir, never `sqlite::memory:` — an in-memory
//! URL gives each connection its own database.
//!
//! `examples/*/app*/tests.rn` is a second layer, not a replacement: it runs through
//! `mintworks test`, which neither `cargo test --all` nor the pre-commit hook invokes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::{
	Router,
	body::Body,
	http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use mintworks_appdb_sqlite::SqliteAppDb;
use mintworks_auth::store::AuthStore;
use mintworks_core::{
	App, AppBuilder, auth_mw::RouteGate, config::Config, objects::ObjectStore, store::CoreStore,
};
use mintworks_invoice::store::InvoiceStore;
use mintworks_nav::{NavOp, NavStore, NavVerdict};
use mintworks_script::{ScriptApp, TxBody, TxHook};
use mintworks_store_sqlite::{FRAMEWORK, SqliteStore};
use serde_json::Value as Json;
use tower::ServiceExt;

const ACCOUNT: &str = "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A";
const ORG_A: &str = "org_01JCZ5X8K9N7QW3M6R2T4V8Y0C";
const ORG_B: &str = "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D";

/// One bundle covering every case below. `main` only declares; nothing here computes.
const SRC: &str = r#"
pub async fn main(app) {
	app.object_type("note", #{ prefix: "nte" });
	app.get("/api/ping", ping).public().tier("public");
	app.get("/api/pub-file", pub_file).public().tier("public");
	app.get("/api/notes/{key}", note);
	app.post("/api/echo", echo);
	app.post("/api/echo-small", echo).max_body(16);
	app.post("/api/upload", upload).raw().max_body(4 * 1024 * 1024);
	app.get("/api/upload/{uid}", download);
	app.post("/api/boom", boom);
	app.post("/api/boom-tx", boom_tx);
	app.post("/api/caught", caught);
	app.get("/api/sudo", sudo);
	app.get("/api/files", files);
	app.table("ledger", #{ cols: #{ uid: "TEXT PRIMARY KEY", total: "INTEGER NOT NULL" } });
	app.get("/api/ledger", ledger);
	app.get("/api/framework-table", framework_table);
	app.post("/api/crossing", crossing);
	app.post("/api/nested", nested);
	app.post("/api/settings-in-tx", settings_in_tx);
	app.post("/api/tx-rollback", tx_rollback);
	app.post("/api/ddl", ddl);
}

pub async fn ledger(ctx, req) {
	#{ items: db::query(ctx, "SELECT uid, total FROM ledger", []).await? }
}

pub async fn framework_table(ctx, req) {
	#{ items: db::query(ctx, "SELECT count(*) AS n FROM invoices", []).await? }
}

pub async fn crossing(ctx, req) {
	tx::with(ctx, async |ctx| {
		db::exec(ctx, "INSERT INTO ledger (uid, total) VALUES ('led_1', 700)", []).await?;
		Ok(())
	}).await
}

pub async fn settings_in_tx(ctx, req) {
	tx::with(ctx, async |ctx| {
		settings::set(ctx, "app.any", "x", #{}).await?;
		Ok(())
	}).await
}

pub async fn nested(ctx, req) {
	db::tx(ctx, async |ctx| {
		db::exec(ctx, "INSERT INTO ledger (uid, total) VALUES ('led_2', 700)", []).await?;
		tx::with(ctx, |ctx| { Err(err::conflict("boom")) }).await
	}).await
}

pub async fn tx_rollback(ctx, req) {
	db::tx(ctx, async |ctx| {
		db::exec(ctx, "INSERT INTO ledger (uid, total) VALUES ('led_3', 700)", []).await?;
		Err(err::conflict("boom"))
	}).await
}

pub async fn ddl(ctx, req) {
	db::exec(ctx, "DROP TABLE ledger", []).await
}

pub async fn files(ctx, req) {
	fs::write("note.txt", "hi").await?;
	#{
		roundTrip: fs::read("note.txt").await?,
		traversal: match fs::read("../../etc/passwd").await { Ok(_) => "read", Err(_) => "refused" },
		absolute: match fs::read("/etc/passwd").await { Ok(_) => "read", Err(_) => "refused" },
		exists: match fs::exists("../../etc/passwd").await { Ok(_) => "read", Err(_) => "refused" },
	}
}

pub async fn ping(ctx, req) {
	#{ ok: true, actor: ctx.actor() }
}

pub async fn note(ctx, req) {
	match objects::get(ctx, "note", `${req.path.key}`).await? {
		() => Err(err::not_found()),
		found => Ok(found),
	}
}

pub async fn echo(ctx, req) {
	#{ body: req.body, q: req.query, method: req.method, actor: ctx.actor() }
}

pub async fn upload(ctx, req) {
	let early = blob::put(ctx, req.body, #{ org: "org_00000000000000000000000000", mime: "x/y" })
		.await
		.is_err();
	let uid = blob::put(ctx, req.body, #{ org: req.query.org, mime: "application/octet-stream" })
		.await?;
	let again = match blob::put(ctx, req.body, #{ org: req.query.org, mime: "x/y" }).await {
		Ok(_) => "stored",
		Err(_) => "refused",
	};
	#{ uid: uid, again: again, early: early }
}

pub async fn download(ctx, req) {
	resp::stream(blob::open(ctx, req.path.uid).await?, #{ cache_control: "max-age=60" })
}

pub async fn pub_file(ctx, req) {
	fs::write("pub.txt", "hi").await?;
	resp::stream(fs::open("pub.txt").await?, #{ cache_control: "public, max-age=60" })
}

pub async fn sudo(ctx, req) {
	#{ actor: sys::escalate(ctx)?.actor() }
}

pub async fn boom(ctx, req) {
	Err(err::app(418, "E-APP-TEAPOT", "no coffee here")?)
}

pub async fn caught(ctx, req) {
	let app = match boom(ctx, req).await {
		Err(e) => #{ code: e.code(), status: e.status() },
		Ok(_) => #{},
	};
	let nf = err::not_found();
	let core = #{ code: nf.code(), status: nf.status() };
	#{ app: app, core: core }
}

pub async fn boom_tx(ctx, req) {
	tx::with(ctx, |ctx| {
		Err(err::app(418, "E-APP-TEAPOT", "no coffee here")?)
	}).await
}
"#;

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-script-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [7; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
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

/// The `TmpDb` comes back too: dropping it deletes the database, so the caller has to hold it
/// for the length of the test.
async fn serve(name: &str, gate: RouteGate, consented: bool) -> (TmpDb, App, Router, SqliteStore) {
	serve_src(name, gate, consented, SRC, false).await
}

async fn serve_src(
	name: &str,
	gate: RouteGate,
	consented: bool,
	src: &str,
	suite: bool,
) -> (TmpDb, App, Router, SqliteStore) {
	let (db, builder, store) = install(name, gate, consented, src, suite).await;
	let (app, router) = builder.unwrap().into_service().await.unwrap();
	(db, app, router, store)
}

async fn install(
	name: &str,
	gate: RouteGate,
	consented: bool,
	src: &str,
	suite: bool,
) -> (TmpDb, mintworks_core::ClResult<AppBuilder>, SqliteStore) {
	let db = TmpDb::new(name);
	let store = SqliteStore::open(&db.config()).await.unwrap();
	store.migrate(&[FRAMEWORK]).await.unwrap();
	seed(&store, consented).await;
	let builder = build(&db, &store, gate, src, suite).await;
	(db, builder, store)
}

/// The app over an existing database: what a second boot of the same deployment builds.
async fn build(
	db: &TmpDb,
	store: &SqliteStore,
	gate: RouteGate,
	src: &str,
	suite: bool,
) -> mintworks_core::ClResult<AppBuilder> {
	let builder = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		.settings(mintworks_auth::SETTINGS)
		.ref_purposes(mintworks_auth::REF_PURPOSES)
		.settings(mintworks_core::blob::SETTINGS)
		.settings(mintworks_invoice::SETTINGS)
		.secrets(mintworks_auth::SECRETS)
		.extension(Arc::new(store.clone()) as Arc<dyn AuthStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn InvoiceStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn NavStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_core::refs::RefStore>);

	let script = ScriptApp::new(
		vec![("t.rn".to_owned(), src.to_owned())],
		Arc::new(store.clone()) as Arc<dyn ObjectStore>,
		db.0.clone(),
	)
	.gate(gate)
	.tx_hook(Arc::new(TestTxHook(store.clone())))
	.app_db(Arc::new(SqliteAppDb::new(db.0.join("app.db"))))
	.modules(vec![nap_module]);

	if suite {
		let root = db.0.clone();
		script.install_tests(builder, |b, _| Ok(b), None, root).await.map(|(b, _)| b)
	} else {
		script.install(builder, |b, _| Ok(b)).await
	}
}

/// `tx::with`'s hook, the same shape `bin/mintworks/src/app.rs` registers: the raise inside a
/// block only reaches the wire once the hook has rolled back and handed the error on.
struct TestTxHook(SqliteStore);

#[async_trait::async_trait(?Send)]
impl TxHook for TestTxHook {
	async fn run(&self, _app: &App, body: TxBody<'_>) -> mintworks_core::ClResult<Json> {
		let tx = self.0.write_tx().await?;
		match SqliteStore::scope_writes(&tx, body).await {
			Ok(v) => {
				tx.commit().await?;
				Ok(v)
			}
			Err(e) => {
				let _ = tx.rollback().await;
				Err(e)
			}
		}
	}
}

/// An `ACTIVE` account, two orgs it owns, and — when `consented` — the gating documents plus
/// its acceptance of them. `consents_required` fails **closed**, so a run with no legal row at
/// all is exactly the "owes a consent" case the gate exists for.
async fn seed(store: &SqliteStore, consented: bool) {
	let pool = store.write_pool();
	sqlx::query(
		"INSERT INTO accounts (id, uid, email, status, created_at)
		 VALUES (1, ?, 'test@example.test', 'ACTIVE', 0)",
	)
	.bind(ACCOUNT)
	.execute(pool)
	.await
	.unwrap();
	for org in [ORG_A, ORG_B] {
		sqlx::query(
			"INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, created_at)
			 VALUES (?, (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'T', 1, 0)",
		)
		.bind(org)
		.execute(pool)
		.await
		.unwrap();
		sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES ((SELECT id FROM orgs WHERE uid = ?), 1, 'OWNER', 0, 0)",
		)
		.bind(org)
		.execute(pool)
		.await
		.unwrap();
	}
	if !consented {
		return;
	}
	for kind in ["TOS", "PRIVACY"] {
		sqlx::query(
			"INSERT INTO legal_docs
			   (kind, locale, version, title, body, sha256, effective_from, created_at)
			 VALUES (?, 'en', 'v1', 't', 'b', 'deadbeef', 0, 0)",
		)
		.bind(kind)
		.execute(pool)
		.await
		.unwrap();
		sqlx::query(
			"INSERT INTO consents
			   (account_id, kind, legal_doc_id, doc_version, doc_sha256, granted, at)
			 SELECT 1, kind, id, version, sha256, 1, 0 FROM legal_docs WHERE kind = ?",
		)
		.bind(kind)
		.execute(pool)
		.await
		.unwrap();
	}
}

/// A real access token for the seeded account, minted against the app's own signing key — the
/// middleware verifies it exactly as it verifies a browser's.
async fn token(app: &App, org: &str) -> String {
	let key = app
		.secrets
		.get_or_create(mintworks_core::auth_mw::JWT_SECRET_KEY, 32)
		.await
		.unwrap();
	let claims = mintworks_core::auth_mw::Claims {
		sub: ACCOUNT.to_owned(),
		org: Some(org.to_owned()),
		rol: Some("OWNER".to_owned()),
		opr: false,
		ep: 0,
		auth_at: Some(mintworks_core::types::Timestamp::now().0),
		ses: None,
		imp: None,
		typ: None,
		iat: 0,
		exp: i64::from(u32::MAX),
	};
	jsonwebtoken::encode(
		&jsonwebtoken::Header::default(),
		&claims,
		&jsonwebtoken::EncodingKey::from_secret(&key),
	)
	.unwrap()
}

async fn call(
	router: &Router,
	method: &str,
	path: &str,
	tok: Option<&str>,
	body: Option<&str>,
) -> (StatusCode, Json) {
	let mut req = Request::builder().method(method).uri(path);
	if let Some(t) = tok {
		req = req.header("authorization", format!("Bearer {t}"));
	}
	let req = match body {
		Some(b) => req
			.header("content-type", "application/json")
			.body(Body::from(b.to_owned()))
			.unwrap(),
		None => req.body(Body::empty()).unwrap(),
	};
	send(router, req).await
}

async fn send(router: &Router, req: Request<Body>) -> (StatusCode, Json) {
	let res = router.clone().oneshot(req).await.unwrap();
	let status = res.status();
	let bytes = res.into_body().collect().await.unwrap().to_bytes();
	let json = if bytes.is_empty() { Json::Null } else { serde_json::from_slice(&bytes).unwrap() };
	(status, json)
}

/// `nap::nap(ms)`: the only way to make a script await for a set time.
#[rune::function]
async fn nap(ms: u64) {
	tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

fn nap_module() -> Result<rune::Module, rune::ContextError> {
	let mut m = rune::Module::with_item(["nap"])?;
	m.function_meta(nap)?;
	Ok(m)
}

fn err_code(json: &Json) -> &str {
	json.pointer("/error/errCode").and_then(Json::as_str).unwrap_or("")
}

#[tokio::test]
async fn a_public_route_needs_no_token_and_a_gated_one_does() {
	let (_db, app, router, _store) =
		serve("public-split", mintworks_auth::routes::consent_gate(), true).await;

	let (status, body) = call(&router, "GET", "/api/ping", None, None).await;
	assert_eq!(status, StatusCode::OK);
	// `.public()` drops the auth layer, so `dispatch` mints `Ctx::public("script")`.
	assert_eq!(body["actor"], "public");

	let (status, body) = call(&router, "POST", "/api/echo", None, Some("{}")).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&router, "POST", "/api/echo", Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(body["actor"], "user");

	// `sys::escalate` is the one privilege escalation a script has, and the kind is the only
	// thing `ctx.actor()` hands out — no org_id, no account_id.
	let (status, body) = call(&router, "GET", "/api/sudo", Some(&tok), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["actor"], "system");
}

/// `put_seller` is a boot-time grant: `on_init` may call it, a request handler may not — not
/// even after `sys::escalate`, which is also `Actor::System`.
#[tokio::test]
async fn escalating_does_not_satisfy_init_only_but_on_init_does() {
	const GRANT: &str = r#"
pub async fn main(app) {
	app.on_init(init);
	app.get("/api/grant", grant);
}

pub async fn init(ctx) {
	invoices::put_seller(ctx, #{ seriesCode: "TST", navBaseUrl: "" }).await?;
}

pub async fn grant(ctx, req) {
	invoices::put_seller(sys::escalate(ctx)?, #{ seriesCode: "EVL", navBaseUrl: "" }).await?;
	#{ ok: true }
}
"#;
	let (_db, app, router, store) =
		serve_src("init-only", mintworks_auth::routes::consent_gate(), true, GRANT, false).await;
	let series: Vec<String> = sqlx::query_scalar("SELECT series_code FROM sellers")
		.fetch_all(store.read_pool())
		.await
		.unwrap();
	assert_eq!(series, ["TST"], "on_init did not mint the seller");

	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&router, "GET", "/api/grant", Some(&tok), None).await;
	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
	assert_eq!(err_code(&body), "E-SCRIPT-INIT-ONLY");
}

/// `sys::in_org` moves an init or job handler's system ctx into a tenant org, and `sys::orgs`
/// lists exactly the active seeded tenants to it; a request handler can do neither, escalated or
/// not. Declared `app.every`/`app.next` kinds are seeded at boot — which is also the only thing
/// that ever runs `TENANT_GRANT` here.
#[tokio::test]
async fn in_org_serves_init_and_jobs_never_a_request_and_periodic_kinds_are_seeded() {
	const IN_ORG: &str = r#"
pub async fn main(app) {
	app.on_init(init);
	app.every("TENANT_GRANT", 3600, grant);
	app.next("TENANT_CHAIN", chain);
	app.get("/api/jump", jump);
	app.get("/api/jump-escalated", jump_escalated);
	app.get("/api/orgs", list_orgs);
}

pub async fn init(ctx) {
	let orgs = sys::orgs(ctx).await?;
	if orgs.len() != 2 || orgs[0] != "org_01JCZ5X8K9N7QW3M6R2T4V8Y0C"
		|| orgs[1] != "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D" {
		return Err(err::app(500, "E-APP-ORGS", "sys::orgs is not [ORG_A, ORG_B]")?);
	}
	if sys::in_org(ctx, "org_01JCZ5X8K9N7QW3M6R2T4V8Y0Z").await.is_ok() {
		return Err(err::app(500, "E-APP-LEAK", "an unknown org resolved")?);
	}
	let tenant = sys::in_org(ctx, "org_01JCZ5X8K9N7QW3M6R2T4V8Y0C").await?;
	if auth::members(tenant).await?.len() == 0 {
		return Err(err::app(500, "E-APP-MEMBERS", "auth::members is empty under sys::in_org")?);
	}
	if auth::org(tenant).await?.uid != "org_01JCZ5X8K9N7QW3M6R2T4V8Y0C" {
		return Err(err::app(500, "E-APP-ORG", "auth::org is not the in_org tenant")?);
	}
	invoices::put_seller(tenant, #{ seriesCode: "INI", navBaseUrl: "" }).await?;
}

pub async fn grant(ctx, payload) {
	let tenant = sys::in_org(ctx, "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D").await?;
	invoices::put_seller(tenant, #{ seriesCode: "JOB", navBaseUrl: "" }).await?;
}

pub async fn chain(ctx, payload) {
	4102444800
}

pub async fn jump(ctx, req) {
	sys::in_org(ctx, "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D").await?;
	#{ ok: true }
}

pub async fn jump_escalated(ctx, req) {
	sys::in_org(sys::escalate(ctx)?, "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D").await?;
	#{ ok: true }
}

pub async fn list_orgs(ctx, req) {
	sys::orgs(ctx).await?;
	#{ ok: true }
}
"#;
	let (_db, builder, store) =
		install("in-org", mintworks_auth::routes::consent_gate(), true, IN_ORG, false).await;
	// Before boot, so `init`'s `sys::orgs` must leave a suspended org out.
	sqlx::query(
		"INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, status, created_at)
		 VALUES ('org_01JCZ5X8K9N7QW3M6R2T4V8Y0S', (SELECT id FROM orgs WHERE kind = 'ROOT'),
		         'SHARED', 'S', 1, 'SUSPENDED', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	let (app, router) = builder.unwrap().into_service().await.unwrap();
	let org_of = |series: &'static str| {
		let store = store.clone();
		async move {
			sqlx::query_scalar::<_, String>(
				"SELECT o.uid FROM sellers s JOIN orgs o ON o.id = s.org_id WHERE s.series_code = ?",
			)
			.bind(series)
			.fetch_optional(store.read_pool())
			.await
			.unwrap()
		}
	};
	assert_eq!(org_of("INI").await.as_deref(), Some(ORG_A), "on_init did not act in ORG_A");

	// Nothing enqueues `TENANT_GRANT` but the boot-time seed; the default workers run it.
	let mut job_org = None;
	for _ in 0..100 {
		job_org = org_of("JOB").await;
		if job_org.is_some() {
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
	}
	assert_eq!(job_org.as_deref(), Some(ORG_B), "the seeded TENANT_GRANT never acted in ORG_B");
	assert!(store.job_has_live("TENANT_GRANT", None).await.unwrap());
	assert!(store.job_has_live("TENANT_CHAIN", None).await.unwrap(), "app.next was not seeded");

	let tok = token(&app, ORG_A).await;
	for path in ["/api/jump", "/api/jump-escalated", "/api/orgs"] {
		let (status, body) = call(&router, "GET", path, Some(&tok), None).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
		assert_eq!(err_code(&body), "E-SCRIPT-INIT-ONLY", "{path}");
	}
}

/// `jobs` has no org column, so a job id from a request would read across tenants: an init or job
/// handler may ask, under `sys::in_org` too, and a request handler may not, escalated or not.
#[tokio::test]
async fn jobs_status_serves_init_and_jobs_never_a_request() {
	const STATUS: &str = r#"
pub async fn main(app) {
	app.on_init(init);
	app.job("LATER", later);
	app.get("/api/status", status);
	app.get("/api/status-escalated", status_escalated);
}

pub async fn init(ctx) {
	let id = jobs::enqueue(ctx, "LATER", #{}, #{ run_at: 4102444800 }).await?.unwrap();
	let tenant = sys::in_org(ctx, "org_01JCZ5X8K9N7QW3M6R2T4V8Y0C").await?;
	if jobs::status(ctx, id).await? != "PENDING" || jobs::status(tenant, id).await? != "PENDING" {
		return Err(err::app(500, "E-APP-STATUS", "jobs::status did not answer")?);
	}
}

pub async fn later(ctx, payload) {}

pub async fn status(ctx, req) {
	jobs::status(ctx, 1).await?;
	#{ ok: true }
}

pub async fn status_escalated(ctx, req) {
	jobs::status(sys::escalate(ctx)?, 1).await?;
	#{ ok: true }
}
"#;
	let (_db, app, router, _store) =
		serve_src("jobs-status", mintworks_auth::routes::consent_gate(), true, STATUS, false).await;
	let tok = token(&app, ORG_A).await;
	for path in ["/api/status", "/api/status-escalated"] {
		let (status, body) = call(&router, "GET", path, Some(&tok), None).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
		assert_eq!(err_code(&body), "E-SCRIPT-INIT-ONLY", "{path}");
	}
}

/// The deployment's NAV alerts are operator or boot-system only: an escalated request ctx is
/// `System` too, and must not reach them.
#[tokio::test]
async fn an_escalated_request_cannot_read_nav_alerts() {
	const ALERTS: &str = r#"
pub async fn main(app) {
	app.get("/api/alerts", alerts);
}

pub async fn alerts(ctx, req) {
	nav::alerts(sys::escalate(ctx)?).await?;
	#{ ok: true }
}
"#;
	let (_db, app, router, _store) =
		serve_src("nav-alerts", mintworks_auth::routes::consent_gate(), true, ALERTS, false).await;
	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&router, "GET", "/api/alerts", Some(&tok), None).await;
	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
	assert_eq!(err_code(&body), "E-SCRIPT-INIT-ONLY");
}

/// These bindings trust a `System` actor, so an escalated request ctx would read a member list
/// its own role refuses, reach another org by naming it, or grant an account role.
#[tokio::test]
async fn an_escalated_request_cannot_call_the_auth_bindings_that_trust_system() {
	const READS: &str = r#"
pub async fn main(app) {
	app.get("/api/members", members);
	app.get("/api/members-in", members_in);
	app.get("/api/org-peers", org_peers);
	app.get("/api/org", org);
	app.get("/api/set-member-role", set_member_role);
	app.get("/api/create-invite-code", create_invite_code);
	app.get("/api/invite-codes", invite_codes);
	app.get("/api/set-account-role", set_account_role);
	app.get("/api/accounts-with-role", accounts_with_role);
	app.get("/api/account-roles", account_roles);
	app.get("/api/accounts", accounts);
	app.get("/api/set-handle", set_handle);
}

pub async fn set_handle(ctx, req) {
	auth::set_handle(sys::escalate(ctx)?, "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0E", "x").await?;
	#{ ok: true }
}

pub async fn set_member_role(ctx, req) {
	auth::set_member_role(sys::escalate(ctx)?, #{ org: "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D", account: "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0E", appRole: "x" }).await?;
	#{ ok: true }
}

pub async fn create_invite_code(ctx, req) {
	auth::create_invite_code(sys::escalate(ctx)?, "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D", #{}).await?;
	#{ ok: true }
}

pub async fn invite_codes(ctx, req) {
	auth::invite_codes(sys::escalate(ctx)?, "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D").await?;
	#{ ok: true }
}

pub async fn set_account_role(ctx, req) {
	auth::set_account_role(sys::escalate(ctx)?, #{ account: "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0E", role: "x", on: true }).await?;
	#{ ok: true }
}

pub async fn accounts_with_role(ctx, req) {
	auth::accounts_with_role(sys::escalate(ctx)?, "x").await?;
	#{ ok: true }
}

pub async fn accounts(ctx, req) {
	auth::accounts(sys::escalate(ctx)?, []).await?;
	#{ ok: true }
}

pub async fn account_roles(ctx, req) {
	auth::account_roles(sys::escalate(ctx)?, "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0E").await?;
	#{ ok: true }
}

pub async fn members(ctx, req) {
	auth::members(sys::escalate(ctx)?).await?;
	#{ ok: true }
}

pub async fn members_in(ctx, req) {
	auth::members_in(sys::escalate(ctx)?, #{ org: "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D" }).await?;
	#{ ok: true }
}

pub async fn org_peers(ctx, req) {
	auth::org_peers(sys::escalate(ctx)?, #{ org: "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D", appRole: "x" }).await?;
	#{ ok: true }
}

pub async fn org(ctx, req) {
	auth::org(sys::escalate(ctx)?).await?;
	#{ ok: true }
}
"#;
	let (_db, app, router, _store) =
		serve_src("auth-reads", mintworks_auth::routes::consent_gate(), true, READS, false).await;
	let tok = token(&app, ORG_A).await;
	for path in [
		"/api/members",
		"/api/members-in",
		"/api/org-peers",
		"/api/org",
		"/api/set-member-role",
		"/api/create-invite-code",
		"/api/invite-codes",
		"/api/set-account-role",
		"/api/accounts-with-role",
		"/api/account-roles",
		"/api/accounts",
		"/api/set-handle",
	] {
		let (status, body) = call(&router, "GET", path, Some(&tok), None).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
		assert_eq!(err_code(&body), "E-SCRIPT-INIT-ONLY", "{path}");
	}
}

/// `seller_admin` passes a `System` actor and `seller_for_org` climbs to an ancestor, so an
/// escalated tenant request would rewrite the seller it inherits. Raising invoices stays open:
/// that is what `sys::escalate` is for.
#[tokio::test]
async fn an_escalated_request_cannot_write_seller_master_data() {
	const WRITES: &str = r#"
pub async fn main(app) {
	app.get("/api/create-service", create_service);
	app.get("/api/update-service", update_service);
	app.get("/api/sync-services", sync_services);
	app.get("/api/set-org-currency", set_org_currency);
	app.get("/api/seller-draft", seller_draft);
	app.get("/api/save-seller-draft", save_seller_draft);
	app.get("/api/publish-seller", publish_seller);
	app.get("/api/sync-seller", sync_seller);
}

pub async fn create_service(ctx, req) {
	invoices::create_service(sys::escalate(ctx)?, #{}).await?;
	#{ ok: true }
}

pub async fn update_service(ctx, req) {
	invoices::update_service(sys::escalate(ctx)?, "x", #{}).await?;
	#{ ok: true }
}

pub async fn sync_services(ctx, req) {
	invoices::sync_services(sys::escalate(ctx)?, []).await?;
	#{ ok: true }
}

pub async fn set_org_currency(ctx, req) {
	invoices::set_org_currency(sys::escalate(ctx)?, "EUR", #{ convert: false }).await?;
	#{ ok: true }
}

pub async fn seller_draft(ctx, req) {
	invoices::seller_draft(sys::escalate(ctx)?).await?;
	#{ ok: true }
}

pub async fn save_seller_draft(ctx, req) {
	invoices::save_seller_draft(sys::escalate(ctx)?, #{}).await?;
	#{ ok: true }
}

pub async fn publish_seller(ctx, req) {
	invoices::publish_seller(sys::escalate(ctx)?).await?;
	#{ ok: true }
}

pub async fn sync_seller(ctx, req) {
	invoices::sync_seller(sys::escalate(ctx)?, #{}).await?;
	#{ ok: true }
}
"#;
	let (_db, app, router, _store) =
		serve_src("seller-writes", mintworks_auth::routes::consent_gate(), true, WRITES, false)
			.await;
	let tok = token(&app, ORG_A).await;
	for path in [
		"/api/create-service",
		"/api/update-service",
		"/api/sync-services",
		"/api/set-org-currency",
		"/api/seller-draft",
		"/api/save-seller-draft",
		"/api/publish-seller",
		"/api/sync-seller",
	] {
		let (status, body) = call(&router, "GET", path, Some(&tok), None).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
		assert_eq!(err_code(&body), "E-SCRIPT-INIT-ONLY", "{path}");
	}
}

/// A request handler attaches only its own org's blobs.
#[tokio::test]
async fn a_request_cannot_attach_another_orgs_blob() {
	const MAIL: &str = r#"
pub async fn main(app) {
	app.post("/api/mail", mail);
}

pub async fn mail(ctx, req) {
	let a = #{ sha256: req.body.sha, filename: "a.pdf", mime: "application/pdf" };
	email::send(ctx, #{ to: "x@example.test", template: "welcome", attachments: [a] }).await?;
	#{ ok: true }
}
"#;
	let (_db, app, router, store) =
		serve_src("mail-blob", mintworks_auth::routes::consent_gate(), true, MAIL, false).await;
	sqlx::query(
		"INSERT INTO blobs (uid, org_id, sha256, mime, size, created_at, unref_at)
		 VALUES ('blb_01JCZ5X8K9N7QW3M6R2T4V8Y0F', (SELECT id FROM orgs WHERE uid = ?), 'b0b',
		         'application/pdf', 1, 0, 0)",
	)
	.bind(ORG_B)
	.execute(store.write_pool())
	.await
	.unwrap();

	let tok = token(&app, ORG_A).await;
	let (status, body) =
		call(&router, "POST", "/api/mail", Some(&tok), Some(r#"{"sha":"b0b"}"#)).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
	assert_eq!(err_code(&body), "E-CORE-NOTFOUND");
}

/// The invoice and NAV bindings over org A's seller, for the tests below.
const BILLING: &str = r#"
pub async fn main(app) {
	app.mount("invoice.org_read");
	app.mount("invoice.org_seller");
	app.mount("invoice.public");
	app.post("/api/vies", vies);
	app.post("/api/vies-public", vies).public().tier("public");
	app.post("/api/retry/{uid}", retry);
	app.post("/api/retry-escalated/{uid}", retry_escalated);
	app.get("/api/filing-escalated/{uid}", filing_escalated);
	app.get("/api/parties", parties);
	app.post("/api/org-currency", org_currency);
	app.get("/api/document/{uid}", document);
	app.post("/api/link/{uid}", link);
	app.post("/api/link-escalated/{uid}", link_escalated);
	app.post("/api/cc", cc);
}

pub async fn vies(ctx, req) {
	invoices::vies(ctx, req.body.id).await?
}

pub async fn retry(ctx, req) {
	nav::retry(ctx, req.path.uid).await?
}

pub async fn retry_escalated(ctx, req) {
	nav::retry(sys::escalate(ctx)?, req.path.uid).await?
}

pub async fn filing_escalated(ctx, req) {
	nav::filing(sys::escalate(ctx)?, req.path.uid).await?
}

pub async fn parties(ctx, req) {
	invoices::list_parties_page(ctx, #{ q: "Vevo", limit: 10 }).await?
}

pub async fn org_currency(ctx, req) {
	invoices::set_org_currency(ctx, "EUR", #{ convert: true, feeBp: 200 }).await?;
	invoices::set_org_currency(ctx, "EUR", #{ convert: false }).await?
}

pub async fn document(ctx, req) {
	invoices::document(ctx, req.path.uid).await?
}

pub async fn link(ctx, req) {
	#{ url: invoices::download_link(ctx, req.path.uid).await? }
}

pub async fn link_escalated(ctx, req) {
	#{ url: invoices::download_link(sys::escalate(ctx)?, req.path.uid).await? }
}

pub async fn cc(ctx, req) {
	#{ job: email::send(ctx, #{ to: "x@example.test", template: "welcome", cc: req.body.cc }).await? }
}
"#;

const INVOICE: &str = "inv_01JCZ5X8K9N7QW3M6R2T4V8Y0E";

/// `BILLING` over a seller and an issued, rendered invoice in org A: rows, not a real `issue`,
/// since only the bindings are under test.
async fn serve_billing(name: &str) -> (TmpDb, Router, SqliteStore, String) {
	let (db, app, router, store) =
		serve_src(name, mintworks_auth::routes::consent_gate(), true, BILLING, false).await;
	let pool = store.write_pool();
	let org: i64 = sqlx::query_scalar("SELECT id FROM orgs WHERE uid = ?")
		.bind(ORG_A)
		.fetch_one(pool)
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO sellers (id, uid, org_id, nav_base_url, created_at) VALUES (1, ?, ?, '', 0)",
	)
	.bind(mintworks_core::ids::SellerId::generate().as_str())
	.bind(org)
	.execute(pool)
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO seller_versions (seller_ver, seller_id, status, name, tax_number, postcode,
		   city, street, vat_scheme, created_at, valid_from)
		 VALUES (1, 1, 'CURRENT', 'A', '12345678-1-42', '1111', 'Bp', 'U 1', 'NORMAL', 0, 0)",
	)
	.execute(pool)
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO billing_parties (id, uid, org_id, kind, name, country, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', ?, 'C', 'Vevo Zrt.', 'HU', 0, 0)",
	)
	.bind(org)
	.execute(pool)
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO invoices (id, uid, org_id, seller_id, seller_ver, status, number, issued_at,
		   fulfilment_date, buyer_name, currency, created_at, updated_at)
		 VALUES (1, ?, ?, 1, 1, 'ISSUED', 'A-1', 0, '2026-01-31', 'Vevo Zrt.', 'HUF', 0, 0)",
	)
	.bind(INVOICE)
	.bind(org)
	.execute(pool)
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO invoice_documents (invoice_id, kind, sha256, bytes, template_version, rendered_at)
		 VALUES (1, 'PDF', ?, 1, 't', 0)",
	)
	.bind("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")
	.execute(pool)
	.await
	.unwrap();
	store.currency_enable("EUR").await.unwrap();
	let tok = token(&app, ORG_A).await;
	(db, router, store, tok)
}

/// A fresh cache row, so `vies::check` never reaches the network.
#[tokio::test]
async fn invoices_vies_answers_an_org_and_refuses_public() {
	let (_db, router, store, tok) = serve_billing("rune-vies").await;
	store
		.vies_store(&mintworks_invoice::vies::ViesResult {
			eu_vat_id: "DE811907980".to_owned(),
			valid: true,
			name: Some("Kaufer GmbH".to_owned()),
			address: None,
			request_id: None,
			requester: None,
			checked_at: mintworks_core::types::Timestamp::now(),
			cached: false,
		})
		.await
		.unwrap();
	let id = r#"{"id":"DE811907980"}"#;

	let (status, body) = call(&router, "POST", "/api/vies", Some(&tok), Some(id)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!((&body["valid"], &body["name"]), (&Json::Bool(true), &"Kaufer GmbH".into()));

	let (status, body) = call(&router, "POST", "/api/vies-public", None, Some(id)).await;
	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
	assert_eq!(err_code(&body), "E-AUTH-FORBIDDEN");
}

#[tokio::test]
async fn nav_retry_refiles_a_rejected_invoice_and_answers_the_filing() {
	let (_db, router, store, tok) = serve_billing("rune-nav-retry").await;
	let now = mintworks_core::types::Timestamp::now();
	let batch = store.claim_batch(1, NavOp::Create, &[1], false).await.unwrap().unwrap().0.id;
	let row = store.batch_rows(batch).await.unwrap()[0].id;
	assert!(store.begin_post(batch, "<x/>", &[(row, 1)], now).await.unwrap());
	assert!(store.set_sent(batch, "TX").await.unwrap());
	assert!(store.finish(row, NavVerdict::Rejected, Some(("X", "no")), now).await.unwrap());

	let path = format!("/api/retry/{INVOICE}");
	let (status, body) = call(&router, "POST", &path, Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["state"], "CLAIMED", "{body}");
	assert_eq!(body["verdict"], Json::Null, "{body}");
	let history = store.filings_by_invoice(1).await.unwrap();
	assert_eq!(history.len(), 2);
	assert_eq!(history[1].row.verdict, Some(NavVerdict::Rejected), "the verdict stands");
	assert!(history[1].row.resolved_at.is_some());

	// A second click finds the refiled row's job live, and files nothing more.
	let (status, body) = call(&router, "POST", &path, Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(store.filings_by_invoice(1).await.unwrap().len(), 2);

	assert!(store.cancel_filing(1, NavOp::Create, now).await.unwrap());
	let (status, body) = call(&router, "POST", &path, Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::CONFLICT, "{body}");
	assert_eq!(err_code(&body), "E-NAV-NOT-RETRYABLE");
}

#[tokio::test]
async fn invoices_list_parties_page_answers_items_and_total() {
	let (_db, router, _store, tok) = serve_billing("rune-parties").await;
	let (status, body) = call(&router, "GET", "/api/parties", Some(&tok), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["total"], 1, "{body}");
	assert_eq!(body["items"][0]["name"], "Vevo Zrt.", "{body}");
}

#[tokio::test]
async fn invoices_set_org_currency_keeps_the_fee_when_feebp_is_absent_on_opt_out() {
	let (_db, router, _store, tok) = serve_billing("rune-org-currency").await;
	let (status, body) = call(&router, "POST", "/api/org-currency", Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body, serde_json::json!({ "code": "EUR", "convert": false, "feeBp": 200 }));
}

/// An escalated ctx holding no org, as on a public route, would reach any tenant's invoice.
#[tokio::test]
async fn an_escalated_ctx_cannot_read_or_retry_a_filing() {
	let (_db, router, _store, tok) = serve_billing("rune-nav-escalated").await;
	for (method, route) in [("POST", "retry-escalated"), ("GET", "filing-escalated")] {
		let path = format!("/api/{route}/{INVOICE}");
		let (status, body) = call(&router, method, &path, Some(&tok), Some("{}")).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "{route}: {body}");
		assert_eq!(err_code(&body), "E-SCRIPT-INIT-ONLY", "{route}");
	}
}

/// A read must not write: only `invoices::download_link` adds a `refs` row.
#[tokio::test]
async fn invoices_document_reads_and_download_link_mints_one_ref() {
	let (_db, router, store, tok) = serve_billing("rune-document").await;
	let refs = async || -> i64 {
		sqlx::query_scalar("SELECT COUNT(*) FROM refs WHERE type = 'invoice_download'")
			.fetch_one(store.read_pool())
			.await
			.unwrap()
	};
	let path = format!("/api/document/{INVOICE}");
	let (status, body) = call(&router, "GET", &path, Some(&tok), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["invoice"]["uid"], INVOICE, "{body}");
	assert!(body.get("downloadUrl").is_none(), "{body}");
	assert_eq!(refs().await, 0);

	let path = format!("/api/link/{INVOICE}");
	let (status, body) = call(&router, "POST", &path, Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let url = body["url"].as_str().unwrap();
	assert!(url.contains("/api/public/invoices/") && url.ends_with("/pdf"), "{url}");
	assert!(!url.contains(INVOICE), "the code, not the uid: {url}");
	assert_eq!(refs().await, 1);
}

/// The seeded account is the fixture's one member of every org: this takes it down to MEMBER.
async fn demote(store: &SqliteStore) {
	sqlx::query("UPDATE memberships SET role = 'MEMBER' WHERE account_id = 1")
		.execute(store.write_pool())
		.await
		.unwrap();
}

#[tokio::test]
async fn invoices_download_link_needs_an_org_admin() {
	let (_db, router, store, tok) = serve_billing("rune-link-member").await;
	demote(&store).await;
	let path = format!("/api/link/{INVOICE}");
	let (status, body) = call(&router, "POST", &path, Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
	let minted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM refs")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(minted, 0);
}

/// `System` passes `document_link`'s org-admin check, so the guard is all that stops a member.
#[tokio::test]
async fn invoices_download_link_refuses_an_escalated_ctx() {
	let (_db, router, store, tok) = serve_billing("rune-link-escalated").await;
	let path = format!("/api/link-escalated/{INVOICE}");
	let (status, body) = call(&router, "POST", &path, Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
	assert_eq!(err_code(&body), "E-SCRIPT-INIT-ONLY");
	let minted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM refs")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(minted, 0);
}

/// A link can be revoked, so the file must not outlive it in a cache; the app's own URL can.
#[tokio::test]
async fn the_public_pdf_is_not_cached_and_the_in_app_one_is() {
	let (db, router, _store, tok) = serve_billing("rune-pdf-cache").await;
	let file = mintworks_core::blob::blob_path(
		&db.config().data_dir,
		"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
	)
	.unwrap();
	std::fs::create_dir_all(file.parent().unwrap()).unwrap();
	std::fs::write(&file, b"%PDF").unwrap();

	let path = format!("/api/link/{INVOICE}");
	let (_, body) = call(&router, "POST", &path, Some(&tok), Some("{}")).await;
	let url = body["url"].as_str().unwrap();
	let public = &url[url.find("/api/public/").unwrap()..];
	let in_app = format!("/api/invoices/{INVOICE}/pdf");
	for (uri, auth, cache) in [
		(public, None, "private, no-store"),
		(&in_app, Some(&tok), "private, max-age=31536000, immutable"),
	] {
		let mut req = Request::builder().uri(uri);
		if let Some(t) = auth {
			req = req.header("authorization", format!("Bearer {t}"));
		}
		let res = router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(res.status(), StatusCode::OK, "{uri}");
		assert_eq!(res.headers()["cache-control"], cache, "{uri}");
	}
}

/// An `AMOUNT` is money on the wire, a `PERCENT` is basis points; each leaves the other null.
#[tokio::test]
async fn the_invoice_view_splits_its_discount_by_kind() {
	let (_db, router, store, tok) = serve_billing("invoice-discount-wire").await;
	let path = format!("/api/invoices/{INVOICE}");
	let set = "UPDATE invoices SET discount_kind = ?, discount_value = ? WHERE uid = ?";
	for (kind, value) in [("AMOUNT", 1500), ("PERCENT", 1000)] {
		sqlx::query(set)
			.bind(kind)
			.bind(value)
			.bind(INVOICE)
			.execute(store.write_pool())
			.await
			.unwrap();
		let (status, body) = call(&router, "GET", &path, Some(&tok), None).await;
		assert_eq!(status, StatusCode::OK, "{body}");
		assert_eq!(body["discountKind"], kind, "{body}");
		if kind == "AMOUNT" {
			let fixed = serde_json::json!({ "amount": "15.00", "currency": "HUF" });
			assert_eq!(body["discountFixed"], fixed, "{body}");
			assert!(body["discountValue"].is_null(), "{body}");
		} else {
			assert_eq!(body["discountValue"], 1000, "{body}");
			assert!(body["discountFixed"].is_null(), "{body}");
		}
	}
}

/// The only way an org already running opts in: the upgrade leaves conversion off for it.
#[tokio::test]
async fn the_seller_currency_routes_opt_in_and_refuse_a_member() {
	let (_db, router, store, tok) = serve_billing("seller-currencies").await;
	let path = "/api/seller/currencies/EUR";
	let put = Some(r#"{"convert":true,"feeBp":150}"#);
	let (status, body) = call(&router, "PUT", path, Some(&tok), put).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let row = serde_json::json!({ "code": "EUR", "convert": true, "feeBp": 150 });
	assert_eq!(body, row);

	demote(&store).await;
	let (status, body) = call(&router, "GET", "/api/seller/currencies", Some(&tok), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["items"], serde_json::json!([row]), "{body}");
	let (status, body) = call(&router, "PUT", path, Some(&tok), put).await;
	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn email_send_queues_cc_and_refuses_a_bad_one() {
	let (_db, router, store, tok) = serve_billing("rune-email-cc").await;
	let (status, body) =
		call(&router, "POST", "/api/cc", Some(&tok), Some(r#"{"cc":["a@example.test"]}"#)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let payload: String = sqlx::query_scalar("SELECT payload FROM jobs WHERE id = ?")
		.bind(body["job"].as_i64().unwrap())
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert!(payload.contains("a@example.test"), "{payload}");

	let (status, body) =
		call(&router, "POST", "/api/cc", Some(&tok), Some(r#"{"cc":["not an address"]}"#)).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// An `app.next` chain is seeded once: one whose handler returned `()` stays ended on reboot.
#[tokio::test]
async fn an_ended_next_chain_is_not_reseeded_on_boot() {
	const ONCE: &str = r#"
pub async fn main(app) {
	app.next("ONE_SHOT", once);
}

pub async fn once(ctx, payload) {
}
"#;
	let (db, _app, _router, store) =
		serve_src("next-once", mintworks_auth::routes::consent_gate(), true, ONCE, false).await;
	for _ in 0..100 {
		if !store.job_has_live("ONE_SHOT", None).await.unwrap() {
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
	}
	assert!(!store.job_has_live("ONE_SHOT", None).await.unwrap(), "the seeded chain never ran");

	let builder = build(&db, &store, mintworks_auth::routes::consent_gate(), ONCE, false).await;
	let _service = builder.unwrap().into_service().await.unwrap();
	assert!(!store.job_has_live("ONE_SHOT", None).await.unwrap(), "the reboot revived the chain");
}

/// Another org's row is `E-CORE-NOTFOUND`, never a 403 — a 403 confirms the uid exists.
#[tokio::test]
async fn another_orgs_object_is_not_found_never_forbidden() {
	let (_db, app, router, store) =
		serve("org-boundary", mintworks_auth::routes::consent_gate(), true).await;
	let org_a: i64 = sqlx::query_scalar("SELECT id FROM orgs WHERE uid = ?")
		.bind(ORG_A)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	store
		.object_put(org_a, "note", "nte_1", &serde_json::json!({ "a": 1 }), &[])
		.await
		.unwrap();

	let a = token(&app, ORG_A).await;
	let (status, body) = call(&router, "GET", "/api/notes/nte_1", Some(&a), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");

	let b = token(&app, ORG_B).await;
	let (status, body) = call(&router, "GET", "/api/notes/nte_1", Some(&b), None).await;
	assert_eq!(status, StatusCode::NOT_FOUND);
	assert_eq!(err_code(&body), "E-CORE-NOTFOUND");
}

/// `ScriptApp::gate(RouteGate::none())` is the opt-out for a deployment that publishes no legal
/// documents: it drops the consent check, never authentication.
#[tokio::test]
async fn route_gate_none_drops_consent_but_not_auth() {
	let (_db, app, gated, _store) =
		serve("gate-on", mintworks_auth::routes::consent_gate(), false).await;
	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&gated, "POST", "/api/echo", Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::FORBIDDEN);
	assert_eq!(err_code(&body), "E-AUTH-CONSENT-REQUIRED");

	let (_db, app, ungated, _store) = serve("gate-off", RouteGate::none(), false).await;
	let (status, _) = call(&ungated, "POST", "/api/echo", None, Some("{}")).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&ungated, "POST", "/api/echo", Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["actor"], "user");
}

/// The script database is separate: a framework table is not off-limits, it is not there.
#[tokio::test]
async fn db_cannot_reach_the_framework_db() {
	let (_db, app, router, _store) =
		serve("db-framework", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&router, "GET", "/api/framework-table", Some(&tok), None).await;
	assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
}

/// Lock order is script first: a `db::` write inside `tx::with` is refused, not queued.
#[tokio::test]
async fn a_script_write_inside_tx_with_is_refused() {
	let (_db, app, router, _store) =
		serve("db-crossing", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&router, "POST", "/api/crossing", Some(&tok), None).await;
	assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
	assert_eq!(err_code(&body), "E-SCRIPT-DB");
	assert_ledger_empty(&router, &tok).await;
}

/// The settings cache is invalidated before commit, so a rollback would leave a phantom value.
#[tokio::test]
async fn settings_set_inside_tx_with_is_refused() {
	let (_db, app, router, _store) =
		serve("settings-in-tx", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&router, "POST", "/api/settings-in-tx", Some(&tok), None).await;
	assert_eq!(err_code(&body), mintworks_script::E_TX_REMOTE, "{status} {body}");
}

#[tokio::test]
async fn a_framework_rollback_inside_db_tx_rolls_both_back() {
	let (_db, app, router, _store) =
		serve("db-nested", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&router, "POST", "/api/nested", Some(&tok), None).await;
	assert_eq!(status, StatusCode::CONFLICT, "{body}");
	assert_ledger_empty(&router, &tok).await;
}

#[tokio::test]
async fn db_tx_rolls_back() {
	let (_db, app, router, _store) =
		serve("db-rollback", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&router, "POST", "/api/tx-rollback", Some(&tok), None).await;
	assert_eq!(status, StatusCode::CONFLICT, "{body}");
	assert_ledger_empty(&router, &tok).await;
}

#[tokio::test]
async fn db_exec_refuses_ddl() {
	let (_db, app, router, _store) =
		serve("db-ddl", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;
	let (status, body) = call(&router, "POST", "/api/ddl", Some(&tok), None).await;
	assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
	assert_eq!(err_code(&body), "E-SCRIPT-DB");
}

async fn assert_ledger_empty(router: &Router, tok: &str) {
	let (status, body) = call(router, "GET", "/api/ledger", Some(tok), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["items"], serde_json::json!([]));
}

/// A body is parsed as JSON only under a JSON media type: a `text/plain` POST is what a
/// cross-site form can send without a preflight.
#[tokio::test]
async fn a_non_json_content_type_is_refused() {
	let (_db, app, router, _store) =
		serve("content-type", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;
	let post = |ct: &str| {
		Request::builder()
			.method("POST")
			.uri("/api/echo")
			.header("authorization", format!("Bearer {tok}"))
			.header("content-type", ct)
			.body(Body::from("{}"))
			.unwrap()
	};

	let (status, body) = send(&router, post("text/plain")).await;
	assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
	assert_eq!(err_code(&body), "E-CORE-UNSUPPORTED");

	let (status, body) = send(&router, post("application/merge-patch+json")).await;
	assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_body_past_max_body_is_a_coded_413() {
	let (_db, app, router, _store) =
		serve("max-body", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;
	let big = r#"{"n":"0123456789abcdef"}"#;
	let (status, body) = call(&router, "POST", "/api/echo-small", Some(&tok), Some(big)).await;
	assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
	assert_eq!(err_code(&body), "E-SCRIPT-TOOLARGE");
}

/// A `.raw()` body is spooled to disk, so it may pass the 2 MiB a buffered body is held to.
#[tokio::test]
async fn a_raw_route_stores_a_body_past_two_mib() {
	let (_db, app, router, _store) =
		serve("raw-upload", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;
	let bytes: Vec<u8> = (0..3 * 1024 * 1024).map(|i: u32| (i % 251) as u8).collect();
	let req = Request::builder()
		.method("POST")
		.uri(format!("/api/upload?org={ORG_A}"))
		.header("authorization", format!("Bearer {tok}"))
		.header("content-type", "application/octet-stream")
		.body(Body::from(bytes.clone()))
		.unwrap();
	let (status, body) = send(&router, req).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["again"], "refused");
	assert_eq!(body["early"], true, "an unknown org fails before the body is taken");
	let uid = body["uid"].as_str().unwrap();
	let req = Request::builder()
		.uri(format!("/api/upload/{uid}"))
		.header("authorization", format!("Bearer {tok}"))
		.body(Body::empty())
		.unwrap();
	let res = router.clone().oneshot(req).await.unwrap();
	assert_eq!(res.status(), StatusCode::OK);
	assert_eq!(res.headers()["cache-control"], "private, max-age=60");
	assert!(res.into_body().collect().await.unwrap().to_bytes() == bytes);
}

#[tokio::test]
async fn a_streamed_cache_control_is_verbatim_public_and_private_authed() {
	let (_db, _app, router, _store) =
		serve("pub-file", mintworks_auth::routes::consent_gate(), true).await;
	let req = Request::builder().uri("/api/pub-file").body(Body::empty()).unwrap();
	let res = router.clone().oneshot(req).await.unwrap();
	assert_eq!(res.status(), StatusCode::OK);
	assert_eq!(res.headers()["cache-control"], "public, max-age=60");
}

#[tokio::test]
async fn a_raised_max_body_without_raw_fails_the_boot() {
	let src = r#"
pub async fn main(app) {
	app.post("/api/big", big).max_body(3 * 1024 * 1024);
}
pub async fn big(ctx, req) { #{} }
"#;
	let (_db, builder, _store) =
		install("big-buffered", mintworks_auth::routes::consent_gate(), true, src, false).await;
	let Err(e) = builder else { panic!("a buffered 3 MiB body booted") };
	assert!(e.to_string().contains("requires .raw()"), "{e}");
}

#[tokio::test]
async fn dispatch_hands_the_handler_path_query_and_body() {
	let (_db, app, router, _store) =
		serve("dispatch", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;

	let (status, body) =
		call(&router, "POST", "/api/echo?a=1&b=two", Some(&tok), Some(r#"{"n":3}"#)).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["body"]["n"], 3);
	assert_eq!(body["q"]["a"], "1");
	assert_eq!(body["q"]["b"], "two");
	assert_eq!(body["method"], "POST");

	// An empty body is `Null`, not a parse failure: a POST with nothing to say is legitimate.
	let (status, body) = call(&router, "POST", "/api/echo", Some(&tok), None).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(body["body"], Json::Null);

	let (status, body) = call(&router, "POST", "/api/echo", Some(&tok), Some("{not json")).await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(err_code(&body), "E-CORE-VALIDATION");

	// A path parameter reaches `req.path`, which the `note` route reads its key from.
	let (status, body) = call(&router, "GET", "/api/notes/nte_missing", Some(&tok), None).await;
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// `IoProfile::app` confines `fs` to the application directory — for a `mintworks` bundle, where
/// its `.rn` sources live. Driven through the binding, which closes over the profile.
#[tokio::test]
async fn the_fs_module_is_confined_to_the_profiles_root() {
	let (_db, app, router, _store) =
		serve("fs-root", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;

	let (status, body) = call(&router, "GET", "/api/files", Some(&tok), None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["roundTrip"], "hi");
	assert_eq!(body["traversal"], "refused");
	assert_eq!(body["absolute"], "refused");
	assert_eq!(body["exists"], "refused");
}

/// A declared `E-APP-*` keeps both halves on the wire: the code the bundle raised and the
/// status it declared for it.
#[tokio::test]
async fn a_declared_app_code_reaches_the_wire_with_its_status() {
	let (_db, app, router, _store) =
		serve("app-code", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;

	let (status, body) = call(&router, "POST", "/api/boom", Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::IM_A_TEAPOT);
	assert_eq!(err_code(&body), "E-APP-TEAPOT");
	assert_eq!(body.pointer("/error/errStr").and_then(Json::as_str), Some("no coffee here"));
}

/// A script reads a caught error's `code()` and `status()`, for a declared app code and a core one.
#[tokio::test]
async fn a_script_reads_a_caught_errors_code_and_status() {
	let (_db, app, router, _store) =
		serve("caught", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;

	let (status, body) = call(&router, "POST", "/api/caught", Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["app"]["code"], "E-APP-TEAPOT");
	assert_eq!(body["app"]["status"], 418);
	assert_eq!(body["core"]["code"], "E-CORE-NOTFOUND");
	assert_eq!(body["core"]["status"], 404);
}

/// An `err::app` raised inside `tx::with` keeps its code and status across the boundary.
#[tokio::test]
async fn an_app_code_raised_inside_tx_with_keeps_its_code_and_status() {
	let (_db, app, router, _store) =
		serve("app-code-tx", mintworks_auth::routes::consent_gate(), true).await;
	let tok = token(&app, ORG_A).await;

	let (status, body) = call(&router, "POST", "/api/boom-tx", Some(&tok), Some("{}")).await;
	assert_eq!(status, StatusCode::IM_A_TEAPOT, "{body}");
	assert_eq!(err_code(&body), "E-APP-TEAPOT");
	assert_eq!(body.pointer("/error/errStr").and_then(Json::as_str), Some("no coffee here"));
}

#[tokio::test]
async fn a_script_job_cannot_replace_a_framework_kind() {
	let src = r#"
pub async fn main(app) {
	app.job("SWEEP_JOBS", sweep);
}

pub async fn sweep(ctx, payload) {
	()
}
"#;
	let (_db, builder, _store) =
		install("job-clash", mintworks_auth::routes::consent_gate(), true, src, false).await;
	let Err(e) = builder.unwrap().into_service().await else {
		panic!("a script job overriding SWEEP_JOBS booted");
	};
	assert!(e.to_string().contains("SWEEP_JOBS"), "{e}");
}

#[tokio::test]
async fn a_declared_setting_default_reaches_settings() {
	let src = r#"
pub async fn main(app) {
	app.setting_default("admin.alert_email", "ops@example.test");
}
"#;
	let (_db, app, _router, _store) =
		serve_src("setting-default", mintworks_auth::routes::consent_gate(), true, src, false)
			.await;
	assert_eq!(app.settings.text("admin.alert_email").await.unwrap(), "ops@example.test");
}

#[tokio::test]
async fn script_timeout_ms_from_settings_bounds_a_route() {
	let src = r#"
pub async fn main(app) {
	app.setting_default("script.timeout_ms", "200");
	app.get("/api/nap", slow).public().tier("public");
}

pub async fn slow(ctx, req) {
	nap::nap(400).await;
	Ok(#{})
}
"#;
	let (_db, _app, router, _store) =
		serve_src("limits-timeout", mintworks_auth::routes::consent_gate(), true, src, false).await;
	let (_, body) = call(&router, "GET", "/api/nap", None, None).await;
	assert_eq!(err_code(&body), "E-SCRIPT-TIMEOUT", "{body}");
}

#[tokio::test]
async fn req_headers_reach_the_script_without_credentials() {
	let src = r#"
pub async fn main(app) {
	app.get("/api/headers", headers).public().tier("public");
}

pub async fn headers(ctx, req) {
	let out = req.headers;
	out["(ip)"] = req.ip;
	Ok(out)
}
"#;
	let (_db, app, router, _store) =
		serve_src("req-headers", mintworks_auth::routes::consent_gate(), true, src, false).await;
	let tok = token(&app, ORG_A).await;
	let req = Request::builder()
		.method("GET")
		.uri("/api/headers")
		.header("authorization", format!("Bearer {tok}"))
		.header("cookie", "sid=1")
		.header("x-qr-secret", "s3cret")
		.header("x-api-key", "k")
		.header("x-auth-token", "t")
		.header("x-session-id", "s")
		.header("x-hub-signature-256", "sha256=ab")
		.header("x-jwt-assertion", "j")
		.header("x-access-key", "ak")
		.header("x-csrf-token", "c")
		.header("if-match", "\"v3\"")
		.header("x-custom", "a")
		.header("x-custom", "b")
		.header("x-forwarded-for", "1.2.3.4")
		.header("x-real-ip", "1.2.3.4")
		.header("forwarded", "for=1.2.3.4")
		.extension(axum::extract::ConnectInfo(std::net::SocketAddr::from(([10, 0, 0, 7], 1))))
		.body(Body::empty())
		.unwrap();
	let (status, body) = send(&router, req).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["(ip)"], "10.0.0.7", "the peer, not a spoofed forwarding header");
	assert_eq!(body["if-match"], "\"v3\"");
	assert_eq!(body["x-custom"], "a, b");
	assert_eq!(body["x-hub-signature-256"], "sha256=ab", "a webhook HMAC is visible");
	for hidden in [
		"authorization",
		"cookie",
		"x-qr-secret",
		"x-api-key",
		"x-auth-token",
		"x-session-id",
		"x-csrf-token",
		"x-jwt-assertion",
		"x-access-key",
		"x-forwarded-for",
		"x-real-ip",
		"forwarded",
	] {
		assert!(body.get(hidden).is_none(), "{hidden} leaked: {body}");
	}
}

#[tokio::test]
async fn script_budget_from_settings_bounds_a_route() {
	let src = r#"
pub async fn main(app) {
	app.setting_default("script.budget", "1000");
	app.get("/api/spin", spin).public().tier("public");
}

pub async fn spin(ctx, req) {
	let n = 0;
	while n < 100000 {
		n += 1;
	}
	Ok(#{ n })
}
"#;
	let (_db, _app, router, _store) =
		serve_src("limits-budget", mintworks_auth::routes::consent_gate(), true, src, false).await;
	let (_, body) = call(&router, "GET", "/api/spin", None, None).await;
	assert_eq!(err_code(&body), "E-SCRIPT-BUDGET", "{body}");
}

#[tokio::test]
async fn a_setting_default_for_an_undeclared_key_fails_the_boot() {
	let src = r#"
pub async fn main(app) {
	app.setting_default("no.such_key", "1");
}
"#;
	let (_db, builder, _store) = install(
		"setting-default-unknown",
		mintworks_auth::routes::consent_gate(),
		true,
		src,
		false,
	)
	.await;
	let Err(e) = builder.unwrap().into_service().await else {
		panic!("an undeclared setting default booted");
	};
	assert!(e.to_string().contains("no.such_key"), "{e}");
}

#[tokio::test]
async fn a_setting_default_registered_twice_fails_the_boot() {
	let src = r#"
pub async fn main(app) {
	app.setting_default("admin.alert_email", "a@example.test");
	app.setting_default("admin.alert_email", "a@example.test");
}
"#;
	let (_db, builder, _store) =
		install("setting-default-twice", mintworks_auth::routes::consent_gate(), true, src, false)
			.await;
	let Err(e) = builder.unwrap().into_service().await else {
		panic!("a duplicate setting default booted");
	};
	assert!(e.to_string().contains("registered twice"), "{e}");
}

#[tokio::test]
async fn a_test_env_outside_app_or_declared_twice_fails_the_install() {
	for (name, body, want) in [
		(
			"test-env-twice",
			r#"app.test_env("APP_X", "1"); app.test_env("APP_X", "2");"#,
			"declared twice",
		),
		("test-env-not-app", r#"app.test_env("SELLER_NAME", "x");"#, "APP_"),
	] {
		let src = format!("pub async fn main(app) {{ {body} }}");
		let (_db, builder, _store) =
			install(name, mintworks_auth::routes::consent_gate(), true, &src, true).await;
		let Err(e) = builder else {
			panic!("{name} installed");
		};
		assert!(e.to_string().contains(want), "{e}");
	}
}

#[tokio::test]
async fn a_bad_or_duplicate_account_role_fails_the_install() {
	for (name, body, want) in [
		(
			"account-role-twice",
			r#"app.account_role("teacher"); app.account_role("teacher");"#,
			"declared twice",
		),
		("account-role-bad-name", r#"app.account_role("Teacher");"#, "must match"),
	] {
		let src = format!("pub async fn main(app) {{ {body} }}");
		let (_db, builder, _store) =
			install(name, mintworks_auth::routes::consent_gate(), true, &src, true).await;
		let Err(e) = builder else {
			panic!("{name} installed");
		};
		assert!(e.to_string().contains(want), "{e}");
	}
}

#[tokio::test]
async fn a_test_env_value_reaches_env_get_only_under_the_suite() {
	let src = r#"
pub async fn main(app) {
	app.test_env("APP_MW_TEST_ENV_PROBE", "from-suite");
	app.get("/api/probe", probe).public().tier("public");
}

pub async fn probe(ctx, req) {
	#{ value: env::get("APP_MW_TEST_ENV_PROBE")? }
}
"#;
	// The probe name is unique to this test and never set in the process, so `Null` means the
	// served app did not consult test_env.
	let gate = mintworks_auth::routes::consent_gate;
	let (_db, _app, router, _store) = serve_src("test-env-served", gate(), true, src, false).await;
	let (status, body) = call(&router, "GET", "/api/probe", None, None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["value"], Json::Null);

	let (_db, _app, router, _store) = serve_src("test-env-suite", gate(), true, src, true).await;
	let (status, body) = call(&router, "GET", "/api/probe", None, None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["value"], "from-suite");
}

#[tokio::test]
async fn an_env_scoped_setting_default_follows_deployment_env() {
	let src = r#"
pub async fn main(app) {
	app.setting_default_for("test", "admin.alert_email", "test@example.test");
}
"#;
	let (_db, app, _router, _store) =
		serve_src("setting-default-for", mintworks_auth::routes::consent_gate(), true, src, false)
			.await;
	assert_eq!(app.settings.text("admin.alert_email").await.unwrap(), "");
	app.settings.set("deployment.env", "test", None).await.unwrap();
	assert_eq!(app.settings.text("admin.alert_email").await.unwrap(), "test@example.test");
}

/// Script routes: one public, one authed, one `.scope("x")`, one `.raw()`, one escalating.
/// `auth.authenticated` is mounted only so `mint` can reach `POST /api/api-keys`.
const MATRIX_SRC: &str = r#"
pub async fn main(app) {
	app.mount("auth.authenticated");
	app.get("/api/m/public", ok).public().tier("public");
	app.get("/api/m/authed", ok);
	app.get("/api/m/scoped", ok).scope("x");
	app.post("/api/m/raw", ok).raw();
	app.get("/api/m/escalated", escalated);
	app.get("/api/m/escalated-members", escalated_members);
	app.get("/api/m/escalated-role", escalated_role);
}

pub async fn ok(ctx, req) {
	#{ ok: true }
}

pub async fn escalated(ctx, req) {
	nav::retry(sys::escalate(ctx)?, "inv_01JCZ5X8K9N7QW3M6R2T4V8Y0E").await?
}

pub async fn escalated_members(ctx, req) {
	auth::members(sys::escalate(ctx)?).await?
}

pub async fn escalated_role(ctx, req) {
	let grant = #{ account: "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0E", role: "x", on: true };
	auth::set_account_role(sys::escalate(ctx)?, grant).await?
}
"#;

/// A real key through `POST /api/api-keys` (the session token carries a fresh `auth_at`).
async fn mint(router: &Router, tok: &str, name: &str, scopes: &[&str]) -> String {
	let body = serde_json::json!({ "name": name, "scopes": scopes }).to_string();
	let (status, json) = call(router, "POST", "/api/api-keys", Some(tok), Some(&body)).await;
	assert!(status.is_success(), "minting {name} {scopes:?}: {status} {json}");
	json["key"].as_str().unwrap().to_owned()
}

/// `([METHOD ]path, subject, expected errCode)`; `None` is a 2xx. Subjects: `anon`, `session`, `key_xr`
/// (`x:read`), `key_xw` (`x:write` only), `tampered` (a session token with a broken signature).
const MECH: &[(&str, &str, Option<&str>)] = &[
	("/api/m/public", "anon", None),
	("/api/m/public", "session", None),
	("/api/m/public", "key_xr", None),
	("/api/m/public", "key_xw", None),
	("/api/m/public", "tampered", None),
	("/api/m/authed", "anon", Some("E-AUTH-TOKEN")),
	("/api/m/authed", "session", None),
	("/api/m/authed", "key_xr", Some("E-AUTH-SCOPE")),
	("/api/m/authed", "key_xw", Some("E-AUTH-SCOPE")),
	("/api/m/authed", "tampered", Some("E-AUTH-TOKEN")),
	("/api/m/scoped", "anon", Some("E-AUTH-TOKEN")),
	("/api/m/scoped", "session", None),
	("/api/m/scoped", "key_xw", Some("E-AUTH-SCOPE")),
	("/api/m/scoped", "tampered", Some("E-AUTH-TOKEN")),
	("POST /api/m/raw", "anon", Some("E-AUTH-TOKEN")),
	("POST /api/m/raw", "session", None),
	("POST /api/m/raw", "tampered", Some("E-AUTH-TOKEN")),
	("POST /api/m/raw", "key_xw", Some("E-AUTH-SCOPE")),
	("/api/m/escalated", "session", Some("E-SCRIPT-INIT-ONLY")),
	("/api/m/escalated-members", "session", Some("E-SCRIPT-INIT-ONLY")),
	("/api/m/escalated-role", "session", Some("E-SCRIPT-INIT-ONLY")),
];

/// The intended policy where the server is known to disagree: `consent::gate` reads `Claims`,
/// which a key request lacks, so a correctly scoped key gets `E-AUTH-TOKEN`.
const MECH_RED: &[(&str, &str, Option<&str>)] = &[("/api/m/scoped", "key_xr", None)];

async fn run_mech(name: &str, rows: &[(&str, &str, Option<&str>)]) {
	let (_db, app, router, _store) =
		serve_src(name, mintworks_auth::routes::consent_gate(), true, MATRIX_SRC, false).await;
	let session = token(&app, ORG_A).await;
	let key_xr = mint(&router, &session, "xr", &["x:read"]).await;
	let key_xw = mint(&router, &session, "xw", &["x:write"]).await;
	let (head, sig) = session.rsplit_once('.').unwrap();
	let flipped = if sig.starts_with('A') { 'B' } else { 'A' };
	let tampered = format!("{head}.{flipped}{}", &sig[1..]);

	let mut fails = Vec::new();
	for &(path, subject, expect) in rows {
		let tok = match subject {
			"anon" => None,
			"session" => Some(session.as_str()),
			"key_xr" => Some(key_xr.as_str()),
			"key_xw" => Some(key_xw.as_str()),
			"tampered" => Some(tampered.as_str()),
			other => panic!("unknown subject {other}"),
		};
		let (method, path) = path.split_once(' ').unwrap_or(("GET", path));
		let (status, body) = call(&router, method, path, tok, None).await;
		let ok = match expect {
			None => status.is_success(),
			Some(code) => err_code(&body) == code,
		};
		if !ok {
			fails.push(format!("{path} as {subject}: expected {expect:?}, got {status} {body}"));
		}
	}
	assert!(fails.is_empty(), "{}", fails.join("\n"));
}

#[tokio::test]
async fn script_route_mechanism_rows() {
	run_mech("mech", MECH).await;
}

#[tokio::test]
async fn script_route_mechanism_rows_red() {
	run_mech("mech-red", MECH_RED).await;
}

// vim: ts=4
