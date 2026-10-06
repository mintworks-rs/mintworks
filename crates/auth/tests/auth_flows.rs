// SPDX-License-Identifier: MPL-2.0
//! `mintworks-auth`'s system behaviour, end to end against a real store: the credential paths that
//! used to skip the checks their siblings apply — password reset handing out a session without
//! the second factor, reset and change accepting any password at all, step-up brute-forceable
//! at line rate — and the org, membership, consent and GDPR paths around them.
//!
//! Every test here reaches `mintworks-auth` through the [`Auth`] service handle, or — where what is
//! under test *is* a layer (the consent gate, the middleware tiers, the `Set-Cookie` only
//! `token::respond` writes) — through a mounted route bundle. Nothing calls a handler or a
//! free function, which is what lets this crate's modules stay `pub(crate)`.
//!
//! It takes `mintworks-store-sqlite` as a **dev**-dependency: Cargo permits the cycle, and a
//! `tests/` integration test compiles against the crate's normal build, so the adapter's trait
//! impls unify. (An inline `#[cfg(test)] mod tests` would not — that is a distinct crate.)

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use mintworks_auth::consent::PublishLegalDoc;
use mintworks_auth::service_api::{Auth, ConsentGrant, Credentials, LoginOutcome, Registration};
use mintworks_auth::store::{
	AccountStatus, AuthStore, NewAccount, NewApiKey, NewTotpCredential, NewWebauthnCredential,
	OrgKind, Role,
};
use mintworks_auth::store::{LegalKind, NewConsent, NewLegalDoc};
use mintworks_auth::{pow, register};
use mintworks_core::account_data::AccountDataHook;
use mintworks_core::app::RouterScopeExt;
use mintworks_core::auth_mw::ClientIp;
use mintworks_core::ctx::{Actor, Ctx};
use mintworks_core::objects::ObjectStore;
use mintworks_core::store::CoreStore;
use mintworks_core::{App, AppBuilder, config::Config, prelude::*};
use mintworks_store_sqlite::SqliteStore;
use sha2::{Digest, Sha256};

const PASSWORD: &str = "correct-horse-battery";
const PEER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 4242);

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, so the two pools must be over a file.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-auth-crate-flow-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [0; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: self.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: "https://app.example".to_owned(),
			jobs_workers: None,
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// `mintworks-invoice`'s tables come along because `orgs.billing_currency` references
/// `currencies(code)`, which `schema.rs`'s `INVOICE` block seeds.
async fn setup(db: &TmpDb) -> (App, SqliteStore) {
	setup_with(db, AppBuilder::new()).await
}

/// [`setup`] over a builder the caller has already added to.
async fn setup_with(db: &TmpDb, builder: AppBuilder) -> (App, SqliteStore) {
	let store = SqliteStore::open(&db.config()).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let auth: Arc<dyn AuthStore> = Arc::new(store.clone());
	let app = builder
		.config(db.config())
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		.settings(mintworks_auth::SETTINGS)
		// One test reads `currency.base` to prove an uncached setting does not wait on the
		// write connection; the key belongs to `mintworks-invoice`, a dev-dependency here.
		.settings(mintworks_invoice::SETTINGS)
		.extension(auth)
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_core::refs::RefStore>)
		.build()
		.await
		.unwrap();
	// The proof of work is not what any of this is testing, and 2^18 hashes per token is
	// pure test latency.
	app.settings.set("pow.difficulty.password-reset", "1", None).await.unwrap();
	(app, store)
}

/// An activated account with a real argon2 hash — `verify_password` parses it, so a
/// placeholder would be an internal error rather than a wrong password.
async fn account(store: &SqliteStore, email: &str) -> mintworks_auth::store::Account {
	let hash = register::hash_password(PASSWORD.to_owned()).await.unwrap();
	let (account, _) = store
		.create_account(
			&NewAccount {
				email: email.to_owned(),
				pwd_hash: Some(hash),
				name: None,
				locale: "hu".to_owned(),
				org_name: email.to_owned(),
			},
			&[],
		)
		.await
		.unwrap();
	store.set_account_status(account.id, AccountStatus::Active).await.unwrap();
	// `activate_account` is what stamps this in production, and it is the evidence
	// `set_account_status` now reads: an ACTIVE row with it NULL cannot exist outside a test.
	sqlx::query("UPDATE accounts SET activated_at = ? WHERE id = ?")
		.bind(Timestamp::now().0)
		.bind(account.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	store.account_by_id(account.id).await.unwrap().unwrap()
}

/// Undo what [`account`] stamps, for the tests that need a never-activated row.
async fn unactivate(store: &SqliteStore, account_id: i64) {
	sqlx::query("UPDATE accounts SET activated_at = NULL WHERE id = ?")
		.bind(account_id)
		.execute(store.write_pool())
		.await
		.unwrap();
}

/// A confirmed second factor. The secret is never checked here — only that its presence
/// forces the challenge.
async fn enrol_totp(store: &SqliteStore, account_id: i64) {
	store
		.put_totp(&NewTotpCredential {
			account_id,
			secret_nonce: vec![0; 12],
			secret_enc: vec![0; 32],
			digits: 6,
			period: 30,
			recovery_hashes: "[]".to_owned(),
		})
		.await
		.unwrap();
	store.confirm_totp(account_id, Timestamp::now(), "[]").await.unwrap();
}

/// A confirmed second factor whose secret is known here, so a *valid* code can be presented.
/// [`enrol_totp`]'s placeholder ciphertext cannot be decrypted, which is fine while only the
/// credential's presence matters and useless once a code has to verify.
///
/// Returns the raw secret for [`totp_code`].
async fn enrol_real_totp(app: &App, account: &mintworks_auth::store::Account) -> Vec<u8> {
	let auth = Auth::new(app.clone());
	let enrolment = auth.enrol_totp(&ctx_for(account)).await.unwrap();
	let secret = base32_decode(&enrolment.secret);
	auth.confirm_totp(&ctx_for(account), &totp_code(&secret, 0)).await.unwrap();
	secret
}

/// RFC 4648 base32, unpadded — the inverse of `mintworks_auth::totp`'s encoder.
fn base32_decode(s: &str) -> Vec<u8> {
	const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
	let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
	for c in s.bytes() {
		let v = ALPHABET.iter().position(|a| *a == c).expect("not base32");
		acc = (acc << 5) | u32::try_from(v).unwrap();
		bits += 5;
		if bits >= 8 {
			bits -= 8;
			out.push(u8::try_from((acc >> bits) & 0xff).unwrap());
		}
	}
	out
}

/// RFC 6238 over HMAC-SHA256 at the current 30 s step plus `delta`, mirroring `totp::code_at`.
/// Six digits, which is `totp::DIGITS`.
///
/// `delta` matters: `advance_totp_step` is the replay guard and refuses a step that is not
/// *greater* than the stored one, so a second code inside the same window is rejected however
/// correct it is. `SKEW_STEPS` is 1, so the next step is both accepted and unspent.
fn totp_code(secret: &[u8], delta: i64) -> String {
	use hmac::Mac;
	let step = Timestamp::now().0 / 30 + delta;
	let mut mac = <hmac::Hmac<Sha256> as Mac>::new_from_slice(secret).unwrap();
	mac.update(&step.to_be_bytes());
	let tag = mac.finalize().into_bytes();
	let off = usize::from(tag[tag.len() - 1] & 0x0f);
	let bin = u32::from_be_bytes([tag[off] & 0x7f, tag[off + 1], tag[off + 2], tag[off + 3]]);
	format!("{:06}", bin % 1_000_000)
}

/// `leading_zero_bits(SHA256(salt || nonce)) >= difficulty`, the preimage exactly as
/// `pow::solved` builds it.
fn solve(challenge: &pow::Challenge) -> pow::Proof {
	serde_json::from_value(solve_json(challenge)).unwrap()
}

/// The same proof as a request body carries it. `pow::Proof` is `Deserialize` only, so a
/// router test cannot serialize one back out.
fn solve_json(challenge: &pow::Challenge) -> serde_json::Value {
	// Bounded rather than `(0u64..)`: at difficulty 1 this finds one within a handful.
	let nonce = (0u64..1_000_000)
		.find(|n| {
			let mut h = Sha256::new();
			h.update(challenge.salt.as_bytes());
			h.update(n.to_string().as_bytes());
			let digest = h.finalize();
			let mut bits = 0i64;
			for byte in digest {
				bits += i64::from(byte.leading_zeros());
				if byte != 0 {
					break;
				}
			}
			bits >= challenge.difficulty
		})
		.unwrap();
	serde_json::json!({
		"salt": challenge.salt,
		"exp": challenge.exp,
		"sig": challenge.sig,
		"nonce": nonce,
	})
}

/// Drives the real flow rather than minting a token behind the handle's back: request a
/// reset, then read the link out of the queued mail.
async fn reset_token(app: &App, store: &SqliteStore, email: &str) -> String {
	let challenge = pow::issue(app, "password-reset").await.unwrap();
	Auth::new(app.clone())
		.request_password_reset(&ip_ctx(), email, Some(&solve(&challenge)))
		.await
		.unwrap();

	link_from_queued_mail(app, store, "reset_link").await
}

/// The token is minted when the `AUTH_LINK_EMAIL` job *renders*, not when it is queued — the
/// whole point of `mintworks_auth::job` is that `jobs.payload` never holds a live link. So the
/// test drives the same render the handle drives, rather than reaching behind it.
async fn link_from_queued_mail(app: &App, store: &SqliteStore, var: &str) -> String {
	let payload: String = sqlx::query_scalar(
		"SELECT payload FROM jobs WHERE kind = 'AUTH_LINK_EMAIL' ORDER BY id DESC LIMIT 1",
	)
	.fetch_one(store.read_pool())
	.await
	.expect("the mail was never queued");
	assert!(!payload.contains("token="), "a live link is sitting in jobs.payload: {payload}");

	let mail: mintworks_auth::job::LinkMail = serde_json::from_str(&payload).unwrap();
	let rendered = mintworks_auth::job::render(app, &mail).await.unwrap();
	let link = rendered.vars[var].as_str().expect("the mail carries no link");
	link.split_once("token=").expect("the mail carries no token").1.to_owned()
}

async fn parts(resp: Response) -> (StatusCode, serde_json::Value) {
	let status = resp.status();
	let bytes = resp.into_body().collect().await.unwrap().to_bytes();
	(status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// The `Ctx` an unauthenticated request off a socket arrives with.
fn ip_ctx() -> Ctx {
	Ctx::system("test").with_ip(PEER.ip())
}

fn ctx_for(account: &mintworks_auth::store::Account) -> Ctx {
	Ctx {
		actor: Actor::User { account_id: account.id },
		org_id: None,
		ip: None,
		auth_at: Some(Timestamp::now().0),
		request_id: String::new(),
		on_behalf_of: None,
	}
}

/// The same actor, whose credential was presented well outside `auth.stepup_window`.
fn stale_ctx(account: &mintworks_auth::store::Account) -> Ctx {
	Ctx { auth_at: Some(Timestamp::now().0 - 1_000), ..ctx_for(account) }
}

async fn credential(store: &SqliteStore, id: i64) -> (Option<String>, i64) {
	sqlx::query_as("SELECT pwd_hash, token_epoch FROM accounts WHERE id = ?")
		.bind(id)
		.fetch_one(store.read_pool())
		.await
		.unwrap()
}

/// The unverified payload of a JWT. Enough to see which claims were minted; the signature
/// is not what this file is testing.
fn jwt_payload(token: &str) -> serde_json::Value {
	use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
	let payload = token.split('.').nth(1).expect("a three-segment JWT");
	serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap()
}

fn credentials(email: &str, password: &str) -> Credentials {
	Credentials { email: email.to_owned(), password: password.to_owned(), pow: None }
}

/// A real access token for a one-factor account, minted through the handle.
async fn access_token(app: &App, email: &str) -> String {
	match Auth::new(app.clone())
		.login(&Ctx::system("test"), &credentials(email, PASSWORD))
		.await
		.unwrap()
	{
		LoginOutcome::Signed(t) => t.access_token,
		LoginOutcome::TotpRequired { .. } => panic!("a one-factor account was expected"),
	}
}

async fn call(
	router: &axum::Router,
	method: &str,
	uri: &str,
	token: &str,
	body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
	let req = axum::http::Request::builder()
		.method(method)
		.uri(uri)
		.header("authorization", format!("Bearer {token}"))
		.header("content-type", "application/json")
		.body(axum::body::Body::from(body.unwrap_or(serde_json::Value::Null).to_string()))
		.unwrap();
	parts(tower::ServiceExt::oneshot(router.clone(), req).await.unwrap()).await
}

/// `register` enforces a minimum length; the two routes that also set a password went
/// straight to `hash_password`, so the whole policy was optional for anyone who used them.
#[tokio::test]
async fn neither_reset_nor_change_accepts_a_one_character_password() {
	let db = TmpDb::new("password-policy");
	let (app, store) = setup(&db).await;
	let account = account(&store, "short@e.st").await;
	let auth = Auth::new(app.clone());

	let token = reset_token(&app, &store, "short@e.st").await;
	let err = auth
		.reset_password(&ip_ctx(), &token, None, None, "x".to_owned())
		.await
		.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::BAD_REQUEST);

	let err = auth
		.change_password(&ctx_for(&account), PASSWORD.to_owned(), None, "x".to_owned())
		.await
		.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::BAD_REQUEST);

	// And neither call got as far as writing a password.
	let fresh = store.account_by_id(account.id).await.unwrap().unwrap();
	assert_eq!(fresh.pwd_hash, account.pwd_hash);
}

/// Step-up takes the same two credentials as login and had none of login's gates, which made
/// it the cheapest place in the API to grind either of them.
#[tokio::test]
async fn step_up_is_rate_limited_and_its_failures_are_counted() {
	let db = TmpDb::new("stepup-limit");
	let (app, store) = setup(&db).await;
	let account = account(&store, "grind@e.st").await;

	// The budget is a `scoped_account_mw` layer on the route, so this has to drive the
	// router: the handle charges nothing.
	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "grind@e.st").await;
	let wrong = || {
		call(
			&router,
			"POST",
			"/api/auth/step-up",
			&token,
			Some(serde_json::json!({ "password": "not-it" })),
		)
	};

	// `step_up` is 5/5min, keyed on the account uid the layer reads off `Claims`.
	for attempt in 1..=5 {
		let (status, _) = wrong().await;
		assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
	}
	let (status, body) = wrong().await;
	assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
	assert_eq!(body["error"]["errCode"], "E-CORE-RATELIMIT");

	let failures: i64 = sqlx::query_scalar("SELECT failed_logins FROM accounts WHERE id = ?")
		.bind(account.id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(failures, 5, "a failure here has to feed the same counter as a failed login");
}

/// Only `DELETE /api/auth/totp` was step-up. An attacker with a 15-minute stolen access
/// token on an account with no second factor could enrol their own authenticator and confirm
/// it, turning a transient token into a permanent lockout of the legitimate owner.
#[tokio::test]
async fn enrolling_a_second_factor_needs_a_fresh_credential() {
	let db = TmpDb::new("totp-stepup");
	let (app, store) = setup(&db).await;
	let account = account(&store, "enrol@e.st").await;
	let auth = Auth::new(app.clone());

	// `auth.stepup_window` defaults to 300s, and `stale_ctx` is 1000s old.
	for outcome in [
		auth.enrol_totp(&stale_ctx(&account)).await.map(|_| ()),
		auth.confirm_totp(&stale_ctx(&account), "000000").await.map(|_| ()),
	] {
		let err = outcome.expect_err("a stale credential must not arm a second factor");
		assert_eq!(err.parts(), (StatusCode::UNAUTHORIZED, "E-AUTH-STEPUP"));
	}

	// A fresh one enrols as before.
	assert!(auth.enrol_totp(&ctx_for(&account)).await.is_ok());
}

/// The `audit_logs.action` column comment names `AUDIT_EXPORT`, and nothing ever wrote one.
/// Removing the second factor — the exact move an attacker with a stolen token makes — was
/// equally invisible.
#[tokio::test]
async fn an_export_and_a_totp_removal_each_leave_an_audit_row() {
	let db = TmpDb::new("audit-rows");
	let (app, store) = setup(&db).await;
	let account = account(&store, "audit@e.st").await;
	let auth = Auth::new(app.clone());

	// The `Ctx` `auth_mw` would have inserted: the audit row's `account_id` comes off the
	// actor, so a `Ctx::system` here would prove nothing about the real path.
	auth.export_account(&ctx_for(&account)).await.unwrap();

	enrol_totp(&store, account.id).await;
	auth.remove_totp(&ctx_for(&account)).await.unwrap();

	let actions: Vec<(String, i64)> = sqlx::query_as(
		"SELECT action, account_id FROM audit_logs WHERE entity = 'account' ORDER BY id",
	)
	.fetch_all(store.read_pool())
	.await
	.unwrap();
	assert_eq!(
		actions,
		vec![("AUDIT_EXPORT".to_owned(), account.id), ("TOTP_REMOVED".to_owned(), account.id)]
	);
}

/// Records every call, so a test can see the hook ran and with which account.
#[derive(Default)]
struct RecordingHook(parking_lot::Mutex<Vec<String>>);

#[async_trait::async_trait]
impl AccountDataHook for RecordingHook {
	fn name(&self) -> &'static str {
		"recorder"
	}

	async fn export(&self, acc: &AccountId) -> ClResult<serde_json::Value> {
		self.0.lock().push(format!("export {acc}"));
		Ok(serde_json::json!({ "notes": 2 }))
	}

	async fn erase(&self, acc: &AccountId) -> ClResult<()> {
		self.0.lock().push(format!("erase {acc}"));
		Ok(())
	}

	async fn org_deleted(&self, org: &OrgId) -> ClResult<()> {
		self.0.lock().push(format!("org_deleted {org}"));
		Ok(())
	}
}

#[tokio::test]
async fn account_data_hooks_see_export_and_erase() {
	let db = TmpDb::new("account-data-hook");
	let hook = Arc::new(RecordingHook::default());
	let (app, store) = setup_with(&db, AppBuilder::new().account_data_hook(hook.clone())).await;
	let account = account(&store, "hook@e.st").await;
	let auth = Auth::new(app.clone());

	let (_, doc) = auth.export_account(&ctx_for(&account)).await.unwrap();
	assert_eq!(doc["recorder"], serde_json::json!({ "notes": 2 }));
	assert!(doc.get("accounts").is_some(), "the framework sections are still there");

	auth.erase_account(&ctx_for(&account), &account.email).await.unwrap();
	assert_eq!(
		*hook.0.lock(),
		vec![format!("export {}", account.uid), format!("erase {}", account.uid)]
	);
}

/// Erasure blanked `agent_runs.spec` but left the run's events — prompts, deltas, tool
/// arguments — replayable by every member of the org.
#[tokio::test]
async fn erasure_deletes_the_accounts_agent_run_events() {
	let db = TmpDb::new("erase-agent-runs");
	let (app, store) = setup(&db).await;
	let account = account(&store, "runner@e.st").await;
	let run_id: i64 = sqlx::query_scalar(
		"INSERT INTO agent_runs (uid, thread_uid, org_id, account_id, role, spec, status, error,
			created_at)
		 VALUES ('run_x', 'thr_x', 1, ?, 'USER', '{\"input\":\"secret\"}', 'error', 'boom', 0)
		 RETURNING id",
	)
	.bind(account.id)
	.fetch_one(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO agent_run_events (run_id, seq, kind, payload, at)
		 VALUES (?, 1, 'delta', '{\"text\":\"secret\"}', 0)",
	)
	.bind(run_id)
	.execute(store.write_pool())
	.await
	.unwrap();
	let auth = Auth::new(app.clone());

	let (_, doc) = auth.export_account(&ctx_for(&account)).await.unwrap();
	let runs = doc["agentRuns"].as_array().unwrap();
	assert_eq!(runs.len(), 1);
	assert_eq!(runs[0]["uid"], "run_x");

	auth.erase_account(&ctx_for(&account), &account.email).await.unwrap();
	let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
		.bind(run_id)
		.fetch_one(store.write_pool())
		.await
		.unwrap();
	assert_eq!(events, 0);
	let (spec, error, acc): (String, Option<String>, Option<i64>) =
		sqlx::query_as("SELECT spec, error, account_id FROM agent_runs WHERE id = ?")
			.bind(run_id)
			.fetch_one(store.write_pool())
			.await
			.unwrap();
	assert_eq!((spec.as_str(), error, acc), ("{}", None, None));
}

/// `password_reset` buckets on the request body's address, and the bucket used to be created
/// before the proof of work was checked — so one request with a random email put a bucket in
/// the process-local map for free, from a single IP, with nothing throttling it (this route has
/// no IP bucket by design, so a shared office cannot be locked out of resets).
///
/// Behaviourally: a rejected request must spend none of the address's 3/h budget.
#[tokio::test]
async fn a_reset_request_with_a_bad_proof_of_work_spends_no_budget() {
	let db = TmpDb::new("reset-pow-order");
	let (app, store) = setup(&db).await;
	let account = account(&store, "budget@e.st").await;
	let auth = Auth::new(app.clone());

	let garbage: pow::Proof = serde_json::from_value(serde_json::json!({
		"salt": "nope",
		"exp": "2099-01-01T00:00:00Z",
		"sig": "00",
		"nonce": 0,
	}))
	.unwrap();

	// More than the whole 3/h budget for that address, all rejected before the limiter.
	for _ in 0..5 {
		assert!(
			auth.request_password_reset(&ip_ctx(), &account.email, Some(&garbage))
				.await
				.is_err(),
			"a garbage proof of work must be refused"
		);
	}

	// The budget is untouched, so a genuine request still goes through and mails the link.
	let token = reset_token(&app, &store, &account.email).await;
	assert!(!token.is_empty());
}

/// Step-up carries the org the caller is *working in*, never the default `pick_org`
/// would choose. With two organisations that default is the personal org, so a re-pick
/// would hand someone working in org B a token scoped to their personal one — and the
/// destructive operation step-up was gating would then run against the wrong org.
#[tokio::test]
async fn step_up_keeps_the_org_the_caller_is_working_in() {
	let db = TmpDb::new("stepup-keeps-org");
	let (app, store) = setup(&db).await;
	let account = account(&store, "two-orgs@e.st").await;

	let mut orgs = Vec::new();
	for name in ["Org A Kft.", "Org B Kft."] {
		let org = store
			.create_org(
				mintworks_auth::store::OrgKind::Shared,
				store.root_org_id().await.unwrap(),
				name,
				account.id,
				None,
			)
			.await
			.unwrap();
		store.accept_membership(org.id, account.id, Timestamp::now()).await.unwrap();
		orgs.push(org);
	}
	let b = &orgs[1];
	let out = Auth::new(app.clone())
		.step_up(&ctx_for(&account).with_org(b.id), Some(PASSWORD.to_owned()), None, None)
		.await
		.unwrap();
	assert_eq!(
		jwt_payload(&out.access_token).get("org").and_then(|v| v.as_str()),
		Some(b.uid.as_str()),
		"stepping up re-picked a default instead of carrying the active org"
	);
}

/// `ensure_usable` ran *after* the password verify, so a wrong password answered
/// `401 E-AUTH-CREDENTIALS` and the correct one `403 E-AUTH-SUSPENDED` — confirming, at the
/// `login.ip` rate, a password very likely reused elsewhere. The locked case two lines above
/// was collapsed into `bad_credentials` for exactly this reason; this one was missed.
///
/// Suspension is still discoverable, just not from an unauthenticated guess: `auth_mw` admits
/// a suspended account and answers `E-AUTH-SUSPENDED` on any authenticated request.
#[tokio::test]
async fn a_suspended_account_answers_the_same_whatever_the_password_was() {
	let db = TmpDb::new("suspended-oracle");
	let (app, store) = setup(&db).await;
	// 20 is the ceiling `auth.pow_after_failures` accepts (the `AUTH_FAILED` bucket's own
	// limit), and this test makes two attempts — so the PoW gate never fires.
	app.settings.set("auth.pow_after_failures", "20", None).await.unwrap();
	let account = account(&store, "suspended@e.st").await;
	sqlx::query("UPDATE accounts SET status = 'SUSPENDED' WHERE id = ?")
		.bind(account.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let auth = Auth::new(app.clone());

	let attempt = |password: &str| {
		let (auth, creds) = (auth.clone(), credentials(&account.email, password));
		async move { auth.login(&ip_ctx(), &creds).await }
	};

	let (wrong_status, wrong_body) =
		parts(attempt("not-it").await.unwrap_err().into_response()).await;
	let (right_status, right_body) =
		parts(attempt(PASSWORD).await.unwrap_err().into_response()).await;
	assert_eq!(right_status, StatusCode::UNAUTHORIZED, "403 E-AUTH-SUSPENDED confirmed the guess");
	assert_eq!(right_status, wrong_status);
	assert_eq!(right_body["error"]["errCode"], "E-AUTH-CREDENTIALS");
	assert_eq!(right_body["error"], wrong_body["error"]);
}

/// The proof-of-work gate used to read `accounts.failed_logins`, which an attacker
/// drives. Three bad passwords against an address and then a fourth: a registered address
/// answered `E-CORE-POW`, an unknown one `E-AUTH-CREDENTIALS`. The counter survives until a
/// successful login, so the probe was repeatable and cost four requests.
///
/// The gate reads the IP-keyed `auth.failed` counter now, which says nothing about the
/// submitted address at all — so each address is driven from its own address here, and the two
/// runs must be indistinguishable.
#[tokio::test]
async fn the_proof_of_work_gate_does_not_say_whether_the_address_exists() {
	let db = TmpDb::new("pow-oracle");
	let (app, store) = setup(&db).await;
	app.settings.set("auth.pow_after_failures", "2", None).await.unwrap();
	let account = account(&store, "known@e.st").await;

	let attempt = |email: String, ip: std::net::IpAddr| {
		let app = app.clone();
		async move {
			let out = Auth::new(app.clone())
				.login(&Ctx::system("test").with_ip(ip), &credentials(&email, "not-the-password"))
				.await;
			// What `auth_mw::run_public` charges on the 401 this produces. These tests call
			// the handle directly, so no layer runs and the charge is made by hand.
			if out.is_err() {
				let key = mintworks_core::ratelimit::bucket_key(ip);
				let _ = app
					.limits
					.check(&app.settings, mintworks_core::ratelimit::AUTH_FAILED, &key)
					.await;
			}
			out
		}
	};

	let elsewhere = std::net::IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 7));
	let mut answers = Vec::new();
	for (email, ip) in [(account.email.clone(), PEER.ip()), ("nobody@e.st".to_owned(), elsewhere)] {
		// Two failures each, then the request the gate is armed for — with no `pow` member.
		for _ in 0..3 {
			let err = attempt(email.clone(), ip).await.unwrap_err();
			answers.push(parts(err.into_response()).await);
		}
	}

	let (known, unknown) = answers.split_at(3);
	for (i, ((ks, kb), (us, ub))) in known.iter().zip(unknown).enumerate() {
		assert_eq!(ks, us, "attempt {i}: status differs");
		assert_eq!(kb["error"]["errCode"], ub["error"]["errCode"], "attempt {i}: {kb} vs {ub}");
	}
	// And the gate really did fire, so this is not passing by never arming.
	assert_eq!(known[2].1["error"]["errCode"], "E-CORE-POW", "{:?}", known[2]);
	assert_eq!(known[2].0, StatusCode::BAD_REQUEST, "E-CORE-POW is 400");
}

/// `login.email` buckets on the *caller-supplied* address, and it used to be charged
/// **before** the proof-of-work gate — so one request naming any address drained that
/// address's budget for free, and anyone knowing a victim's address could hold them at 429
/// indefinitely. Behind the gate each further drain costs the caller a solve;
/// `request_password_reset` states the same rule and explains why.
#[tokio::test]
async fn an_unsolved_login_does_not_drain_the_victims_address_budget() {
	let db = TmpDb::new("login-email-bucket-order");
	let (app, store) = setup(&db).await;
	app.settings.set("pow.difficulty.login", "1", None).await.unwrap();
	let account = account(&store, "victim2@e.st").await;

	// Arm the gate the way a parked attacker does: the IP-keyed `auth.failed` counter.
	// Charged by hand, because these tests call the handles and so pass through no layer.
	let key = mintworks_core::ratelimit::bucket_key(PEER.ip());
	let threshold = app.settings.int("auth.pow_after_failures").await.unwrap();
	for _ in 0..threshold {
		let _ = app
			.limits
			.check(&app.settings, mintworks_core::ratelimit::AUTH_FAILED, &key)
			.await;
	}

	let auth = Auth::new(app.clone());
	let creds =
		|pow| Credentials { email: account.email.clone(), password: PASSWORD.to_owned(), pow };

	// `login.email` is 10/5min. Far more unsolved attempts than that, none of which may
	// spend a token — the request never gets past the gate.
	for n in 0..30 {
		let err = auth.login(&ip_ctx(), &creds(None)).await.unwrap_err();
		assert_eq!(err.parts().1, "E-CORE-POW", "attempt {n}: {err:?}");
	}

	// The owner's budget is untouched: one solve and they are in.
	let challenge = pow::issue(&app, "login").await.unwrap();
	assert!(
		matches!(
			auth.login(&ip_ctx(), &creds(Some(solve(&challenge)))).await.unwrap(),
			LoginOutcome::Signed(_)
		),
		"unsolved requests drained the address's login budget"
	);
}

/// The lockout itself. `login.email` keys on the address in the request body, so anyone can
/// drain a known victim's bucket and — refilling at one token per 30 s — hold it at zero with
/// two requests a minute, forever. A 429 there was a lasting account lockout aimed by a
/// stranger. The bucket still counts; exhaustion now demands a proof instead of refusing.
#[tokio::test]
async fn a_drained_address_budget_asks_for_work_instead_of_locking_the_account() {
	let db = TmpDb::new("login-email-lockout");
	let (app, store) = setup(&db).await;
	app.settings.set("pow.difficulty.login", "1", None).await.unwrap();
	let account = account(&store, "held-at-zero@e.st").await;
	let auth = Auth::new(app.clone());

	// What an attacker on some other address leaves behind: `login.email` is 10/5min, keyed on
	// the submitted address alone, so draining it takes no credential and no account.
	for _ in 0..10 {
		app.limits.check(&app.settings, "login.email", &account.email).await.unwrap();
	}
	assert!(
		app.limits.check(&app.settings, "login.email", &account.email).await.is_err(),
		"the bucket must be drained for this to test anything"
	);

	// The owner, from an address of their own — so the IP-keyed `auth.failed` gate is not
	// armed and the drained bucket is the only thing standing in the way. It asks for work.
	let owner =
		Ctx::system("test").with_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 9)));
	let creds =
		|pow| Credentials { email: account.email.clone(), password: PASSWORD.to_owned(), pow };
	let err = auth.login(&owner, &creds(None)).await.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::BAD_REQUEST, "E-CORE-POW"), "{err:?}");

	// And one solve gets them in. This is `E-CORE-RATELIMIT` forever on the old code.
	let challenge = pow::issue(&app, "login").await.unwrap();
	assert!(
		matches!(
			auth.login(&owner, &creds(Some(solve(&challenge)))).await.unwrap(),
			LoginOutcome::Signed(_)
		),
		"a stranger draining the address budget locked the owner out of their own account"
	);
}

/// The half of the same lockout that survived fixing `Auth::login`: `login_totp` charged
/// `login.email` too, so an attacker draining it at the password stage still blocked a 2FA
/// user at the *second* step — the lockout persisted for exactly the users who enabled a
/// second factor. Draining the login-side second-factor budget then locked the account out of
/// its own password reset, and whoever holds the password can drain that one at
/// `POST /api/auth/login/totp`, which is exactly the attacker 2FA defends against.
///
/// `login.totp.account` is its own budget, keyed on the account uid and reachable only with a
/// valid ticket, and the reset path draws from `reset.totp.account`.
#[tokio::test]
async fn a_drained_budget_does_not_block_the_step_beyond_it() {
	let db = TmpDb::new("drained-budget-does-not-cascade");
	let (app, store) = setup(&db).await;
	app.settings.set("pow.difficulty.login", "1", None).await.unwrap();
	let held = account(&store, "twofactor-held@e.st").await;
	let victim = account(&store, "reset-held@e.st").await;
	let secret = enrol_real_totp(&app, &held).await;
	let victim_secret = enrol_real_totp(&app, &victim).await;
	let auth = Auth::new(app.clone());
	let owner =
		Ctx::system("test").with_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 10)));

	// The address budget an attacker parked on the account drains at the password stage.
	for _ in 0..10 {
		app.limits.check(&app.settings, "login.email", &held.email).await.unwrap();
	}

	let challenge = pow::issue(&app, "login").await.unwrap();
	let ticket = match auth
		.login(
			&owner,
			&Credentials {
				email: held.email.clone(),
				password: PASSWORD.to_owned(),
				pow: Some(solve(&challenge)),
			},
		)
		.await
		.unwrap()
	{
		LoginOutcome::TotpRequired { totp_token } => {
			totp_token.expect("the login path mints a ticket")
		}
		LoginOutcome::Signed(t) => panic!("a confirmed factor must stop the login: {t:?}"),
	};

	// The second step must not answer 429 out of a bucket a stranger drained. One step on:
	// `enrol_real_totp` already spent the current one confirming the enrolment.
	assert!(
		auth.login_totp(&owner, &ticket, Some(&totp_code(&secret, 1)), None)
			.await
			.is_ok(),
		"the drained address bucket blocked the second factor"
	);

	// And the step beyond that: a drained second-factor budget must leave the documented
	// recovery path open. Its own account, because `SKEW_STEPS` is 1 and the second factor
	// above already spent the one future code the first account had.
	for _ in 0..5 {
		app.limits
			.check(&app.settings, "login.totp.account", victim.uid.as_str())
			.await
			.unwrap();
	}
	let token = reset_token(&app, &store, &victim.email).await;
	let outcome = auth
		.reset_password(
			&ip_ctx(),
			&token,
			Some(&totp_code(&victim_secret, 1)),
			None,
			"a-brand-new-password".to_owned(),
		)
		.await
		.expect("the drained login bucket blocked the documented recovery path");
	assert!(matches!(outcome, LoginOutcome::Signed(_)), "{outcome:?}");
}

