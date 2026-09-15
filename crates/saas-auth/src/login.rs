//! Login, the second-factor hand-off, refresh and client-side logout.
//!
//! Every failure path answers `E-AUTH-CREDENTIALS` with the same message whether the address
//! exists or not, runs one argon2id pass either way, **and takes the same writer round-trip
//! either way**: [`record_failure`] is called with [`NO_ACCOUNT`] when the address is
//! unknown, so both branches pay for one `UPDATE accounts` on the single write connection.
//!
//! That last part is not decoration. The argon2id pass alone did not hide the branch: the
//! wrong-password path also went to the writer — one connection, 5 s busy timeout — and that
//! round-trip is both larger and far more variable than the hash it was meant to hide behind,
//! so under concurrent write load `POST /api/auth/login` was a reliable account-existence
//! oracle. Anything added to one branch has to be added to the other.

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use saas_core::app::App;
use saas_core::auth_mw::{Claims, ClientIp, JWT_SECRET_KEY};
use saas_core::ctx::Ctx;
use saas_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::activate::bad_token;
use crate::service_api::{Auth, Credentials, LoginOutcome};
use crate::store::{Account, AccountStatus, AuthStore};
use crate::{activate, pow, token};

/// How long the second-factor ticket handed out by `login` stays usable.
//
// A constant, like the PoW and activation TTLs. It is a hand-off window, not a
// per-deployment policy; `auth.totp_ticket_ttl` in the REGISTRY is the upgrade path.
pub const TICKET_TTL_SECONDS: i64 = 300;

/// Bound into the ticket signature so it can never be replayed as an activation or reset
/// token, all three sharing `activate::KEY_NAME`.
pub(crate) const TICKET_PURPOSE: &str = "totp";

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
	pub email: String,
	pub password: String,
	/// Mandatory once the caller's address has spent `auth.pow_after_failures` tokens from the
	/// `auth.failed` bucket — never per-account state; see `Auth::login`.
	#[serde(default)]
	pub pow: Option<pow::Proof>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotpRequest {
	pub totp_token: String,
	#[serde(default)]
	pub code: Option<String>,
	#[serde(default)]
	pub recovery_code: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshRequest {
	#[serde(default)]
	pub refresh_token: Option<String>,
}

/// The `401` body that hands a ticket to the second-factor step.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TotpChallenge {
	error: serde_json::Value,
	/// Absent on the password-reset path, where there is no ticket to spend: the caller
	/// re-submits the mailed reset token together with `code` or `recoveryCode`.
	#[serde(skip_serializing_if = "Option::is_none")]
	totp_token: Option<String>,
}

pub(crate) fn bad_credentials() -> Error {
	Error::coded(StatusCode::UNAUTHORIZED, "E-AUTH-CREDENTIALS", "invalid email or password")
}

// ---------------------------------------------------------------- login

/// `POST /api/auth/login`.
pub async fn login(
	State(app): State<App>,
	ClientIp(ip): ClientIp,
	Json(req): Json<LoginRequest>,
) -> ClResult<Response> {
	let ctx = Ctx::public("auth.login").with_ip(ip);
	let credentials = Credentials { email: req.email, password: req.password, pow: req.pow };
	match Auth::new(app).login(&ctx, &credentials).await? {
		LoginOutcome::Signed(tokens) => token::respond(*tokens),
		LoginOutcome::TotpRequired { totp_token } => Ok(totp_response(totp_token)),
	}
}

/// The second-factor ticket, with no HTTP around it: `Some` when the account has a
/// *confirmed* credential.
///
/// One gate, because the password is not the only single factor that reaches a token pair —
/// a mailed reset token is another, and skipping this there turns a mailbox compromise into
/// a 2FA bypass (and, via `auth_at = now`, into step-up for `DELETE /api/auth/totp`).
/// [`crate::service_api::Auth::login`] and `Auth::reset_password` both return it as
/// [`LoginOutcome::TotpRequired`].
pub(crate) async fn totp_ticket(
	app: &App,
	store: &dyn AuthStore,
	account: &Account,
) -> ClResult<Option<String>> {
	if !has_confirmed_totp(store, account).await? {
		return Ok(None);
	}
	Ok(Some(mint_ticket(app, account, TICKET_PURPOSE).await?))
}

/// One definition of "this account has a second factor", shared by [`totp_ticket`] and
/// [`crate::service_api::Auth::step_up`]. An unconfirmed credential is a half-finished
/// enrolment and does not count — it would otherwise lock the account out of both routes.
///
/// The two callers have to agree: step-up used to accept a password on its own, so an
/// attacker with a stolen access token plus the password got `auth_at = now` where `login`
/// would only have handed out a `TotpRequired` ticket for the same credential.
pub(crate) async fn has_confirmed_totp(store: &dyn AuthStore, account: &Account) -> ClResult<bool> {
	Ok(store
		.totp_by_account(account.id)
		.await?
		.is_some_and(|c| c.confirmed_at.is_some()))
}

