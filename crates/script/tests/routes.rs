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
	app.get("/api/notes/{key}", note);
	app.post("/api/echo", echo);
	app.post("/api/boom", boom);
	app.post("/api/boom-tx", boom_tx);
	app.get("/api/sudo", sudo);
	app.get("/api/files", files);
	app.table("ledger", #{ cols: #{ uid: "TEXT PRIMARY KEY", total: "INTEGER NOT NULL" } });
	app.get("/api/ledger", ledger);
	app.get("/api/framework-table", framework_table);
	app.post("/api/crossing", crossing);
	app.post("/api/nested", nested);
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

pub async fn sudo(ctx, req) {
	#{ actor: sys::escalate(ctx)?.actor() }
}

pub async fn boom(ctx, req) {
	Err(err::app(418, "E-APP-TEAPOT", "no coffee here")?)
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

	let builder = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		.settings(mintworks_auth::SETTINGS)
		.secrets(mintworks_auth::SECRETS)
		.extension(Arc::new(store.clone()) as Arc<dyn AuthStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn InvoiceStore>);

	let script = ScriptApp::new(
		vec![("t.rn".to_owned(), src.to_owned())],
		Arc::new(store.clone()) as Arc<dyn ObjectStore>,
		db.0.clone(),
	)
	.gate(gate)
	.tx_hook(Arc::new(TestTxHook(store.clone())))
	.app_db(Arc::new(SqliteAppDb::new(db.0.join("app.db"))));

	let builder = if suite {
		script.install_tests(builder, |b, _| Ok(b)).await.map(|(b, _, _)| b)
	} else {
		script.install(builder, |b, _| Ok(b)).await
	};
	(db, builder, store)
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

/// Three script routes: one public, one authed, one `.scope("x")`. `auth.authenticated` is
/// mounted only so `mint` can reach `POST /api/api-keys`.
const MATRIX_SRC: &str = r#"
pub async fn main(app) {
	app.mount("auth.authenticated");
	app.get("/api/m/public", ok).public().tier("public");
	app.get("/api/m/authed", ok);
	app.get("/api/m/scoped", ok).scope("x");
}

pub async fn ok(ctx, req) {
	#{ ok: true }
}
"#;

/// A real key through `POST /api/api-keys` (the session token carries a fresh `auth_at`).
async fn mint(router: &Router, tok: &str, name: &str, scopes: &[&str]) -> String {
	let body = serde_json::json!({ "name": name, "scopes": scopes }).to_string();
	let (status, json) = call(router, "POST", "/api/api-keys", Some(tok), Some(&body)).await;
	assert!(status.is_success(), "minting {name} {scopes:?}: {status} {json}");
	json["key"].as_str().unwrap().to_owned()
}

/// `(path, subject, expected errCode)`; `None` is a 2xx. Subjects: `anon`, `session`, `key_xr`
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
		let (status, body) = call(&router, "GET", path, tok, None).await;
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
#[ignore = "access-matrix: red until the fix plan un-ignores it (claude-docs/access-matrix-design.md §11)"]
async fn script_route_mechanism_rows_red() {
	run_mech("mech-red", MECH_RED).await;
}

// vim: ts=4