/// IP-keyed `auth.failed` counter, so an attacker parked on an address spends only their own
/// budget — asked for work, not refused — and the owner signing in from anywhere else never
/// meets the gate at all.
#[tokio::test]
async fn an_exhausted_address_budget_demands_work_rather_than_locking_the_owner_out() {
	let db = TmpDb::new("address-budget-lockout");
	let (app, store) = setup(&db).await;
	app.settings.set("pow.difficulty.login", "1", None).await.unwrap();
	let account = account(&store, "victim@e.st").await;

	// What an attacker parking on the address leaves behind on their *own* address: the
	// failure counter `auth_mw` charges on every 401 out of a public bundle. Spent by hand,
	// because these tests call the handles and so pass through no layer.
	let key = mintworks_core::ratelimit::bucket_key(PEER.ip());
	let threshold = app.settings.int("auth.pow_after_failures").await.unwrap();
	for _ in 0..threshold {
		let _ = app
			.limits
			.check(&app.settings, mintworks_core::ratelimit::AUTH_FAILED, &key)
			.await;
	}
	assert_eq!(
		app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &key),
		threshold,
		"the counter must be at the threshold for this to test anything"
	);

	let auth = Auth::new(app.clone());
	let creds =
		|pow| Credentials { email: account.email.clone(), password: PASSWORD.to_owned(), pow };

	// Without a proof it is a demand for work, not a refusal — and `E-CORE-POW` is 400.
	let err = auth.login(&ip_ctx(), &creds(None)).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-POW", "{err:?}");
	assert_eq!(err.parts().0, StatusCode::BAD_REQUEST);

	// With one, the right password gets in. This is a 429 on the old code.
	let challenge = pow::issue(&app, "login").await.unwrap();
	assert!(
		matches!(
			auth.login(&ip_ctx(), &creds(Some(solve(&challenge)))).await.unwrap(),
			LoginOutcome::Signed(_)
		),
		"the owner was locked out of their own account"
	);

	// And the owner signing in from any other address never meets the gate: the counter the
	// attacker filled is the attacker's, not the account's.
	let elsewhere =
		Ctx::system("test").with_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 7)));
	assert!(
		matches!(auth.login(&elsewhere, &creds(None)).await.unwrap(), LoginOutcome::Signed(_)),
		"a counter filled from one address must not gate another"
	);
}

/// `verify_password` returned on a `NULL` hash without spending a pass, so an
/// invited-but-never-activated address answered in ~1 ms where an unknown one takes a full
/// argon2id pass — an invitation oracle. Not a timing assertion (flaky); the contract is
/// that the answer is indistinguishable from an unknown address.
#[tokio::test]
async fn an_account_without_a_password_answers_like_an_unknown_address() {
	let db = TmpDb::new("null-hash-login");
	let (app, store) = setup(&db).await;
	store
		.create_account(
			&NewAccount {
				email: "invited@e.st".to_owned(),
				pwd_hash: None,
				name: None,
				locale: "hu".to_owned(),
				org_name: "invited@e.st".to_owned(),
			},
			&[],
		)
		.await
		.unwrap();
	let auth = Auth::new(app.clone());

	let attempt = |email: &str| {
		let (auth, creds) = (auth.clone(), credentials(email, PASSWORD));
		async move { auth.login(&ip_ctx(), &creds).await }
	};
	let (invited, invited_body) =
		parts(attempt("invited@e.st").await.unwrap_err().into_response()).await;
	let (unknown, unknown_body) =
		parts(attempt("nobody@e.st").await.unwrap_err().into_response()).await;
	assert_eq!(invited, unknown);
	assert_eq!(invited_body, unknown_body);
	assert_eq!(invited_body["error"]["errCode"], "E-AUTH-CREDENTIALS", "{invited_body}");
}

/// The ticket was documented as "single-use" and is not — nothing consumes it, and making it
/// so would need either a `token_epoch` bump (signing the account out of every other device on
/// each login) or a consumed-ticket table. The property that actually holds, and the one worth
/// testing, is that the *factor* is single-use: the same ticket presented twice with the same
/// code succeeds once.
///
/// The path minted a session and wrote no `audit_logs` row, so the accounts with the
/// strongest authentication had no login history and `export_account` — which selects on
/// `account_id` — silently omitted it.
#[tokio::test]
async fn the_second_factor_is_single_use_even_though_the_ticket_is_reusable() {
	let db = TmpDb::new("ticket-replay");
	let (app, store) = setup(&db).await;
	let account = account(&store, "2fa-audit@e.st").await;

	// A confirmed factor whose one recovery code is known here. (A TOTP code would have to be
	// recomputed from the enrolment secret; the recovery code exercises the same
	// `login_totp` path and the same "spendable exactly once" contract.)
	let recovery = "ABCD1234EFGH";
	let hashes = format!("[{:?}]", register::hash_password(recovery.to_owned()).await.unwrap());
	store
		.put_totp(&NewTotpCredential {
			account_id: account.id,
			secret_nonce: vec![0; 12],
			secret_enc: vec![0; 32],
			digits: 6,
			period: 30,
			recovery_hashes: hashes.clone(),
		})
		.await
		.unwrap();
	store.confirm_totp(account.id, Timestamp::now(), &hashes).await.unwrap();

	let auth = Auth::new(app.clone());
	let ticket = match auth.login(&ip_ctx(), &credentials(&account.email, PASSWORD)).await.unwrap()
	{
		LoginOutcome::TotpRequired { totp_token } => {
			totp_token.expect("the login path mints a ticket")
		}
		LoginOutcome::Signed(t) => panic!("a confirmed factor must stop the login: {t:?}"),
	};

	let spend = |ticket: String| {
		let auth = auth.clone();
		async move { auth.login_totp(&ip_ctx(), &ticket, None, Some(recovery)).await }
	};
	spend(ticket.clone()).await.expect("the first spend mints a pair");

	// The very same ticket, the very same code. The ticket still opens — it is not consumed —
	// and the code is what refuses.
	let err = spend(ticket).await.unwrap_err();
	assert_eq!(
		err.parts(),
		(StatusCode::UNAUTHORIZED, "E-AUTH-TOTP-INVALID"),
		"the code must be what fails: {err:?}"
	);
	// The successful one is in the subject's own history.
	let logged: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM audit_logs WHERE account_id = ? AND action = 'LOGIN_TOTP'",
	)
	.bind(account.id)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(logged, 1, "a second-factor login must audit exactly once");
}

/// The sibling of the test above, for a real TOTP code rather than a recovery code:
/// `advance_totp_step`'s `last_used_step < ?` is the only thing stopping a shoulder-surfed,
/// logged or proxied six digits being spent twice, and every other test here either uses a
/// recovery code or steps `delta` deliberately to *avoid* the guard.
#[tokio::test]
async fn a_totp_code_cannot_be_spent_twice() {
	let db = TmpDb::new("totp-code-replay");
	let (app, store) = setup(&db).await;
	let account = account(&store, "replay@e.st").await;

	let auth = Auth::new(app.clone());
	let enrolment = auth.enrol_totp(&ctx_for(&account)).await.unwrap();
	let secret = base32_decode(&enrolment.secret);
	// Confirmed on step 0, so step 1 is the first code a login can spend.
	auth.confirm_totp(&ctx_for(&account), &totp_code(&secret, 0)).await.unwrap();

	let ticket = || {
		let auth = auth.clone();
		let email = account.email.clone();
		async move {
			match auth.login(&ip_ctx(), &credentials(&email, PASSWORD)).await.unwrap() {
				LoginOutcome::TotpRequired { totp_token } => {
					totp_token.expect("the login path mints a ticket")
				}
				LoginOutcome::Signed(t) => panic!("a confirmed factor must stop the login: {t:?}"),
			}
		}
	};

	let code = totp_code(&secret, 1);
	auth.login_totp(&ip_ctx(), &ticket().await, Some(&code), None)
		.await
		.expect("the first spend mints a pair");

	// A fresh ticket, so the ticket is not what refuses — the code is.
	let err = auth
		.login_totp(&ip_ctx(), &ticket().await, Some(&code), None)
		.await
		.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::UNAUTHORIZED, "E-AUTH-TOTP-INVALID"), "{err:?}");
}

/// `step_up`'s password arm was `(Some(password), _)`, so the TOTP code was ignored
/// whenever a password was present. `Auth::login` refuses to mint anything for that same
/// password without the second factor — it answers `TotpRequired` — so step-up granted
/// strictly more than login does on identical credentials, and the token it hands back
/// carries `auth_at = now`, which is exactly what passes the gate on `DELETE /api/auth/totp`.
#[tokio::test]
async fn step_up_demands_the_second_factor_login_would_have_demanded() {
	let db = TmpDb::new("stepup-second-factor");
	let (app, store) = setup(&db).await;
	let account = account(&store, "twofactor@e.st").await;
	let secret = enrol_real_totp(&app, &account).await;
	let auth = Auth::new(app.clone());

	// Login on the password alone hands out a ticket, never a session.
	let outcome = auth
		.login(&Ctx::system("test"), &credentials("twofactor@e.st", PASSWORD))
		.await
		.unwrap();
	assert!(
		matches!(outcome, LoginOutcome::TotpRequired { .. }),
		"login must stop at the second factor for this account"
	);

	// Neither factor alone does better than login, whichever one is missing. A code with no
	// password used to be enough on its own: a caller holding a stolen access token plus one
	// observed 6-digit code got `auth_at = now`, which passes `require_stepup` on
	// `DELETE /api/auth/totp`, `GET /api/account/export` and `POST /api/account/delete`.
	for (password, code) in [(Some(PASSWORD.to_owned()), None), (None, Some(totp_code(&secret, 1)))]
	{
		// The same code a wrong password gets, never a distinct one: the route must not
		// become an oracle for whether an account has a second factor.
		let err = auth
			.step_up(&ctx_for(&account), password, code.as_deref(), None)
			.await
			.unwrap_err();
		assert_eq!(err.parts(), (StatusCode::UNAUTHORIZED, "E-AUTH-CREDENTIALS"));
	}

	// A narrowing, not a blanket refusal: both factors still go through.
	let ok = auth
		.step_up(&ctx_for(&account), Some(PASSWORD.to_owned()), Some(&totp_code(&secret, 1)), None)
		.await
		.unwrap();
	assert!(!ok.access_token.is_empty());
}

/// A wrong password on an existing address ran `record_failure` — an `UPDATE accounts` on
/// the *single* writer connection — while an unknown address returned straight after the
/// argon2id pass with no write at all. The writer round-trip is both larger and far more
/// variable than the hash meant to hide the branch, so under concurrent write load login was
/// an existence oracle.
///
/// Asserted structurally, not by wall clock: both branches now go through `record_failure`,
/// and the no-account id it is given must be a harmless no-op rather than a stray write.
#[tokio::test]
async fn an_unknown_address_and_a_wrong_password_both_reach_the_writer() {
	let db = TmpDb::new("login-oracle");
	let (app, store) = setup(&db).await;
	let _known = account(&store, "known@e.st").await;
	let auth = Auth::new(app.clone());

	let attempt = |email: &str| {
		let (auth, creds) = (auth.clone(), credentials(email, "not-the-password"));
		async move { auth.login(&Ctx::system("test"), &creds).await.unwrap_err().parts().1 }
	};
	assert_eq!(attempt("known@e.st").await, "E-AUTH-CREDENTIALS");
	assert_eq!(attempt("nobody@e.st").await, "E-AUTH-CREDENTIALS");

	// The phantom account wrote nothing anywhere. `failed_logins` is not asserted: nothing
	// reads that column back, so it gates nothing — `adapters/…/tests/auth.rs` owns the
	// increment as a trait contract.
	let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(rows, 1, "the equalizing write must not create or touch another row");
}

/// `POST /api/auth/password` mints `auth_at = now`, which is what passes `require_stepup` on
/// `DELETE /api/auth/totp` and `POST /api/account/delete` — so it has to demand everything
/// `step_up` demands. It verified the password alone, granting on a confirmed-TOTP account
/// exactly what `step_up` refuses.
#[tokio::test]
async fn change_password_demands_the_second_factor_that_step_up_does() {
	let db = TmpDb::new("change-totp");
	let (app, store) = setup(&db).await;
	let account = account(&store, "changetotp@e.st").await;
	let secret = enrol_real_totp(&app, &account).await;
	let before = credential(&store, account.id).await;
	let auth = Auth::new(app.clone());

	// The same code a wrong password gets: never an oracle for whether there is a factor.
	let err = auth
		.change_password(
			&ctx_for(&account),
			PASSWORD.to_owned(),
			None,
			"a-brand-new-password".to_owned(),
		)
		.await
		.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::UNAUTHORIZED, "E-AUTH-CREDENTIALS"));
	assert_eq!(credential(&store, account.id).await, before, "the password changed anyway");

	// A narrowing, not a blanket refusal.
	auth.change_password(
		&ctx_for(&account),
		PASSWORD.to_owned(),
		Some(&totp_code(&secret, 1)),
		"a-brand-new-password".to_owned(),
	)
	.await
	.expect("both factors must go through");
	assert_ne!(credential(&store, account.id).await, before);

	// And a printed recovery code is refused here exactly as `step_up` refuses it: its job is
	// to recover a locked-out account, not to mint `auth_at = now` inside a live session.
	let before = credential(&store, account.id).await;
	let err = auth
		.change_password(
			&ctx_for(&account),
			"a-brand-new-password".to_owned(),
			Some("not-a-totp-code"),
			"a-fifth-password".to_owned(),
		)
		.await
		.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::UNAUTHORIZED, "E-AUTH-CREDENTIALS"));
	assert_eq!(credential(&store, account.id).await, before);
}

/// The second factor decides **before** the write. `set_password` and `record_login_success`
/// used to run unconditionally and only the token pair was withheld, so someone reading the
/// victim's mailbox could not sign in but did overwrite `pwd_hash` and bump `token_epoch` —
/// killing every live session and leaving the owner no route back but another reset.
#[tokio::test]
async fn a_refused_reset_does_not_move_the_password() {
	let db = TmpDb::new("reset-totp-write");
	let (app, store) = setup(&db).await;
	let account = account(&store, "resetwrite@e.st").await;
	let secret = enrol_real_totp(&app, &account).await;
	let before = credential(&store, account.id).await;
	let auth = Auth::new(app.clone());

	let token = reset_token(&app, &store, "resetwrite@e.st").await;
	let outcome = auth
		.reset_password(&ip_ctx(), &token, None, None, "a-brand-new-password".to_owned())
		.await
		.unwrap();
	assert!(
		matches!(outcome, LoginOutcome::TotpRequired { .. }),
		"the second factor must stop this reset"
	);
	assert_eq!(credential(&store, account.id).await, before, "the password changed anyway");

	// The reset token is deliberately not single-use, so the same one completes the flow once
	// the code is in hand — and the code itself is still spendable exactly once.
	let outcome = auth
		.reset_password(
			&ip_ctx(),
			&token,
			Some(&totp_code(&secret, 1)),
			None,
			"a-brand-new-password".to_owned(),
		)
		.await
		.unwrap();
	assert!(matches!(outcome, LoginOutcome::Signed(_)));
	assert_ne!(credential(&store, account.id).await, before);
}

/// The reset path used to mint a `totp-reset` ticket that nothing could open: `login_totp`
/// binds the purpose into the HMAC, so a caller following §4.2 — the only place that `401`
/// body shape is documented — got `E-AUTH-TOKEN`, and the action actually required (re-POST
/// the *mailed* token plus a code) was derivable from no response.
#[tokio::test]
async fn a_reset_on_a_totp_account_returns_no_ticket() {
	let db = TmpDb::new("reset-ticket-purpose");
	let (app, store) = setup(&db).await;
	let account = account(&store, "ticketswap@e.st").await;
	let secret = enrol_real_totp(&app, &account).await;
	let auth = Auth::new(app.clone());

	let token = reset_token(&app, &store, "ticketswap@e.st").await;
	match auth
		.reset_password(&ip_ctx(), &token, None, None, "a-brand-new-password".to_owned())
		.await
		.unwrap()
	{
		LoginOutcome::TotpRequired { totp_token } => {
			assert!(totp_token.is_none(), "the reset path has no ticket to hand out");
		}
		LoginOutcome::Signed(t) => panic!("a confirmed factor must stop this reset: {t:?}"),
	}

	// The login path still mints one: an `E-AUTH-TOTP-INVALID` for a deliberately wrong code
	// is the proof, without spending a real one.
	let login_ticket =
		match auth.login(&ip_ctx(), &credentials(&account.email, PASSWORD)).await.unwrap() {
			LoginOutcome::TotpRequired { totp_token } => {
				totp_token.expect("the login path mints a ticket")
			}
			LoginOutcome::Signed(t) => panic!("a confirmed factor must stop this login: {t:?}"),
		};
	let err = auth
		.login_totp(&ip_ctx(), &login_ticket, Some("000000"), None)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-TOTP-INVALID");

	// …and the reset completes with the same mailed token plus a code, as it always did.
	let outcome = auth
		.reset_password(
			&ip_ctx(),
			&token,
			Some(&totp_code(&secret, 1)),
			None,
			"a-brand-new-password".to_owned(),
		)
		.await
		.unwrap();
	assert!(matches!(outcome, LoginOutcome::Signed(_)));
}

/// The hit branch paid for a writer round-trip on the single write connection and the
/// miss branch returned straight away, which is the account-existence oracle `login`'s module
/// doc describes — larger and far more variable than any hash it was meant to hide behind.
/// Both branches make the same write now; `NO_ACCOUNT` is 0, so the miss one matches no row.
#[tokio::test]
async fn reset_request_and_resend_activation_write_on_both_branches() {
	let db = TmpDb::new("reset-request-oracle");
	let (app, store) = setup(&db).await;
	// `setup` only lowers the reset scope, and the default 18 bits is pure test latency —
	// `solve`'s bounded search is not even certain to find a nonce.
	app.settings.set("pow.difficulty.resend-activation", "1", None).await.unwrap();
	let account = account(&store, "known@e.st").await;
	let auth = Auth::new(app.clone());

	let request = |email: &str| {
		let (app, auth, email) = (app.clone(), auth.clone(), email.to_owned());
		async move {
			let challenge = pow::issue(&app, "password-reset").await.unwrap();
			auth.request_password_reset(&ip_ctx(), &email, Some(&solve(&challenge))).await
		}
	};

	// Both succeed, and neither is an error — the answer says nothing about the address.
	request("known@e.st").await.unwrap();
	request("nobody@e.st").await.unwrap();

	// The observable proof that the miss branch really did run `record_failure(NO_ACCOUNT)`:
	// the write went out and matched nothing, so no real account was touched by it.
	let failures: i64 = sqlx::query_scalar("SELECT failed_logins FROM accounts WHERE id = ?")
		.bind(account.id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(failures, 0, "the padding write must not count against a real account");

	// The same on the activation path, whose miss branch is any account that is not PENDING —
	// including the ACTIVE one above.
	let resend = |email: &str| {
		let (app, auth, email) = (app.clone(), auth.clone(), email.to_owned());
		async move {
			let challenge = pow::issue(&app, "resend-activation").await.unwrap();
			auth.resend_activation(&ip_ctx(), &email, Some(&solve(&challenge))).await
		}
	};
	resend("known@e.st").await.unwrap();
	resend("nobody@e.st").await.unwrap();
	let failures: i64 = sqlx::query_scalar("SELECT failed_logins FROM accounts WHERE id = ?")
		.bind(account.id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(failures, 0);
}

/// `set_password` was an unconditional `UPDATE`, and `reset::open` verified the mailed
/// token against the account row *before* it — so two redemptions that both read the account
/// first both succeeded. The token is `pwd_hash` + `token_epoch`, so the write is now a
/// compare-and-swap on the epoch the caller verified and the loser sees a spent link.
#[tokio::test]
async fn a_reset_token_can_only_be_redeemed_once() {
	let db = TmpDb::new("reset-single-use");
	let (app, store) = setup(&db).await;
	let _account = account(&store, "once@e.st").await;
	let auth = Auth::new(app.clone());

	let token = reset_token(&app, &store, "once@e.st").await;
	let ctx = Ctx::system("test");

	auth.reset_password(&ctx, &token, None, None, "first-new-password".to_owned())
		.await
		.expect("the first redemption");
	let err = auth
		.reset_password(&ctx, &token, None, None, "second-new-password".to_owned())
		.await
		.expect_err("the same link must not work twice");
	assert_eq!(err.parts().1, "E-AUTH-TOKEN");

	// The first password is the one that stuck — the epoch moved once, not twice.
	let fresh = store.account_by_email("once@e.st").await.unwrap().unwrap();
	assert_eq!(fresh.token_epoch, 1);
}

/// The proof-of-work gate read `login.account`, which is spent on **every** attempt — so
/// a user signing in from three devices inside the five-minute window was answered
/// `400 E-CORE-POW` on a request that carried the right password. It now reads the IP-keyed
/// `auth.failed` counter, which `auth_mw` charges on a 401 and never on a success — so it says
/// nothing about whether an account exists either.
#[tokio::test]
async fn proof_of_work_follows_failures_and_not_successes() {
	let db = TmpDb::new("pow-failures");
	let (app, store) = setup(&db).await;
	let _account = account(&store, "many@e.st").await;
	let auth = Auth::new(app.clone());
	// A caller off a socket, which is the only kind a proof is demanded of.
	let ctx = ip_ctx();

	let threshold = app.settings.int("auth.pow_after_failures").await.unwrap();
	assert!(threshold < 20, "`auth.failed` is 20/5min, so the run below has to fit inside it");

	// More successes than the threshold, all inside the window: none may demand a proof.
	for n in 0..=threshold {
		auth.login(&ctx, &credentials("many@e.st", PASSWORD))
			.await
			.unwrap_or_else(|e| panic!("success {n} was refused: {e:?}"));
	}

	// The same count of failures does — charged by hand, since a direct handle call passes
	// through no `auth_mw` layer.
	let key = mintworks_core::ratelimit::bucket_key(PEER.ip());
	for _ in 0..threshold {
		let err = auth
			.login(&ctx, &credentials("wrong@e.st", "not-the-password"))
			.await
			.unwrap_err();
		assert_eq!(err.parts().1, "E-AUTH-CREDENTIALS", "{err:?}");
		let _ = app
			.limits
			.check(&app.settings, mintworks_core::ratelimit::AUTH_FAILED, &key)
			.await;
	}
	let err = auth
		.login(&ctx, &credentials("wrong@e.st", "not-the-password"))
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-POW", "{err:?}");
}

/// `confirm_totp` was a read-then-unconditional-write. Two `POST /api/auth/totp/verify`
/// with codes from adjacent skew steps both passed `advance_totp_step` and both returned a
/// recovery-code set — the user kept the first, the database stored the second, and
/// `enrol` refuses an already-confirmed credential, so there was no way back. The precondition
/// rides in the `WHERE` now and the loser is told the factor is already armed.
#[tokio::test]
async fn a_second_confirmation_is_refused_and_leaves_the_recovery_codes_alone() {
	let db = TmpDb::new("totp-confirm-cas");
	let (app, store) = setup(&db).await;
	let account = account(&store, "totp@e.st").await;

	let auth = Auth::new(app.clone());
	let enrolment = auth.enrol_totp(&ctx_for(&account)).await.unwrap();
	let secret = base32_decode(&enrolment.secret);
	let first = auth.confirm_totp(&ctx_for(&account), &totp_code(&secret, 0)).await.unwrap();
	let stored = store.totp_by_account(account.id).await.unwrap().unwrap().recovery_hashes;

	// A code from the next skew step, which `advance_totp_step` accepts — that guard is
	// against replay, not against a second confirmation.
	let second = auth.confirm_totp(&ctx_for(&account), &totp_code(&secret, 1)).await;
	assert_eq!(second.unwrap_err().parts().1, "E-AUTH-TOTP-ENROLLED");

	let after = store.totp_by_account(account.id).await.unwrap().unwrap().recovery_hashes;
	assert_eq!(after, stored, "the refused confirmation overwrote the recovery codes");
	assert!(!first.recovery_codes.is_empty());
}

// ------------------------------------------------ orgs, members, consent, GDPR

/// Publish `version` of `kind` in the account locale the fixtures use, effective now.
async fn publish_legal(store: &SqliteStore, kind: LegalKind, version: &str) {
	store
		.insert_legal_doc(&NewLegalDoc {
			kind,
			locale: "hu".to_owned(),
			version: version.to_owned(),
			title: format!("{kind:?} {version}"),
			body: "…".to_owned(),
			sha256: format!("{:064x}", 0),
			effective_from: Timestamp(0),
		})
		.await
		.unwrap();
	// What `Auth::publish_legal_document` does for itself; a raw `insert_legal_doc` has to.
	mintworks_auth::consent::invalidate_document_cache();
}

async fn accept_current(
	store: &SqliteStore,
	account: &mintworks_auth::store::Account,
	kind: LegalKind,
) {
	let doc = store
		.current_legal_doc(kind, &account.locale, Timestamp::now())
		.await
		.unwrap()
		.expect("a document is in force");
	store
		.record_consent(
			&NewConsent {
				account_id: account.id,
				org_id: None,
				kind,
				legal_doc_id: Some(doc.id),
				doc_version: doc.version,
				doc_sha256: doc.sha256,
				granted: true,
				ip: None,
				user_agent: None,
			},
			Timestamp::now(),
		)
		.await
		.unwrap();
}

/// Everything `consent::gate` demands, for a test whose subject is some other gated route:
/// both gating kinds published and accepted. An *unpublished* kind gates too — a document
/// nobody can accept is not a waiver — so a fixture that publishes nothing is refused by the
/// gate before it reaches what it is testing.
async fn gate_satisfied(store: &SqliteStore, account: &mintworks_auth::store::Account) {
	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		publish_legal(store, kind, "1").await;
		accept_current(store, account, kind).await;
	}
}

/// A request carrying its credential exactly as the framework's own `Set-Cookie` handed it
/// out, and a request whose bearer is garbage.
async fn call_raw(
	router: &axum::Router,
	uri: &str,
	header: (&str, String),
) -> (StatusCode, serde_json::Value) {
	let req = axum::http::Request::builder()
		.method("GET")
		.uri(uri)
		.header(header.0, header.1)
		.header("content-type", "application/json")
		.body(axum::body::Body::empty())
		.unwrap();
	parts(tower::ServiceExt::oneshot(router.clone(), req).await.unwrap()).await
}

/// [`call`] without consuming the body, for the one test whose subject is a response *header*.
async fn call_resp(
	router: &axum::Router,
	method: &str,
	uri: &str,
	token: &str,
	body: Option<serde_json::Value>,
) -> Response {
	let req = axum::http::Request::builder()
		.method(method)
		.uri(uri)
		.header("authorization", format!("Bearer {token}"))
		.header("content-type", "application/json")
		.body(axum::body::Body::from(body.unwrap_or(serde_json::Value::Null).to_string()))
		.unwrap();
	tower::ServiceExt::oneshot(router.clone(), req).await.unwrap()
}

/// An org with `account` as its OWNER, plus the admin `Ctx` for acting inside it.
async fn org(
	store: &SqliteStore,
	account: &mintworks_auth::store::Account,
	name: &str,
) -> (i64, Ctx) {
	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			name,
			account.id,
			None,
		)
		.await
		.unwrap();
	(org.id, Ctx { org_id: Some(org.id), ..ctx_for(account) })
}

/// A `POST /api/invoices` body the route deserializes. Whether the draft can actually be
/// created is not that test's subject — only which middleware the request reaches.
fn json_draft() -> serde_json::Value {
	serde_json::json!({ "currency": "HUF", "lines": [] })
}

/// The writer pool is one connection on purpose (SQLite serialises writes anyway), so
/// `Settings` and `SecretStore` must not read through it: authentication would then queue
/// behind every INSERT, every audit row and the job runner's claim — and a service method
/// holding `write_tx()` that read an uncached setting would wait 30s on the connection it
/// was itself holding.
#[tokio::test]
async fn authentication_reads_do_not_wait_on_the_write_connection() {
	let db = TmpDb::new("reader-pool");
	let (app, store) = setup(&db).await;

	// `set` invalidates the cached entry, so the `get` below is a real read.
	app.secrets
		.set(mintworks_core::auth_mw::JWT_SECRET_KEY, b"signing-key", None)
		.await
		.unwrap();

	let tx = store.write_pool().begin().await.unwrap();
	let reads = tokio::time::timeout(std::time::Duration::from_secs(5), async {
		// What `auth_mw::verify` does on every authenticated request.
		let key = app.secrets.get(mintworks_core::auth_mw::JWT_SECRET_KEY).await.unwrap();
		assert_eq!(key.as_deref(), Some(b"signing-key".as_slice()));
		// And an uncached setting, the deadlock half of the same finding.
		app.settings.text("currency.base").await.unwrap()
	})
	.await
	.expect("a read blocked on the open write transaction");
	assert_eq!(reads, "HUF");
	tx.rollback().await.unwrap();
}

/// The eleven dumps ran on the 5-connection reader pool, each taking its own WAL
/// snapshot, so a write landing mid-export tore the document — orphan `invoiceLines` with no
/// parent invoice, or a half-erased subject access response. They share one `BEGIN DEFERRED`
/// read transaction now. The isolation itself has no deterministic test without racing a
/// write against the read, and a flaky test is worse than none; this is the regression guard
/// on the mechanical `dump(&mut tx, …)` change — every key still present, still an array.
#[tokio::test]
async fn the_export_still_carries_every_section_after_the_single_snapshot_refactor() {
	let db = TmpDb::new("export-one-snapshot");
	let (app, store) = setup(&db).await;
	let account = account(&store, "export@e.st").await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	accept_current(&store, &account, LegalKind::Tos).await;
	store
		.put_webauthn_credential(
			&NewWebauthnCredential {
				account_id: account.id,
				credential_id: "cred-export".to_owned(),
				credential: "{}".to_owned(),
				name: "This device".to_owned(),
				created_at: Timestamp(10),
			},
			i64::MAX,
		)
		.await
		.unwrap();

	let export = Auth::new(app.clone()).export_account(&ctx_for(&account)).await.unwrap().1;
	let sections = export.as_object().unwrap();
	for key in [
		"accounts",
		"orgs",
		"memberships",
		"consents",
		"billingParties",
		"objects",
		"invoices",
		"invoiceLines",
		"invoiceVatGroups",
		"payments",
		"apiKeys",
		"passkeys",
		"auditLog",
		"refUses",
		"usage",
		"agentRuns",
	] {
		assert!(sections[key].is_array(), "{key} is missing or not an array: {export}");
	}
	assert_eq!(sections.len(), 16, "a section appeared or vanished: {export}");
	// The four that actually have rows for this fixture, so this is not passing on empties.
	assert_eq!(export["accounts"].as_array().unwrap().len(), 1);
	assert_eq!(export["orgs"].as_array().unwrap().len(), 1, "the personal org");
	assert_eq!(export["consents"].as_array().unwrap().len(), 1);
	assert_eq!(export["passkeys"].as_array().unwrap().len(), 1, "the registered passkey");
}

/// `LoginBody::consents_required` documented that "the client must collect them before any
/// other route will accept the token", and nothing enforced it: the field was advisory, so
/// publishing a new ToS left every existing account with full API access indefinitely.
///
/// The fix is the router split in `routes::authenticated`, so this is a bundle test by
/// necessity — what it has to prove is which sub-router a path lands in, which the handle
/// cannot show.
#[tokio::test]
async fn an_outstanding_consent_gates_the_authenticated_routes() {
	let db = TmpDb::new("consent-gate");
	let (app, store) = setup(&db).await;
	let account = account(&store, "consent@e.st").await;

	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		publish_legal(&store, kind, "1.0").await;
		accept_current(&store, &account, kind).await;
	}

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "consent@e.st").await;

	// Nothing outstanding: a gated route is reachable.
	let (status, _) = call(&router, "GET", "/api/orgs", &token, None).await;
	assert_eq!(status, StatusCode::OK);

	// A new ToS version supersedes the accepted one.
	publish_legal(&store, LegalKind::Tos, "2.0").await;
	let (status, body) = call(&router, "GET", "/api/orgs", &token, None).await;
	assert_eq!(status, StatusCode::FORBIDDEN);
	assert_eq!(body["error"]["errCode"], "E-AUTH-CONSENT-REQUIRED");
	assert!(body["error"]["errStr"].as_str().unwrap().contains("TOS"), "{body}");

	// The exempt routes stay reachable, so the client can see who it is and accept.
	let (status, _) = call(&router, "GET", "/api/auth/me", &token, None).await;
	assert_eq!(status, StatusCode::OK, "/api/auth/me must not be gated");

	let (status, _) = call(
		&router,
		"POST",
		"/api/consents",
		&token,
		// `docSha256` is required — `publish_legal` files every document under this hash.
		Some(serde_json::json!({
			"kind": "TOS", "version": "2.0", "docSha256": format!("{:064x}", 0),
		})),
	)
	.await;
	assert_eq!(status, StatusCode::CREATED, "POST /api/consents must not be gated");

	// And accepting restores access.
	let (status, _) = call(&router, "GET", "/api/orgs", &token, None).await;
	assert_eq!(status, StatusCode::OK);
}

/// `token::issue` sets `access_token` as an `HttpOnly` cookie and `auth_mw::bearer` read
/// only `Authorization`, so a browser client relying on the cookie was unauthenticated on
/// every route — pushing the token into JS-readable storage, which is the one thing the
/// `HttpOnly` cookie exists to prevent.
///
/// A *bad* token must not fail the request outright either. That closed
/// `POST /api/auth/refresh` — the documented recovery from a 401 — for any client that
/// attaches `Authorization` globally, along with `/login`, `/healthz` and `/readyz`.
#[tokio::test]
async fn the_access_cookie_authenticates_and_a_bad_bearer_does_not_fail_the_request() {
	let db = TmpDb::new("access-cookie");
	let (app, store) = setup(&db).await;
	let account = account(&store, "cookie@e.st").await;

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "cookie@e.st").await;

	let (bearer, _) =
		call_raw(&router, "/api/auth/me", ("authorization", format!("Bearer {token}"))).await;
	assert_eq!(bearer, StatusCode::OK);

	let (cookie, _) = call_raw(
		&router,
		"/api/auth/me",
		("cookie", format!("{}={token}; other=x", mintworks_core::auth_mw::ACCESS_COOKIE)),
	)
	.await;
	assert_eq!(cookie, StatusCode::OK, "the HttpOnly cookie the framework sets must work");

	// A garbage bearer reaches the handler unauthenticated — 401 from the extractor, in the
	// error envelope, not a hard failure in the middleware.
	let (status, body) =
		call_raw(&router, "/api/auth/me", ("authorization", "Bearer xxx".to_owned())).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["error"]["errCode"], "E-AUTH-TOKEN");

	// A bearer *and* a good cookie together: the bearer is read first and wins, so a browser
	// with a stale `Authorization` header still attached gets the 401, not the cookie's
	// identity. The point of not failing in the middleware is that the request reaches its
	// handler at all — which is what keeps `/api/auth/refresh`, `/login`, `/healthz` and
	// `/readyz` open to a client that attaches a stale bearer globally.
	let req = axum::http::Request::builder()
		.method("GET")
		.uri("/api/auth/me")
		.header("authorization", "Bearer xxx")
		.header("cookie", format!("{}={token}", mintworks_core::auth_mw::ACCESS_COOKIE))
		.body(axum::body::Body::empty())
		.unwrap();
	let (status, _) = parts(tower::ServiceExt::oneshot(router.clone(), req).await.unwrap()).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "bearer wins, and it is garbage");

	// `verify` answers three 403s with their own codes, which are the contract: a client told to
	// log in again cannot tell "activate your account" from "your token expired", and logging in
	// fixes neither. `AuthDenied` carries the reason to the `Ctx` extractor, so an authenticated
	// route still answers with the code.
	//
	// `PENDING` rather than `SUSPENDED`: suspending bumps `token_epoch` in the same
	// transaction, so a suspended account's token is superseded before `verify` ever reads
	// the status, and the 403 under test would be masked by a 401.
	store.set_account_status(account.id, AccountStatus::Pending).await.unwrap();
	let (status, body) =
		call_raw(&router, "/api/auth/me", ("authorization", format!("Bearer {token}"))).await;
	assert_eq!(status, StatusCode::FORBIDDEN);
	assert_eq!(body["error"]["errCode"], "E-AUTH-PENDING");
}