/// Every factor `login` would demand for `account`, checked in one place so [`super::service_api::Auth::step_up`]
/// and `Auth::change_password` cannot drift apart again. `Err` is always [`bad_credentials`] —
/// never a distinct code, or the caller becomes an oracle for whether an account has a second
/// factor.
///
/// **A recovery code is deliberately not accepted here.** Both callers mint `auth_at = now`,
/// which is what passes `require_stepup` on `DELETE /api/auth/totp` and
/// `POST /api/account/delete` — a printed code's job is to recover an account, not to authorize
/// a destructive route from inside a live session. `Auth::login_totp` and `reset_password` are
/// where one belongs, because there the caller is locked out.
pub(crate) async fn verify_all_factors(
	app: &App,
	store: &dyn AuthStore,
	account: &Account,
	password: String,
	code: Option<&str>,
) -> ClResult<()> {
	let mut ok = verify_password(account.pwd_hash.clone(), password).await?;
	if ok && has_confirmed_totp(store, account).await? {
		// Only a *wrong* factor collapses into `bad_credentials`: `.is_ok()` also swallowed the
		// `Error::internal` from a secret that will not decrypt, so a `MASTER_KEY` rotation
		// silently locked every TOTP account out of step-up and password change.
		let checked = match code {
			Some(code) => Some(crate::totp::check_code(app, store, account, code).await),
			None => None,
		};
		ok = match checked {
			Some(Ok(())) => true,
			Some(Err(e)) if e.parts().1 == "E-AUTH-TOTP-INVALID" => false,
			Some(Err(e)) => return Err(e),
			None => false,
		};
	}
	if ok { Ok(()) } else { Err(bad_credentials()) }
}

/// `api-surface.md` §4.2 renders the ticket as a `401` carrying `totpToken`.
///
/// The status is the contract and stays, but the marker says what it means: the first factor
/// *succeeded*, so `auth_mw::run_public` must not charge the auth-failed bucket for it. Both
/// producers — `routes::login` and `reset::reset` — come through here, so this is the only
/// place it has to be inserted.
pub(crate) fn totp_response(totp_token: Option<String>) -> Response {
	let body = TotpChallenge {
		error: serde_json::json!({
			"errCode": "E-AUTH-TOTP-REQUIRED",
			"errStr": "second factor required",
		}),
		totp_token,
	};
	let mut resp = (StatusCode::UNAUTHORIZED, Json(body)).into_response();
	resp.extensions_mut().insert(saas_core::auth_mw::NotAnAuthFailure);
	resp
}

/// The id [`record_failure`] is given when there is no account.
///
/// `accounts.id` is `INTEGER PRIMARY KEY`, whose rowids start at 1, so this matches nothing
/// and the `UPDATE` is a no-op that still costs the same writer round-trip.
pub(crate) const NO_ACCOUNT: i64 = 0;

/// Counts one failed attempt. Called for a wrong password and for a wrong second factor
/// alike — one counter, so it cannot be dodged by failing at the step that does not count.
///
/// Takes an id rather than an `&Account` so [`crate::service_api::Auth::login`] can call it
/// with [`NO_ACCOUNT`] on the unknown-address branch: both branches then pay the same writer
/// round-trip, which is the timing oracle the module doc describes.
pub(crate) async fn record_failure(store: &dyn AuthStore, account_id: i64) -> ClResult<()> {
	store.record_login_failure(account_id).await
}

