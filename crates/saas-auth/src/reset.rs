//! Password reset and password change.
//!
//! There is no reset-token table. The token is HMAC-signed over `accounts.uid`, the
//! current `pwd_hash` and `token_epoch`, so a completed reset — which changes both —
//! invalidates it, and so does any other password change in the meantime. That is what
//! makes it single-use without storing anything.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use saas_core::app::App;
use saas_core::auth_mw::ClientIp;
use saas_core::ctx::Ctx;
use saas_core::prelude::*;
use serde::Deserialize;

use crate::service_api::{Auth, LoginOutcome};
use crate::store::{Account, AuthStore};
use crate::{activate, login, pow, token};

/// How long a reset link stays valid. Short, because the mail is the whole credential.
//
// A constant, matching `pow::TTL_SECONDS` and `activate::TTL_SECONDS`.
// `auth.reset_ttl_hours` in this crate's `SETTINGS` is the upgrade path.
pub const TTL_SECONDS: i64 = 2 * 3600;

/// Distinct from `activate`'s prefix, over the same key, so neither token can be replayed
/// as the other.
const PURPOSE: &str = "reset";

#[derive(Debug, Deserialize)]
pub struct RequestBody {
	pub email: String,
	pub pow: pow::Proof,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetBody {
	pub token: String,
	pub password: String,
	/// Mandatory on an account with a confirmed second factor — the mailed token is one
	/// factor, and nothing is written until both are in hand.
	#[serde(default)]
	pub code: Option<String>,
	/// A printed recovery code, accepted in place of `code`. Without it a user who has lost
	/// the authenticator *and* forgotten the password has no path back at all: this route
	/// refuses the mailed token without a second factor, and they cannot log in to reach
	/// `POST /api/auth/password`.
	#[serde(default)]
	pub recovery_code: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeBody {
	pub current_password: String,
	pub new_password: String,
	/// Mandatory on an account with a confirmed second factor: this route mints
	/// `auth_at = now`, so it has to demand everything step-up does — and, for the same
	/// reason, it takes no `recoveryCode`. A printed code recovers a locked-out account
	/// (`ResetBody::recovery_code`); it does not authorize a destructive route from inside a
	/// live session, which is what `auth_at = now` does.
	#[serde(default)]
	pub code: Option<String>,
}

// ---------------------------------------------------------------- token

/// The signed string. `pwd_hash` and `token_epoch` are covered but not carried, so the
/// signature is recomputed from the account's *current* state at redemption.
fn signed(uid: &str, pwd_hash: Option<&str>, epoch: i64, exp: i64) -> String {
	format!("{PURPOSE}:{uid}:{}:{epoch}:{exp}", pwd_hash.unwrap_or(""))
}

pub(crate) async fn mint(app: &App, account: &Account) -> ClResult<String> {
	let exp = activate::bucketed_exp(Timestamp::now(), TTL_SECONDS);
	let key = pow::hmac_key(app, activate::KEY_NAME).await?;
	let sig = pow::hmac_hex(
		&key,
		&signed(account.uid.as_str(), account.pwd_hash.as_deref(), account.token_epoch, exp),
	)?;
	let public = format!("{PURPOSE}:{}:{exp}", account.uid.as_str());
	Ok(format!("{}.{sig}", B64.encode(public.as_bytes())))
}

pub(crate) async fn open(app: &App, store: &dyn AuthStore, presented: &str) -> ClResult<Account> {
	activate::open_signed(app, store, presented, PURPOSE, |account, exp| {
		signed(account.uid.as_str(), account.pwd_hash.as_deref(), account.token_epoch, exp)
	})
	.await
}

// ---------------------------------------------------------------- the mail

/// Queues the reset mail. The caller has already decided the account is usable; this only
/// sends.
///
/// The token is minted by the job when it renders, not here — see [`crate::job`]. A `FAILED`
/// row keeps its payload as the diagnostic, and a live reset link is not something to keep.
pub(crate) async fn send_link(app: &App, account: &Account) -> ClResult<()> {
	crate::job::enqueue(&app.store, account, crate::job::LinkKind::PasswordReset).await
}

// ---------------------------------------------------------------- routes

/// `POST /api/auth/password/reset-request`. Always `204`, whatever the address, so it
/// cannot be used to find out which addresses are registered.
pub async fn request(
	State(app): State<App>,
	ClientIp(ip): ClientIp,
	Json(req): Json<RequestBody>,
) -> ClResult<StatusCode> {
	let ctx = Ctx::public("auth.password.reset-request").with_ip(ip);
	Auth::new(app).request_password_reset(&ctx, &req.email, Some(&req.pow)).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/auth/password/reset`. Bumps `token_epoch`, signing out every live token,
/// then logs the caller straight in.
pub async fn reset(
	State(app): State<App>,
	ClientIp(ip): ClientIp,
	Json(req): Json<ResetBody>,
) -> ClResult<Response> {
	let ctx = Ctx::public("auth.password.reset").with_ip(ip);
	match Auth::new(app)
		.reset_password(
			&ctx,
			&req.token,
			req.code.as_deref(),
			req.recovery_code.as_deref(),
			req.password,
		)
		.await?
	{
		LoginOutcome::Signed(tokens) => token::respond(*tokens),
		LoginOutcome::TotpRequired { totp_token } => Ok(login::totp_response(totp_token)),
	}
}

/// `POST /api/auth/password`. The current password is required, so this route needs no
/// step-up — presenting it *is* the step-up.
pub async fn change(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<ChangeBody>,
) -> ClResult<Response> {
	let tokens = Auth::new(app)
		.change_password(&ctx, req.current_password, req.code.as_deref(), req.new_password)
		.await?;
	token::respond(tokens)
}

// vim: ts=4