/// The global `auth_mw::auth` layer used to turn a denied account's 403 into the response for
/// *every* route it wrapped. A browser sends its cookie on everything, so such a user was
/// refused by `POST /api/auth/logout` — the one route that clears the cookie causing it — and
/// a monitoring probe that happened to carry one flipped `/healthz` to 403.
///
/// `logout` lives in `public()`, which is layered with `optional_auth`, so the router here
/// merges that bundle too.
///
/// `PENDING` is the account-state denial that leaves the token live: `SUSPENDED` bumps
/// `token_epoch` in the same transaction, so that one is a 401 before the status is read.
#[tokio::test]
async fn a_denied_cookie_still_reaches_the_routes_that_need_no_actor() {
	let db = TmpDb::new("denied-public");
	let (app, store) = setup(&db).await;
	let account = account(&store, "denied@e.st").await;
	let token = access_token(&app, "denied@e.st").await;
	store.set_account_status(account.id, AccountStatus::Pending).await.unwrap();

	let router = mintworks_auth::routes::authenticated()
		.merge(mintworks_auth::routes::public())
		.merge(mintworks_core::health::public())
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let cookie = || ("cookie", format!("{}={token}", mintworks_core::auth_mw::ACCESS_COOKIE));

	let (health, _) = call_raw(&router, "/healthz", cookie()).await;
	assert_eq!(health, StatusCode::OK, "a probe carrying a stale cookie is still a probe");

	let logout = axum::http::Request::builder()
		.method("POST")
		.uri("/api/auth/logout")
		.header("cookie", format!("{}={token}", mintworks_core::auth_mw::ACCESS_COOKIE))
		.body(axum::body::Body::empty())
		.unwrap();
	let (status, _) =
		parts(tower::ServiceExt::oneshot(router.clone(), logout).await.unwrap()).await;
	assert!(
		status.is_success(),
		"the user must be able to clear the cookie that is refusing them, got {status}"
	);

	// And an authenticated route still refuses them, with the code that says why.
	let (me, body) = call_raw(&router, "/api/auth/me", cookie()).await;
	assert_eq!(me, StatusCode::FORBIDDEN);
	assert_eq!(body["error"]["errCode"], "E-AUTH-PENDING");
}

/// `authenticate` charged the auth-failed bucket on every client error out of a required
/// bundle, and an account-state denial is a 403 — a client error that is not a *credential*
/// failure at all. So one such user's browser replaying a still-valid cookie drained the
/// shared `20/5min/ip` bucket and 429'd every other user behind that address, and pushed them
/// into proof-of-work besides, since `Auth::login` reads `consumed()` on this very bucket.
///
/// `PENDING`, not `SUSPENDED`: suspending revokes the token in the same transaction, so that
/// path is a credential failure and is charged — correctly.
#[tokio::test]
async fn an_account_state_denial_does_not_spend_the_shared_auth_failure_budget() {
	let db = TmpDb::new("denied-auth-failed");
	let (app, store) = setup(&db).await;
	let account = account(&store, "denied-bucket@e.st").await;
	let token = access_token(&app, "denied-bucket@e.st").await;
	store.set_account_status(account.id, AccountStatus::Pending).await.unwrap();

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::middleware::from_fn_with_state(
			app.clone(),
			mintworks_core::auth_mw::client_ip_mw,
		))
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	// `client_ip_mw` reads the peer off `ConnectInfo`, which a served socket supplies.
	let get = |bearer: String| {
		let req = axum::http::Request::builder()
			.method("GET")
			.uri("/api/auth/me")
			.header("authorization", bearer)
			.extension(axum::extract::ConnectInfo(PEER))
			.body(axum::body::Body::empty())
			.unwrap();
		tower::ServiceExt::oneshot(router.clone(), req)
	};
	let key = mintworks_core::ratelimit::bucket_key(PEER.ip());

	for _ in 0..5 {
		let (status, body) = parts(get(format!("Bearer {token}")).await.unwrap()).await;
		assert_eq!(status, StatusCode::FORBIDDEN);
		assert_eq!(body["error"]["errCode"], "E-AUTH-PENDING", "{body}");
	}
	assert_eq!(
		app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &key),
		0,
		"a suspended account's 403s drained the budget shared with everyone on that address"
	);

	// The control: a forged token is a credential failure and still costs a token.
	let (status, _) = parts(get("Bearer not-a-token".to_owned()).await.unwrap()).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(
		app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &key),
		1,
		"the auth-failed tier stopped counting real failures"
	);
}

/// `POST /api/auth/totp/verify` is authenticated — it sits inside `consent_gated`, behind
/// `require_auth` — but charged the IP-keyed `login.totp`, which it shared with the *public*
/// `/api/auth/login/totp` and `/api/auth/password/reset`. One user mistyping their enrolment
/// code five times from an office NAT 429'd everyone else behind that address out of
/// second-factor login and password reset. It keys on the account, like its siblings.
#[tokio::test]
async fn one_account_exhausting_enrolment_verification_does_not_block_another() {
	let db = TmpDb::new("totp-verify-scope");
	let (app, store) = setup(&db).await;
	let noisy = account(&store, "noisy@e.st").await;
	let quiet = account(&store, "quiet@e.st").await;
	// Published once — the documents are global — and accepted by each account.
	gate_satisfied(&store, &noisy).await;
	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		accept_current(&store, &quiet, kind).await;
	}
	let noisy_token = access_token(&app, "noisy@e.st").await;
	let quiet_token = access_token(&app, "quiet@e.st").await;

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let verify = |token: String| {
		let router = router.clone();
		async move {
			call(
				&router,
				"POST",
				"/api/auth/totp/verify",
				&token,
				Some(serde_json::json!({ "code": "000000" })),
			)
			.await
			.0
		}
	};

	// `login.totp.account` is 5/5min, so the sixth is refused whatever the body was — the
	// layer runs before the handler.
	let mut refused = false;
	for _ in 0..8 {
		if verify(noisy_token.clone()).await == StatusCode::TOO_MANY_REQUESTS {
			refused = true;
			break;
		}
	}
	assert!(refused, "enrolment verification must have a budget at all");

	assert_ne!(
		verify(quiet_token).await,
		StatusCode::TOO_MANY_REQUESTS,
		"one account's wrong codes locked another out of the same route"
	);
}

/// `default_mw` was merged *inside* `health::public()`, which is what exempted the
/// per-second probes. It now wraps the auth layers — so the budget is charged before the
/// account lookup it protects — and the exemption has to be a path check instead. Losing it
/// would rate-limit readiness monitoring.
///
/// Only `/healthz` is exempt. `/readyz` reads `db_version()` off the 5-connection reader pool,
/// so a flood of it starved every authenticated request into `E-CORE-UNAVAILABLE` while the
/// probe that actually needed the exemption cost nothing. It gets its own generous scope.
#[tokio::test]
async fn healthz_is_exempt_from_the_blanket_budget_and_readyz_is_merely_generous() {
	let db = TmpDb::new("health-exempt");
	let (app, _store) = setup(&db).await;
	app.settings.set("ratelimit.default", "1/h/ip", None).await.unwrap();
	// The shipped `readyz` budget is 120/min; an operator row resolves ahead of it, and three
	// requests make the point that 121 would.
	app.settings.set("ratelimit.readyz", "2/h/ip", None).await.unwrap();

	let router = mintworks_core::health::public()
		.layer(axum::middleware::from_fn_with_state(
			app.clone(),
			mintworks_core::ratelimit::default_mw,
		))
		.layer(axum::middleware::from_fn_with_state(
			app.clone(),
			mintworks_core::auth_mw::client_ip_mw,
		))
		.with_state(app.clone());
	let probe = |uri: &'static str| {
		let req = axum::http::Request::builder()
			.uri(uri)
			// Without a peer there is no bucket key and every limit passes vacuously.
			.extension(axum::extract::ConnectInfo(PEER))
			.body(axum::body::Body::empty())
			.unwrap();
		tower::ServiceExt::oneshot(router.clone(), req)
	};

	for _ in 0..5 {
		let (status, _) = parts(probe("/healthz").await.unwrap()).await;
		assert_eq!(status, StatusCode::OK, "a probe must never spend the default budget");
	}

	for _ in 0..2 {
		let (status, _) = parts(probe("/readyz").await.unwrap()).await;
		assert_eq!(status, StatusCode::OK, "readiness inside its own budget");
	}
	let (status, body) = parts(probe("/readyz").await.unwrap()).await;
	assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
	assert_eq!(body["error"]["errCode"], "E-CORE-RATELIMIT", "{body}");

	// …and the exempt probe is still exempt after the other one has been cut off.
	let (status, _) = parts(probe("/healthz").await.unwrap()).await;
	assert_eq!(status, StatusCode::OK);
}

/// `put_membership` is an upsert that never read the target's existing role, while
/// `set_role` and `remove_member` both refuse an `OWNER`. So an admin posted the owner's
/// address with `role: "MEMBER"`, silently overwrote the `OWNER` row, and could then
/// `DELETE` them — a full org takeover through the invite route.
#[tokio::test]
async fn the_invite_route_cannot_demote_the_owner() {
	let db = TmpDb::new("invite-owner");
	let (app, store) = setup(&db).await;
	// The account owns its personal org, which is the org its token is scoped to.
	let owner = account(&store, "owner@e.st").await;
	gate_satisfied(&store, &owner).await;

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "owner@e.st").await;

	let (status, body) = call(
		&router,
		"POST",
		"/api/org/members",
		&token,
		Some(serde_json::json!({ "email": owner.email, "role": "MEMBER" })),
	)
	.await;
	assert_eq!(status, StatusCode::CONFLICT, "{body}");

	let role: Option<String> = sqlx::query_scalar(
		"SELECT role FROM memberships m JOIN orgs t ON t.id = m.org_id \
		 WHERE m.account_id = ?",
	)
	.bind(owner.id)
	.fetch_optional(store.read_pool())
	.await
	.unwrap();
	assert_eq!(role.as_deref(), Some("OWNER"), "the owner row survived");
}

/// `AuthStore::members` had no `LIMIT` at all, and an org admin grows the table by inviting:
/// the listing fully materialised whatever was there.
#[tokio::test]
async fn the_member_listing_stops_at_the_ceiling() {
	let db = TmpDb::new("member-ceiling");
	let (_app, store) = setup(&db).await;
	let owner = account(&store, "owner@e.st").await;
	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Org Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();

	for i in 0..5 {
		let m = account(&store, &format!("m{i}@e.st")).await;
		store
			.put_membership(org.id, m.id, mintworks_auth::store::Role::Member)
			.await
			.unwrap();
		store.accept_membership(org.id, m.id, Timestamp::now()).await.unwrap();
	}

	assert_eq!(store.members(org.id, 3).await.unwrap().len(), 3, "the LIMIT binds");
	assert_eq!(
		store
			.members(org.id, mintworks_auth::service_api::MAX_MEMBERS)
			.await
			.unwrap()
			.len(),
		6
	);
}

/// `MemberBody::status` is `skip_serializing_if = "Option::is_none"`, so setting it only
/// for a newly created invitee made the key's *presence* an account-existence oracle — and
/// any authenticated user can create an org, become its admin, and probe addresses.
#[tokio::test]
async fn neither_the_invite_nor_the_pending_row_says_the_address_was_registered() {
	let db = TmpDb::new("invite-oracle");
	let (app, store) = setup(&db).await;
	let admin = account(&store, "admin@e.st").await;
	let existing = account(&store, "already@e.st").await;
	// An organisation, because a personal org refuses invitations entirely (see
	// `a_personal_org_refuses_a_second_member`). `pick_org` picks the sole accepted org.
	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Org Kft.",
			admin.id,
			None,
		)
		.await
		.unwrap();
	store.accept_membership(org.id, admin.id, Timestamp::now()).await.unwrap();
	gate_satisfied(&store, &admin).await;

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "admin@e.st").await;

	for email in [existing.email.as_str(), "brand-new@e.st"] {
		let (status, body) = call(
			&router,
			"POST",
			"/api/org/members",
			&token,
			Some(serde_json::json!({ "email": email, "role": "MEMBER" })),
		)
		.await;
		// `accountUid` was the last field left, and a prefixed ULID's first ten
		// characters are the millisecond it was minted — so the uid of a pre-existing
		// account decoded to when that address first registered. Nothing survives it, so
		// there is no body at all.
		assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
		assert!(body.is_null(), "the invite response must carry no body: {body}");
	}

	// …and the listings do not undo it: an invitation is a ref, not a membership row, so the
	// member listing holds the admin alone and the invite listing only the typed-in addresses.
	let (status, body) = call(&router, "GET", "/api/org/members", &token, None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["items"].as_array().unwrap().len(), 1, "{body}");
	let (status, body) = call(&router, "GET", "/api/org/invites", &token, None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let invites = body["items"].as_array().unwrap();
	assert_eq!(invites.len(), 2, "{body}");
	assert!(invites.iter().all(|r| r.get("accountUid").is_none()), "{body}");

	// Once the invitation is accepted the uid is there, so the listing still works.
	let code = invites
		.iter()
		.find(|r| r["email"] == serde_json::json!(existing.email))
		.unwrap()["code"]
		.as_str()
		.unwrap()
		.to_owned();
	Auth::new(app.clone()).accept_invite(&ctx_for(&existing), &code).await.unwrap();
	let (_, body) = call(&router, "GET", "/api/org/members", &token, None).await;
	let accepted = body["items"]
		.as_array()
		.unwrap()
		.iter()
		.find(|m| m["accountUid"].as_str() == Some(existing.uid.as_str()))
		.expect("the accepted member carries their uid");
	assert_eq!(accepted["email"], serde_json::json!(existing.email));
}

/// The body says nothing, and neither may the work: an invitation reads no account, so a
/// registered and an unknown address each cost one ref and one `org_invite` mail, and no
/// `accounts` row is created or touched.
#[tokio::test]
async fn inviting_a_registered_or_unknown_address_does_the_same_work() {
	let db = TmpDb::new("invite-roundtrip");
	let (app, store) = setup(&db).await;
	let admin = account(&store, "rtadmin@e.st").await;
	let active = account(&store, "rtactive@e.st").await;
	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Org Kft.",
			admin.id,
			None,
		)
		.await
		.unwrap();
	store.accept_membership(org.id, admin.id, Timestamp::now()).await.unwrap();

	let ctx = Ctx { org_id: Some(org.id), ..ctx_for(&admin) };
	let auth = Auth::new(app.clone());
	let count = |sql: &'static str| {
		let store = store.clone();
		async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(store.read_pool()).await.unwrap() }
	};
	let accounts = count("SELECT count(*) FROM accounts").await;

	auth.add_member(&ctx, &active.email, Role::Member).await.unwrap();
	let after_active = (
		count("SELECT count(*) FROM refs WHERE type = 'org_invite'").await,
		count("SELECT count(*) FROM jobs WHERE kind = 'SEND_EMAIL'").await,
	);
	assert_eq!(after_active, (1, 1));

	auth.add_member(&ctx, "rtnew@e.st", Role::Member).await.unwrap();
	let after_new = (
		count("SELECT count(*) FROM refs WHERE type = 'org_invite'").await,
		count("SELECT count(*) FROM jobs WHERE kind = 'SEND_EMAIL'").await,
	);
	assert_eq!(after_new, (2, 2), "the unknown address costs exactly what the registered one did");
	assert_eq!(count("SELECT count(*) FROM accounts").await, accounts, "no account is created");
	assert_eq!(count("SELECT count(*) FROM accounts WHERE failed_logins <> 0").await, 0);
}

/// An invitation to a mistyped address must be cancellable, or the invitee could accept it at
/// any time: it is listed in `GET /api/org/invites` and revoked as the ref it is.
#[tokio::test]
async fn a_pending_invitation_can_be_cancelled() {
	let db = TmpDb::new("invite-revoke");
	let (app, store) = setup(&db).await;
	let admin = account(&store, "admin@e.st").await;
	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Org Kft.",
			admin.id,
			None,
		)
		.await
		.unwrap();
	store.accept_membership(org.id, admin.id, Timestamp::now()).await.unwrap();
	gate_satisfied(&store, &admin).await;

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "admin@e.st").await;
	let mistyped = account(&store, "mistyped@e.st").await;

	let (status, body) = call(
		&router,
		"POST",
		"/api/org/members",
		&token,
		Some(serde_json::json!({ "email": "mistyped@e.st", "role": "MEMBER" })),
	)
	.await;
	assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

	let (status, body) = call(&router, "GET", "/api/org/invites", &token, None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let invite = body["items"][0].clone();
	assert_eq!(invite["email"], "mistyped@e.st", "{body}");
	let uid = mintworks_core::ids::RefId::parse(invite["uid"].as_str().unwrap()).unwrap();

	let admin_ctx = Ctx { org_id: Some(org.id), ..ctx_for(&admin) };
	let refs = mintworks_core::refs::Refs::from_app(&app).unwrap();
	refs.revoke(&admin_ctx, &uid).await.unwrap();
	refs.revoke(&admin_ctx, &uid).await.unwrap(); // twice is a no-op
	// Keyed by an unguessable uid, so a 404 for one this org never minted is no address oracle.
	let err = refs
		.revoke(&admin_ctx, &mintworks_core::ids::RefId::generate())
		.await
		.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::NOT_FOUND);
	let (status, body) = call(
		&router,
		"DELETE",
		"/api/org/members",
		&token,
		Some(serde_json::json!({ "email": "stranger@e.st" })),
	)
	.await;
	assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

	let err = Auth::new(app.clone())
		.accept_invite(&ctx_for(&mistyped), invite["code"].as_str().unwrap())
		.await
		.unwrap_err();
	let (status, body) = parts(err.into_response()).await;
	assert_eq!(status, StatusCode::GONE, "{body}");
	assert_eq!(body["error"]["errCode"], "E-AUTH-INVITE-EXPIRED");
	assert!(
		store
			.orgs_for_account(mistyped.id)
			.await
			.unwrap()
			.iter()
			.all(|o| o.uid != org.uid)
	);
}

/// `POST /api/auth/register` answered `201 {"accountUid"}`, and a ULID opens with a
/// millisecond timestamp — so decoding the uid off the duplicate path said exactly when the
/// address was first registered, defeating a duplicate path built to be indistinguishable in
/// status, body and cost.
///
/// A bundle test rather than a handle one: what it pins is the *answer* — status and body —
/// and `Auth::register` returns `()`, which carries neither.
#[tokio::test]
async fn registering_twice_answers_the_same_and_returns_no_uid() {
	let db = TmpDb::new("register-oracle");
	let (app, store) = setup(&db).await;
	app.settings.set("pow.difficulty.register", "1", None).await.unwrap();
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;

	let router = mintworks_auth::routes::public()
		.layer(axum::Extension(ClientIp(PEER.ip())))
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());

	let register = |email: &'static str| {
		let (app, router) = (app.clone(), router.clone());
		async move {
			let challenge = pow::issue(&app, "register").await.unwrap();
			call(
				&router,
				"POST",
				"/api/auth/register",
				"",
				Some(serde_json::json!({
					"email": email,
					"locale": "hu",
					"consents": [{ "kind": "TOS", "version": "1" },
								 { "kind": "PRIVACY", "version": "1" }],
					"pow": solve_json(&challenge),
				})),
			)
			.await
		}
	};

	let first = register("twice@e.st").await;
	let second = register("twice@e.st").await;
	assert_eq!(first.0, StatusCode::NO_CONTENT, "{}", first.1);
	assert_eq!(first, second, "the duplicate path must be indistinguishable");
	assert!(first.1.is_null(), "no uid may come back at all: {}", first.1);

	// And exactly one account exists, so the second call really did take the duplicate path.
	let n: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts WHERE email = 'twice@e.st'")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(n, 1);
}

/// An existing account accepts an invitation by its code, and only the addressee can.
/// `/me` reports the token's org, not the default one.
/// `switch` never adds an `auth_at` the token lacked.
#[tokio::test]
async fn accepting_an_invitation_by_code_joins_and_switching_carries_the_claims_through() {
	let db = TmpDb::new("accept-membership");
	let (app, store) = setup(&db).await;
	let invitee = account(&store, "invitee@e.st").await;
	let owner = account(&store, "owner@e.st").await;
	let stranger = account(&store, "stranger@e.st").await;
	let auth = Auth::new(app.clone());

	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Org Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();
	store.accept_membership(org.id, owner.id, Timestamp::now()).await.unwrap();
	let owner_ctx = Ctx { org_id: Some(org.id), ..ctx_for(&owner) };
	auth.add_member(&owner_ctx, &invitee.email, Role::Member).await.unwrap();
	let code = auth.invites(&owner_ctx).await.unwrap()[0].code.clone();

	// Not a member until accepted: the invitation is a ref, not a membership row.
	let pending = store.orgs_for_account(invitee.id).await.unwrap();
	assert!(pending.iter().all(|t| t.uid != org.uid), "an invitation is not a membership");

	// Someone else's invitation, which also leaves it unspent for the addressee.
	let err = auth.accept_invite(&ctx_for(&stranger), &code).await.unwrap_err();
	let (status, body) = parts(err.into_response()).await;
	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
	assert_eq!(body["error"]["errCode"], "E-AUTH-INVITE-EMAIL");

	assert_eq!(auth.accept_invite(&ctx_for(&invitee), &code).await.unwrap(), org.uid);
	let after = store.orgs_for_account(invitee.id).await.unwrap();
	let accepted = after.iter().find(|t| t.uid == org.uid).expect("accepting joined the org");
	assert!(accepted.accepted_at.is_some());

	// Switch in with a `Ctx` that carries **no** `auth_at`, as an impersonation token does.
	// It must come back out the other side still absent.
	let impersonating = Ctx { auth_at: None, ..ctx_for(&invitee) };
	let switched = auth.switch_org(&impersonating, org.uid.as_str()).await.unwrap().access_token;
	assert!(
		jwt_payload(&switched).get("auth_at").is_none(),
		"switching manufactured step-up for a token that had none"
	);

	// `/me` now reports the org the caller is working in, not the default.
	let me = auth.me(&ctx_for(&invitee).with_org(org.id)).await.unwrap();
	assert_eq!(me.org.as_ref().map(|t| t.uid.as_str()), Some(org.uid.as_str()));
}

/// The refresh twin. `refresh` ignored its claims entirely and re-ran `pick_org`, so
/// the token that comes back on the first 401 could be scoped to a *different* org than
/// the one the caller had been working in — and the next `POST /api/invoices` would then
/// allocate a number in the wrong org, on a document that is immutable at issue.
///
/// The setup is the smallest thing that moves `pick_org`'s answer between mint and spend:
/// log in with one organisation (so it is picked), then join a second (so the default falls
/// back to the personal org). `step_up` documents the same hazard and carries the org
/// through for the same reason.
#[tokio::test]
async fn refresh_keeps_the_org_the_token_was_minted_for() {
	let db = TmpDb::new("refresh-keeps-org");
	let (app, store) = setup(&db).await;
	let account = account(&store, "refresher@e.st").await;
	let auth = Auth::new(app.clone());

	let a = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Org A Kft.",
			account.id,
			None,
		)
		.await
		.unwrap();
	store.accept_membership(a.id, account.id, Timestamp::now()).await.unwrap();

	let login = auth
		.login(&Ctx::public("test"), &credentials(&account.email, PASSWORD))
		.await
		.unwrap();
	let LoginOutcome::Signed(tokens) = login else { panic!("expected a signed pair") };
	assert_eq!(
		jwt_payload(&tokens.refresh_token).get("org").and_then(|v| v.as_str()),
		Some(a.uid.as_str()),
		"the refresh token dropped the org it was minted for"
	);

	// A second organisation moves `pick_org` off org A and onto the personal org.
	let b = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Org B Kft.",
			account.id,
			None,
		)
		.await
		.unwrap();
	store.accept_membership(b.id, account.id, Timestamp::now()).await.unwrap();

	let fresh = auth.refresh(&Ctx::public("test"), &tokens.refresh_token).await.unwrap();
	assert_eq!(
		jwt_payload(&fresh.access_token).get("org").and_then(|v| v.as_str()),
		Some(a.uid.as_str()),
		"refreshing silently switched the caller to a different org"
	);

	// And the revocation case the old blanking was protecting against: a membership that no
	// longer resolves drops the caller to *no* org, never to a re-picked default.
	sqlx::query("UPDATE orgs SET status = 'SUSPENDED' WHERE id = ?")
		.bind(a.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let after = auth.refresh(&Ctx::public("test"), &fresh.refresh_token).await.unwrap();
	assert_eq!(
		jwt_payload(&after.access_token).get("org").and_then(|v| v.as_str()),
		None,
		"a revoked membership was swapped for another org instead of dropped"
	);
}

/// `erase_account` told the caller to "transfer ownership or delete the organisation" and
/// neither operation existed: no `DELETE` route on `/api/orgs`, no transfer method on
/// `AuthStore`, and `set_member_role`/`remove_member` both refuse to touch an `OWNER`. An
/// account that clicked "create organisation" once got `409` forever, in the one module that
/// exists to serve GDPR Art. 17.
#[tokio::test]
async fn a_solo_owner_can_delete_the_organisation_and_then_erase() {
	let db = TmpDb::new("owner-erasure-delete");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "solo@e.st").await;
	let auth = Auth::new(app.clone());
	let org = auth.create_org(&ctx_for(&owner), "Solo Kft.", None).await.unwrap();

	assert_eq!(
		auth.erase_account(&ctx_for(&owner), &owner.email).await.unwrap_err().parts().1,
		"E-AUTH-OWNER-ERASURE"
	);

	auth.delete_org(&ctx_for(&owner), org.uid.as_str()).await.unwrap();
	assert!(store.org_by_uid(&org.uid).await.unwrap().is_none());
	auth.erase_account(&ctx_for(&owner), &owner.email).await.unwrap();
}

/// A cascade dropped `documents` rows and left their files behind; app-DB data keyed by the
/// org's uid heard nothing, and the FK-less `agent_runs` outlived the org.
#[tokio::test]
async fn delete_org_refuses_documents_and_tells_the_hooks() {
	let db = TmpDb::new("delete-org-hooks");
	let hook = Arc::new(RecordingHook::default());
	let (app, store) = setup_with(&db, AppBuilder::new().account_data_hook(hook.clone())).await;
	let owner = account(&store, "docs@e.st").await;
	let auth = Auth::new(app.clone());
	let org = auth.create_org(&ctx_for(&owner), "Docs Kft.", None).await.unwrap();
	sqlx::query(
		"INSERT INTO documents (uid, org_id, template, job_key, created_at)
		 VALUES ('doc_x', ?, 't.typ', 'k', 0)",
	)
	.bind(org.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO agent_runs (uid, thread_uid, org_id, role, spec, status, created_at)
		 VALUES ('run_x', 'thr_x', ?, 'USER', '{}', 'done', 0)",
	)
	.bind(org.id)
	.execute(store.write_pool())
	.await
	.unwrap();

	assert_eq!(
		auth.delete_org(&ctx_for(&owner), org.uid.as_str()).await.unwrap_err().parts().1,
		"E-AUTH-ORG-NOT-EMPTY"
	);
	sqlx::query("DELETE FROM documents").execute(store.write_pool()).await.unwrap();
	auth.delete_org(&ctx_for(&owner), org.uid.as_str()).await.unwrap();

	assert_eq!(*hook.0.lock(), vec![format!("org_deleted {}", org.uid)]);
	let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs")
		.fetch_one(store.write_pool())
		.await
		.unwrap();
	assert_eq!(runs, 0);
}

/// The other way out: an organisation with a second member is handed over, not deleted.
#[tokio::test]
async fn an_owner_transfers_the_organisation_and_then_erases() {
	let db = TmpDb::new("owner-erasure-transfer");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "handover@e.st").await;
	let successor = account(&store, "successor2@e.st").await;
	let auth = Auth::new(app.clone());
	let org = auth.create_org(&ctx_for(&owner), "Kft.", None).await.unwrap();

	// A member who has not accepted must not be promoted: that hands the organisation to
	// somebody who never agreed to have it.
	store.put_membership(org.id, successor.id, Role::Member).await.unwrap();
	assert_eq!(
		auth.transfer_ownership(&ctx_for(&owner), org.uid.as_str(), successor.uid.as_str())
			.await
			.unwrap_err()
			.parts()
			.1,
		"E-AUTH-TRANSFER-TARGET"
	);

	store.accept_membership(org.id, successor.id, Timestamp::now()).await.unwrap();
	auth.transfer_ownership(&ctx_for(&owner), org.uid.as_str(), successor.uid.as_str())
		.await
		.unwrap();

	let moved = store.org_by_uid(&org.uid).await.unwrap().unwrap();
	assert_eq!(moved.owner_account_id, Some(successor.id));
	assert_eq!(store.membership_role(org.id, successor.id).await.unwrap(), Some(Role::Owner));
	// The former owner stays on as an admin rather than being dropped out of the org.
	assert_eq!(store.membership_role(org.id, owner.id).await.unwrap(), Some(Role::Admin));

	auth.erase_account(&ctx_for(&owner), &owner.email).await.unwrap();
}

/// A `POST /api/orgs` from a caller whose active org is their **personal** one parents the new
/// org at the root, not at that personal org: the alternative made the creator a permanent
/// inherited `OWNER` of it, which `transfer_ownership` cannot take back.
#[tokio::test]
async fn a_created_org_is_not_parented_under_a_personal_one() {
	let db = TmpDb::new("create-org-parent");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "creator@e.st").await;
	let auth = Auth::new(app.clone());
	let personal = store
		.orgs_for_account(owner.id)
		.await
		.unwrap()
		.into_iter()
		.find(|o| o.kind == mintworks_auth::store::OrgKind::Personal)
		.expect("an account is created with a personal org");
	let personal_id = store.org_by_uid(&personal.uid).await.unwrap().unwrap().id;
	let ctx = Ctx { org_id: Some(personal_id), ..ctx_for(&owner) };

	let org = auth.create_org(&ctx, "Kft.", None).await.unwrap();
	let parent: Option<i64> = sqlx::query_scalar("SELECT parent_id FROM orgs WHERE id = ?")
		.bind(org.id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(parent, Some(store.root_org_id().await.unwrap()));

	// The handover demotes the former owner to `ADMIN`, and that is the role they must resolve:
	// `OWNER` here would be coming back through the creator's personal org as the parent.
	let successor = account(&store, "taker@e.st").await;
	store.put_membership(org.id, successor.id, Role::Member).await.unwrap();
	store.accept_membership(org.id, successor.id, Timestamp::now()).await.unwrap();
	auth.transfer_ownership(&ctx_for(&owner), org.uid.as_str(), successor.uid.as_str())
		.await
		.unwrap();
	let on_org = Ctx { org_id: Some(org.id), ..ctx_for(&owner) };
	assert_eq!(auth.me(&on_org).await.unwrap().org.unwrap().role, Role::Admin);
}

/// A child org blocks its parent's `delete_org` for good, so creating one is an org-admin
/// action: the new org hangs off the caller's own membership on the parent.
#[tokio::test]
async fn a_member_cannot_create_a_child_under_the_active_org() {
	let db = TmpDb::new("create-org-gate");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "bosss@e.st").await;
	let member = account(&store, "staffer@e.st").await;
	let auth = Auth::new(app.clone());
	let org = auth.create_org(&ctx_for(&owner), "Parent Kft.", None).await.unwrap();
	store.put_membership(org.id, member.id, Role::Member).await.unwrap();
	store.accept_membership(org.id, member.id, Timestamp::now()).await.unwrap();
	let ctx = Ctx { org_id: Some(org.id), ..ctx_for(&member) };

	assert_eq!(
		auth.create_org(&ctx, "Child Kft.", None).await.unwrap_err().parts().1,
		"E-AUTH-FORBIDDEN"
	);

	store.put_membership(org.id, member.id, Role::Admin).await.unwrap();
	let child = auth.create_org(&ctx, "Child Kft.", None).await.unwrap();
	let parent: Option<i64> = sqlx::query_scalar("SELECT parent_id FROM orgs WHERE id = ?")
		.bind(child.id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(parent, Some(org.id));
}

/// `invoices.org_id` is a plain FK under an eight-year retention obligation, so an
/// organisation that ever issued one is transferred, never deleted — and the refusal is the
/// store's own `false`, not an opaque constraint error.
#[tokio::test]
async fn an_organisation_holding_records_is_not_deletable() {
	let db = TmpDb::new("owner-erasure-records");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "hasinvoices@e.st").await;
	let member = account(&store, "colleague@e.st").await;
	let auth = Auth::new(app.clone());

	let with_invoice = auth.create_org(&ctx_for(&owner), "Books Kft.", None).await.unwrap();
	sqlx::raw_sql(
		"INSERT INTO sellers (id, uid, org_id, nav_base_url, created_at)
		 VALUES (1, 'sel_test', (SELECT id FROM orgs WHERE kind = 'ROOT'), '', 0);
		 INSERT INTO seller_versions (seller_ver, seller_id, status, name, country, tax_number,
		                              postcode, city, street, created_at, valid_from)
		 VALUES (1, 1, 'CURRENT', 'Teszt Kft.', 'HU', '12345678242', '1011', 'Budapest',
		         'Fo utca 1.', 0, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO invoices (uid, org_id, seller_id, currency, rate_e6, created_at,
		                       updated_at)
		 VALUES ('inv_test', ?, 1, 'HUF', 1000000, 0, 0)",
	)
	.bind(with_invoice.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	assert_eq!(
		auth.delete_org(&ctx_for(&owner), with_invoice.uid.as_str())
			.await
			.unwrap_err()
			.parts()
			.1,
		"E-AUTH-ORG-NOT-EMPTY"
	);

	// And so is one that still has a second accepted member.
	let with_member = auth.create_org(&ctx_for(&owner), "Crowd Kft.", None).await.unwrap();
	store.put_membership(with_member.id, member.id, Role::Member).await.unwrap();
	store
		.accept_membership(with_member.id, member.id, Timestamp::now())
		.await
		.unwrap();
	assert_eq!(
		auth.delete_org(&ctx_for(&owner), with_member.uid.as_str())
			.await
			.unwrap_err()
			.parts()
			.1,
		"E-AUTH-ORG-NOT-EMPTY"
	);
}

/// The root org's `owner_account_id` is `NULL`, so the store's "other members" count compared
/// against `NULL` and always found zero; the service now refuses `OrgKind::Root` outright.
#[tokio::test]
async fn the_root_org_cannot_be_deleted() {
	let db = TmpDb::new("root-delete");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "rootowner@e.st").await;
	let auth = Auth::new(app.clone());
	let root = store.root_org_id().await.unwrap();
	store.put_membership(root, owner.id, Role::Owner).await.unwrap();
	store.accept_membership(root, owner.id, Timestamp::now()).await.unwrap();
	let uid = store.org_by_id(root).await.unwrap().unwrap().uid;

	assert_eq!(
		auth.delete_org(&ctx_for(&owner), uid.as_str()).await.unwrap_err().parts().1,
		"E-CORE-CONFLICT"
	);
	assert!(store.org_by_id(root).await.unwrap().is_some(), "and the root is still there");
}