/// argon2id verification on a blocking thread. A missing hash — an invited account that
/// never set a password — is a plain `false`, not an error.
pub(crate) async fn verify_password(hash: Option<String>, password: String) -> ClResult<bool> {
	let Some(hash) = hash else {
		// Spend a pass anyway. Returning here answered in ~1 ms where an unknown address
		// takes a full argon2id pass (`service_api::login`), so an invited-but-unactivated
		// address was distinguishable by timing — the oracle that branch exists to close.
		crate::register::hash_password(password).await?;
		return Ok(false);
	};
	crate::register::hash_blocking(move || {
		let parsed = PasswordHash::new(&hash)
			.map_err(|e| Error::internal(format!("stored password hash is unreadable: {e}")))?;
		Ok(Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
	})
	.await
}

/// The status gate shared by every route that resolves an account outside the middleware.
pub(crate) fn ensure_usable(account: &Account) -> ClResult<()> {
	match account.status {
		AccountStatus::Active => Ok(()),
		AccountStatus::Pending => {
			Err(Error::coded(StatusCode::FORBIDDEN, "E-AUTH-PENDING", "activation not completed"))
		}
		AccountStatus::Suspended => {
			Err(Error::coded(StatusCode::FORBIDDEN, "E-AUTH-SUSPENDED", "account suspended"))
		}
		AccountStatus::Anonymized => {
			Err(Error::coded(StatusCode::FORBIDDEN, "E-AUTH-ANONYMIZED", "account anonymized"))
		}
	}
}

// ---------------------------------------------------------------- second factor

/// The stateless hand-off ticket. Signed over `token_epoch` as well as the uid, so a
/// password change between the two calls kills it.
pub(crate) async fn mint_ticket(app: &App, account: &Account, purpose: &str) -> ClResult<String> {
	let exp = Timestamp::now().0 + TICKET_TTL_SECONDS;
	let key = pow::hmac_key(app, activate::KEY_NAME).await?;
	let payload = ticket_payload(purpose, account.uid.as_str(), account.token_epoch, exp);
	let sig = pow::hmac_hex(&key, &payload)?;
	Ok(format!("{}:{exp}.{sig}", account.uid.as_str()))
}

fn ticket_payload(purpose: &str, uid: &str, epoch: i64, exp: i64) -> String {
	format!("{purpose}:{uid}:{epoch}:{exp}")
}

/// The ticket is the one signed token here that is not base64'd — it never travels in a
/// mail — so it splits its own `uid:exp.sig` and shares only the verification tail.
pub(crate) async fn open_ticket(
	app: &App,
	store: &dyn AuthStore,
	ticket: &str,
	purpose: &str,
) -> ClResult<Account> {
	let (claimed, sig) = ticket.split_once('.').ok_or_else(bad_token)?;
	let (uid, exp) = claimed.split_once(':').ok_or_else(bad_token)?;
	let account = activate::verify(app, store, uid, exp, sig, |account, exp| {
		ticket_payload(purpose, account.uid.as_str(), account.token_epoch, exp)
	})
	.await?;
	ensure_usable(&account)?;
	Ok(account)
}

/// `POST /api/auth/login/totp` — the ticket plus either a code or a recovery code.
pub async fn login_totp(
	State(app): State<App>,
	ClientIp(ip): ClientIp,
	Json(req): Json<TotpRequest>,
) -> ClResult<Response> {
	let ctx = Ctx::public("auth.login.totp").with_ip(ip);
	let tokens = Auth::new(app)
		.login_totp(&ctx, &req.totp_token, req.code.as_deref(), req.recovery_code.as_deref())
		.await?;
	token::respond(tokens)
}

// ---------------------------------------------------------------- refresh, logout

/// `POST /api/auth/refresh`. Sliding: a new pair every time, `auth_at` carried over
/// unchanged so refreshing can never manufacture step-up.
pub async fn refresh(
	State(app): State<App>,
	ClientIp(ip): ClientIp,
	headers: HeaderMap,
	body: Option<Json<RefreshRequest>>,
) -> ClResult<Response> {
	let presented = body
		.and_then(|Json(r)| r.refresh_token)
		.or_else(|| saas_core::auth_mw::cookie_value(&headers, token::REFRESH_COOKIE))
		.ok_or_else(bad_token)?;
	let ctx = Ctx::public("auth.refresh").with_ip(ip);
	token::respond(Auth::new(app).refresh(&ctx, &presented).await?)
}

/// Verifies a refresh token's signature and shape. The account behind it is loaded by
/// [`account_from_claims`], which re-checks the epoch and the status.
pub(crate) async fn open_refresh(app: &App, presented: &str) -> ClResult<Claims> {
	let key = pow::hmac_key(app, JWT_SECRET_KEY).await?;
	let claims = jsonwebtoken::decode::<Claims>(
		presented,
		&jsonwebtoken::DecodingKey::from_secret(&key),
		&jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256),
	)
	.map_err(|_| bad_token())?
	.claims;
	if claims.typ.as_deref() != Some("refresh") {
		return Err(bad_token());
	}
	// The absolute lifetime: `refresh` carries `auth_at` over unchanged, so a captured token
	// renewed itself forever, and with no session table allowed (`architecture.md`) the start
	// claim is the only cap. `auth_at` is `None` only on an impersonation token.
	let max = app.settings.int("auth.session_max_seconds").await?;
	if claims.auth_at.is_some_and(|started| Timestamp::now().0 - started > max) {
		return Err(bad_token());
	}
	Ok(claims)
}

/// Re-reads the account a verified claim set names, and re-checks the two things that can
/// have changed since it was signed: the epoch and the status.
pub(crate) async fn account_from_claims(
	store: &dyn AuthStore,
	claims: &Claims,
) -> ClResult<Account> {
	let uid = AccountId::parse(&claims.sub).map_err(|_| bad_token())?;
	let account = store.account_by_uid(&uid).await?.ok_or_else(bad_token)?;
	if account.token_epoch != claims.ep {
		return Err(bad_token());
	}
	ensure_usable(&account)?;
	Ok(account)
}

/// `POST /api/auth/logout` — clears the cookies and nothing else.
///
/// The presented tokens stay cryptographically valid until they expire. That is the
/// accepted consequence of a stateless design; the levers that really revoke are
/// `accounts.token_epoch` and rotating `auth.jwt_key`.
///
/// **No extractor, and it lives in [`crate::routes::public`].** Requiring auth would make
/// clearing your own cookies a privileged act, so a suspended or pending account would be
/// refused by the one route that removes the cookie causing the refusal. Nothing here reads an
/// actor and nothing here can be abused without one: the response is two `Max-Age=0` headers.
pub async fn logout() -> ClResult<Response> {
	let mut resp = StatusCode::NO_CONTENT.into_response();
	token::clear_cookies(&mut resp)?;
	Ok(resp)
}

// vim: ts=4