/// The retention pre-checks covered `invoices` and `consents` only, so an org holding a
/// `payments` row surfaced as an FK constraint error the caller read as a 500.
#[tokio::test]
async fn an_org_holding_a_payment_is_not_deletable() {
	let db = TmpDb::new("owner-erasure-payment");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "haspayment@e.st").await;
	let auth = Auth::new(app.clone());
	let org = auth.create_org(&ctx_for(&owner), "Fizet Kft.", None).await.unwrap();
	sqlx::query(
		"INSERT INTO payments (uid, org_id, kind, amount, currency, created_at, updated_at)
		 VALUES ('pay_test', ?, 'MANUAL', 0, 'HUF', 0, 0)",
	)
	.bind(org.id)
	.execute(store.write_pool())
	.await
	.unwrap();

	assert_eq!(
		auth.delete_org(&ctx_for(&owner), org.uid.as_str()).await.unwrap_err().parts().1,
		"E-AUTH-ORG-NOT-EMPTY"
	);
}

/// `owner_of` reads a **direct** membership while `admin_of` reads the effective role: an
/// inherited owner can administer a child org, but the two irreversible routes stay with the
/// org's own owner — otherwise a root `OWNER` could move or delete any org in the deployment.
#[tokio::test]
async fn an_inherited_owner_administers_a_child_but_does_not_own_it() {
	let db = TmpDb::new("inherited-owner");
	let (app, store) = setup(&db).await;
	let parent_owner = account(&store, "parentowner@e.st").await;
	let child_owner = account(&store, "childowner@e.st").await;
	let auth = Auth::new(app.clone());
	let parent = auth.create_org(&ctx_for(&parent_owner), "Parent Kft.", None).await.unwrap();
	let child = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			parent.id,
			"Child Kft.",
			child_owner.id,
			None,
		)
		.await
		.unwrap();
	let ctx = Ctx { org_id: Some(child.id), ..ctx_for(&parent_owner) };

	let patch = mintworks_auth::org::OrgPatch {
		name: Some("Child renamed".into()),
		billing_currency: Patch::Undefined,
		slug: Patch::Undefined,
	};
	assert_eq!(auth.update_org(&ctx, &patch).await.unwrap().name, "Child renamed");

	for code in [
		auth.transfer_ownership(&ctx, child.uid.as_str(), child_owner.uid.as_str())
			.await
			.unwrap_err()
			.parts()
			.1,
		auth.delete_org(&ctx, child.uid.as_str()).await.unwrap_err().parts().1,
	] {
		assert_eq!(code, "E-CORE-NOTFOUND");
	}
}

/// A direct `MEMBER` who inherits `OWNER` from an ancestor used to see `me.org.role ==
/// MEMBER` while `GET /api/org` said `OWNER`; `active_membership` now resolves the effective
/// role once, so the three agree.
#[tokio::test]
async fn me_reports_the_effective_role() {
	let db = TmpDb::new("me-effective-role");
	let (app, store) = setup(&db).await;
	let heir = account(&store, "heir@e.st").await;
	let pawn = account(&store, "pawn@e.st").await;
	let auth = Auth::new(app.clone());
	let parent = auth.create_org(&ctx_for(&heir), "Parent Kft.", None).await.unwrap();
	let child = store
		.create_org(mintworks_auth::store::OrgKind::Shared, parent.id, "Child Kft.", pawn.id, None)
		.await
		.unwrap();
	store.put_membership(child.id, heir.id, Role::Member).await.unwrap();
	store.accept_membership(child.id, heir.id, Timestamp::now()).await.unwrap();
	let ctx = Ctx { org_id: Some(child.id), ..ctx_for(&heir) };

	let me = auth.me(&ctx).await.unwrap();
	assert_eq!(me.org.as_ref().unwrap().role, Role::Owner, "the inherited role wins");
	assert_eq!(auth.org(&ctx).await.unwrap().role, Role::Owner);

	let switched = auth.switch_org(&ctx, child.uid.as_str()).await.unwrap();
	assert_eq!(
		jwt_payload(&switched.access_token).get("rol").and_then(|v| v.as_str()),
		Some("OWNER")
	);
}

/// Both routes are the owner's alone, derived from a fresh `memberships` read. A non-member
/// gets `E-CORE-NOTFOUND` — `403` would confirm another actor's organisation exists — while a
/// member who simply is not the owner gets the ordinary `E-AUTH-FORBIDDEN`.
#[tokio::test]
async fn neither_route_is_reachable_by_a_non_owner() {
	let db = TmpDb::new("owner-routes-authz");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "theowner@e.st").await;
	let admin = account(&store, "theadmin@e.st").await;
	let stranger = account(&store, "stranger@e.st").await;
	let auth = Auth::new(app.clone());
	let org = auth.create_org(&ctx_for(&owner), "Kft.", None).await.unwrap();
	store.put_membership(org.id, admin.id, Role::Admin).await.unwrap();
	store.accept_membership(org.id, admin.id, Timestamp::now()).await.unwrap();

	let uid = org.uid.as_str();
	for (who, code) in [(&admin, "E-AUTH-FORBIDDEN"), (&stranger, "E-CORE-NOTFOUND")] {
		let ctx = ctx_for(who);
		assert_eq!(auth.delete_org(&ctx, uid).await.unwrap_err().parts().1, code);
		assert_eq!(
			auth.transfer_ownership(&ctx, uid, who.uid.as_str())
				.await
				.unwrap_err()
				.parts()
				.1,
			code
		);
	}

	// And a stale credential is refused even for the owner: both are destructive.
	assert_eq!(
		auth.delete_org(&stale_ctx(&owner), uid).await.unwrap_err().parts().1,
		"E-AUTH-STEPUP"
	);
}

/// Erasure nulled `accounts.name` and placeholdered `accounts.email` but never touched
/// `orgs.name` — which `org::add_member` fills with the invitee's **full address**.
/// After a completed erasure the address was still sitting in the personal org's name,
/// and `export_account` dumps that table.
///
/// The sweep is over every `TEXT` column of both tables rather than the two that were known
/// to be wrong, so it also catches the next column someone forgets.
#[tokio::test]
async fn erasure_leaves_the_address_in_no_text_column() {
	let db = TmpDb::new("erasure-sweep");
	let (app, store) = setup(&db).await;
	let victim = account(&store, "victim@e.st").await;
	// The invite path's naming, which is the one that stored the full address.
	sqlx::query("UPDATE orgs SET name = ? WHERE owner_account_id = ? AND kind = 'PERSONAL'")
		.bind(&victim.email)
		.bind(victim.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	// An organisation the account merely owns keeps its trading name.
	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Org Kft.",
			victim.id,
			None,
		)
		.await
		.unwrap();
	let successor = account(&store, "successor@e.st").await;
	mintworks_core::refs::RefStore::ref_insert(
		&store,
		&mintworks_core::refs::NewRef {
			uid: mintworks_core::ids::RefId::generate(),
			code: "victim-invite".to_owned(),
			ref_type: "org_invite".to_owned(),
			org_id: org.id,
			created_by: Some(successor.id),
			target: None,
			email: Some(victim.email.clone()),
			params: serde_json::json!({}),
			uses_left: Some(1),
			expires_at: None,
		},
	)
	.await
	.unwrap();

	let auth = Auth::new(app.clone());
	let erase = async || auth.erase_account(&ctx_for(&victim), &victim.email).await;
	// While the account owns the organisation, the erasure is refused outright. Allowing
	// it orphaned the org permanently — `remove_member` refuses to remove an `OWNER` and
	// `set_member_role` refuses to assign one, so the ghost could never be replaced.
	let refused = erase().await.unwrap_err();
	assert_eq!(refused.parts().1, "E-AUTH-OWNER-ERASURE");
	assert!(refused.to_string().contains(org.uid.as_str()), "name the org uid: {refused}");
	let untouched = store.account_by_id(victim.id).await.unwrap().unwrap();
	assert_eq!(untouched.email, victim.email, "a refused erasure changed the accounts row");
	assert_eq!(untouched.status, AccountStatus::Active);

	// Ownership transferred, the erasure proceeds.
	sqlx::query("UPDATE orgs SET owner_account_id = ? WHERE id = ?")
		.bind(successor.id)
		.bind(org.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	let _erased = erase().await.unwrap();
	// The placeholder used to be built from `accounts.id`, and `members()` hands
	// `accounts.email` to any admin of an org the erased account had *accepted* — so
	// `GET /api/org/members` returned the internal integer key. Only `uid` may leave.
	let placeholder = store.account_by_id(victim.id).await.unwrap().unwrap().email;
	assert!(placeholder.contains(victim.uid.as_str()), "{placeholder}");
	assert!(
		!placeholder.contains(&format!("+{}@", victim.id)),
		"the internal accounts.id is in the placeholder: {placeholder}"
	);

	for table in ["accounts", "orgs", "refs"] {
		let columns: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
			"SELECT name FROM pragma_table_info('{table}') WHERE type = 'TEXT'"
		)))
		.fetch_all(store.read_pool())
		.await
		.unwrap();
		assert!(!columns.is_empty(), "{table} has no TEXT columns?");
		for column in columns {
			let hits: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
				"SELECT count(*) FROM {table} WHERE \"{column}\" LIKE ?"
			)))
			.bind(format!("%{}%", victim.email))
			.fetch_one(store.read_pool())
			.await
			.unwrap();
			assert_eq!(hits, 0, "{table}.{column} still holds the erased address");
		}
	}

	let survived: String = sqlx::query_scalar("SELECT name FROM orgs WHERE id = ?")
		.bind(org.id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(survived, "Org Kft.", "an organisation's trading name is not personal data");
}

/// `export_account` scoped by `orgs.owner_account_id` with no `kind = 'PERSONAL'`, so a
/// subject access request came back with every invoice, payment and key of every
/// organisation the account owns — rows created by *other members*. It was also reachable
/// with nothing but a stolen 15-minute access token.
#[tokio::test]
async fn the_export_is_stepped_up_and_stops_at_the_personal_org() {
	let db = TmpDb::new("export-scope");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "owner@e.st").await;
	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Org Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();
	let personal = store
		.orgs_for_account(owner.id)
		.await
		.unwrap()
		.into_iter()
		.find(|t| t.uid != org.uid)
		.expect("the personal org");

	let mut personal_id = 0;
	// One billing party under each org. The organisation's belongs to the organisation.
	for (org_uid, name) in [(&personal.uid, "Personal"), (&org.uid, "Someone Else Kft.")] {
		let id: i64 = sqlx::query_scalar("SELECT id FROM orgs WHERE uid = ?")
			.bind(org_uid.as_str())
			.fetch_one(store.read_pool())
			.await
			.unwrap();
		if org_uid == &personal.uid {
			personal_id = id;
		}
		sqlx::query(
			"INSERT INTO billing_parties
			 (uid, org_id, kind, name, country, is_default, created_at, updated_at)
			 VALUES (?, ?, 'C', ?, 'HU', 0, 0, 0)",
		)
		.bind(format!("prt_{name}"))
		.bind(id)
		.bind(name)
		.execute(store.write_pool())
		.await
		.unwrap();
	}

	// An object body under each org. Erasure blanks the personal one, which is what makes it
	// personal data — so Art. 15 has to hand back the same rows and no others.
	for (org_id, note) in [(personal_id, "Personal"), (org.id, "Someone Else Kft.")] {
		store
			.object_put(
				org_id,
				"invoice.ext",
				&format!("inv_{note}"),
				&serde_json::json!({ "note": note }),
				&[],
			)
			.await
			.unwrap();
	}

	let auth = Auth::new(app.clone());

	// A token past the step-up window is refused outright.
	let err = auth
		.export_account(&stale_ctx(&owner))
		.await
		.expect_err("a stale credential must not dump the database");
	assert_eq!(err.parts(), (StatusCode::UNAUTHORIZED, "E-AUTH-STEPUP"));

	let (_subject, doc) = auth.export_account(&ctx_for(&owner)).await.unwrap();
	let names: Vec<&str> = doc["billingParties"]
		.as_array()
		.unwrap()
		.iter()
		.map(|p| p["name"].as_str().unwrap())
		.collect();
	assert_eq!(names, vec!["Personal"], "the organisation's rows are not this subject's data");
	// Which organisations the person belongs to still is.
	assert_eq!(doc["orgs"].as_array().unwrap().len(), 2);

	// `body` is a TEXT column and exports verbatim, the way `audit_logs.detail` does.
	let bodies: Vec<&str> = doc["objects"]
		.as_array()
		.unwrap()
		.iter()
		.map(|o| o["body"].as_str().unwrap())
		.collect();
	assert_eq!(
		bodies,
		vec![r#"{"note":"Personal"}"#],
		"an object under an org the account merely owns"
	);
}

/// The contract names an `Auth` handle and there was none — every handler in `mintworks-auth` *was*
/// the service, wired to `State<App>`/`ConnectInfo`/`HeaderMap`. A consumer
/// could not register an account or invite a member from its own code, or from a job, without
/// constructing an axum request. This test is that capability: **no axum request anywhere.**
#[tokio::test]
async fn the_auth_handle_registers_and_creates_an_org_with_no_http_request() {
	let db = TmpDb::new("auth-handle");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	let auth = Auth::new(app.clone());

	// `Ctx::system` has no IP, so no proof of work is demanded and no IP-keyed bucket is
	// spent — the framework's own code is not the caller admission control exists for.
	let ctx = Ctx::system("consumer");
	auth.register(
		&ctx,
		&Registration {
			email: "inhouse@e.st".to_owned(),
			name: Some("In House".to_owned()),
			// `publish_legal` files both documents under `hu`.
			locale: Some("hu".to_owned()),
			consents: vec![
				serde_json::from_value(serde_json::json!({ "kind": "TOS", "version": "1" }))
					.unwrap(),
				serde_json::from_value(serde_json::json!({ "kind": "PRIVACY", "version": "1" }))
					.unwrap(),
			],
			..Registration::default()
		},
	)
	.await
	.unwrap();

	let created = store.account_by_email("inhouse@e.st").await.unwrap().expect("the account");
	assert_eq!(created.status, AccountStatus::Pending, "activation still has to happen");
	// Both consents were recorded against the person, not an org.
	let consents: i64 =
		sqlx::query_scalar("SELECT count(*) FROM consents WHERE account_id = ? AND org_id IS NULL")
			.bind(created.id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(consents, 2);

	// `register` creates an account, an org, an OWNER membership and the consent rows
	// that are legal evidence, and wrote nothing to `audit_logs` — while every other
	// account-lifecycle event in the handle does. Named against the account, not `System`,
	// or the row drops out of the subject's own GDPR export.
	let registered: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM audit_logs WHERE action = 'REGISTERED' AND account_id = ?",
	)
	.bind(created.id)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(registered, 1);

	// An org, as that account, still with no request in the loop.
	let org = auth.create_org(&ctx_for(&created), "Consumer Kft.", None).await.unwrap();
	assert_eq!(org.name, "Consumer Kft.");
	assert_eq!(
		store.accepted_membership_role(org.id, created.id).await.unwrap(),
		Some(mintworks_auth::store::Role::Owner)
	);

	// And inviting a member from Rust, which the route bundle now merely forwards to.
	let admin_ctx = Ctx { org_id: Some(org.id), ..ctx_for(&created) };
	auth.add_member(&admin_ctx, "invited@e.st", mintworks_auth::store::Role::Member)
		.await
		.unwrap();
	let invites = auth.invites(&admin_ctx).await.unwrap();
	assert_eq!(invites.len(), 1);
	assert_eq!(invites[0].email.as_deref(), Some("invited@e.st"));
	assert_eq!(invites[0].params["role"], "MEMBER");
	assert!(store.account_by_email("invited@e.st").await.unwrap().is_none(), "no account row");
}

/// Adding an address answers nothing about it. Any authenticated user can create a
/// org and become its admin, so `POST /api/org/members` is reachable by anyone for any
/// address — and the member list used to hand back that account's `email`, `name` and
/// `accounts.status`, which is a registration oracle plus PII disclosure.
#[tokio::test]
async fn a_pending_invitation_discloses_nothing_about_the_address() {
	let db = TmpDb::new("invite-pending");
	let (app, store) = setup(&db).await;
	let admin = account(&store, "admin@e.st").await;
	let victim = account(&store, "victim@e.st").await;
	let (org_id, admin_ctx) = org(&store, &admin, "Céges Kft.").await;

	let auth = Auth::new(app.clone());
	for email in [victim.email.as_str(), "never-heard-of@e.st"] {
		auth.add_member(&admin_ctx, email, mintworks_auth::store::Role::Member)
			.await
			.unwrap();
	}

	// The member listing holds no pending rows at all; the owner reads in full.
	let members = store.members(org_id, mintworks_auth::service_api::MAX_MEMBERS).await.unwrap();
	assert_eq!(members.len(), 1, "an invitation is not a membership");
	assert_eq!(members[0].email.as_str(), admin.email.as_str());

	// The two invitations are the same shape: nothing in them depends on the address being
	// registered, only the address the admin typed.
	let shape = |r: &mintworks_core::refs::Ref| {
		(r.ref_type.clone(), r.params.clone(), r.uses_left, r.status, r.target.clone())
	};
	let invites = auth.invites(&admin_ctx).await.unwrap();
	assert_eq!(invites.len(), 2);
	assert_eq!(shape(&invites[0]), shape(&invites[1]));
	assert!(store.account_by_email("never-heard-of@e.st").await.unwrap().is_none());
}

/// `create_org` committed the org row *and* its OWNER membership before checking
/// the currency, so a `400` left an org nobody could delete in `GET /api/orgs`.
#[tokio::test]
async fn a_rejected_currency_creates_no_org() {
	let db = TmpDb::new("org-currency");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "owner@e.st").await;
	let auth = Auth::new(app.clone());

	let before = store.orgs_for_account(owner.id).await.unwrap().len();
	let err = auth
		.create_org(&ctx_for(&owner), "Céges Kft.", Some(&CurrencyCode::parse("XXX").unwrap()))
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-CURRENCY-DISABLED");
	assert_eq!(
		store.orgs_for_account(owner.id).await.unwrap().len(),
		before,
		"the rejected call must leave no org behind"
	);
}

/// A `400 E-CORE-VALIDATION` for an unknown `org_` uid and a `403 E-AUTH-FORBIDDEN` for
/// a real org the caller is not in let any authenticated user enumerate valid uids by
/// diffing the status.
#[tokio::test]
async fn recording_a_consent_does_not_say_whether_the_org_exists() {
	let db = TmpDb::new("consent-oracle");
	let (app, store) = setup(&db).await;
	// `EINVOICE`, not `TOS`: the two gating kinds are refused with an `orgUid` before the
	// org is ever resolved, which would make both calls prove nothing.
	publish_legal(&store, LegalKind::EInvoice, "1").await;
	let outsider = account(&store, "outsider@e.st").await;
	let owner = account(&store, "owner@e.st").await;
	let real = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Céges Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();

	let auth = Auth::new(app.clone());
	let grant = |uid: String| ConsentGrant {
		kind: LegalKind::EInvoice,
		version: "1".to_owned(),
		// Required, and checked before the org is resolved — so it has to be the real
		// hash or both calls answer the validation error and prove nothing.
		doc_sha256: Some(format!("{:064x}", 0)),
		org_uid: Some(uid),
		user_agent: None,
	};

	let unknown = mintworks_core::prelude::OrgId::generate().into_string();
	let a = auth.record_consent(&ctx_for(&outsider), &grant(unknown)).await.unwrap_err();
	let b = auth
		.record_consent(&ctx_for(&outsider), &grant(real.uid.as_str().to_owned()))
		.await
		.unwrap_err();
	assert_eq!(a.parts(), b.parts(), "the two answers must be indistinguishable");
	assert_eq!(a.parts().1, "E-CORE-NOTFOUND");
	assert_eq!(a.to_string(), b.to_string());
}

/// `consents.org_id` was written and never read: `latest_consent` had no org predicate,
/// so `GET /api/consents` returned the latest row *per kind* across all orgs and
/// `DELETE /api/consents/{kind}` stamped that same row. Grant `EINVOICE` for org A and
/// then for org B and A's grant became invisible through the API and impossible to
/// withdraw — and `consents` rows are never deleted, not even by `gdpr::ERASURE`.
#[tokio::test]
async fn an_org_scoped_consent_is_listed_and_withdrawn_in_its_own_scope() {
	let db = TmpDb::new("consent-org-scope");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::EInvoice, "1").await;
	let owner = account(&store, "two-orgs@e.st").await;
	let (a, ctx_a) = org(&store, &owner, "A Kft.").await;
	let (b, ctx_b) = org(&store, &owner, "B Kft.").await;

	let auth = Auth::new(app.clone());
	let grant = |org: &str| ConsentGrant {
		kind: LegalKind::EInvoice,
		version: "1".to_owned(),
		doc_sha256: Some(format!("{:064x}", 0)),
		org_uid: Some(org.to_owned()),
		user_agent: None,
	};
	let uid = async |id: i64| -> String {
		sqlx::query_scalar("SELECT uid FROM orgs WHERE id = ?")
			.bind(id)
			.fetch_one(store.read_pool())
			.await
			.unwrap()
	};
	let (uid_a, uid_b) = (uid(a).await, uid(b).await);

	auth.record_consent(&ctx_a, &grant(&uid_a)).await.unwrap();
	auth.record_consent(&ctx_b, &grant(&uid_b)).await.unwrap();

	// Both scopes are listed, not just whichever was granted last.
	let listed = auth.list_consents(&ctx_a).await.unwrap();
	let scopes: Vec<Option<String>> = listed
		.iter()
		.filter(|c| c.kind == LegalKind::EInvoice)
		.map(|c| c.org_uid.clone())
		.collect();
	assert_eq!(scopes.len(), 2, "one org's grant shadowed the other: {scopes:?}");
	assert!(scopes.contains(&Some(uid_a.clone())) && scopes.contains(&Some(uid_b.clone())));

	// Withdrawing in B leaves A in force.
	auth.withdraw_consent(&ctx_b, LegalKind::EInvoice).await.unwrap();
	let after = auth.list_consents(&ctx_a).await.unwrap();
	let live = |uid: &str| {
		after
			.iter()
			.find(|c| c.org_uid.as_deref() == Some(uid))
			.map(|c| c.withdrawn_at.is_none())
	};
	assert_eq!(live(&uid_a), Some(true), "withdrawing in B took A's grant with it");
	assert_eq!(live(&uid_b), Some(false));
}

/// Another org's `accountUid` is `E-CORE-NOTFOUND`, never `403` — the same answer as
/// a uid that does not exist at all.
#[tokio::test]
async fn another_orgs_account_uid_is_not_found_not_forbidden() {
	let db = TmpDb::new("member-uid-scope");
	let (app, store) = setup(&db).await;
	let admin_a = account(&store, "a-admin@e.st").await;
	let owner_b = account(&store, "b-owner@e.st").await;
	let only_in_b = account(&store, "b-member@e.st").await;
	let (_a, _) = org(&store, &admin_a, "A Kft.").await;
	let (b, _) = org(&store, &owner_b, "B Kft.").await;
	store
		.put_membership(b, only_in_b.id, mintworks_auth::store::Role::Member)
		.await
		.unwrap();
	gate_satisfied(&store, &admin_a).await;

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "a-admin@e.st").await;
	let nowhere = mintworks_core::prelude::AccountId::generate().into_string();

	let mut answers = Vec::new();
	for uid in [only_in_b.uid.as_str(), nowhere.as_str()] {
		answers.push(
			call(
				&router,
				"PATCH",
				&format!("/api/org/members/{uid}"),
				&token,
				Some(serde_json::json!({ "role": "MEMBER" })),
			)
			.await,
		);
	}
	assert_eq!(answers[0].0, StatusCode::NOT_FOUND, "{:?}", answers[0].1);
	assert_eq!(answers[0], answers[1], "the two answers must be indistinguishable");
	assert_eq!(answers[0].1["error"]["errCode"], "E-CORE-NOTFOUND");
}

/// The consent rows used to be written *after* `create_account` committed, so a failure
/// there left an account with no ToS/privacy rows — which `consent::gate` then blocks on
/// every gated route, with no way back through the API.
#[tokio::test]
async fn an_account_and_its_consents_commit_together() {
	let db = TmpDb::new("register-atomic");
	let (_app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	let doc = store
		.current_legal_doc(LegalKind::Tos, "hu", Timestamp::now())
		.await
		.unwrap()
		.unwrap();

	let new = NewAccount {
		email: "atomic@e.st".to_owned(),
		pwd_hash: Some("argon2-placeholder".to_owned()),
		name: None,
		locale: "hu".to_owned(),
		org_name: "atomic@e.st".to_owned(),
	};
	let consent = |legal_doc_id| NewConsent {
		account_id: 0,
		org_id: None,
		kind: LegalKind::Tos,
		legal_doc_id,
		doc_version: doc.version.clone(),
		doc_sha256: doc.sha256.clone(),
		granted: true,
		ip: None,
		user_agent: None,
	};

	// A consent naming a `legal_docs` row that does not exist violates the foreign key, so
	// the whole write must roll back — account included.
	assert!(store.create_account(&new, &[consent(Some(9_999))]).await.is_err());
	assert!(
		store.account_by_email("atomic@e.st").await.unwrap().is_none(),
		"a failed consent write must leave no account behind"
	);

	// And the happy path really does write both.
	let (account, _) = store.create_account(&new, &[consent(Some(doc.id))]).await.unwrap();
	assert!(store.latest_consent(account.id, LegalKind::Tos, None).await.unwrap().is_some());
}

/// A login and a password reset used to write `audit_logs.account_id` NULL —
/// `Ctx::system` has no account — so the per-account index never saw a login and the
/// subject's own login history was missing from their GDPR export, which selects on that
/// column. The export itself must carry no internal key, no snake_case and no raw unix time.
#[tokio::test]
async fn the_export_carries_the_login_history_and_no_internals() {
	let db = TmpDb::new("export-shape");
	let (app, store) = setup(&db).await;
	let account = account(&store, "subject@e.st").await;
	let auth = Auth::new(app.clone());

	auth.login(&ip_ctx(), &credentials(&account.email, PASSWORD)).await.unwrap();

	let token = reset_token(&app, &store, "subject@e.st").await;
	auth.reset_password(&ip_ctx(), &token, None, None, "a-brand-new-password".to_owned())
		.await
		.unwrap();
	// Both rows name the account, which is what puts them in the export at all.
	let named: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM audit_logs WHERE account_id = ? AND action IN ('LOGIN',
		 'PASSWORD_RESET')",
	)
	.bind(account.id)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(named, 2, "LOGIN and PASSWORD_RESET must both carry account_id");

	let export = Auth::new(app.clone()).export_account(&ctx_for(&account)).await.unwrap().1;
	let actions: Vec<&str> = export["auditLog"]
		.as_array()
		.unwrap()
		.iter()
		.filter_map(|r| r["action"].as_str())
		.collect();
	assert!(actions.contains(&"LOGIN"), "{export}");
	assert!(actions.contains(&"PASSWORD_RESET"), "{export}");
	// The subject still gets their own identity and consent history …
	let me = &export["accounts"][0];
	assert_eq!(me["uid"], account.uid.as_str());
	assert_eq!(me["email"], account.email);
	assert!(me["createdAt"].as_str().unwrap().ends_with('Z'), "{me}");

	// … and nothing else. Walk every object in every array.
	let mut checked = 0;
	for (table, rows) in export.as_object().unwrap() {
		for row in rows.as_array().unwrap() {
			for (key, value) in row.as_object().unwrap() {
				checked += 1;
				assert_ne!(key, "id", "{table}.{key} is an internal key");
				assert!(!key.ends_with("_id"), "{table}.{key} is an internal key");
				assert!(!key.contains('_'), "{table}.{key} is not camelCase");
				if key == "at" || key.ends_with("At") {
					assert!(
						value.is_null() || value.as_str().is_some_and(|v| v.ends_with('Z')),
						"{table}.{key} is not ISO-8601: {value}"
					);
				}
			}
		}
	}
	assert!(checked > 20, "the walk must have seen the rows: {checked}");
	for hidden in ["tokenEpoch", "failedLogins", "lockedUntil", "isOperator", "pwdHash"] {
		assert!(me.get(hidden).is_none(), "{hidden} is not the subject's personal data: {me}");
	}
}

/// The `auditLog` section exported `entity_id` and `detail` verbatim, and `MEMBER_ADDED`,
/// `MEMBER_ROLE_CHANGED`, `MEMBER_REMOVED` and `ORG_OWNER_CHANGED` all carry a third party's
/// `acc_` uid in one or the other — the same ULID-timestamp decode as the invite oracle, from
/// the admin's own subject access request. `orgs.owner_account_id` was dropped for exactly
/// this reason two sections above.
#[tokio::test]
async fn the_export_names_no_other_account() {
	let db = TmpDb::new("export-third-party");
	let (app, store) = setup(&db).await;
	let admin = account(&store, "exporter@e.st").await;
	let invitee = account(&store, "invited@e.st").await;
	let auth = Auth::new(app.clone());

	let org = auth.create_org(&ctx_for(&admin), "Org Kft.", None).await.unwrap();
	let ctx = Ctx { org_id: Some(org.id), ..ctx_for(&admin) };
	auth.add_member(&ctx, &invitee.email, Role::Member).await.unwrap();
	let code = auth.invites(&ctx).await.unwrap()[0].code.clone();
	auth.accept_invite(&ctx_for(&invitee), &code).await.unwrap();
	auth.transfer_ownership(&ctx, org.uid.as_str(), invitee.uid.as_str())
		.await
		.unwrap();

	let export = auth.export_account(&ctx_for(&admin)).await.unwrap().1;
	let doc = export.to_string();
	assert!(doc.contains(admin.uid.as_str()), "the subject's own uid belongs here: {doc}");
	assert!(
		!doc.contains(invitee.uid.as_str()),
		"another account's uid is not the subject's personal data: {doc}"
	);

	// …and the masking is per row, not per column. Dropping `entity_id` wholesale left every
	// action that passes `detail: None` as an unidentifiable stub: a `ORG_CREATED` with no
	// say which org. `entity = 'org'` is the subject's own.
	let created = export["auditLog"]
		.as_array()
		.unwrap()
		.iter()
		.find(|r| r["action"] == "ORG_CREATED")
		.expect("the subject created this org");
	assert_eq!(created["entityId"], serde_json::json!(org.uid.as_str()), "{created}");
	// The membership entry in the same export is the one that stays blank.
	let transferred = export["auditLog"]
		.as_array()
		.unwrap()
		.iter()
		.find(|r| r["action"] == "ORG_OWNER_CHANGED")
		.expect("the subject handed the organisation over");
	assert!(transferred["entityId"].is_null(), "{transferred}");
}

/// `add_member` only asked for org-admin, and the owner of their own personal org
/// is `OWNER`. A second member in a `kind='PERSONAL'` org breaks the single-occupant assumption
/// `AuthStore::export_account` and `anonymize_account` scope by, so the owner's GDPR export
/// would have carried the other member's invoices and their erasure would have overwritten
/// the other member's billing parties.
#[tokio::test]
async fn a_personal_org_refuses_a_second_member() {
	let db = TmpDb::new("personal-no-members");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "solo@e.st").await;

	let personal = store
		.orgs_for_account(owner.id)
		.await
		.unwrap()
		.into_iter()
		.find(|t| t.kind == mintworks_auth::store::OrgKind::Personal)
		.expect("an account is created with its personal org");
	let personal = store.org_by_uid(&personal.uid).await.unwrap().unwrap();

	let err = Auth::new(app.clone())
		.add_member(
			&ctx_for(&owner).with_org(personal.id),
			"intruder@e.st",
			mintworks_auth::store::Role::Member,
		)
		.await
		.unwrap_err();
	let (status, body) = parts(err.into_response()).await;
	assert_eq!(status, StatusCode::CONFLICT, "{body}");
	assert_eq!(
		store
			.members(personal.id, mintworks_auth::service_api::MAX_MEMBERS)
			.await
			.unwrap()
			.len(),
		1,
		"the personal org must keep exactly one membership"
	);
	// And the address must not have been created on the way in.
	assert!(store.account_by_email("intruder@e.st").await.unwrap().is_none());
}

/// Activation establishes an identity and mints a session, and wrote no `audit_logs` row —
/// so the subject's GDPR export opened with no trace of the account ever being activated.
#[tokio::test]
async fn activation_is_in_the_subjects_own_audit_history() {
	let db = TmpDb::new("activate-audit");
	let (app, store) = setup(&db).await;
	app.settings.set("pow.difficulty.resend-activation", "1", None).await.unwrap();

	let (account, _) = store
		.create_account(
			&NewAccount {
				email: "fresh@e.st".to_owned(),
				// Registration no longer takes a password, so every `PENDING` account has a
				// NULL hash and sets one by activating.
				pwd_hash: None,
				name: None,
				locale: "hu".to_owned(),
				org_name: "fresh@e.st".to_owned(),
			},
			&[],
		)
		.await
		.unwrap();

	// The link comes out of the mail the handle queues, rather than `activate::mint` behind
	// its back: the account is `PENDING`, which is the resend path's hit branch.
	let auth = Auth::new(app.clone());
	let challenge = pow::issue(&app, "resend-activation").await.unwrap();
	auth.resend_activation(&ip_ctx(), "fresh@e.st", Some(&solve(&challenge)))
		.await
		.unwrap();
	let token = link_from_queued_mail(&app, &store, "activation_link").await;

	auth.activate(&ip_ctx(), &token, Some(PASSWORD.to_owned())).await.unwrap();

	let logged: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM audit_logs WHERE account_id = ? AND action = 'ACTIVATED'",
	)
	.bind(account.id)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(logged, 1, "activation must name the account it activated");
}

/// `current_legal_doc` matched the locale exactly and `token::consents_required` read a
/// miss as "nothing to consent to yet", so a deployment publishing its ToS and privacy policy
/// in `hu` only gave every `en` account full access having accepted nothing at all.
#[tokio::test]
async fn a_document_published_in_one_locale_still_gates_every_account() {
	let db = TmpDb::new("consent-locale");
	let (app, store) = setup(&db).await;
	// `publish_legal` files under `hu` only.
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;

	let (english, _) = store
		.create_account(
			&NewAccount {
				email: "english@e.st".to_owned(),
				pwd_hash: None,
				name: None,
				locale: "en".to_owned(),
				org_name: "english@e.st".to_owned(),
			},
			&[],
		)
		.await
		.unwrap();
	let me = Auth::new(app.clone()).me(&ctx_for(&english)).await.unwrap();
	assert!(
		!me.consents_required.is_empty(),
		"a kind published in *any* locale must gate; this account owes nothing and has \
		 accepted nothing"
	);
}

/// The mirror: `register::check_consents` demanded a current document in the account's
/// exact locale, so self-registration with an unpublished one was permanently
/// `E-AUTH-CONSENT-REQUIRED` — impossible, not merely awkward.
#[tokio::test]
async fn registering_in_an_unpublished_locale_falls_back_rather_than_failing() {
	let db = TmpDb::new("consent-fallback");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;

	Auth::new(app.clone())
		.register(
			&Ctx::system("test"),
			&Registration {
				email: "english@e.st".to_owned(),
				name: None,
				locale: Some("en".to_owned()),
				consents: vec![
					serde_json::from_value(serde_json::json!({ "kind": "TOS", "version": "1" }))
						.unwrap(),
					serde_json::from_value(
						serde_json::json!({ "kind": "PRIVACY", "version": "1" }),
					)
					.unwrap(),
				],
				..Registration::default()
			},
		)
		.await
		.expect("the hu documents are the ones an en account consents to");
}

/// The fallback is a *fallback*. An exact match must still win, or publishing a
/// translation would be undone by whichever row sorts last.
#[tokio::test]
async fn an_exact_locale_match_beats_the_fallback() {
	let db = TmpDb::new("consent-exact");
	let (_app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await; // hu
	store
		.insert_legal_doc(&NewLegalDoc {
			kind: LegalKind::Tos,
			locale: "en".to_owned(),
			version: "1-en".to_owned(),
			title: "TOS 1 en".to_owned(),
			body: "…".to_owned(),
			sha256: format!("{:064x}", 1),
			// Later than the hu row, so `ORDER BY effective_from DESC` would pick it.
			effective_from: Timestamp(10),
		})
		.await
		.unwrap();

	let hu = store
		.current_legal_doc(LegalKind::Tos, "hu", Timestamp::now())
		.await
		.unwrap()
		.expect("the hu document");
	assert_eq!(hu.locale, "hu", "an hu account must get the hu row, not the newer en one");
}

/// `/api/auth/step-up` sat behind the consent gate, and both routes `consent_exempt`
/// deliberately holds — `/api/account/export` and `/api/account/delete` — call
/// `stepup::require`. A user owing a new ToS whose session was older than
/// `auth.stepup_window` got `401 E-AUTH-STEPUP` from the erasure route and `403
/// E-AUTH-CONSENT-REQUIRED` from the only route that could fix it. That is the GDPR erasure
/// path, and the only way out was a full re-login.
#[tokio::test]
async fn step_up_is_reachable_from_behind_the_consent_wall() {
	let db = TmpDb::new("stepup-consent");
	let (app, store) = setup(&db).await;
	let account = account(&store, "owes-tos@e.st").await;
	let auth = Auth::new(app.clone());
	// Published *after* the account was created, so it owes a consent it has not given.
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	assert!(
		!auth.me(&ctx_for(&account)).await.unwrap().consents_required.is_empty(),
		"the setup must owe a consent"
	);

	// The erasure route is exempt but demands step-up, and this session is well outside the
	// window.
	assert_eq!(
		auth.erase_account(&stale_ctx(&account), &account.email)
			.await
			.unwrap_err()
			.parts()
			.1,
		"E-AUTH-STEPUP",
		"the setup must be the one step-up exists to resolve"
	);

	// The gate is a router layer, so drive the bundle rather than the handle.
	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "owes-tos@e.st").await;
	let (status, body) = call(
		&router,
		"POST",
		"/api/auth/step-up",
		&token,
		Some(serde_json::json!({ "password": PASSWORD })),
	)
	.await;
	assert_eq!(
		status,
		StatusCode::OK,
		"step-up must not be consent-gated; got {}",
		body["error"]["errCode"]
	);

	// And the token it returns unblocks the exempt destructive route.
	let fresh = Ctx { auth_at: Some(Timestamp::now().0), ..ctx_for(&account) };
	auth.erase_account(&fresh, &account.email)
		.await
		.expect("a stepped-up session reaches the erasure it was blocked from");
}

/// `erase_account` refuses an account that still owns an organisation and names two routes as
/// the way out. Both sat in `consent_gated`, so publishing a new ToS made closing an account
/// conditional on accepting it — exactly what the exempt set exists to prevent.
#[tokio::test]
async fn owner_can_erase_while_a_new_tos_is_outstanding() {
	let db = TmpDb::new("owner-erasure-consent");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "owner-owes@e.st").await;
	let auth = Auth::new(app.clone());
	let org = auth.create_org(&ctx_for(&owner), "Org Kft.", None).await.unwrap();

	// Published after the account was created, so it owes a consent it has not given.
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;

	// The gate is a router layer, so drive the bundle rather than the handle.
	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "owner-owes@e.st").await;

	let (status, body) =
		call(&router, "DELETE", &format!("/api/orgs/{}", org.uid.as_str()), &token, None).await;
	assert_eq!(status, StatusCode::NO_CONTENT, "{}", body["error"]["errCode"]);

	let (status, body) = call(
		&router,
		"POST",
		"/api/account/delete",
		&token,
		Some(serde_json::json!({ "confirmEmail": owner.email })),
	)
	.await;
	assert_eq!(status, StatusCode::OK, "{}", body["error"]["errCode"]);
}

/// `add_member` spent the shared `register` bucket (3/h/ip) *before* `admin_of`. Org
/// creation is self-service, so any authenticated account could POST the route three times
/// and lock every genuine registration from its source address for an hour — one office NAT
/// is one address.
#[tokio::test]
async fn a_refused_invitation_does_not_spend_the_registration_budget() {
	let db = TmpDb::new("invite-budget");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	let owner = account(&store, "owner@e.st").await;
	let outsider = account(&store, "outsider@e.st").await;
	let (org_id, _) = org(&store, &owner, "Céges Kft.").await;

	let auth = Auth::new(app.clone());
	let intruder = Ctx { org_id: Some(org_id), ip: Some(PEER.ip()), ..ctx_for(&outsider) };
	for _ in 0..4 {
		// `E-CORE-NOTFOUND`, not 403: an org the caller is not a member of does not exist
		// as far as they are concerned. What matters here is that it is not `429`, and that
		// it costs the shared bucket nothing.
		assert_eq!(
			auth.add_member(&intruder, "victim@e.st", mintworks_auth::store::Role::Member)
				.await
				.unwrap_err()
				.parts()
				.1,
			"E-CORE-NOTFOUND",
			"a non-admin is refused before the bucket is charged"
		);
	}

	let registration = Registration {
		email: "genuine@e.st".to_owned(),
		name: None,
		locale: Some("hu".to_owned()),
		consents: vec![
			serde_json::from_value(serde_json::json!({ "kind": "TOS", "version": "1" })).unwrap(),
			serde_json::from_value(serde_json::json!({ "kind": "PRIVACY", "version": "1" }))
				.unwrap(),
		],
		..Registration::default()
	};
	let ctx = Ctx { ip: Some(PEER.ip()), ..Ctx::system("test") };
	// `register` demands a proof of work for an IP-bearing caller; the bucket is checked
	// first, which is all this is asserting about.
	let err = auth.register(&ctx, &registration).await.unwrap_err();
	assert_ne!(
		err.parts().0,
		StatusCode::TOO_MANY_REQUESTS,
		"the register bucket was drained by refusals that never authorized"
	);
}

/// A consented registration for `email` carrying `ref_code`.
fn signup_with(email: &str, ref_code: Option<&str>) -> Registration {
	Registration {
		email: email.to_owned(),
		locale: Some("hu".to_owned()),
		consents: vec![
			serde_json::from_value(serde_json::json!({ "kind": "TOS", "version": "1" })).unwrap(),
			serde_json::from_value(serde_json::json!({ "kind": "PRIVACY", "version": "1" }))
				.unwrap(),
		],
		ref_code: ref_code.map(str::to_owned),
		..Registration::default()
	}
}

/// A new address registers with the invitation's code, and activating joins the org —
/// even with registration `closed`, which only an `org_invite` gets through.
#[tokio::test]
async fn an_invited_address_registers_with_the_code_and_activation_joins_the_org() {
	let db = TmpDb::new("invited-then-registers");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	let auth = Auth::new(app.clone());

	let admin = account(&store, "admin@e.st").await;
	let (org_id, admin_ctx) = org(&store, &admin, "Céges Kft.").await;
	auth.add_member(&admin_ctx, "invited@e.st", mintworks_auth::store::Role::Admin)
		.await
		.unwrap();
	let code = auth.invites(&admin_ctx).await.unwrap()[0].code.clone();
	app.settings.set("auth.registration", "closed", None).await.unwrap();

	auth.register(&Ctx::system("test"), &signup_with("invited@e.st", Some(&code)))
		.await
		.unwrap();
	let invitee = store.account_by_email("invited@e.st").await.unwrap().unwrap();
	assert_eq!(invitee.status, AccountStatus::Pending);
	assert!(invitee.pwd_hash.is_none(), "a PENDING account never carries a hash");

	let token = link_from_queued_mail(&app, &store, "activation_link").await;
	auth.activate(&Ctx::system("test"), &token, Some(PASSWORD.to_owned()))
		.await
		.unwrap();
	assert_eq!(
		store.accepted_membership_role(org_id, invitee.id).await.unwrap(),
		Some(Role::Admin),
		"the role the invitation carried"
	);
	// Spent: nobody else registers on it.
	let r = auth.invites(&admin_ctx).await.unwrap().remove(0);
	assert_eq!(r.uses_left, Some(0));

	let outcome = auth
		.login(&Ctx::system("test"), &credentials("invited@e.st", PASSWORD))
		.await
		.unwrap();
	assert!(matches!(outcome, LoginOutcome::Signed(_)));
}

#[tokio::test]
async fn a_failed_invited_join_leaves_the_activation_retryable() {
	let db = TmpDb::new("invited-join-fails");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	let auth = Auth::new(app.clone());

	let admin = account(&store, "admin@e.st").await;
	let (org_id, admin_ctx) = org(&store, &admin, "Céges Kft.").await;
	auth.add_member(&admin_ctx, "invited@e.st", Role::Member).await.unwrap();
	let code = auth.invites(&admin_ctx).await.unwrap()[0].code.clone();
	auth.register(&Ctx::system("test"), &signup_with("invited@e.st", Some(&code)))
		.await
		.unwrap();
	let invitee = store.account_by_email("invited@e.st").await.unwrap().unwrap();
	let token = link_from_queued_mail(&app, &store, "activation_link").await;

	sqlx::query(
		"CREATE TRIGGER fail_join BEFORE INSERT ON memberships
		 BEGIN SELECT RAISE(ABORT, 'forced'); END",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	assert!(
		auth.activate(&Ctx::system("test"), &token, Some(PASSWORD.to_owned()))
			.await
			.is_err()
	);
	let still = store.account_by_email("invited@e.st").await.unwrap().unwrap();
	assert_eq!(still.status, AccountStatus::Pending, "the join runs before the flip");

	sqlx::query("DROP TRIGGER fail_join").execute(store.write_pool()).await.unwrap();
	auth.activate(&Ctx::system("test"), &token, Some(PASSWORD.to_owned()))
		.await
		.unwrap();
	assert_eq!(
		store.accepted_membership_role(org_id, invitee.id).await.unwrap(),
		Some(Role::Member)
	);
}

/// With no operator, boot mints one ROOT `org_invite` (reused across restarts); registering
/// with it makes the operator, after which nothing more is minted.
#[tokio::test]
async fn bootstrap_operator_invite_makes_the_first_operator_once() {
	let db = TmpDb::new("bootstrap-operator");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	app.settings.set("auth.registration", "invite", None).await.unwrap();
	let auth = Auth::new(app.clone());
	let root = app.store.root_org_id().await.unwrap();
	let refs = mintworks_core::refs::Refs::from_app(&app).unwrap();
	let invites = || async { refs.of_org(root, Some("org_invite")).await.unwrap() };

	mintworks_auth::bootstrap_operator(&app).await.unwrap();
	mintworks_auth::bootstrap_operator(&app).await.unwrap();
	let minted = invites().await;
	assert_eq!(minted.len(), 1, "a restart reuses the live invite");
	assert_eq!(minted[0].params["role"], "ADMIN");

	auth.register(&Ctx::system("test"), &signup_with("first@e.st", Some(&minted[0].code)))
		.await
		.unwrap();
	let token = link_from_queued_mail(&app, &store, "activation_link").await;
	auth.activate(&Ctx::system("test"), &token, Some(PASSWORD.to_owned()))
		.await
		.unwrap();
	let operator = store.account_by_email("first@e.st").await.unwrap().unwrap();
	assert_eq!(store.accepted_membership_role(root, operator.id).await.unwrap(), Some(Role::Admin));

	mintworks_auth::bootstrap_operator(&app).await.unwrap();
	assert_eq!(invites().await.len(), 1, "an operator exists: nothing more is minted");

	// Removing that operator is an operator decision: the spent bootstrap ref keeps boot quiet.
	sqlx::query("DELETE FROM memberships WHERE org_id = ? AND account_id = ?")
		.bind(root)
		.bind(operator.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	mintworks_auth::bootstrap_operator(&app).await.unwrap();
	assert_eq!(invites().await.len(), 1, "a used bootstrap ref mints nothing more");
}

#[tokio::test]
async fn invite_mode_refuses_an_unknown_ref_at_register() {
	let db = TmpDb::new("invite-mode-refusal");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	app.settings.set("auth.registration", "invite", None).await.unwrap();
	let auth = Auth::new(app.clone());

	let err = auth
		.register(&Ctx::system("test"), &signup_with("hopeful@e.st", Some("NOSUCHCODE12")))
		.await
		.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::FORBIDDEN, "E-AUTH-INVITE-REQUIRED"));
	assert!(store.account_by_email("hopeful@e.st").await.unwrap().is_none());
}

#[tokio::test]
async fn closed_mode_refuses_a_bogus_ref_and_creates_no_account() {
	let db = TmpDb::new("closed-mode-bogus-ref");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	app.settings.set("auth.registration", "closed", None).await.unwrap();
	let root = store.root_org_id().await.unwrap();
	admitting_affiliate(&store, root, "not-an-invite", None).await;
	let auth = Auth::new(app.clone());

	for code in ["NOSUCHCODE12", "not-an-invite"] {
		let err = auth
			.register(&Ctx::system("test"), &signup_with("hopeful@e.st", Some(code)))
			.await
			.unwrap_err();
		assert_eq!(err.parts(), (StatusCode::FORBIDDEN, "E-AUTH-CLOSED"), "{code}");
	}
	assert!(store.account_by_email("hopeful@e.st").await.unwrap().is_none());
}

/// Accepting an invitation into the org one owns must not demote the owner nor spend it.
#[tokio::test]
async fn the_owner_accepting_an_invite_to_their_org_is_refused() {
	let db = TmpDb::new("owner-accepts-invite");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let owner = account(&store, "owner@e.st").await;
	let (org_id, owner_ctx) = org(&store, &owner, "Céges Kft.").await;
	auth.add_member(&owner_ctx, "owner@e.st", Role::Member).await.unwrap();
	let code = auth.invites(&owner_ctx).await.unwrap()[0].code.clone();

	let err = auth.accept_invite(&owner_ctx, &code).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-CONFLICT");
	assert_eq!(store.membership_role(org_id, owner.id).await.unwrap(), Some(Role::Owner));
	assert_eq!(auth.invites(&owner_ctx).await.unwrap()[0].uses_left, Some(1));
}

#[tokio::test]
async fn a_revoked_invite_cannot_be_accepted_again() {
	let db = TmpDb::new("revoked-invite-reuse");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let admin = account(&store, "admin@e.st").await;
	let invitee = account(&store, "invited@e.st").await;
	let org = auth.create_org(&ctx_for(&admin), "Org Kft.", None).await.unwrap();
	let ctx = Ctx { org_id: Some(org.id), ..ctx_for(&admin) };
	auth.add_member(&ctx, &invitee.email, Role::Admin).await.unwrap();
	let invite = auth.invites(&ctx).await.unwrap()[0].clone();
	auth.accept_invite(&ctx_for(&invitee), &invite.code).await.unwrap();

	auth.set_member_role(&ctx, invitee.uid.as_str(), Role::Member).await.unwrap();
	mintworks_core::refs::Refs::from_app(&app)
		.unwrap()
		.revoke(&ctx, &invite.uid)
		.await
		.unwrap();
	let err = auth.accept_invite(&ctx_for(&invitee), &invite.code).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-INVITE-EXPIRED");
	assert_eq!(store.membership_role(org.id, invitee.id).await.unwrap(), Some(Role::Member));
}

#[tokio::test]
async fn an_invite_with_a_lower_role_does_not_demote_an_admin() {
	let db = TmpDb::new("invite-no-demote");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let owner = account(&store, "owner@e.st").await;
	let admin = account(&store, "admin@e.st").await;
	let org = auth.create_org(&ctx_for(&owner), "Org Kft.", None).await.unwrap();
	let ctx = Ctx { org_id: Some(org.id), ..ctx_for(&owner) };
	auth.add_member(&ctx, &admin.email, Role::Admin).await.unwrap();
	let first = auth.invites(&ctx).await.unwrap()[0].code.clone();
	auth.accept_invite(&ctx_for(&admin), &first).await.unwrap();

	let req = mintworks_core::refs::CreateRef {
		ref_type: "org_invite".to_owned(),
		email: Some(admin.email.clone()),
		params: Some(serde_json::json!({ "role": "MEMBER" })),
		..mintworks_core::refs::CreateRef::default()
	};
	let refs = mintworks_core::refs::Refs::from_app(&app).unwrap();
	let member_invite = refs.mint(&ctx, &req).await.unwrap();
	auth.accept_invite(&ctx_for(&admin), &member_invite.code).await.unwrap();
	assert_eq!(store.membership_role(org.id, admin.id).await.unwrap(), Some(Role::Admin));
}

/// The account's ctx acting in its personal org.
async fn personal(store: &SqliteStore, account: &mintworks_auth::store::Account) -> Ctx {
	let org: i64 =
		sqlx::query_scalar("SELECT id FROM orgs WHERE owner_account_id = ? AND kind = 'PERSONAL'")
			.bind(account.id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	Ctx { org_id: Some(org), ..ctx_for(account) }
}

/// Every account owns a personal org, so under `invite` anyone could mint their way in.
#[tokio::test]
async fn invite_only_refuses_signup_refs_from_a_personal_org() {
	let db = TmpDb::new("invite-personal-refused");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let user = account(&store, "user@e.st").await;
	let ctx = personal(&store, &user).await;
	let req = mintworks_core::refs::CreateRef::default();
	auth.create_signup_ref(&ctx, &req).await.unwrap();

	app.settings.set("auth.registration", "invite", None).await.unwrap();
	let err = auth.create_signup_ref(&ctx, &req).await.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::FORBIDDEN, "E-AUTH-FORBIDDEN"));
	let (_org_id, shared) = org(&store, &user, "Céges Kft.").await;
	auth.create_signup_ref(&shared, &req).await.unwrap();
}

#[tokio::test]
async fn invite_personal_allows_referral_only_signup() {
	let db = TmpDb::new("invite-personal-allowed");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	app.settings.set("auth.registration", "invite", None).await.unwrap();
	app.settings.set("auth.invite_personal", "true", None).await.unwrap();
	let user = account(&store, "user@e.st").await;
	let ctx = personal(&store, &user).await;
	let r = auth
		.create_signup_ref(&ctx, &mintworks_core::refs::CreateRef::default())
		.await
		.unwrap();
	assert_eq!(r.ref_type, "signup");
}

/// An `affiliate` ref owned by `org_id` whose params say it admits.
async fn admitting_affiliate(store: &SqliteStore, org_id: i64, code: &str, email: Option<&str>) {
	use mintworks_core::refs::{NewRef, RefStore};
	store
		.ref_insert(&NewRef {
			uid: mintworks_core::ids::RefId::generate(),
			code: code.to_owned(),
			ref_type: "affiliate".to_owned(),
			org_id,
			created_by: None,
			target: None,
			email: email.map(str::to_owned),
			params: serde_json::json!({ "admits": true }),
			uses_left: None,
			expires_at: None,
		})
		.await
		.unwrap();
}

/// Any org admin mints affiliates, so `params.admits` counts only on a root-owned one.
#[tokio::test]
async fn invite_mode_ignores_admits_on_a_user_minted_affiliate_ref() {
	let db = TmpDb::new("invite-mode-affiliate");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	app.settings.set("auth.registration", "invite", None).await.unwrap();
	let auth = Auth::new(app.clone());
	let admin = account(&store, "admin@e.st").await;
	let (org_id, _) = org(&store, &admin, "Céges Kft.").await;
	admitting_affiliate(&store, org_id, "user-minted", None).await;
	admitting_affiliate(&store, store.root_org_id().await.unwrap(), "root-minted", None).await;

	let err = auth
		.register(&Ctx::system("test"), &signup_with("forged@e.st", Some("user-minted")))
		.await
		.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::FORBIDDEN, "E-AUTH-INVITE-REQUIRED"));

	auth.register(&Ctx::system("test"), &signup_with("real@e.st", Some("root-minted")))
		.await
		.unwrap();
	let token = link_from_queued_mail(&app, &store, "activation_link").await;
	auth.activate(&Ctx::system("test"), &token, Some(PASSWORD.to_owned()))
		.await
		.unwrap();
	let account = store.account_by_email("real@e.st").await.unwrap().unwrap();
	assert_eq!(account.status, AccountStatus::Active);
}

/// The ref's `email` addresses the mail; it does not restrict who registers with the code.
#[tokio::test]
async fn an_addressed_ref_admits_another_email() {
	let db = TmpDb::new("affiliate-email");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	app.settings.set("auth.registration", "invite", None).await.unwrap();
	let root = store.root_org_id().await.unwrap();
	admitting_affiliate(&store, root, "addressed", Some("other@e.st")).await;
	let auth = Auth::new(app.clone());

	auth.register(&Ctx::system("test"), &signup_with("real@e.st", Some("addressed")))
		.await
		.unwrap();
	let token = link_from_queued_mail(&app, &store, "activation_link").await;
	auth.activate(&Ctx::system("test"), &token, Some(PASSWORD.to_owned()))
		.await
		.unwrap();
	let account = store.account_by_email("real@e.st").await.unwrap().unwrap();
	assert_eq!(account.status, AccountStatus::Active);
}

/// Re-registering a pending address must not swap the ref its owner registered with.
#[tokio::test]
async fn re_registering_a_pending_address_keeps_the_first_ref() {
	let db = TmpDb::new("pending-ref-first-wins");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	let root = store.root_org_id().await.unwrap();
	admitting_affiliate(&store, root, "first-ref", None).await;
	admitting_affiliate(&store, root, "second-ref", None).await;
	let auth = Auth::new(app.clone());

	auth.register(&Ctx::system("test"), &signup_with("twice@e.st", Some("first-ref")))
		.await
		.unwrap();
	auth.register(&Ctx::system("test"), &signup_with("twice@e.st", Some("second-ref")))
		.await
		.unwrap();
	let account = store.account_by_email("twice@e.st").await.unwrap().unwrap();
	let pending = store.pending_ref(account.id).await.unwrap().unwrap();
	let first = mintworks_core::refs::RefStore::ref_by_code(&store, "first-ref")
		.await
		.unwrap()
		.unwrap();
	assert_eq!(pending, first.uid);
}

/// A stranger re-registering must not attach a ref to a pending account registered without one.
#[tokio::test]
async fn re_registering_never_attaches_a_ref_to_a_bare_pending_account() {
	let db = TmpDb::new("pending-ref-bare");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	let root = store.root_org_id().await.unwrap();
	admitting_affiliate(&store, root, "stranger-ref", None).await;
	let auth = Auth::new(app.clone());

	auth.register(&Ctx::system("test"), &signup_with("bare@e.st", None))
		.await
		.unwrap();
	auth.register(&Ctx::system("test"), &signup_with("bare@e.st", Some("stranger-ref")))
		.await
		.unwrap();
	let account = store.account_by_email("bare@e.st").await.unwrap().unwrap();
	assert_eq!(store.pending_ref(account.id).await.unwrap(), None);
}

/// `signup` and `org_invite` refs are minted through `mintworks-auth`, which applies
/// `auth.invite_by` and the `InviteGate`; the generic refs service refuses them.
#[tokio::test]
async fn the_generic_refs_service_refuses_the_auth_types() {
	let db = TmpDb::new("refs-auth-types");
	let (app, store) = setup(&db).await;
	let admin = account(&store, "admin@e.st").await;
	let (_org_id, admin_ctx) = org(&store, &admin, "Céges Kft.").await;
	let refs = mintworks_core::refs::Refs::from_app(&app).unwrap();
	for ref_type in ["signup", "org_invite"] {
		let req = mintworks_core::refs::CreateRef {
			ref_type: ref_type.to_owned(),
			..mintworks_core::refs::CreateRef::default()
		};
		let err = refs.create(&admin_ctx, &req).await.unwrap_err();
		assert_eq!(err.parts(), (StatusCode::UNPROCESSABLE_ENTITY, "E-CORE-REF-TYPE"));
	}
	// The auth route mints one.
	let r = Auth::new(app.clone())
		.create_signup_ref(&admin_ctx, &mintworks_core::refs::CreateRef::default())
		.await
		.unwrap();
	assert_eq!(r.ref_type, "signup");
}

/// An unlimited referral is a reward any burner account farms, so the referral types default
/// to one use; a coupon is meant for many orgs and stays unlimited.
#[tokio::test]
async fn a_signup_ref_defaults_to_one_use() {
	let db = TmpDb::new("refs-default-uses");
	let (app, store) = setup(&db).await;
	let admin = account(&store, "admin@e.st").await;
	let (_org_id, admin_ctx) = org(&store, &admin, "Céges Kft.").await;
	let auth = Auth::new(app.clone());
	let refs = mintworks_core::refs::Refs::from_app(&app).unwrap();
	let of = |ref_type: &str, uses_left| mintworks_core::refs::CreateRef {
		ref_type: ref_type.to_owned(),
		uses_left,
		..mintworks_core::refs::CreateRef::default()
	};
	let signup = auth.create_signup_ref(&admin_ctx, &of("signup", None)).await.unwrap();
	assert_eq!(signup.uses_left, Some(1));
	let many = auth.create_signup_ref(&admin_ctx, &of("signup", Some(5))).await.unwrap_err();
	assert_eq!(many.parts().0, StatusCode::FORBIDDEN, "multi-use is the operator's");
	let affiliate = refs.create(&admin_ctx, &of("affiliate", None)).await.unwrap();
	assert_eq!(affiliate.uses_left, Some(1));
	let coupon = refs.create(&admin_ctx, &of("coupon", None)).await.unwrap();
	assert_eq!(coupon.uses_left, None);
}

#[tokio::test]
async fn open_registration_does_not_spend_a_coupon() {
	let db = TmpDb::new("open-coupon");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	app.settings.set("auth.registration", "open", None).await.unwrap();
	let admin = account(&store, "admin@e.st").await;
	let (_org_id, admin_ctx) = org(&store, &admin, "Céges Kft.").await;
	let auth = Auth::new(app.clone());
	let refs = mintworks_core::refs::Refs::from_app(&app).unwrap();
	let req = mintworks_core::refs::CreateRef {
		ref_type: "coupon".to_owned(),
		uses_left: Some(1),
		..mintworks_core::refs::CreateRef::default()
	};
	let coupon = refs.create(&admin_ctx, &req).await.unwrap();

	auth.register(&Ctx::system("test"), &signup_with("new@e.st", Some(&coupon.code)))
		.await
		.unwrap();
	let token = link_from_queued_mail(&app, &store, "activation_link").await;
	auth.activate(&Ctx::system("test"), &token, Some(PASSWORD.to_owned()))
		.await
		.unwrap();
	assert_eq!(refs.by_id(coupon.id).await.unwrap().unwrap().uses_left, Some(1));
	let (uses,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM ref_uses WHERE ref_id = ?")
		.bind(coupon.id)
		.fetch_one(store.write_pool())
		.await
		.unwrap();
	assert_eq!(uses, 0);
}

#[tokio::test]
async fn a_non_operator_cannot_mint_a_multi_use_affiliate_ref() {
	let db = TmpDb::new("refs-multi-use-operator");
	let (app, store) = setup(&db).await;
	let admin = account(&store, "admin@e.st").await;
	let (_org_id, admin_ctx) = org(&store, &admin, "Céges Kft.").await;
	let refs = mintworks_core::refs::Refs::from_app(&app).unwrap();
	let req = mintworks_core::refs::CreateRef {
		ref_type: "affiliate".to_owned(),
		uses_left: Some(5),
		..mintworks_core::refs::CreateRef::default()
	};
	let err = refs.create(&admin_ctx, &req).await.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::FORBIDDEN);

	let boss = account(&store, "boss@e.st").await;
	let root = store.root_org_id().await.unwrap();
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, ?, 'OWNER', 0, 0)",
	)
	.bind(root)
	.bind(boss.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	let operator = Ctx { org_id: Some(root), ..ctx_for(&boss) };
	assert_eq!(refs.create(&operator, &req).await.unwrap().uses_left, Some(5));
}

/// Registration takes no password, so activation always demands one.
#[tokio::test]
async fn activation_without_a_password_is_refused() {
	let db = TmpDb::new("activate-needs-password");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	let auth = Auth::new(app.clone());

	auth.register(
		&Ctx::system("test"),
		&Registration {
			email: "fresh@e.st".to_owned(),
			locale: Some("hu".to_owned()),
			consents: vec![
				serde_json::from_value(serde_json::json!({ "kind": "TOS", "version": "1" }))
					.unwrap(),
				serde_json::from_value(serde_json::json!({ "kind": "PRIVACY", "version": "1" }))
					.unwrap(),
			],
			..Registration::default()
		},
	)
	.await
	.unwrap();

	let token = link_from_queued_mail(&app, &store, "activation_link").await;
	assert_eq!(
		auth.activate(&Ctx::system("test"), &token, None).await.unwrap_err().parts().1,
		"E-AUTH-PASSWORD-REQUIRED"
	);
	// The token is not spent by the refusal.
	auth.activate(&Ctx::system("test"), &token, Some(PASSWORD.to_owned()))
		.await
		.unwrap();
}

/// `add_member` validated the invitee's address with `contains('@')` where every other
/// entry point calls `register::validate`. `accounts.email` has no CHECK, so the string was
/// stored verbatim and copied into `orgs.name`, and only failed at `Mailbox::parse` inside
/// the mail job hours later — the admin got `204` and the invitee was never mailed.
#[tokio::test]
async fn add_member_validates_the_address_the_way_register_does() {
	let db = TmpDb::new("add-member-validation");
	let (app, store) = setup(&db).await;
	let admin = account(&store, "admin@e.st").await;
	let (_org_id, admin_ctx) = org(&store, &admin, "Céges Kft.").await;
	let auth = Auth::new(app.clone());

	// One address, not the whole table: `register::tests::validate_rejects_what_it_should` owns
	// the cases, and what is under test here is that this route reaches the same function.
	let err = auth
		.add_member(&admin_ctx, "a@b\r\nBcc: c@d.hu", mintworks_auth::store::Role::Member)
		.await
		.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::BAD_REQUEST);

	// Only the admin's own account exists: nothing was created along the way.
	let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(rows, 1);
}

/// `Runner::complete` blanks a `DONE` payload, but `terminate` deliberately keeps a
/// `FAILED` one as the diagnostic — so an SMTP outage past `max_attempts` parked a live
/// single-use reset link, and the address it belongs to, in the database indefinitely.
/// `anonymize_account` never touches `jobs`. The token is minted at render time now, so
/// there is nothing in the payload to keep.
#[tokio::test]
async fn a_queued_link_mail_carries_no_token_whatever_becomes_of_the_job() {
	let db = TmpDb::new("no-token-in-payload");
	let (app, store) = setup(&db).await;
	account(&store, "victim@e.st").await;

	// Drives `request_password_reset`, which queues the mail.
	let _token = reset_token(&app, &store, "victim@e.st").await;

	// Fail the job terminally, the one state that retains its payload.
	sqlx::query("UPDATE jobs SET status = 'FAILED', last_error = 'smtp down' WHERE id = ?")
		.bind(
			sqlx::query_scalar::<_, i64>(
				"SELECT id FROM jobs WHERE kind = 'AUTH_LINK_EMAIL' ORDER BY id DESC LIMIT 1",
			)
			.fetch_one(store.read_pool())
			.await
			.unwrap(),
		)
		.execute(store.write_pool())
		.await
		.unwrap();

	let payload: String = sqlx::query_scalar(
		"SELECT payload FROM jobs WHERE status = 'FAILED' AND kind = 'AUTH_LINK_EMAIL'",
	)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert!(!payload.contains("token="), "{payload}");
	assert!(!payload.contains("reset-password"), "{payload}");
	assert!(payload.contains("PASSWORD_RESET"), "the diagnostic still says what failed");
}

/// One version exists in several locales, so the version alone was not evidence about
/// anything: a `hu` account whose client called `GET /api/legal/TOS` with no `?locale=` read
/// the English text — `legal_document` defaulted to `"en"` — and `record_consent` stored the
/// **Hungarian** row's `doc_sha256`.
#[tokio::test]
async fn consent_evidence_is_about_the_text_that_was_presented() {
	// A name of its own: `TmpDb::new` builds a *fixed* path and wipes it on construction, so
	// two tests sharing a name in one binary delete each other's database mid-run.
	let db = TmpDb::new("consent-evidence-locale");
	let (app, store) = setup(&db).await;
	let account = account(&store, "locale@e.st").await; // `account` creates it `hu`.

	// The same version, two locales, two different texts.
	publish_legal(&store, LegalKind::Tos, "1").await; // hu, sha 000…0
	store
		.insert_legal_doc(&NewLegalDoc {
			kind: LegalKind::Tos,
			locale: "en".to_owned(),
			version: "1".to_owned(),
			title: "TOS 1".to_owned(),
			body: "the English wording".to_owned(),
			sha256: format!("{:064x}", 1),
			effective_from: Timestamp(0),
		})
		.await
		.unwrap();

	let auth = Auth::new(app.clone());
	// No `?locale=`, but the caller is a signed-in `hu` account.
	let served = auth.legal_document(&ctx_for(&account), LegalKind::Tos, None).await.unwrap();
	assert_eq!(served.locale, "hu", "a hu account was served the English text");

	let grant = |sha: &str| ConsentGrant {
		kind: LegalKind::Tos,
		version: "1".to_owned(),
		doc_sha256: Some(sha.to_owned()),
		org_uid: None,
		user_agent: None,
	};
	let err = auth
		.record_consent(&ctx_for(&account), &grant(&format!("{:064x}", 1)))
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-CONFLICT");

	let ok = auth.record_consent(&ctx_for(&account), &grant(&served.sha256)).await.unwrap();
	assert_eq!(ok.doc_sha256, served.sha256);
	// The hash is mandatory. It was optional, so the whole check above was opt-in and a
	// client that simply omitted the field got no protection at all.
	let err = auth
		.record_consent(&ctx_for(&account), &ConsentGrant { doc_sha256: None, ..grant("") })
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-VALIDATION");
	assert_eq!(err.parts().0, StatusCode::BAD_REQUEST);

	// The source of the mismatch: `?locale=` cannot override a signed-in caller's own
	// locale, because their consent is anchored on it whatever they were shown.
	let served = auth
		.legal_document(&ctx_for(&account), LegalKind::Tos, Some("en"))
		.await
		.unwrap();
	assert_eq!(served.locale, "hu", "?locale= overrode the locale the evidence is anchored on");

	// An anonymous caller still gets the default locale — the route is public — and still
	// chooses with `?locale=`.
	let public = auth.legal_document(&Ctx::system("test"), LegalKind::Tos, None).await.unwrap();
	assert_eq!(public.locale, "en");
	let public = auth
		.legal_document(&Ctx::system("test"), LegalKind::Tos, Some("hu"))
		.await
		.unwrap();
	assert_eq!(public.locale, "hu");
}

/// `consents_required` read "no document in force" as "nothing to consent to yet" and
/// `continue`d. `current_legal_doc` falls back to any published locale, so a missing
/// *translation* does gate — but a gating kind published in **no** locale opened the gate for
/// every account, which is the advisory-field failure `consent::gate` exists to close. It
/// fails closed now: nothing to consent to is not a waiver.
#[tokio::test]
async fn an_unpublished_gating_kind_closes_the_gate_rather_than_opening_it() {
	let db = TmpDb::new("unpublished-gating-kind");
	let (app, store) = setup(&db).await;
	let account = account(&store, "ungated@e.st").await;
	let auth = Auth::new(app.clone());

	// TOS is published; PRIVACY is published nowhere.
	publish_legal(&store, LegalKind::Tos, "1").await;
	accept_current(&store, &account, LegalKind::Tos).await;

	let me = auth.me(&ctx_for(&account)).await.unwrap();
	assert!(
		me.consents_required.contains(&LegalKind::Privacy),
		"an unpublished mandatory kind opened the gate: {:?}",
		me.consents_required
	);

	// And once it is published and accepted, the gate opens again — so this is a gate, not a
	// deadlock.
	publish_legal(&store, LegalKind::Privacy, "1").await;
	accept_current(&store, &account, LegalKind::Privacy).await;
	let me = auth.me(&ctx_for(&account)).await.unwrap();
	assert!(me.consents_required.is_empty(), "{:?}", me.consents_required);
}

/// The service half above was a no-op on the only path that reaches it. `consent::legal`
/// built its own `Ctx::system("auth.legal")`, whose `Actor::account_id()` is always `None`, so
/// the account-locale rung could never fire — a signed-in `hu` caller still read English. The
/// handler takes an optional `Ctx` now, which the middleware leaves behind on this public
/// route whenever a token happens to be present.
#[tokio::test]
async fn the_legal_route_serves_a_signed_in_caller_their_own_locale() {
	let db = TmpDb::new("legal-route-locale");
	let (app, store) = setup(&db).await;
	let account = account(&store, "legalroute@e.st").await; // `account` creates it `hu`.

	publish_legal(&store, LegalKind::Tos, "1").await; // hu, sha 000…0
	store
		.insert_legal_doc(&NewLegalDoc {
			kind: LegalKind::Tos,
			locale: "en".to_owned(),
			version: "1".to_owned(),
			title: "TOS 1".to_owned(),
			body: "the English wording".to_owned(),
			sha256: format!("{:064x}", 1),
			effective_from: Timestamp(0),
		})
		.await
		.unwrap();

	// The real router, because what this has to prove is that the middleware's `Ctx` reaches
	// a route in `public()` — which calling the handle directly cannot show.
	let router = mintworks_auth::routes::public()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "legalroute@e.st").await;

	let (status, body) = call(&router, "GET", "/api/legal/TOS", &token, None).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(body["locale"], "hu", "a signed-in hu account was served the English text");

	// And the evidence `record_consent` stores is about that same text.
	let stored = Auth::new(app.clone())
		.record_consent(
			&ctx_for(&account),
			&ConsentGrant {
				kind: LegalKind::Tos,
				version: "1".to_owned(),
				doc_sha256: body["sha256"].as_str().map(str::to_owned),
				org_uid: None,
				user_agent: None,
			},
		)
		.await
		.unwrap();
	assert_eq!(stored.doc_sha256, body["sha256"].as_str().unwrap());

	// Still public: no token at all is still the default locale, not a 401.
	let req = axum::http::Request::builder()
		.method("GET")
		.uri("/api/legal/TOS")
		.body(axum::body::Body::empty())
		.unwrap();
	let (status, body) =
		parts(tower::ServiceExt::oneshot(router.clone(), req).await.unwrap()).await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(body["locale"], "en", "an anonymous caller must still get the default");
}

/// Both routes mint an access token and `token::respond` — the only thing that sets the
/// cookies — is reached by neither. Cookies are a supported authentication source
/// (`auth_mw` reads `access_token` when there is no Bearer header), so a browser session kept
/// the stale `auth_at` after a step-up, making every `require_stepup` route permanently
/// unreachable, and kept working in the old org after a switch.
///
/// A bundle test by necessity: `Auth::step_up` and `Auth::switch_org` hand back the token,
/// and only the route writes the `Set-Cookie` this is about.
#[tokio::test]
async fn step_up_and_switch_org_both_refresh_the_access_cookie() {
	let db = TmpDb::new("stepup-switch-cookie");
	let (app, store) = setup(&db).await;
	let account = account(&store, "cookie@e.st").await;
	gate_satisfied(&store, &account).await;
	let auth = Auth::new(app.clone());
	let org = auth.create_org(&ctx_for(&account), "Org", None).await.unwrap();

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(ClientIp(PEER.ip())))
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "cookie@e.st").await;

	let cookie_token = |resp: &Response| {
		resp.headers()
			.get_all(axum::http::header::SET_COOKIE)
			.iter()
			.map(|v| v.to_str().unwrap().to_owned())
			.find(|c| c.starts_with("access_token="))
			.map(|c| c["access_token=".len()..].split(';').next().unwrap().to_owned())
			.expect("no access_token cookie was set")
	};

	let resp = call_resp(
		&router,
		"POST",
		"/api/auth/step-up",
		&token,
		Some(serde_json::json!({ "password": PASSWORD })),
	)
	.await;
	assert_eq!(resp.status(), StatusCode::OK);
	let stepped = cookie_token(&resp);
	let auth_at = jwt_payload(&stepped)["auth_at"].as_i64().expect("a fresh auth_at");
	assert!(
		auth_at >= Timestamp::now().0 - 5,
		"the cookie carries a stale auth_at, so step-up is unreachable in a browser"
	);

	let resp = call_resp(
		&router,
		"POST",
		"/api/auth/switch-org",
		&token,
		Some(serde_json::json!({ "orgUid": org.uid.as_str() })),
	)
	.await;
	assert_eq!(resp.status(), StatusCode::OK);
	let switched = cookie_token(&resp);
	assert_eq!(
		jwt_payload(&switched)["org"].as_str(),
		Some(org.uid.as_str()),
		"the cookie still names the previous org"
	);
}

/// `Runner::complete` blanks a `DONE` payload but a `FAILED` one is kept on purpose as the
/// delivery diagnostic, and a `SEND_EMAIL` payload carries the recipient address and the
/// display name. `jobs` was not on `anonymize_account`'s allowlist, so an SMTP outage that
/// exhausted `max_attempts` left a data subject's address in the database after erasure.
#[tokio::test]
async fn erasure_clears_the_address_out_of_failed_email_jobs() {
	let db = TmpDb::new("erase-email-jobs");
	let (app, store) = setup(&db).await;
	let account = account(&store, "erased@e.st").await;

	let payload = serde_json::json!({
		"to": "erased@e.st",
		"template": "welcome",
		"lang": "hu",
		"vars": { "name": "Erased Person" },
	})
	.to_string();
	let id = mintworks_core::job::enqueue(&app.store, "SEND_EMAIL", &payload, None, Timestamp(0))
		.await
		.unwrap()
		.unwrap();
	// What an exhausted delivery leaves behind.
	sqlx::query("UPDATE jobs SET status = 'FAILED', attempts = 5 WHERE id = ?")
		.bind(id)
		.execute(store.write_pool())
		.await
		.unwrap();
	// A job addressed to somebody else, which must survive untouched.
	let other = mintworks_core::job::enqueue(
		&app.store,
		"SEND_EMAIL",
		&serde_json::json!({ "to": "other@e.st", "vars": {} }).to_string(),
		None,
		Timestamp(0),
	)
	.await
	.unwrap()
	.unwrap();

	Auth::new(app.clone())
		.erase_account(&ctx_for(&account), &account.email)
		.await
		.unwrap();

	let leaked: i64 =
		sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE payload LIKE '%erased@e.st%'")
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(leaked, 0, "an erased subject's address survived in a job payload");
	// The row itself stays — kind, status and attempts are operational data with no personal
	// content once the payload is gone.
	let (status, payload): (String, String) =
		sqlx::query_as("SELECT status, payload FROM jobs WHERE id = ?")
			.bind(id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(status, "FAILED");
	assert_eq!(payload, "");

	let payload: String = sqlx::query_scalar("SELECT payload FROM jobs WHERE id = ?")
		.bind(other)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert!(payload.contains("other@e.st"), "another subject's job was blanked too");
}

/// `job_complete` blanks a DONE payload to `''`, and `json_extract('')` raises `malformed
/// JSON`, which aborts the *whole* UPDATE — not just that row. So one delivered email rolled
/// the entire `anonymize_account` transaction back and erasure answered `E-CORE-INTERNAL`,
/// for every deployment, from the first activation mail onward.
#[tokio::test]
async fn erasure_survives_an_already_completed_email_job() {
	let db = TmpDb::new("erase-done-email-job");
	let (app, store) = setup(&db).await;
	let account = account(&store, "erased@e.st").await;

	let payload = serde_json::json!({ "to": "erased@e.st", "vars": {} }).to_string();
	// A delivered job, blanked by `job_complete` — the poison row.
	let done = mintworks_core::job::enqueue(&app.store, "SEND_EMAIL", &payload, None, Timestamp(0))
		.await
		.unwrap()
		.unwrap();
	store.job_complete(done, Timestamp(1)).await.unwrap();
	// And a pending one that still has to be blanked.
	mintworks_core::job::enqueue(&app.store, "SEND_EMAIL", &payload, None, Timestamp(0))
		.await
		.unwrap()
		.unwrap();

	Auth::new(app.clone())
		.erase_account(&ctx_for(&account), &account.email)
		.await
		.unwrap();

	let email: String = sqlx::query_scalar("SELECT email FROM accounts WHERE id = ?")
		.bind(account.id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert!(email.ends_with("@invalid"), "the erasure rolled back: {email}");
	let leaked: i64 =
		sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE payload LIKE '%erased@e.st%'")
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(leaked, 0);
}

/// `auth_mw` resolved the `org` claim by uid plus accepted membership and never looked at
/// `orgs.status`, while `token::pick_org` and `Auth::switch_org` both did. It was the
/// one per-request path that did not, so a suspended org's members kept operating — issuing
/// invoices among other things — for the remaining life of their access token.
#[tokio::test]
async fn a_suspended_org_resolves_to_no_active_org_on_the_next_request() {
	let db = TmpDb::new("suspended-org");
	let (app, store) = setup(&db).await;
	account(&store, "suspended@e.st").await;

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	// `pick_org` puts the personal org's uid in `org`, and the token outlives the
	// suspension by up to 15 minutes.
	let token = access_token(&app, "suspended@e.st").await;
	let (status, body) = call(&router, "GET", "/api/auth/me", &token, None).await;
	assert_eq!(status, StatusCode::OK);
	let uid = body["org"]["uid"].as_str().expect("an org to suspend").to_owned();

	sqlx::query("UPDATE orgs SET status = 'SUSPENDED' WHERE uid = ?")
		.bind(&uid)
		.execute(store.write_pool())
		.await
		.unwrap();

	let (status, body) = call(&router, "GET", "/api/auth/me", &token, None).await;
	assert_eq!(status, StatusCode::OK);
	assert!(
		body["org"].is_null(),
		"a suspended org is still the active one on an unexpired token: {body}"
	);
}

/// `register` refuses every account until a current `TOS` and `PRIVACY` exist, and
/// `insert_legal_doc` had no caller outside these tests — no `Auth` method, no route, no seed.
/// A freshly migrated deployment therefore answered `403 E-AUTH-CONSENT-REQUIRED` to every
/// registration, forever. The fix is a handle method: *what* to publish is the consumer
/// application's decision, so the framework provides the API and nothing else.
#[tokio::test]
async fn publishing_the_legal_documents_is_what_makes_registration_possible() {
	use mintworks_auth::consent::PublishLegalDoc;

	let db = TmpDb::new("publish-legal");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());

	let registration = |kind_version: &str| Registration {
		email: "first@e.st".to_owned(),
		locale: Some("hu".to_owned()),
		consents: vec![
			serde_json::from_value(serde_json::json!({ "kind": "TOS", "version": kind_version }))
				.unwrap(),
			serde_json::from_value(
				serde_json::json!({ "kind": "PRIVACY", "version": kind_version }),
			)
			.unwrap(),
		],
		..Registration::default()
	};

	// Nothing published: the framework is unusable out of the box, which is the bug.
	let err = auth.register(&Ctx::system("boot"), &registration("1")).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-CONSENT-REQUIRED");

	let doc = |kind, body: &str| PublishLegalDoc {
		kind,
		locale: "hu".to_owned(),
		version: "1".to_owned(),
		title: "Feltételek".to_owned(),
		body: body.to_owned(),
		effective_from: Timestamp(0),
	};

	// A signed-in user is not an operator: this is the text every consent is evidence against.
	let user = account(&store, "nobody@e.st").await;
	let err = auth
		.publish_legal_document(&ctx_for(&user), doc(LegalKind::Tos, "…"))
		.await
		.expect_err("publishing is operator-only");
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");

	// A real operator, whose credential was presented outside `auth.stepup_window`: a new
	// version gates every account at once, so an access token alone must not reach it.
	let boss = account(&store, "boss@e.st").await;
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES ((SELECT id FROM orgs WHERE kind = 'ROOT'), ?, 'OWNER', 0, 0)",
	)
	.bind(boss.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	let operator = |ctx: Ctx| Ctx { actor: Actor::Operator { account_id: boss.id }, ..ctx };
	let err = auth
		.publish_legal_document(&operator(stale_ctx(&boss)), doc(LegalKind::Tos, "x"))
		.await
		.expect_err("publishing is step-up");
	assert_eq!(err.parts().1, "E-AUTH-STEPUP");

	// Multi-line on purpose: `bounded` refused every `\n`, so no realistic ToS could be
	// published and `register::check_consents` locked every fresh deployment out of signup.
	let tos = "1. §\nA feltételek.\n\n2. §\nTovábbi feltételek.\n";
	let summary = auth
		.publish_legal_document(&operator(ctx_for(&boss)), doc(LegalKind::Tos, tos))
		.await
		.unwrap();
	// The hash is computed here, over the body, not accepted from the caller.
	let expected = format!("{:x}", Sha256::digest(tos.as_bytes()));
	assert_eq!(summary.sha256, expected);

	// `Ctx::system` is the application's own code — exempt from step-up, as everywhere else in
	// this crate, so a consumer seeding the first documents from its boot code still can.
	let op = Ctx::system("admin");
	auth.publish_legal_document(&op, doc(LegalKind::Privacy, "1. §\nAz adatkezelés.\n"))
		.await
		.unwrap();

	// Republishing a version that already exists is a clean 409, never a silent rewrite.
	let err = auth
		.publish_legal_document(&op, doc(LegalKind::Tos, "más szöveg"))
		.await
		.expect_err("a shipped version is evidence");
	assert_eq!(err.parts().0, StatusCode::CONFLICT);

	// Validation, at the trust boundary the store has no CHECK for.
	assert!(auth.publish_legal_document(&op, doc(LegalKind::EInvoice, "")).await.is_err());
	let bad_locale =
		PublishLegalDoc { locale: "../../etc".to_owned(), ..doc(LegalKind::EInvoice, "x") };
	assert!(auth.publish_legal_document(&op, bad_locale).await.is_err());

	auth.register(&Ctx::system("boot"), &registration("1"))
		.await
		.expect("registration works now");
	let created = store.account_by_email("first@e.st").await.unwrap().expect("the account");
	assert_eq!(created.status, AccountStatus::Pending);

	let logged: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM audit_logs WHERE entity = 'legal_doc' AND action = 'LEGAL_DOC_PUBLISHED'",
	)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(logged, 2, "a privileged mutation writes an audit row");
}

/// Another org's uid is `E-CORE-NOTFOUND`, never 403 — and `switch_org` is the one route
/// that takes an arbitrary `org_` uid in its body. Answering `403` confirmed that the org
/// exists.
#[tokio::test]
async fn switching_to_someone_elses_org_does_not_confirm_it_exists() {
	let db = TmpDb::new("switch-notfound");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());

	let mine = account(&store, "mine@e.st").await;
	let theirs = account(&store, "theirs@e.st").await;
	let other = auth.create_org(&ctx_for(&theirs), "Másik Kft.", None).await.unwrap();

	let err = auth
		.switch_org(&ctx_for(&mine), other.uid.as_str())
		.await
		.expect_err("a non-member must not be able to enter");
	assert_eq!(err.parts().1, "E-CORE-NOTFOUND");
	assert_eq!(err.parts().0, StatusCode::NOT_FOUND);

	// An org uid that exists nowhere answers identically, which is the whole point.
	let nowhere = auth
		.switch_org(&ctx_for(&mine), "org_00000000000000000000000000")
		.await
		.unwrap_err();
	assert_eq!(nowhere.parts().1, err.parts().1);
}

/// Switch eligibility is the **effective** role, so an org held only through an ancestor is
/// reachable by uid even though `/api/orgs` never enumerates it. `accept_membership` still
/// runs only for a direct row — an ancestor grant has none to accept.
#[tokio::test]
async fn switching_into_an_org_held_only_through_an_ancestor_is_allowed() {
	let db = TmpDb::new("switch-ancestor");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());

	let boss = account(&store, "boss@e.st").await;
	let staff = account(&store, "staff@e.st").await;
	let parent = auth.create_org(&ctx_for(&boss), "Anya Kft.", None).await.unwrap();
	let child = store
		.create_org(mintworks_auth::store::OrgKind::Shared, parent.id, "Lanya Kft.", boss.id, None)
		.await
		.unwrap();
	store
		.put_membership(parent.id, staff.id, mintworks_auth::store::Role::Admin)
		.await
		.unwrap();
	store.accept_membership(parent.id, staff.id, Timestamp::now()).await.unwrap();

	auth.switch_org(&ctx_for(&staff), child.uid.as_str())
		.await
		.expect("the walk reaches down");

	assert!(
		!store
			.orgs_for_account(staff.id)
			.await
			.unwrap()
			.iter()
			.any(|o| o.uid == child.uid),
		"the listing stays direct memberships — the subtree is reached by uid, not enumerated"
	);
}

/// `issue_in` resolves the refresh token's `prefer` through the same ancestor walk `switch_org`
/// is admitted by. Against direct memberships alone, a caller who entered a descendant org
/// through an ancestor role came back from `refresh` with no org at all.
#[tokio::test]
async fn refresh_keeps_an_org_held_only_through_an_ancestor() {
	let db = TmpDb::new("refresh-ancestor-org");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());

	let boss = account(&store, "anc-boss@e.st").await;
	let staff = account(&store, "anc-staff@e.st").await;
	let parent = auth.create_org(&ctx_for(&boss), "Anya Kft.", None).await.unwrap();
	let child = store
		.create_org(mintworks_auth::store::OrgKind::Shared, parent.id, "Lanya Kft.", boss.id, None)
		.await
		.unwrap();
	let grandchild = store
		.create_org(mintworks_auth::store::OrgKind::Shared, child.id, "Unoka Kft.", boss.id, None)
		.await
		.unwrap();

	// While the direct row lasts, `pick_org` names the grandchild and the refresh token carries
	// it — the only way a refresh token ever names this org.
	store
		.put_membership(grandchild.id, staff.id, mintworks_auth::store::Role::Admin)
		.await
		.unwrap();
	store
		.accept_membership(grandchild.id, staff.id, Timestamp::now())
		.await
		.unwrap();
	let login = auth
		.login(&Ctx::public("test"), &credentials(&staff.email, PASSWORD))
		.await
		.unwrap();
	let LoginOutcome::Signed(tokens) = login else { panic!("expected a signed pair") };
	assert_eq!(
		jwt_payload(&tokens.refresh_token).get("org").and_then(|v| v.as_str()),
		Some(grandchild.uid.as_str())
	);

	// The direct row goes; a role on the parent remains, and reaches down.
	sqlx::query("DELETE FROM memberships WHERE org_id = ? AND account_id = ?")
		.bind(grandchild.id)
		.bind(staff.id)
		.execute(store.write_pool())
		.await
		.unwrap();
	store
		.put_membership(parent.id, staff.id, mintworks_auth::store::Role::Admin)
		.await
		.unwrap();
	store.accept_membership(parent.id, staff.id, Timestamp::now()).await.unwrap();
	auth.switch_org(&ctx_for(&staff), grandchild.uid.as_str())
		.await
		.expect("the walk reaches down");

	let fresh = auth.refresh(&Ctx::public("test"), &tokens.refresh_token).await.unwrap();
	assert_eq!(
		jwt_payload(&fresh.access_token).get("org").and_then(|v| v.as_str()),
		Some(grandchild.uid.as_str()),
		"refreshing dropped an org the caller still holds through an ancestor"
	);
}

/// `dump` resolved every `*_id` foreign key to the referenced row's public `uid` with an
/// *unqualified* outer column — `(SELECT uid FROM invoices WHERE id = original_invoice_id)`.
/// `invoices.original_invoice_id` is the schema's only self-referential FK, so the inner
/// scope owned that name and the subquery degenerated to "an invoice that cancels itself":
/// every STORNO in a subject access request carried `originalInvoiceUid: null`.
#[tokio::test]
async fn the_export_resolves_a_stornos_original_invoice() {
	let db = TmpDb::new("export-self-fk");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "owner@e.st").await;
	let personal: i64 =
		sqlx::query_scalar("SELECT id FROM orgs WHERE owner_account_id = ? AND kind = 'PERSONAL'")
			.bind(owner.id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();

	sqlx::raw_sql(
		"INSERT INTO sellers (id, uid, org_id, nav_base_url, created_at)
		 VALUES (1, 'sel_test', (SELECT id FROM orgs WHERE kind = 'ROOT'), '', 0);
		 INSERT INTO seller_versions (seller_ver, seller_id, status, name, country, tax_number,
		                              postcode, city, street, created_at, valid_from)
		 VALUES (1, 1, 'CURRENT', 'Teszt Kft.', 'HU', '12345678242', '1011', 'Budapest',
		         'Fo utca 1.', 0, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	// The pair by hand: `Invoices` is not wired into this file, and the export reads columns.
	for (id, uid, kind, number, original) in [
		(1_i64, "inv_original", "NORMAL", "A2026/000001", None),
		(2, "inv_storno", "STORNO", "A2026/000002", Some(1_i64)),
	] {
		sqlx::query(
			"INSERT INTO invoices
			 (id, uid, org_id, seller_id, seller_ver, kind, status, number, issued_at,
			  fulfilment_date, buyer_name, original_invoice_id, currency, created_at,
			  updated_at)
			 VALUES (?, ?, ?, 1, 1, ?, 'ISSUED', ?, 0, '2026-01-31', 'Vevo Zrt.', ?, 'HUF', 0, 0)",
		)
		.bind(id)
		.bind(uid)
		.bind(personal)
		.bind(kind)
		.bind(number)
		.bind(original)
		.execute(store.write_pool())
		.await
		.unwrap();
	}

	let doc = Auth::new(app.clone()).export_account(&ctx_for(&owner)).await.unwrap().1;
	let invoices = doc["invoices"].as_array().unwrap();
	let of = |uid: &str| {
		invoices
			.iter()
			.find(|i| i["uid"] == uid)
			.unwrap_or_else(|| panic!("{uid} missing from the export"))
			.clone()
	};
	assert_eq!(
		of("inv_storno")["originalInvoiceUid"],
		"inv_original",
		"a storno has to name the invoice it cancels: {doc:#}"
	);
	assert!(of("inv_original")["originalInvoiceUid"].is_null(), "a NORMAL invoice cancels nothing");
	// The cross-table case that always worked, so the qualification did not break it.
	assert!(of("inv_storno")["orgUid"].is_string());
}

/// `consent::not_published` minted `E-CORE-NOT-FOUND` — a code the registry does not
/// carry (it spells it `E-CORE-NOTFOUND`), and the one place in the workspace where a feature
/// crate hardcoded an `E-CORE-*` code that `mintworks-core` owns. It was live, reached through
/// `.ok_or_else(consent::not_published)` on `GET /api/legal/{kind}`.
#[tokio::test]
async fn an_unpublished_legal_document_answers_the_registered_not_found_code() {
	let db = TmpDb::new("legal-notfound");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app);
	let ctx = Ctx::system("test");

	let err = auth.legal_document(&ctx, LegalKind::Tos, None).await.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::NOT_FOUND, "E-CORE-NOTFOUND"));

	// Published, and the same call answers.
	publish_legal(&store, LegalKind::Tos, "1.0").await;
	assert_eq!(auth.legal_document(&ctx, LegalKind::Tos, None).await.unwrap().version, "1.0");
	// A kind that has no row still answers the registered code, not the hand-rolled one.
	assert_eq!(
		auth.legal_document(&ctx, LegalKind::Privacy, None).await.unwrap_err().parts().1,
		"E-CORE-NOTFOUND"
	);
}

/// `accounts.name` and `orgs.name` had no bound but axum's 2 MB body limit, so one
/// org admin could `PATCH /api/org` a 2 MB name that every member then carried in every
/// login, `GET /api/auth/me` and `GET /api/orgs` response.
#[tokio::test]
async fn an_org_name_is_bounded() {
	let db = TmpDb::new("org-name-bound");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "owner@e.st").await;
	let auth = Auth::new(app.clone());

	// Characters, not bytes: a 200-character Hungarian name is legal and is multi-byte.
	let legal = "á".repeat(200);
	let org = auth.create_org(&ctx_for(&owner), &legal, None).await.unwrap();
	assert_eq!(org.name, legal);

	let too_long = "á".repeat(201);
	assert!(auth.create_org(&ctx_for(&owner), &too_long, None).await.is_err());

	let admin_ctx = Ctx { org_id: Some(org.id), ..ctx_for(&owner) };
	let patch = mintworks_auth::org::OrgPatch {
		name: Some(too_long),
		billing_currency: Patch::Undefined,
		slug: Patch::Undefined,
	};
	assert!(auth.update_org(&admin_ctx, &patch).await.is_err());
}

/// `consent::gate` was layered in exactly one place — `routes::authenticated`'s own gated
/// sub-router — so an account owing a newly published ToS was blocked from `/api/org*` and
/// could still issue a numbered legal invoice through `mintworks_invoice::routes::org_invoices`.
/// `mintworks-invoice` cannot depend on `mintworks-auth`, so the gate is handed to the bundle as a
/// `RouteGate`, which is what makes it impossible to build one ungated by accident.
#[tokio::test]
async fn the_consent_gate_reaches_another_crates_bundle() {
	let db = TmpDb::new("consent-gate-invoice");
	let (app, store) = setup(&db).await;
	let account = account(&store, "invoicer@e.st").await;
	publish_legal(&store, LegalKind::Tos, "1.0").await;
	publish_legal(&store, LegalKind::Privacy, "1.0").await;

	let router = mintworks_invoice::routes::org_invoices(&mintworks_auth::routes::consent_gate())
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "invoicer@e.st").await;

	let (status, body) = call(&router, "POST", "/api/invoices", &token, Some(json_draft())).await;
	assert_eq!(status, StatusCode::FORBIDDEN, "an unaccepted ToS must reach this bundle too");
	assert_eq!(body["error"]["errCode"], "E-AUTH-CONSENT-REQUIRED");

	// And the gate is the only thing refusing it: once both documents are accepted the
	// request reaches the handler, whatever the handler then makes of it.
	accept_current(&store, &account, LegalKind::Tos).await;
	accept_current(&store, &account, LegalKind::Privacy).await;
	let (status, body) = call(&router, "POST", "/api/invoices", &token, Some(json_draft())).await;
	assert_ne!(status, StatusCode::FORBIDDEN, "still gated after both consents: {body}");
	assert_ne!(body["error"]["errCode"], "E-AUTH-CONSENT-REQUIRED");
}

/// `auth.registration = closed` refuses a ref-less signup at register.
/// Login stays up, which is the whole point of closing only this one route.
#[tokio::test]
async fn closing_registration_refuses_signups_and_leaves_login_up() {
	let db = TmpDb::new("registration-closed");
	let (app, store) = setup(&db).await;
	let existing = account(&store, "already@e.st").await;
	app.settings.set("auth.registration", "closed", None).await.unwrap();

	let auth = Auth::new(app.clone());
	let ctx = Ctx::system("test");
	// No proof of work and no consents: the mode is checked before any of that, so a closed
	// deployment spends nothing on a signup it will refuse.
	let refused = auth
		.register(
			&ctx,
			&Registration {
				email: "new@e.st".to_owned(),
				name: None,
				locale: None,
				consents: Vec::new(),
				user_agent: None,
				pow: None,
				ref_code: None,
			},
		)
		.await
		.unwrap_err();
	assert_eq!(refused.parts(), (StatusCode::FORBIDDEN, "E-AUTH-CLOSED"));

	let outcome = auth
		.login(&ctx, &credentials(&existing.email, PASSWORD))
		.await
		.expect("closing registration must not close login");
	assert!(matches!(outcome, LoginOutcome::Signed(_)));
}

/// Most tests reach `Auth` directly, which skips two things the handle cannot carry: the wire
/// shape the extractors impose, and the middleware tiers that live on the routes rather than
/// the service layer. Both are covered once here rather than in every behavioural test.
///
/// `ClientIp` is layered by hand because `AppBuilder::run` is what normally mounts it, and
/// `axum::Extension(app)` because that is how a bundle-level `from_fn` reaches state.
#[tokio::test]
async fn the_bundles_carry_the_wire_shape_and_charge_the_middleware_tiers() {
	let db = TmpDb::new("bundle-wire");
	let (app, store) = setup(&db).await;
	let account = account(&store, "bundle@e.st").await;
	gate_satisfied(&store, &account).await;

	let mount = |router: axum::Router<App>| {
		router
			.layer(axum::Extension(ClientIp(PEER.ip())))
			.layer(axum::Extension(app.clone()))
			.with_state(app.clone())
	};
	let public = mount(mintworks_auth::routes::public());
	let authed = mount(mintworks_auth::routes::authenticated());
	let token = access_token(&app, "bundle@e.st").await;
	let key = mintworks_core::ratelimit::bucket_key(PEER.ip());

	// A body the extractor cannot parse is the error envelope's 400, not axum's plain text.
	let (status, body) = call(
		&authed,
		"POST",
		"/api/auth/password",
		&token,
		Some(serde_json::json!({ "current_password": PASSWORD })),
	)
	.await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert!(body["error"]["errCode"].is_string(), "a Json rejection left the envelope: {body}");

	// The authenticated tier: `authenticated_mw` charges every call inside `require_auth`,
	// keyed on `Claims.sub`, which is the account uid.
	let before = app
		.limits
		.consumed(mintworks_core::ratelimit::AUTHENTICATED, account.uid.as_str());
	let (status, body) = call(&authed, "GET", "/api/auth/me", &token, None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(
		app.limits
			.consumed(mintworks_core::ratelimit::AUTHENTICATED, account.uid.as_str()),
		before + 1,
		"an authenticated call spent nothing of the authenticated tier"
	);

	// The auth-failed tier, both of the ways it is charged: `optional_auth`'s `run_public`
	// when a public handler answers 401, and `require_auth`'s own rejection.
	let before = app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &key);
	let (status, _) = call(
		&public,
		"POST",
		"/api/auth/login",
		"",
		Some(serde_json::json!({ "email": "bundle@e.st", "password": "not-it" })),
	)
	.await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	let (status, _) = call(&authed, "GET", "/api/auth/me", "not-a-jwt", None).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(
		app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &key),
		before + 2,
		"a 401 out of either bundle has to feed the counter the proof-of-work gate reads"
	);

	// The wire shape itself, last: `ChangeBody` is the one camelCase rename among the
	// credential bodies, and a successful change bumps `token_epoch`, which retires `token`.
	let (status, body) = call(
		&authed,
		"POST",
		"/api/auth/password",
		&token,
		Some(serde_json::json!({
			"currentPassword": PASSWORD,
			"newPassword": "a-brand-new-password",
		})),
	)
	.await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert!(body["accessToken"].is_string(), "the rename did not reach the handle: {body}");
}

/// `login_totp` charged no budget at all, and the reset path had the same hole: a wrong code
/// returns before `set_password`, so neither `pwd_hash` nor `token_epoch` moves and one mailed
/// link re-opens for every further guess. The only ceiling was a route layer keyed on an
/// address masked to /64 — a single routed /48 yields 65 536 buckets — and six digits accepted
/// across three time steps for the ticket's whole 300 s is brute-forceable at that rate.
///
/// `accounts.failed_logins` is not the answer: nothing reads it back, so it gates nothing. The
/// charge is account-keyed, out of `login.totp.account` and `reset.totp.account`, so the
/// caller's address — deliberately not varied here, because the charge never reads it — cannot
/// buy a fresh budget.
#[tokio::test]
async fn a_second_factor_guess_draws_from_the_accounts_own_budget() {
	for path in ["login", "reset"] {
		let db = TmpDb::new(&format!("{path}-totp-budget"));
		let (app, store) = setup(&db).await;
		let account = account(&store, "totpbudget@e.st").await;
		let secret = enrol_real_totp(&app, &account).await;
		let auth = Auth::new(app.clone());
		let ctx = ip_ctx();

		// Far outside `SKEW_STEPS`, so it is deterministically wrong rather than 1-in-a-million.
		let wrong = totp_code(&secret, 500);
		let ticket = match auth.login(&ctx, &credentials(&account.email, PASSWORD)).await.unwrap() {
			LoginOutcome::TotpRequired { totp_token } => {
				totp_token.expect("the login path mints a ticket")
			}
			LoginOutcome::Signed(t) => panic!("a confirmed factor must stop the login: {t:?}"),
		};
		let reset = reset_token(&app, &store, &account.email).await;

		// Both buckets are 5 per 5 minutes and — unlike the `login.email` the login half used
		// to share — the password step spends none of it, so the whole 5 is available here and
		// the budget runs out inside this loop. What must not happen is that it never does.
		let mut refused = None;
		for _ in 0..12 {
			let err = match path {
				"login" => auth.login_totp(&ctx, &ticket, Some(&wrong), None).await.err(),
				_ => auth
					.reset_password(
						&ctx,
						&reset,
						Some(&wrong),
						None,
						"a-brand-new-password".to_owned(),
					)
					.await
					.err(),
			}
			.expect("a wrong code cannot mint a pair or set a password");
			if err.parts().1 != "E-AUTH-TOTP-INVALID" {
				refused = Some(err);
				break;
			}
		}
		let err = refused.expect("unlimited guesses against one account is the defect");
		assert_eq!(
			err.parts(),
			(StatusCode::TOO_MANY_REQUESTS, "E-CORE-RATELIMIT"),
			"{path}: {err:?}"
		);

		// And the credential still stands: the guesses never got as far as the write.
		let fresh = store.account_by_id(account.id).await.unwrap().unwrap();
		assert_eq!(fresh.pwd_hash, account.pwd_hash, "{path}");
	}
}

/// `auth_mw::run_public` charges the auth-failed bucket on *any* 401 out of a public bundle,
/// and the second-factor ticket is rendered as a 401. So every **successful** first factor on
/// a 2FA account counted as an authentication failure: after
/// `auth.pow_after_failures` (3) the proof-of-work gate armed for legitimate users, and after
/// 20 in five minutes the whole address was 429'd on login and on every `require_auth`
/// rejection. An office NAT of 2FA users DoSed itself, and the accounts with the strongest
/// authentication were the ones penalised.
#[tokio::test]
async fn a_second_factor_challenge_is_not_an_authentication_failure() {
	let db = TmpDb::new("totp-challenge-not-a-failure");
	let (app, store) = setup(&db).await;
	let account = account(&store, "challenge@e.st").await;
	let _secret = enrol_real_totp(&app, &account).await;

	let public = mintworks_auth::routes::public()
		.layer(axum::Extension(ClientIp(PEER.ip())))
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let key = mintworks_core::ratelimit::bucket_key(PEER.ip());
	let before = app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &key);

	// Five correct passwords — well past `auth.pow_after_failures` (3), which is the threshold
	// that used to arm the proof-of-work gate for these users. Five and not more because the
	// route's own `login.ip` budget is 10/5min and the wrong password below needs one left.
	for n in 1..=5 {
		let (status, body) = call(
			&public,
			"POST",
			"/api/auth/login",
			"",
			Some(serde_json::json!({ "email": "challenge@e.st", "password": PASSWORD })),
		)
		.await;
		assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {n}: {body}");
		assert_eq!(body["error"]["errCode"], "E-AUTH-TOTP-REQUIRED", "{body}");
		assert!(body["totpToken"].is_string(), "{body}");
	}
	assert_eq!(
		app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &key),
		before,
		"a challenge is the first factor succeeding; it must charge nothing"
	);

	// And a wrong password still counts — the exemption is the challenge, not the route.
	let (status, _) = call(
		&public,
		"POST",
		"/api/auth/login",
		"",
		Some(serde_json::json!({ "email": "challenge@e.st", "password": "not-it" })),
	)
	.await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &key), before + 1);
}

/// `cookie()` hardcoded `Path=/`, so the refresh token — 30 days, unrevocable by design,
/// stronger than any access token — travelled with every API call and landed in every proxy
/// log. It is read in one place, `login::refresh`. The clearing cookies must carry the same
/// two paths, or logout leaves the browser's copy in place.
#[tokio::test]
async fn the_refresh_cookie_is_scoped_to_the_route_that_reads_it() {
	let db = TmpDb::new("refresh-cookie-path");
	let (app, store) = setup(&db).await;
	let _account = account(&store, "cookiepath@e.st").await;

	let public = mintworks_auth::routes::public()
		.layer(axum::Extension(ClientIp(PEER.ip())))
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());

	let paths = |resp: &Response| {
		let mut out: Vec<(String, String)> = resp
			.headers()
			.get_all(axum::http::header::SET_COOKIE)
			.iter()
			.map(|v| {
				let raw = v.to_str().unwrap();
				let name = raw.split('=').next().unwrap().to_owned();
				let path = raw
					.split("; ")
					.find_map(|attr| attr.strip_prefix("Path="))
					.unwrap_or_else(|| panic!("no Path on {raw}"));
				(name, path.to_owned())
			})
			.collect();
		out.sort();
		out
	};
	let expected = vec![
		(mintworks_core::auth_mw::ACCESS_COOKIE.to_owned(), "/".to_owned()),
		("refresh_token".to_owned(), "/api/auth/refresh".to_owned()),
	];

	let resp = call_resp(
		&public,
		"POST",
		"/api/auth/login",
		"",
		Some(serde_json::json!({ "email": "cookiepath@e.st", "password": PASSWORD })),
	)
	.await;
	assert_eq!(resp.status(), StatusCode::OK);
	assert_eq!(paths(&resp), expected);

	let resp = call_resp(&public, "POST", "/api/auth/logout", "", None).await;
	assert!(resp.status().is_success());
	assert_eq!(paths(&resp), expected, "a clearing cookie on the wrong path clears nothing");
}

/// `POST /api/org/members` shared `register`'s 3/h/ip bucket, and the layer runs before
/// `add_member`'s `admin_of` check — so three POSTs naming an org the caller is not an admin
/// of, each answered `E-CORE-NOTFOUND`, drained self-service registration for everyone behind
/// that address for the hour.
#[tokio::test]
async fn draining_the_invite_bucket_leaves_registration_alone() {
	let db = TmpDb::new("invite-bucket");
	let (app, store) = setup(&db).await;
	let account = account(&store, "inviter@e.st").await;
	gate_satisfied(&store, &account).await;
	let token = access_token(&app, "inviter@e.st").await;

	let mount = |router: axum::Router<App>| {
		router
			.layer(axum::Extension(ClientIp(PEER.ip())))
			.layer(axum::Extension(app.clone()))
			.with_state(app.clone())
	};
	let public = mount(mintworks_auth::routes::public());
	let authed = mount(mintworks_auth::routes::authenticated());

	// Four unauthorized invitations — one more than `register`'s whole hourly budget.
	for n in 1..=4 {
		let (status, body) = call(
			&authed,
			"POST",
			"/api/org/members",
			&token,
			Some(serde_json::json!({ "email": format!("invitee{n}@e.st"), "role": "MEMBER" })),
		)
		.await;
		assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "invite {n} hit a limit: {body}");
	}

	// Registration is a different bucket and is untouched.
	let (status, body) = call(
		&public,
		"POST",
		"/api/auth/register",
		"",
		Some(serde_json::json!({ "email": "stranger@e.st", "password": PASSWORD })),
	)
	.await;
	assert_ne!(
		status,
		StatusCode::TOO_MANY_REQUESTS,
		"invitations must not spend the registration budget: {body}"
	);
}

/// `totp::spend_recovery` was reachable from `login_totp` and nowhere else, so a user who
/// had lost the authenticator, held the printed codes and had also forgotten the password had
/// no path at all — the mailed reset token is refused without a TOTP code, and they cannot log
/// in to reach `change_password`.
#[tokio::test]
async fn a_recovery_code_completes_a_reset_but_not_a_password_change() {
	let db = TmpDb::new("recovery-reset");
	let (app, store) = setup(&db).await;
	let account = account(&store, "recovery@e.st").await;
	let auth = Auth::new(app.clone());

	// Enrol for real, keeping the codes `confirm_totp` prints once and never again.
	let enrolment = auth.enrol_totp(&ctx_for(&account)).await.unwrap();
	let secret = base32_decode(&enrolment.secret);
	let codes = auth
		.confirm_totp(&ctx_for(&account), &totp_code(&secret, 0))
		.await
		.unwrap()
		.recovery_codes;
	assert!(codes.len() >= 2, "the test needs two codes");

	// Without a second factor the reset still stops at the ticket.
	let token = reset_token(&app, &store, "recovery@e.st").await;
	assert!(matches!(
		auth.reset_password(&ip_ctx(), &token, None, None, "a-brand-new-password".to_owned())
			.await
			.unwrap(),
		LoginOutcome::TotpRequired { .. }
	));

	// With a printed code it completes.
	let outcome = auth
		.reset_password(&ip_ctx(), &token, None, Some(&codes[0]), "a-brand-new-password".to_owned())
		.await
		.unwrap();
	assert!(matches!(outcome, LoginOutcome::Signed(_)));

	// Single-use: the same code again is refused, and the password does not move.
	let token = reset_token(&app, &store, "recovery@e.st").await;
	let err = auth
		.reset_password(&ip_ctx(), &token, None, Some(&codes[0]), "third-password".to_owned())
		.await
		.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::UNAUTHORIZED);

	// `change_password` deliberately does **not** take one: it is reached from inside a live
	// session and mints `auth_at = now`, which `step_up` refuses a printed code for. The
	// reset route above is the recovery path, and it is the only one.
}

/// `set_account_status` had no non-test caller and `token_epoch` moved only on a password
/// change, so the stateless-JWT design's whole compensation for having no denylist was
/// unreachable: `login` and `auth_mw` branched on `SUSPENDED` for a state nothing could enter.
///
/// `token_epoch` is the other half of that one lever, and `verify`'s `epoch != claims.ep`
/// check was untested — a refactor dropping it would leave password change, suspension and
/// anonymization silently signing nobody out.
#[tokio::test]
async fn suspending_an_account_kills_its_live_tokens_at_once() {
	let db = TmpDb::new("revoke-suspend");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let user = account(&store, "suspendme@e.st").await;
	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "suspendme@e.st").await;

	// The token works before the suspension.
	let (status, _) = call(&router, "GET", "/api/auth/me", &token, None).await;
	assert_eq!(status, StatusCode::OK);

	// A plain member cannot reach either lever — the gate is `ctx.actor`, not the router.
	let plain = account(&store, "plain@e.st").await;
	for err in [
		auth.set_account_status(&ctx_for(&plain), user.uid.as_str(), AccountStatus::Suspended)
			.await
			.unwrap_err(),
		auth.revoke_tokens(&ctx_for(&plain), user.uid.as_str()).await.unwrap_err(),
	] {
		assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");
	}

	let op = Ctx::system("admin");
	auth.set_account_status(&op, user.uid.as_str(), AccountStatus::Suspended)
		.await
		.unwrap();

	let (status, body) = call(&router, "GET", "/api/auth/me", &token, None).await;
	assert_ne!(
		status,
		StatusCode::OK,
		"a suspended account's live token must stop working now, not at exp: {body}"
	);

	// Another account's uid is not found, never forbidden.
	let unknown = mintworks_core::prelude::AccountId::generate().into_string();
	assert_eq!(auth.revoke_tokens(&op, &unknown).await.unwrap_err().parts().1, "E-CORE-NOTFOUND");

	// The epoch on its own, without a status change: `verify` must refuse a token minted
	// under a superseded one, and say which refusal it is.
	let bumped = account(&store, "epoch@e.st").await;
	let live = access_token(&app, "epoch@e.st").await;
	assert_eq!(call(&router, "GET", "/api/auth/me", &live, None).await.0, StatusCode::OK);
	store.bump_token_epoch(bumped.id).await.unwrap();
	let (status, body) = call(&router, "GET", "/api/auth/me", &live, None).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
	assert_eq!(body["error"]["errCode"], "E-AUTH-TOKEN");
}

/// `revoke_tokens` signs every device out without touching the credential: the password still
/// works and the next login mints a token at the new epoch.
#[tokio::test]
async fn revoking_tokens_leaves_the_password_working() {
	let db = TmpDb::new("revoke-tokens");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let user = account(&store, "revokeme@e.st").await;
	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "revokeme@e.st").await;
	assert_eq!(call(&router, "GET", "/api/auth/me", &token, None).await.0, StatusCode::OK);

	auth.revoke_tokens(&Ctx::system("admin"), user.uid.as_str()).await.unwrap();

	assert_eq!(
		call(&router, "GET", "/api/auth/me", &token, None).await.0,
		StatusCode::UNAUTHORIZED,
		"the old token is dead"
	);
	// The credential is untouched: the password still logs in, at the new epoch.
	let fresh = access_token(&app, "revokeme@e.st").await;
	assert_eq!(call(&router, "GET", "/api/auth/me", &fresh, None).await.0, StatusCode::OK);
	assert_ne!(fresh, token);
}

/// Every transition this lever must refuse, and why each is a trap rather than a preference.
/// `AccountStatus` is `Deserialize` and the adapter's SQL permits all of them:
///
/// - `-> ANONYMIZED` flipped an account to erased without scrubbing a field, without the
///   owned-organisation guard and without the GDPR receipt — irreversibly. That status is
///   owned by the GDPR erasure path alone.
/// - `ACTIVE -> PENDING` stranded the account: `activate` refuses a token for an account that
///   already has a `pwd_hash` — the invariant its comment relies on — and `login` refuses a
///   non-`ACTIVE` one.
/// - `PENDING -> ACTIVE` marked the address verified with `pwd_hash` still NULL and
///   invalidated the activation token at the same time, because the signature covers
///   `accounts.status` — so the invitee's only route back was a password reset.
/// - `PENDING -> SUSPENDED -> ACTIVE` reached that same state by a detour, which is why the
///   guard cannot read `target.status == PENDING`: it tested one hop.
/// - `PENDING -> SUSPENDED` has no way back at all: `PENDING` is refused by this route,
///   `ACTIVE` because `activated_at` is NULL, and the mailed link no longer verifies.
#[tokio::test]
async fn the_status_lever_refuses_every_transition_nothing_recovers() {
	let db = TmpDb::new("status-lever");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let op = Ctx::system("admin");

	// Anything but `Active` here means "never activated": `unactivate` clears `activated_at`,
	// which is what makes the way back unreachable.
	for (i, (from, to)) in [
		(AccountStatus::Active, AccountStatus::Anonymized),
		(AccountStatus::Active, AccountStatus::Pending),
		(AccountStatus::Pending, AccountStatus::Active),
		(AccountStatus::Pending, AccountStatus::Suspended),
		(AccountStatus::Suspended, AccountStatus::Active),
	]
	.into_iter()
	.enumerate()
	{
		let email = format!("lever{i}@e.st");
		let user = account(&store, &email).await;
		if from != AccountStatus::Active {
			unactivate(&store, user.id).await;
			store.set_account_status(user.id, AccountStatus::Pending).await.unwrap();
			// Through the store, because the route refuses this hop too: however the row
			// reached SUSPENDED, `activated_at` is still NULL.
			if from == AccountStatus::Suspended {
				store.set_account_status(user.id, AccountStatus::Suspended).await.unwrap();
			}
		}

		let err = auth.set_account_status(&op, user.uid.as_str(), to).await.unwrap_err();
		assert_eq!(err.parts().1, "E-CORE-VALIDATION", "{from:?} -> {to:?}");
		let row = store.account_by_id(user.id).await.unwrap().unwrap();
		assert_eq!(row.status, from, "{from:?} -> {to:?}");
		assert_eq!(row.email, email, "nothing was scrubbed, so nothing may read as erased");
	}

	// A narrowing, not a blanket refusal: ACTIVE <-> SUSPENDED is what the route is for, and
	// an account that made the round trip still logs in.
	let live = account(&store, "live@e.st").await;
	for to in [AccountStatus::Suspended, AccountStatus::Active] {
		auth.set_account_status(&op, live.uid.as_str(), to).await.unwrap();
		assert_eq!(store.account_by_id(live.id).await.unwrap().unwrap().status, to);
	}
	assert!(matches!(
		auth.login(&Ctx::public("test"), &credentials(&live.email, PASSWORD))
			.await
			.unwrap(),
		LoginOutcome::Signed(_)
	));
}

/// The gate holds `current_legal_doc` in a 30 s cache, so a publish that did not invalidate it
/// left every process reporting the superseded version as current for half a minute.
#[tokio::test]
async fn publishing_a_document_is_visible_to_the_consent_gate_immediately() {
	let db = TmpDb::new("publish-invalidates-gate");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let user = account(&store, "gated@e.st").await;

	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		store
			.insert_legal_doc(&NewLegalDoc {
				kind,
				locale: "hu".to_owned(),
				version: "1".to_owned(),
				title: format!("{kind:?} 1"),
				body: "az elso szoveg".to_owned(),
				sha256: format!("{:064x}", 1),
				effective_from: Timestamp(0),
			})
			.await
			.unwrap();
		accept_current(&store, &user, kind).await;
	}
	// Warms the cache with version 1 as current, which is what the publish has to displace.
	assert!(auth.me(&ctx_for(&user)).await.unwrap().consents_required.is_empty());

	auth.publish_legal_document(
		&Ctx::system("admin"),
		PublishLegalDoc {
			kind: LegalKind::Tos,
			locale: "hu".to_owned(),
			version: "2".to_owned(),
			title: "ASZF 2".to_owned(),
			body: "a masodik szoveg".to_owned(),
			effective_from: Timestamp(0),
		},
	)
	.await
	.unwrap();

	assert_eq!(
		auth.me(&ctx_for(&user)).await.unwrap().consents_required,
		vec![LegalKind::Tos],
		"the cache must not pin the superseded version"
	);
}

/// The currency was a second, independent `update_org` after the row was committed, so a
/// failure between them left an org carrying the wrong billing currency while the caller
/// saw a 500. One statement now, inside `create_org`'s own transaction.
#[tokio::test]
async fn an_orgs_billing_currency_is_set_in_the_call_that_creates_it() {
	let db = TmpDb::new("org-currency-atomic");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "currency-owner@e.st").await;
	let auth = Auth::new(app.clone());
	let eur = CurrencyCode::parse("EUR").unwrap();
	// Only HUF is seeded; an operator adds the rest by SQL.
	sqlx::query(
		"INSERT INTO currencies (code, price_round_step, mode, enabled) \
		 VALUES ('EUR', 1, 'OFFICIAL', 1)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	let org = auth.create_org(&ctx_for(&owner), "Céges Kft.", Some(&eur)).await.unwrap();
	assert_eq!(org.billing_currency.as_ref(), Some(&eur));
	// And off the row, not just off the returned struct.
	assert_eq!(
		store.org_by_id(org.id).await.unwrap().unwrap().billing_currency.as_ref(),
		Some(&eur)
	);
}

/// `current_legal_doc` falls back to any published locale, so a consent recorded against
/// that fallback text satisfied a gate that compared the version alone once the account's own
/// locale was published under the same version string — different wording, same `1`.
#[tokio::test]
async fn a_consent_against_another_locales_wording_does_not_satisfy_the_gate() {
	let db = TmpDb::new("consent-sha");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let user = account(&store, "locale@e.st").await;
	assert_eq!(user.locale, "hu");

	// Published in `en` only, so `current_legal_doc` serves it to a `hu` account by fallback,
	// and that is the wording the account accepts.
	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		store
			.insert_legal_doc(&NewLegalDoc {
				kind,
				locale: "en".to_owned(),
				version: "1".to_owned(),
				title: format!("{kind:?} en"),
				body: "the english wording".to_owned(),
				sha256: format!("{:064x}", 1),
				effective_from: Timestamp(0),
			})
			.await
			.unwrap();
		accept_current(&store, &user, kind).await;
	}
	assert!(auth.me(&ctx_for(&user)).await.unwrap().consents_required.is_empty());

	// The same version, now in the account's own locale, with different text. Through the
	// handle, not `insert_legal_doc`: that is what invalidates the gate's `current_legal_doc`
	// cache, and a raw insert is only visible to another process within the cache TTL anyway.
	auth.publish_legal_document(
		&Ctx::system("admin"),
		PublishLegalDoc {
			kind: LegalKind::Tos,
			locale: "hu".to_owned(),
			version: "1".to_owned(),
			title: "ASZF 1".to_owned(),
			body: "a magyar szoveg".to_owned(),
			effective_from: Timestamp(0),
		},
	)
	.await
	.unwrap();

	assert!(
		auth.me(&ctx_for(&user))
			.await
			.unwrap()
			.consents_required
			.contains(&LegalKind::Tos),
		"a grant against the fallback wording is not a grant against this document"
	);
}

/// `refresh` mints a fresh 7-day token every time and carries `auth_at` over unchanged, so a
/// captured refresh token renewed itself indefinitely — step-up never fired and `token_epoch`
/// moves only on a password change. No session table may exist, so the cap rides in the claim.
#[tokio::test]
async fn a_refresh_token_cannot_renew_a_session_past_its_absolute_cap() {
	let db = TmpDb::new("refresh-session-cap");
	let (app, store) = setup(&db).await;
	let user = account(&store, "forever@e.st").await;
	let auth = Auth::new(app.clone());

	let LoginOutcome::Signed(tokens) = auth
		.login(&Ctx::public("test"), &credentials(&user.email, PASSWORD))
		.await
		.unwrap()
	else {
		panic!("expected a signed pair")
	};
	// A fresh session renews.
	assert!(auth.refresh(&Ctx::public("test"), &tokens.refresh_token).await.is_ok());

	// The same token, re-signed with a session start older than the cap. Forged rather than
	// waiting a month, and with the deployment's own key, so only `auth_at` differs.
	let cap = app.settings.int("auth.session_max_seconds").await.unwrap();
	let key = app.secrets.get(mintworks_core::auth_mw::JWT_SECRET_KEY).await.unwrap().unwrap();
	let mut claims = jwt_payload(&tokens.refresh_token);
	claims["auth_at"] = serde_json::json!(Timestamp::now().0 - cap - 1);
	let stale = jsonwebtoken::encode(
		&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
		&claims,
		&jsonwebtoken::EncodingKey::from_secret(&key),
	)
	.unwrap();

	let err = auth.refresh(&Ctx::public("test"), &stale).await.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-TOKEN");
}

/// The gate (`token::consents_required`) reads `latest_consent(account, kind, None)`, so a
/// `TOS` accepted against an org satisfied nothing: every gated route stayed
/// `403 E-AUTH-CONSENT-REQUIRED` forever while `GET /api/consents` reported the grant.
#[tokio::test]
async fn a_gating_consent_cannot_be_scoped_to_an_org() {
	let db = TmpDb::new("consent-gating-scope");
	let (app, store) = setup(&db).await;
	publish_legal(&store, LegalKind::Tos, "1").await;
	publish_legal(&store, LegalKind::Privacy, "1").await;
	let owner = account(&store, "scoped@e.st").await;
	let (_id, ctx) = org(&store, &owner, "A Kft.").await;
	let uid: String = sqlx::query_scalar("SELECT uid FROM orgs WHERE owner_account_id = ?")
		.bind(owner.id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();

	let auth = Auth::new(app.clone());
	let grant = |kind, org: Option<String>| ConsentGrant {
		kind,
		version: "1".to_owned(),
		doc_sha256: Some(format!("{:064x}", 0)),
		org_uid: org,
		user_agent: None,
	};
	// Both entries of `token::GATING_KINDS`, not just the first: the refusal reads that list,
	// so a member it missed would be org-scopable and gate the account out forever.
	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		let err = auth.record_consent(&ctx, &grant(kind, Some(uid.clone()))).await.unwrap_err();
		assert_eq!(err.parts().1, "E-AUTH-CONSENT-SCOPE", "{kind:?}");
	}

	// Account-wide, it clears the gate — which only a gated request can show.
	auth.record_consent(&ctx, &grant(LegalKind::Tos, None)).await.unwrap();
	accept_current(&store, &owner, LegalKind::Privacy).await;
	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "scoped@e.st").await;
	let (status, body) = call(&router, "GET", "/api/orgs", &token, None).await;
	assert_eq!(status, StatusCode::OK, "an account-wide TOS satisfies the gate: {body}");
}

// -------------------------------------------------------------- regression tests

/// The export document is legal evidence in a subject access request, and every money and
/// quantity column left the database as its raw scaled integer: a 12 700 Ft invoice read as
/// `1270000` and a quantity of 2 as `2000000`. The wire shape for amounts holds here too.
#[tokio::test]
async fn the_export_renders_money_and_quantity_as_strings() {
	let db = TmpDb::new("export-money");
	let (app, store) = setup(&db).await;
	let subject = account(&store, "money@e.st").await;
	let personal: i64 =
		sqlx::query_scalar("SELECT id FROM orgs WHERE owner_account_id = ? AND kind = 'PERSONAL'")
			.bind(subject.id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();

	sqlx::query("INSERT INTO currencies (code, mode, fixed_rate_e6) VALUES ('EUR', 'FIXED', 1)")
		.execute(store.write_pool())
		.await
		.unwrap();
	sqlx::raw_sql(
		"INSERT INTO sellers (id, uid, org_id, nav_base_url, created_at)
		 VALUES (1, 'sel_test', (SELECT id FROM orgs WHERE kind = 'ROOT'), '', 0);
		 INSERT INTO seller_versions (seller_ver, seller_id, status, name, country, tax_number,
		                              postcode, city, street, created_at, valid_from)
		 VALUES (1, 1, 'CURRENT', 'Teszt Kft.', 'HU', '12345678242', '1011', 'Budapest',
		         'Fo utca 1.', 0, 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	// A EUR invoice, so `invoice_vat_groups` carries the statutory HUF trio too.
	sqlx::query(
		"INSERT INTO invoices (id, uid, org_id, seller_id, seller_ver, status, number, issued_at,
		                       fulfilment_date, currency, rate_e6, huf_rate_e6,
		                       net, vat, gross, paid_amount, buyer_name, created_at, updated_at)
		 VALUES (1, 'inv_money', ?, 1, 1, 'ISSUED', 'A2026/000001', 0, '2026-01-01', 'EUR',
		         400000000, 400000000, 1000000, 270000, 1270000, 1270000, 'Buyer', 0, 0)",
	)
	.bind(personal)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO invoice_lines (id, invoice_id, line_no, description, unit, qty, unit_price,
		                            net, vat_code, vat_rate_bp, vat, gross)
		 VALUES (1, 1, 1, 'Widget', 'db', 2000000, 500000, 1000000, 'STD27', 2700, 270000,
		         1270000)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO invoice_vat_groups (invoice_id, vat_code, vat_rate_bp, net, vat, gross,
		                                 net_huf, vat_huf, gross_huf)
		 VALUES (1, 'STD27', 2700, 1000000, 270000, 1270000, 400000000, 108000000, 508000000)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	let (_subject, doc) = Auth::new(app).export_account(&ctx_for(&subject)).await.unwrap();

	assert_eq!(
		doc["invoices"][0]["gross"],
		serde_json::json!({ "amount": "12700.00", "currency": "EUR" }),
		"an amount carries its currency where the row has one: {}",
		doc["invoices"][0]
	);
	// `paid_amount` is the multi-word key: the adapter's `dump` decides the export's JSON keys
	// and `gdpr::rescale` looks them back up, and the two used to hold a byte-identical `camel`
	// each — drifted, the money columns would have left a subject access request as raw
	// minor-unit integers, silently.
	assert_eq!(
		doc["invoices"][0]["paidAmount"],
		serde_json::json!({ "amount": "12700.00", "currency": "EUR" })
	);
	assert_eq!(doc["invoiceLines"][0]["qty"], "2.000000");
	// `invoice_lines` has no `currency` column, so a bare decimal string is the shape.
	assert_eq!(doc["invoiceLines"][0]["unitPrice"], "5000.00");
	assert_eq!(
		doc["invoiceVatGroups"][0]["vatHuf"],
		serde_json::json!({ "amount": "1080000.00", "currency": "HUF" }),
		"Áfa tv. 172. § HUF VAT is HUF whatever the invoice's currency is"
	);
	// Not amounts, and must not be rendered as any: a 1e6 rate and integer basis points.
	assert_eq!(doc["invoices"][0]["rateE6"], 400_000_000_i64);
	assert_eq!(doc["invoiceLines"][0]["vatRateBp"], 2700);
}

/// `ExportScope::OwnedOrg` read `orgs.owner_account_id`, so an ordinary member of an
/// organisation got a `memberships` row carrying a bare uid and no org name anywhere —
/// while §9.5 and the scope's own doc both say which organisations a person belongs to is
/// their personal data. `ownerAccountUid` goes the other way: on an org the subject merely
/// belongs to it is another person's public id.
#[tokio::test]
async fn the_export_names_an_organisation_the_subject_only_belongs_to() {
	let db = TmpDb::new("export-membership");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "org-owner@e.st").await;
	let member = account(&store, "org-member@e.st").await;
	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Belongs Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();
	store
		.put_membership(org.id, member.id, mintworks_auth::store::Role::Member)
		.await
		.unwrap();
	store.accept_membership(org.id, member.id, Timestamp::now()).await.unwrap();

	let (_subject, doc) = Auth::new(app).export_account(&ctx_for(&member)).await.unwrap();

	let named = doc["orgs"]
		.as_array()
		.unwrap()
		.iter()
		.find(|t| t["uid"] == org.uid.as_str())
		.expect("the organisation the subject belongs to");
	assert_eq!(named["name"], "Belongs Kft.");
	assert!(
		!doc.to_string().contains("ownerAccountUid"),
		"another member's public id is not this subject's data: {doc}"
	);
}

/// The new-address branch swallowed a mail-queue failure and answered `204`; the
/// already-registered branch propagated it as a `500`. While the `jobs` writer was degraded
/// that pair *was* the account-existence oracle the route exists to deny.
#[tokio::test]
async fn register_answers_alike_when_the_mail_queue_is_broken() {
	let db = TmpDb::new("register-broken-queue");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		publish_legal(&store, kind, "1").await;
	}
	let signup = |email: &str| Registration {
		email: email.to_owned(),
		name: None,
		locale: Some("hu".to_owned()),
		consents: vec![
			serde_json::from_value(serde_json::json!({ "kind": "TOS", "version": "1" })).unwrap(),
			serde_json::from_value(serde_json::json!({ "kind": "PRIVACY", "version": "1" }))
				.unwrap(),
		],
		user_agent: None,
		pow: None,
		ref_code: None,
	};

	// `Ctx::system` has no IP, so the proof-of-work gate is not what this is testing.
	let ctx = Ctx::system("test");
	auth.register(&ctx, &signup("oracle@e.st")).await.unwrap();
	// Both branches queue a `SEND_EMAIL` job, so dropping the table breaks the queue for both
	// and nothing else. `already_registered` also reads the account first, which still works.
	sqlx::query("DROP TABLE jobs").execute(store.write_pool()).await.unwrap();

	assert!(auth.register(&ctx, &signup("fresh@e.st")).await.is_ok(), "the new address");
	assert!(
		auth.register(&ctx, &signup("oracle@e.st")).await.is_ok(),
		"the registered address must answer identically, or the status code is the oracle"
	);
}

/// Both routes document "answers the same way whatever the address" and both pay a matching
/// writer round-trip on the miss branch so the two are indistinguishable by latency — then
/// `?`d the mail enqueue on the hit branch only, making any `jobs` failure the oracle again.
#[tokio::test]
async fn the_silent_routes_answer_alike_when_the_mail_queue_is_broken() {
	let db = TmpDb::new("silent-routes-broken-queue");
	let (app, store) = setup(&db).await;
	let auth = Auth::new(app.clone());
	let live = account(&store, "live@e.st").await;
	let invitee = account(&store, "invitee@e.st").await;
	store.set_account_status(invitee.id, AccountStatus::Pending).await.unwrap();

	// `Ctx::system` has no IP, so the proof-of-work gate is not what this is testing.
	let ctx = Ctx::system("test");
	sqlx::query("DROP TABLE jobs").execute(store.write_pool()).await.unwrap();

	auth.request_password_reset(&ctx, &live.email, None).await.unwrap();
	auth.request_password_reset(&ctx, "nobody@e.st", None).await.unwrap();
	auth.resend_activation(&ctx, &invitee.email, None).await.unwrap();
	auth.resend_activation(&ctx, "nobody@e.st", None).await.unwrap();
}

/// `add_member` read the address on the reader pool and inserted on the writer, so two
/// overlapping invitations of the same new address raced: the loser's `UNIQUE(email)` came
/// back as `409 E-AUTH-EMAIL-TAKEN` out of a route documented `204`-always precisely so a
/// org admin cannot probe arbitrary addresses.
#[tokio::test]
async fn add_member_does_not_leak_an_address_that_appeared_mid_flight() {
	let db = TmpDb::new("add-member-race");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "race-owner@e.st").await;
	let org = store
		.create_org(
			mintworks_auth::store::OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Race Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();
	store.accept_membership(org.id, owner.id, Timestamp::now()).await.unwrap();
	let ctx = Ctx { org_id: Some(org.id), ..ctx_for(&owner) };

	let auth = Auth::new(app.clone());
	let role = mintworks_auth::store::Role::Member;
	let (first, second) = tokio::join!(
		auth.add_member(&ctx, "newcomer@e.st", role),
		auth.add_member(&ctx, "newcomer@e.st", role),
	);
	assert!(first.is_ok(), "{first:?}");
	assert!(second.is_ok(), "the loser of the race may not answer about the address: {second:?}");
}

/// `verify` refuses a `typ: "refresh"` token on an access path, and nothing tested it. A
/// refresh token outlives an access token by weeks, so accepting one would hand every
/// authenticated route that lifetime.
#[tokio::test]
async fn a_refresh_token_is_not_an_access_token() {
	let db = TmpDb::new("refresh-not-access");
	let (app, store) = setup(&db).await;
	let account = account(&store, "typ@e.st").await;
	let auth = Auth::new(app.clone());
	let LoginOutcome::Signed(tokens) = auth
		.login(&Ctx::public("test"), &credentials(&account.email, PASSWORD))
		.await
		.unwrap()
	else {
		panic!("expected a signed pair")
	};

	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let (status, body) = call_raw(
		&router,
		"/api/auth/me",
		("authorization", format!("Bearer {}", tokens.refresh_token)),
	)
	.await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

	// The distinction, not just the rejection: the same token still refreshes.
	assert!(auth.refresh(&Ctx::public("test"), &tokens.refresh_token).await.is_ok());
}

/// The idempotency short-circuit in `auth_mw::authenticate` keyed on a `Ctx` being present in
/// the extensions, and `Ctx::system` is public — so any consumer middleware layered outside a
/// bundle that inserted one turned every route behind `require_auth` into an unauthenticated
/// `Actor::System` path, which `require_operator` grants unconditionally.
#[tokio::test]
async fn an_injected_ctx_does_not_pass_for_authentication() {
	let db = TmpDb::new("injected-ctx");
	let (app, _store) = setup(&db).await;
	let router = mintworks_auth::routes::authenticated()
		.layer(axum::middleware::from_fn(
			|mut req: axum::extract::Request, next: axum::middleware::Next| async move {
				req.extensions_mut().insert(Ctx::system("injected"));
				next.run(req).await
			},
		))
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());

	let req = axum::http::Request::builder()
		.method("GET")
		.uri("/api/auth/me")
		.body(axum::body::Body::empty())
		.unwrap();
	let (status, body) = parts(tower::ServiceExt::oneshot(router, req).await.unwrap()).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "a fabricated Ctx is not a credential: {body}");
}

/// A key row written straight into the store. `Auth::create_api_key` validates its scopes
/// against `AppState::route_scopes`, and the app [`setup`] builds mounts no bundle, so the
/// mint path is not what these tests drive. The hash has to be the real one all the same:
/// `auth_mw::verify_api_key` compares `hex(sha256(key))` in constant time.
async fn insert_api_key(
	store: &SqliteStore,
	org_id: i64,
	account_id: i64,
	prefix: &str,
	scopes: &str,
) -> String {
	let key = format!("sk_{prefix}_c2VjcmV0");
	store
		.create_api_key(
			&NewApiKey {
				org_id,
				account_id,
				name: "CI".to_owned(),
				prefix: prefix.to_owned(),
				key_hash: hex::encode(Sha256::digest(key.as_bytes())),
				scopes: scopes.to_owned(),
				expires_at: None,
			},
			i64::MAX,
		)
		.await
		.unwrap();
	key
}

/// The scope set is the whole containment an API key has, so it has to be what refuses
/// it — and the four ways a key dies without any revocation have to answer alike, or a
/// consumer learns which of its keys is merely suspended from which is deleted.
#[tokio::test]
async fn an_api_key_is_held_to_its_scopes_and_dies_with_its_membership() {
	let db = TmpDb::new("key-scope");
	let (app, store) = setup(&db).await;
	// A second account owns the shared org, so the key's holder can be a plain `MEMBER`:
	// `remove_membership` refuses an `OWNER`, and a removed membership is one of the four
	// deaths this test drives.
	let owner = account(&store, "key-owner@e.st").await;
	let holder = account(&store, "key-holder@e.st").await;
	let shared = store
		.create_org(OrgKind::Shared, store.root_org_id().await.unwrap(), "Kft.", owner.id, None)
		.await
		.unwrap();
	store.put_membership(shared.id, holder.id, Role::Member).await.unwrap();
	store.accept_membership(shared.id, holder.id, Timestamp(1)).await.unwrap();

	let invoice_key =
		insert_api_key(&store, shared.id, holder.id, "aaaaaaaa", r#"["invoice:read"]"#).await;
	let booking_key =
		insert_api_key(&store, shared.id, holder.id, "bbbbbbbb", r#"["booking:read"]"#).await;

	// `scope()` annotates the bundle; `auth_mw::authenticate` does the check.
	let router: axum::Router<App> = axum::Router::<App>::new()
		.route("/api/invoice-read", axum::routing::get(|| async { "ok" }))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth))
		.scope("invoice")
		.into();
	let router = router.layer(axum::Extension(app.clone())).with_state(app.clone());
	// The auth bundle is deliberately unscoped, which is the fail-closed half of the same check.
	let unscoped = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());

	let (status, _) = call(&router, "GET", "/api/invoice-read", &invoice_key, None).await;
	assert_eq!(status, StatusCode::OK, "a key carrying the route's scope is accepted");

	let (status, body) = call(&router, "GET", "/api/invoice-read", &booking_key, None).await;
	assert_eq!(status, StatusCode::FORBIDDEN);
	assert_eq!(body["error"]["errCode"], "E-AUTH-SCOPE");

	// Nothing under `/api/auth/*` is scoped, so no key can reach it whatever it carries.
	let (status, body) = call(&unscoped, "GET", "/api/auth/me", &invoice_key, None).await;
	assert_eq!(status, StatusCode::FORBIDDEN);
	assert_eq!(body["error"]["errCode"], "E-AUTH-SCOPE");

	// The key row is untouched: only the join can kill it, and putting the membership back
	// revives it, which is what proves the join and not a revocation answered.
	assert!(store.remove_membership(shared.id, holder.id).await.unwrap());
	let (status, body) = call(&router, "GET", "/api/invoice-read", &invoice_key, None).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["error"]["errCode"], "E-AUTH-KEY-REVOKED");

	store.put_membership(shared.id, holder.id, Role::Member).await.unwrap();
	store.accept_membership(shared.id, holder.id, Timestamp(2)).await.unwrap();
	let (status, _) = call(&router, "GET", "/api/invoice-read", &invoice_key, None).await;
	assert_eq!(status, StatusCode::OK);

	sqlx::query("UPDATE api_keys SET revoked_at = ? WHERE prefix = ?")
		.bind(Timestamp::now().0)
		.bind("aaaaaaaa")
		.execute(store.write_pool())
		.await
		.unwrap();
	let (status, body) = call(&router, "GET", "/api/invoice-read", &invoice_key, None).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["error"]["errCode"], "E-AUTH-KEY-REVOKED");

	// The expiry is checked in `verify`, before the scope, so a key that is both expired and
	// out of scope still answers as an expired credential rather than a forbidden route.
	sqlx::query("UPDATE api_keys SET expires_at = ? WHERE prefix = ?")
		.bind(Timestamp::now().0 - 1)
		.bind("bbbbbbbb")
		.execute(store.write_pool())
		.await
		.unwrap();
	let (status, body) = call(&router, "GET", "/api/invoice-read", &booking_key, None).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["error"]["errCode"], "E-AUTH-KEY-REVOKED");
}

/// A key is an org credential, but revoking one is not every member's to do: a second member
/// could list and revoke the whole org's keys from the lowest role there is.
#[tokio::test]
async fn a_member_cannot_see_or_revoke_another_members_api_key() {
	let db = TmpDb::new("key-member-scope");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "key-owner2@e.st").await;
	let a = account(&store, "key-a@e.st").await;
	let b = account(&store, "key-b@e.st").await;
	let shared = store
		.create_org(OrgKind::Shared, store.root_org_id().await.unwrap(), "Kft.", owner.id, None)
		.await
		.unwrap();
	for m in [a.id, b.id] {
		store.put_membership(shared.id, m, Role::Member).await.unwrap();
		store.accept_membership(shared.id, m, Timestamp(1)).await.unwrap();
	}
	insert_api_key(&store, shared.id, a.id, "aaaa1111", r#"["invoice:read"]"#).await;
	insert_api_key(&store, shared.id, b.id, "bbbb2222", r#"["invoice:read"]"#).await;
	let rows = store.api_keys_for_org(shared.id).await.unwrap();
	let a_uid = rows.iter().find(|k| k.account_id == a.id).unwrap().uid.clone();
	let b_uid = rows.iter().find(|k| k.account_id == b.id).unwrap().uid.clone();

	let auth = Auth::new(app.clone());
	let a_ctx = ctx_for(&a).with_org(shared.id);
	let b_ctx = ctx_for(&b).with_org(shared.id);

	let seen = auth.list_api_keys(&b_ctx).await.unwrap();
	assert_eq!(
		seen.iter().map(|k| k.uid.as_str()).collect::<Vec<_>>(),
		vec![b_uid.as_str()],
		"a plain member saw the whole org's keys"
	);

	let err = auth.rename_api_key(&b_ctx, a_uid.as_str(), "stolen").await.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::NOT_FOUND, "E-CORE-NOTFOUND"));
	let err = auth.revoke_api_key(&b_ctx, a_uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts(), (StatusCode::NOT_FOUND, "E-CORE-NOTFOUND"));

	// A's own key is A's to revoke, which is the half that must keep working.
	auth.revoke_api_key(&a_ctx, a_uid.as_str()).await.unwrap();

	// Promoted, B is the org's admin and the whole list is B's again.
	store.put_membership(shared.id, b.id, Role::Admin).await.unwrap();
	insert_api_key(&store, shared.id, a.id, "aaaa3333", r#"["invoice:read"]"#).await;
	let seen = auth.list_api_keys(&b_ctx).await.unwrap();
	assert_eq!(seen.len(), 2, "an admin sees the whole org: {seen:?}");
	let second = store
		.api_keys_for_org(shared.id)
		.await
		.unwrap()
		.into_iter()
		.find(|k| k.prefix == "aaaa3333")
		.unwrap()
		.uid;
	auth.revoke_api_key(&b_ctx, second.as_str()).await.unwrap();
}

/// A public bundle serves an anonymous caller anyway, so a key reaching one must be ignored
/// rather than refused: 403ing it made `refresh` and `logout` unreachable for any client that
/// attaches `Authorization` to every request.
#[tokio::test]
async fn an_api_key_on_a_public_route_is_ignored_not_refused() {
	let db = TmpDb::new("key-public-route");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "key-public-owner@e.st").await;
	let holder = account(&store, "key-public-holder@e.st").await;
	let shared = store
		.create_org(OrgKind::Shared, store.root_org_id().await.unwrap(), "Kft.", owner.id, None)
		.await
		.unwrap();
	store.put_membership(shared.id, holder.id, Role::Member).await.unwrap();
	store.accept_membership(shared.id, holder.id, Timestamp(1)).await.unwrap();
	let key = insert_api_key(&store, shared.id, holder.id, "cafebabe", r#"["invoice:read"]"#).await;

	let router = mintworks_auth::routes::public()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let (status, body) = call(&router, "POST", "/api/auth/logout", &key, None).await;
	assert_eq!(status, StatusCode::NO_CONTENT, "a key on a public route was refused: {body}");
	assert!(body["error"].is_null(), "a public route answered an error envelope: {body}");
}

/// The tag is drawn from the base64url alphabet, which contains the `_` that separates
/// `sk_<tag>_<secret>`, so it is read at a fixed offset rather than by splitting on `_`.
#[tokio::test]
async fn an_api_key_whose_prefix_contains_the_separator_still_authenticates() {
	let db = TmpDb::new("key-prefix-underscore");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "key-underscore-owner@e.st").await;
	let holder = account(&store, "key-underscore-holder@e.st").await;
	let shared = store
		.create_org(OrgKind::Shared, store.root_org_id().await.unwrap(), "Kft.", owner.id, None)
		.await
		.unwrap();
	store.put_membership(shared.id, holder.id, Role::Member).await.unwrap();
	store.accept_membership(shared.id, holder.id, Timestamp(1)).await.unwrap();

	let key = insert_api_key(&store, shared.id, holder.id, "ab_cd123", r#"["invoice:read"]"#).await;
	let router: axum::Router<App> = probe_bundle().into();
	let router = router.layer(axum::Extension(app.clone())).with_state(app.clone());

	let (status, body) = call(&router, "GET", "/api/invoice-read", &key, None).await;
	assert_eq!(status, StatusCode::OK, "a tag containing `_` was truncated: {body}");

	// The offset is fixed, not a scan for eight characters somewhere: shifting the tag by one
	// reads `_ab_cd12`, whose ninth byte is not the separator, and is refused.
	let shifted = format!("sk__{}_c2VjcmV0", &key[3..11]);
	let (status, body) = call(&router, "GET", "/api/invoice-read", &shifted, None).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["error"]["errCode"], "E-AUTH-TOKEN");
}

/// A revoked key is a 401 the caller cannot fix by trying again, so it must not feed the per-IP
/// `AUTH_FAILED` budget `Auth::login` reads to demand proof of work.
#[tokio::test]
async fn a_dead_key_does_not_spend_the_shared_auth_failure_budget() {
	let db = TmpDb::new("key-not-a-failure");
	let (app, store) = setup(&db).await;
	let owner = account(&store, "dead-key-owner@e.st").await;
	let holder = account(&store, "dead-key-holder@e.st").await;
	let shared = store
		.create_org(OrgKind::Shared, store.root_org_id().await.unwrap(), "Kft.", owner.id, None)
		.await
		.unwrap();
	store.put_membership(shared.id, holder.id, Role::Member).await.unwrap();
	store.accept_membership(shared.id, holder.id, Timestamp(1)).await.unwrap();
	let key = insert_api_key(&store, shared.id, holder.id, "deadbeef", r#"["invoice:read"]"#).await;

	let router: axum::Router<App> = probe_bundle().into();
	let router = router
		.layer(axum::Extension(ClientIp(PEER.ip())))
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());

	let (status, _) = call(&router, "GET", "/api/invoice-read", &key, None).await;
	assert_eq!(status, StatusCode::OK, "the control: the route and the key work");

	sqlx::query("UPDATE api_keys SET revoked_at = ? WHERE prefix = ?")
		.bind(Timestamp::now().0)
		.bind("deadbeef")
		.execute(store.write_pool())
		.await
		.unwrap();

	let bucket = mintworks_core::ratelimit::bucket_key(PEER.ip());
	let rounds =
		usize::try_from(app.settings.int("auth.pow_after_failures").await.unwrap()).unwrap() + 1;
	for _ in 0..rounds {
		let (status, body) = call(&router, "GET", "/api/invoice-read", &key, None).await;
		assert_eq!(status, StatusCode::UNAUTHORIZED);
		assert_eq!(body["error"]["errCode"], "E-AUTH-KEY-REVOKED");
	}
	assert_eq!(
		app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &bucket),
		0,
		"a revoked key charged the bucket the login proof-of-work gate reads"
	);

	// The control, so the assertion above cannot pass by the bucket being unreadable.
	for _ in 0..rounds {
		let (status, _) = call(&router, "GET", "/api/invoice-read", "garbage", None).await;
		assert_eq!(status, StatusCode::UNAUTHORIZED);
	}
	assert!(app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &bucket) > 0);
}

/// The blob is spent in `open()` before the assertion is checked, so a challenge cannot be
/// retried with a second assertion; the authenticator's signature counter cannot catch this,
/// because synced Apple and Google passkeys always report 0.
#[tokio::test]
async fn a_webauthn_challenge_blob_cannot_be_replayed() {
	let db = TmpDb::new("wa-replay");
	let (app, store) = setup(&db).await;
	let _ = account(&store, "wa-replay@e.st").await;
	let router = mintworks_auth::routes::public()
		.merge(mintworks_auth::routes::authenticated())
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());

	let (status, body) = call(&router, "GET", "/api/auth/wa/login/challenge", "", None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let blob = body["blob"].as_str().unwrap().to_owned();
	let assertion = serde_json::json!({
		"id": "AAAAAAAAAAAAAAAA",
		"rawId": "AAAAAAAAAAAAAAAA",
		"type": "public-key",
		"response": { "clientDataJSON": "AA", "authenticatorData": "AA", "signature": "AA" }
	});
	let attempt = || {
		call(
			&router,
			"POST",
			"/api/auth/wa/login",
			"",
			Some(serde_json::json!({ "blob": blob.clone(), "assertion": assertion.clone() })),
		)
	};

	let (status, body) = attempt().await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
	assert_eq!(body["error"]["errCode"], "E-AUTH-WEBAUTHN");
	// The same blob again: this is the assertion that was refused being retried, and by now the
	// blob is spent.
	let (status, body) = attempt().await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(body["error"]["errCode"], "E-AUTH-CHALLENGE");

	let (status, body) = call(
		&router,
		"POST",
		"/api/auth/wa/login",
		"",
		Some(serde_json::json!({ "blob": "not-a-blob", "assertion": assertion })),
	)
	.await;
	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(body["error"]["errCode"], "E-AUTH-CHALLENGE");
}

/// The blob's key is the persisted secret bound to a per-process nonce, so a blob still round
/// trips inside one process — a restart invalidating every outstanding challenge is the point,
/// and is what makes the process-local spent set sufficient.
#[tokio::test]
async fn a_webauthn_challenge_blob_round_trips_within_one_process() {
	let db = TmpDb::new("wa-round-trip");
	let (app, store) = setup(&db).await;
	let _ = account(&store, "wa-round-trip@e.st").await;
	let router = mintworks_auth::routes::public()
		.merge(mintworks_auth::routes::authenticated())
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());

	let (status, body) = call(&router, "GET", "/api/auth/wa/login/challenge", "", None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let blob = body["blob"].as_str().unwrap().to_owned();
	let assertion = serde_json::json!({
		"id": "AAAAAAAAAAAAAAAA",
		"rawId": "AAAAAAAAAAAAAAAA",
		"type": "public-key",
		"response": { "clientDataJSON": "AA", "authenticatorData": "AA", "signature": "AA" }
	});
	let post = |blob: String| {
		call(
			&router,
			"POST",
			"/api/auth/wa/login",
			"",
			Some(serde_json::json!({ "blob": blob, "assertion": assertion.clone() })),
		)
	};

	// The blob opened: the refusal is the unverifiable assertion, not the challenge.
	let (status, body) = post(blob.clone()).await;
	assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
	assert_eq!(body["error"]["errCode"], "E-AUTH-WEBAUTHN");

	// A payload tampered with does not open at all.
	let (payload, sig) = blob.split_once('.').unwrap();
	let mut chars: Vec<char> = payload.chars().collect();
	chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
	let tampered = format!("{}.{sig}", chars.into_iter().collect::<String>());
	let (status, body) = post(tampered).await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["error"]["errCode"], "E-AUTH-CHALLENGE");
}

/// The first answer to a QR session is the one the waiting browser was promised: a second tap,
/// or two phones disagreeing, must not overwrite it.
#[tokio::test]
async fn a_qr_session_can_only_be_approved_once() {
	let db = TmpDb::new("qr-once");
	let (app, store) = setup(&db).await;
	let _ = account(&store, "qr@e.st").await;
	let router = mintworks_auth::routes::public()
		.merge(mintworks_auth::routes::authenticated())
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "qr@e.st").await;

	let (status, body) = call(&router, "POST", "/api/auth/qr/init", "", None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let session = body["sessionId"].as_str().unwrap().to_owned();
	assert!(!body["secret"].as_str().unwrap().is_empty());
	let code = body["matchCode"].as_str().unwrap().to_owned();
	assert_eq!(code.chars().count(), 6);

	let respond_path = format!("/api/auth/qr/{session}/respond");
	let respond = |code: &str| {
		call(
			&router,
			"POST",
			&respond_path,
			&token,
			Some(serde_json::json!({ "approved": true, "matchCode": code })),
		)
	};
	let (status, _) = respond(&code).await;
	assert_eq!(status, StatusCode::NO_CONTENT);
	let (status, body) = respond(&code).await;
	assert_eq!(status, StatusCode::CONFLICT);
	assert_eq!(body["error"]["errCode"], "E-AUTH-QR-STATE");

	// The refused answer wrote no audit row: `qr_respond` resolves first, audits after. The
	// single row is the *first*, accepted answer.
	let approver = store.account_by_email("qr@e.st").await.unwrap().unwrap().id;
	let audits: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM audit_logs WHERE action = 'QR_LOGIN_APPROVED' AND account_id = ?",
	)
	.bind(approver)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(audits, 1, "a refused QR answer must not write an audit row");
}

/// The code is a check, not something the server hands to whoever saw the QR: `details` does
/// not carry it, and an answer without the code the *initiating* screen shows is refused.
#[tokio::test]
async fn an_approval_needs_the_code_from_the_other_screen() {
	let db = TmpDb::new("qr-code");
	let (app, store) = setup(&db).await;
	let _ = account(&store, "qr-code@e.st").await;
	let router = mintworks_auth::routes::public()
		.merge(mintworks_auth::routes::authenticated())
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "qr-code@e.st").await;

	let (status, body) = call(&router, "POST", "/api/auth/qr/init", "", None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let session = body["sessionId"].as_str().unwrap().to_owned();
	let code = body["matchCode"].as_str().unwrap().to_owned();

	// What the phone is shown: the browser and the address, and no code to copy back.
	let (status, details) =
		call(&router, "GET", &format!("/api/auth/qr/{session}/details"), &token, None).await;
	assert_eq!(status, StatusCode::OK, "{details}");
	assert!(details.get("matchCode").is_none(), "details handed the phone the code: {details}");

	let respond = format!("/api/auth/qr/{session}/respond");
	// Five characters, so it can never equal the six-character code.
	let (status, body) = call(
		&router,
		"POST",
		&respond,
		&token,
		Some(serde_json::json!({ "approved": true, "matchCode": "WRONG" })),
	)
	.await;
	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
	assert_eq!(body["error"]["errCode"], "E-AUTH-QR-CODE");
	// The refusal left the session pending: the real code still answers it.
	let (status, body) = call(
		&router,
		"POST",
		&respond,
		&token,
		Some(serde_json::json!({ "approved": true, "matchCode": code })),
	)
	.await;
	assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// A one-route bundle carrying a `require_auth` layer, so [`probe_bundle`] can give it the
/// `invoice` prefix that `AppState::route_scopes` needs.
fn probe_router() -> axum::Router<App> {
	axum::Router::<App>::new()
		.route("/api/invoice-read", axum::routing::get(|| async { "ok" }))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth))
}

/// [`probe_router`] with the `invoice` prefix registered. [`setup`] mounts nothing, so
/// `AppState::route_scopes` is empty and the mint path — which validates scopes against that
/// registry — has nothing to validate against.
fn probe_bundle() -> mintworks_core::app::Scoped {
	probe_router().scope("invoice")
}

/// [`setup`] plus [`probe_bundle`], for the tests that drive `Auth::create_api_key`.
async fn setup_scoped(db: &TmpDb) -> (App, SqliteStore) {
	let store = SqliteStore::open(&db.config()).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		.settings(mintworks_auth::SETTINGS)
		.settings(mintworks_invoice::SETTINGS)
		.extension(Arc::new(store.clone()) as Arc<dyn AuthStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_core::refs::RefStore>)
		.routes(probe_bundle())
		.build()
		.await
		.unwrap();
	app.settings.set("pow.difficulty.password-reset", "1", None).await.unwrap();
	(app, store)
}

/// The mint path's whole validation surface, and the one place a minted `sk_` key is proven to
/// be the SHA-256 hex `auth_mw::verify_api_key` compares. `route_scopes` comes from the bundle
/// the deployment mounted, so an unregistered prefix and an unknown verb must be refused rather
/// than stored — a typo would otherwise mint a key that is dead on arrival.
#[tokio::test]
async fn create_api_key_validates_every_input_and_the_cap() {
	let db = TmpDb::new("api-key-mint");
	let (app, store) = setup_scoped(&db).await;
	let owner = account(&store, "mint-owner@e.st").await;
	let personal = store
		.orgs_for_account(owner.id)
		.await
		.unwrap()
		.into_iter()
		.find(|o| o.kind == OrgKind::Personal)
		.expect("an account is created with a personal org");
	let org_id = store.org_by_uid(&personal.uid).await.unwrap().unwrap().id;
	let ctx = Ctx { org_id: Some(org_id), ..ctx_for(&owner) };
	let auth = Auth::new(app.clone());

	let reject = |scopes: &[&str], expires_at: Option<Timestamp>| {
		let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
		let ctx = ctx.clone();
		let auth = Auth::new(app.clone());
		async move { auth.create_api_key(&ctx, "CI", &scopes, expires_at).await }
	};

	// An unknown prefix, an unknown verb, no scopes at all.
	for scopes in [&["nope:read"][..], &["invoice:delete"][..], &[][..]] {
		let err = reject(scopes, None).await.unwrap_err();
		assert_eq!(err.parts().0, StatusCode::BAD_REQUEST, "{scopes:?} was accepted: {err}");
	}

	let now = Timestamp::now();
	let err = reject(&["invoice:read"], Some(Timestamp(now.0 - 1))).await.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::BAD_REQUEST, "a past expiresAt was accepted: {err}");
	let days = app.settings.int("auth.api_key_max_days").await.unwrap();
	let err = reject(&["invoice:read"], Some(Timestamp(now.0 + (days + 1) * 86_400)))
		.await
		.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::BAD_REQUEST, "beyond the ceiling was accepted: {err}");

	// A valid mint, then the cap: the count and the insert share one statement, so the second
	// mint sees the first.
	let minted = auth
		.create_api_key(&ctx, "CI", &["invoice:read".to_owned()], None)
		.await
		.unwrap();
	assert!(minted.key.starts_with("sk_"), "{minted:?}");
	app.settings.set("auth.api_keys_max", "1", None).await.unwrap();
	let err = auth
		.create_api_key(&ctx, "CI", &["invoice:read".to_owned()], None)
		.await
		.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::CONFLICT, "the cap was not enforced: {err}");

	// The plaintext key authenticates a scoped route, which is what proves the stored
	// `hex(sha256(key))` is what `auth_mw` compares. Converting drops the prefixes — the `app`
	// above still carries them — so this logs the intended notice.
	let router: axum::Router<App> = probe_bundle().into();
	let router = router.layer(axum::Extension(app.clone())).with_state(app.clone());
	let (status, body) = call(&router, "GET", "/api/invoice-read", &minted.key, None).await;
	assert_eq!(status, StatusCode::OK, "the minted key did not authenticate: {body}");
	// The verb follows the method: a `read` key cannot write.
	let (status, body) = call(&router, "POST", "/api/invoice-read", &minted.key, None).await;
	assert_eq!(status, StatusCode::FORBIDDEN);
	assert_eq!(body["error"]["errCode"], "E-AUTH-SCOPE");
}

/// Revoking a key is the emergency, so it must not be blocked behind a ToS the user has not
/// accepted — while minting a new one is an ordinary gated action and stays behind the gate.
#[tokio::test]
async fn revoking_a_key_is_consent_exempt_but_minting_is_not() {
	let db = TmpDb::new("key-revoke-consent");
	let (app, store) = setup_scoped(&db).await;
	let account = account(&store, "key-consent@e.st").await;
	gate_satisfied(&store, &account).await;

	let personal = store
		.orgs_for_account(account.id)
		.await
		.unwrap()
		.into_iter()
		.find(|o| o.kind == OrgKind::Personal)
		.expect("an account is created with a personal org");
	let org_id = store.org_by_uid(&personal.uid).await.unwrap().unwrap().id;
	let ctx = Ctx { org_id: Some(org_id), ..ctx_for(&account) };
	let minted = Auth::new(app.clone())
		.create_api_key(&ctx, "CI", &["invoice:read".to_owned()], None)
		.await
		.unwrap();

	// A ToS published after the last acceptance is outstanding.
	publish_legal(&store, LegalKind::Tos, "2").await;
	let router = mintworks_auth::routes::authenticated()
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "key-consent@e.st").await;

	let (status, body) = call(&router, "GET", "/api/auth/me", &token, None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert!(
		!body["consentsRequired"].as_array().unwrap().is_empty(),
		"the new ToS is not reported outstanding: {body}"
	);

	let (status, body) =
		call(&router, "DELETE", &format!("/api/api-keys/{}", minted.uid), &token, None).await;
	assert_eq!(status, StatusCode::NO_CONTENT, "revocation was gated: {body}");

	let (status, body) = call(
		&router,
		"POST",
		"/api/api-keys",
		&token,
		Some(serde_json::json!({ "name": "CI", "scopes": ["invoice:read"] })),
	)
	.await;
	assert_eq!(status, StatusCode::FORBIDDEN);
	assert_eq!(body["error"]["errCode"], "E-AUTH-CONSENT-REQUIRED");
}

/// The challenge state is signed as one variant but the two `finish_*` functions are separate
/// routes, and the variant check is what stops a login challenge being spent to enrol a passkey.
#[tokio::test]
async fn a_login_challenge_cannot_be_spent_on_a_registration() {
	let db = TmpDb::new("wa-purpose-binding");
	let (app, store) = setup(&db).await;
	let account = account(&store, "wa-purpose@e.st").await;
	gate_satisfied(&store, &account).await;
	let router = mintworks_auth::routes::public()
		.merge(mintworks_auth::routes::authenticated())
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());
	let token = access_token(&app, "wa-purpose@e.st").await;

	let (status, body) = call(&router, "GET", "/api/auth/wa/login/challenge", "", None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let blob = body["blob"].as_str().unwrap().to_owned();
	let registration = serde_json::json!({
		"id": "AAAAAAAAAAAAAAAA",
		"rawId": "AAAAAAAAAAAAAAAA",
		"type": "public-key",
		"response": { "clientDataJSON": "AA", "attestationObject": "AA", "transports": [] }
	});
	let (status, body) = call(
		&router,
		"POST",
		"/api/auth/wa/register",
		&token,
		Some(serde_json::json!({ "blob": blob, "registration": registration })),
	)
	.await;
	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["error"]["errCode"], "E-AUTH-CHALLENGE");
}

/// The listing is the org's **live** keys, so an expired one leaves it the same way a revoked
/// one does — while staying in the table and the export as the trail.
#[tokio::test]
async fn an_expired_key_leaves_the_org_listing() {
	let db = TmpDb::new("api-key-listing-expiry");
	let (app, store) = setup_scoped(&db).await;
	let owner = account(&store, "list-owner@e.st").await;
	let personal = store
		.orgs_for_account(owner.id)
		.await
		.unwrap()
		.into_iter()
		.find(|o| o.kind == OrgKind::Personal)
		.expect("an account is created with a personal org");
	let org_id = store.org_by_uid(&personal.uid).await.unwrap().unwrap().id;
	let ctx = Ctx { org_id: Some(org_id), ..ctx_for(&owner) };

	let _ = insert_api_key(&store, org_id, owner.id, "11111111", r#"["invoice:read"]"#).await;
	let _ = insert_api_key(&store, org_id, owner.id, "22222222", r#"["invoice:read"]"#).await;
	sqlx::query("UPDATE api_keys SET expires_at = ? WHERE prefix = ?")
		.bind(Timestamp::now().0 - 1)
		.bind("22222222")
		.execute(store.write_pool())
		.await
		.unwrap();

	let listed = Auth::new(app.clone()).list_api_keys(&ctx).await.unwrap();
	assert_eq!(listed.len(), 1, "an expired key stayed in the listing: {listed:?}");
	assert_eq!(listed[0].prefix, "11111111");
}

/// The cap's early refusal, before an authenticator is prompted. The insert re-checks it — the
/// two ends of a registration are separate requests — so this covers the settings read and the
/// `409` the service answers with; the insert itself is the adapter's `put_webauthn_credential`
/// test.
#[tokio::test]
async fn begin_passkey_registration_refuses_at_the_cap() {
	let db = TmpDb::new("webauthn-cap-service");
	let (app, store) = setup(&db).await;
	let account = account(&store, "wa-cap@e.st").await;
	store
		.put_webauthn_credential(
			&NewWebauthnCredential {
				account_id: account.id,
				credential_id: "cred-one".to_owned(),
				credential: "{}".to_owned(),
				name: "This device".to_owned(),
				created_at: Timestamp(10),
			},
			i64::MAX,
		)
		.await
		.unwrap();
	app.settings.set("auth.webauthn_max", "1", None).await.unwrap();

	let err = Auth::new(app.clone())
		.begin_passkey_registration(&ctx_for(&account))
		.await
		.unwrap_err();
	assert_eq!(err.parts().0, StatusCode::CONFLICT, "{err}");
}

/// A QR status poll with the secret in `x-qr-secret`, the header the SPA sends. Returns the
/// headers too: an approval has to sign the desktop in, and [`parts`] drops them.
async fn poll_qr(
	router: &axum::Router,
	session_id: &str,
	secret: &str,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
	let req = axum::http::Request::builder()
		.method("GET")
		.uri(format!("/api/auth/qr/{session_id}/status?wait=1"))
		.header("x-qr-secret", secret)
		.body(axum::body::Body::empty())
		.unwrap();
	let resp = tower::ServiceExt::oneshot(router.clone(), req).await.unwrap();
	let status = resp.status();
	let headers = resp.headers().clone();
	let bytes = resp.into_body().collect().await.unwrap().to_bytes();
	(status, headers, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// The secret `init` returns is the one `qr::status` hashes: it arrives base64url while `init`
/// hashed the raw 32 bytes. An expired session is also a plain `404`, not a `200 {"status":…}`.
#[tokio::test]
async fn a_qr_poll_accepts_the_secret_init_returned() {
	let db = TmpDb::new("qr-secret");
	let (app, store) = setup(&db).await;
	let _ = account(&store, "qr-poll@e.st").await;
	let router = mintworks_auth::routes::public()
		.merge(mintworks_auth::routes::authenticated())
		.layer(axum::Extension(app.clone()))
		.with_state(app.clone());

	let (status, body) = call(&router, "POST", "/api/auth/qr/init", "", None).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let session = body["sessionId"].as_str().unwrap().to_owned();
	let secret = body["secret"].as_str().unwrap().to_owned();
	let code = body["matchCode"].as_str().unwrap().to_owned();

	// The regression: before the decode, this hashed the base64 text and answered 404.
	let (status, _, body) = poll_qr(&router, &session, &secret).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["status"], "pending");
	// The phone is handed the context, never the code it is meant to type.
	let token = access_token(&app, "qr-poll@e.st").await;
	let (status, details) =
		call(&router, "GET", &format!("/api/auth/qr/{session}/details"), &token, None).await;
	assert_eq!(status, StatusCode::OK, "{details}");
	assert!(details.get("matchCode").is_none(), "details carried the code: {details}");

	// A wrong secret and an unknown session answer alike — the session id is printed in the QR.
	let (status, _, _) =
		poll_qr(&router, &session, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").await;
	assert_eq!(status, StatusCode::NOT_FOUND);
	// A secret that is not even base64url is the same answer, not a 500.
	let (status, _, _) = poll_qr(&router, &session, "!!!").await;
	assert_eq!(status, StatusCode::NOT_FOUND);
	let (status, _, _) = poll_qr(&router, "00000000000000000000000000000000", &secret).await;
	assert_eq!(status, StatusCode::NOT_FOUND);

	// Approve from the phone, then the desktop's own next poll collects the session.
	let respond = format!("/api/auth/qr/{session}/respond");
	let (status, body) = call(
		&router,
		"POST",
		&respond,
		&token,
		Some(serde_json::json!({ "approved": true, "matchCode": code })),
	)
	.await;
	assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

	let (status, headers, body) = poll_qr(&router, &session, &secret).await;
	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["account"]["email"], "qr-poll@e.st");
	assert!(body["accessToken"].is_string(), "{body}");
	let cookies = headers
		.get_all(axum::http::header::SET_COOKIE)
		.iter()
		.filter_map(|v| v.to_str().ok())
		.collect::<Vec<_>>();
	assert_eq!(cookies.len(), 2, "an approval signs the desktop in with both cookies: {cookies:?}");
}

/// The `Auth` surface is only usable from Rust if a consumer can name its types. Bodyless by
/// design: failing to compile is the assertion, in the same `-D warnings` run as clippy.
#[test]
fn the_service_api_types_can_be_named_by_a_consumer() {
	use std::mem::size_of;
	let _ = (
		size_of::<mintworks_auth::ApiKeyView>(),
		size_of::<mintworks_auth::MintedKey>(),
		size_of::<mintworks_auth::InitResponse>(),
		size_of::<mintworks_auth::QrDetails>(),
		size_of::<mintworks_auth::QrStatus>(),
		size_of::<mintworks_auth::PasskeyLogin>(),
		size_of::<mintworks_auth::PasskeyView>(),
		size_of::<mintworks_auth::PasskeyRegistration>(),
		size_of::<mintworks_auth::StepUpProof>(),
	);
}

// vim: ts=4
