// SPDX-License-Identifier: MPL-2.0
//! Activation: `POST /api/auth/activate` and `POST /api/auth/resend-activation`.
//!
//! There is no activation-token table, by design. The token is an HMAC over the account uid, the
//! account's *current* status and an expiry, keyed on `secrets['auth.token_key']`. Because the
//! signature covers the very status that activation changes, the same token stops verifying the
//! moment it is used: single-use without a row, and revoked for free by any later status change.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use mintworks_core::app::App;
use mintworks_core::auth_mw::ClientIp;
use mintworks_core::ids::RefId;
use mintworks_core::prelude::*;
use mintworks_core::refs::{Ref, RefUse, Refs};
use mintworks_email::SendEmail;
use serde::Deserialize;
use serde_json::{Value, json};

use mintworks_core::ctx::Ctx;

use crate::service_api::Auth;
use crate::store::{Account, AccountStatus, AuthStore, OrgKind, Role};
use crate::{pow, register, routes, token};

/// How long an activation link stays valid.
//
// A constant, not a setting — nothing has wanted a per-deployment figure yet.
// `auth.activation_ttl_hours` in this crate's `SETTINGS` is the upgrade path if one does.
pub const TTL_SECONDS: i64 = 24 * 3600;

/// The HMAC key behind every stateless token this crate mints — activation here, password
/// reset in the next phase. Minted on first use.
pub const KEY_NAME: &str = "auth.token_key";

/// Bound into the signature so an activation token can never be replayed as a reset token.
const PURPOSE: &str = "activate";

#[derive(Debug, Deserialize)]
pub struct ActivateRequest {
	pub token: String,
	/// Required for an invited account, which `org::add_member` creates with no
	/// password; rejected for a self-registered one, which already has one.
	#[serde(default)]
	pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResendRequest {
	pub email: String,
	pub pow: pow::Proof,
}

/// `POST /api/auth/activate`. Flips `PENDING` → `ACTIVE`, sets `activated_at`, sends the
/// welcome mail and returns the login body so the user lands signed in.
///
/// **This is where the password is set**: `POST /api/auth/register` takes none. It is
/// required, and this is the only route that will take it — without it the account would be
/// `ACTIVE` with no password and `login::verify_password` would answer `false` forever.
///
/// The ref the account registered with is re-judged here under `auth.registration`: register
/// already refused an unusable code after the PoW, so no PENDING account is left behind.
pub async fn activate(
	State(app): State<App>,
	ClientIp(ip): ClientIp,
	Json(req): Json<ActivateRequest>,
) -> ClResult<Response> {
	let ctx = Ctx::public("auth.activate").with_ip(ip);
	token::respond(Auth::new(app).activate(&ctx, &req.token, req.password).await?)
}

/// Spend an activation token and return the account it belonged to, now `ACTIVE`.
///
/// The body of [`activate`], reached through [`crate::service_api::Auth::activate`] so a
/// consumer can complete an invitation from its own code.
pub(crate) async fn redeem(
	app: &App,
	raw_token: &str,
	password: Option<String>,
) -> ClResult<Activated> {
	let app = app.clone();
	let req = ActivateRequest { token: raw_token.to_owned(), password };
	let store = routes::store(&app)?;
	let account = open(&app, store.as_ref(), &req.token).await?;

	// A password is **always** required here: registration does not take one, and neither
	// does an invitation, so every `PENDING` account reaches this with `pwd_hash = NULL`.
	let pwd_hash = match (&account.pwd_hash, req.password) {
		(None, Some(password)) => {
			register::validate_password(&password)?;
			Some(register::hash_password(password).await?)
		}
		(None, None) => {
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-AUTH-PASSWORD-REQUIRED",
				"a password is required to activate",
			));
		}
		// Defensive only: `open` restricts to `PENDING`, and nothing gives a `PENDING`
		// account a hash. Activation is not a password-change route either way.
		(Some(_), _) => {
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-AUTH-PASSWORD-SET",
				"this account already has a password",
			));
		}
	};

	// Before the flip, so a refusal leaves the activation link live.
	let (personal_id, personal) = personal_org(store.as_ref(), account.id).await?;
	let admitted = admit(&app, store.as_ref(), &account, personal_id).await?;
	// A retry re-admits on the account's own use of the ref, so a failed join stays retryable.
	let mut joined = None;
	if let Some((r, _)) = admitted.as_ref().filter(|(r, _)| r.ref_type == "org_invite") {
		joined = join(store.as_ref(), r, &account).await?;
	}

	// `false` means the account was not `PENDING` any more: the token verified against a
	// status that has since moved on, so it is spent.
	if !store
		.activate_account(account.id, pwd_hash.as_deref(), Timestamp::now())
		.await?
	{
		return Err(bad_token());
	}
	// Same gap as `reset::reset`: `activate_account` sets `status` and `pwd_hash` only, so a
	// lockout served before activation would refuse the password just set, for the rest of
	// the hour.
	store.record_login_success(account.id, Timestamp::now()).await?;
	let welcome = SendEmail {
		to: account.email.clone(),
		template: "welcome".to_owned(),
		lang: account.locale.clone(),
		vars: json!({
			"name": register::display_name(&account),
			"login_link": format!("{}/login", app.config.base_url.trim_end_matches('/')),
		}),
	};
	// Non-fatal, as in `Auth::register`: the account is already `ACTIVE` and the link's signature
	// covers `status`, so a 500 here reports failure while the second click answers "invalid or
	// expired token".
	if let Err(e) = mintworks_email::job::enqueue(&app.store, &welcome).await {
		tracing::error!(
			error = %e,
			account = account.uid.as_str(),
			"could not queue the welcome mail; the account is activated regardless"
		);
	}
	Ok(Activated { account, personal, ref_uid: admitted.map(|(r, _)| r.uid), joined })
}

/// What [`redeem`] activated, for the events [`Auth::activate`] emits.
pub(crate) struct Activated {
	pub account: Account,
	pub personal: OrgId,
	/// The ref the account was admitted with; its use is `(ref, account)`.
	pub ref_uid: Option<RefId>,
	/// The org an `org_invite` ref made the account a member of.
	pub joined: Option<OrgId>,
}

pub(crate) async fn personal_org(store: &dyn AuthStore, account_id: i64) -> ClResult<(i64, OrgId)> {
	let personal = store
		.orgs_for_account(account_id)
		.await?
		.into_iter()
		.find(|o| o.kind == OrgKind::Personal)
		.ok_or_else(|| Error::internal("a pending account has no personal org"))?;
	let org = store.org_by_uid(&personal.uid).await?.ok_or(Error::NotFound)?;
	Ok((org.id, personal.uid))
}

/// Judges the account's pending ref under `auth.registration` and redeems it into
/// `personal_id`. `open` admits without a ref and drops a spent one or a non-admitting type
/// (a coupon is never spent at activation) but admits an app-defined type with
/// `params.admits`; `invite` needs a `signup`, an `org_invite` or a
/// root-owned `affiliate` with `params.admits`; `closed` only an `org_invite`.
async fn admit(
	app: &App,
	store: &dyn AuthStore,
	account: &Account,
	personal_id: i64,
) -> ClResult<Option<(Ref, RefUse)>> {
	let mode = Auth::registration_mode(app).await?;
	let open = mode == "open";
	let refs = Refs::from_app(app)?;
	let pending = match store.pending_ref(account.id).await? {
		Some(uid) => refs.by_uid(&uid).await?,
		None => None,
	};
	let root = app.store.root_org_id().await?;
	let Some(r) = pending.filter(|r| admits(&mode, root, r)) else {
		return if open { Ok(None) } else { Err(invite_required()) };
	};
	let ctx = Ctx::public("auth.activate").as_user(account.id);
	match refs.redeem(&ctx, r.id, account.id, personal_id).await? {
		Some((u, _)) => Ok(Some((r, u))),
		None if open => Ok(None),
		None => Err(Error::coded(
			StatusCode::GONE,
			"E-AUTH-INVITE-EXPIRED",
			"this invitation has expired",
		)),
	}
}

/// Whether `r`'s type admits a registration under `mode`. Any org admin mints affiliates, so
/// `params.admits` counts only on the operator's own (`root`).
pub(crate) fn admits(mode: &str, root: i64, r: &Ref) -> bool {
	let invite = r.ref_type == "org_invite";
	match mode {
		"closed" => invite,
		"invite" => {
			invite
				|| r.ref_type == "signup"
				|| (r.ref_type == "affiliate"
					&& r.org_id == root
					&& r.params.get("admits") == Some(&json!(true)))
		}
		_ => {
			invite || r.ref_type == "signup" || r.ref_type == "affiliate" || {
				!matches!(r.ref_type.as_str(), "coupon" | "signup" | "org_invite" | "affiliate")
					&& r.params.get("admits") == Some(&json!(true))
			}
		}
	}
}

pub(crate) fn invite_required() -> Error {
	Error::coded(
		StatusCode::FORBIDDEN,
		"E-AUTH-INVITE-REQUIRED",
		"registration needs an invitation",
	)
}

/// The accepted membership an `org_invite` ref carries; `params.role` defaults to `MEMBER`.
/// `None` when the org is gone.
pub(crate) async fn join(
	store: &dyn AuthStore,
	r: &Ref,
	account: &Account,
) -> ClResult<Option<OrgId>> {
	let Some(org) = store.org_by_id(r.org_id).await? else {
		return Ok(None);
	};
	let role = r
		.params
		.get("role")
		.and_then(Value::as_str)
		.and_then(|s| s.parse().ok())
		.unwrap_or(Role::Member);
	// An invite never demotes: an ADMIN accepting a MEMBER invite stays ADMIN.
	let role = store.membership_role(org.id, account.id).await?.map_or(role, |r| r.max(role));
	if !store.put_membership(org.id, account.id, role).await? {
		return Err(Error::conflict("the owner's role cannot be changed"));
	}
	store.accept_membership(org.id, account.id, Timestamp::now()).await?;
	Ok(Some(org.uid))
}

/// `POST /api/auth/resend-activation`. Always `204`, whatever the address, so it cannot be
/// used to enumerate accounts.
pub async fn resend(
	State(app): State<App>,
	ClientIp(ip): ClientIp,
	Json(req): Json<ResendRequest>,
) -> ClResult<StatusCode> {
	let ctx = Ctx::public("auth.resend-activation").with_ip(ip);
	Auth::new(app).resend_activation(&ctx, &req.email, Some(&req.pow)).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// The deadline a token carries, snapped to [`MINT_GRAIN`] so a redelivery mints the *same*
/// token: `job::render` re-mints on every attempt, and an unbucketed `exp` put several
/// simultaneously-valid links in the relay logs. `rem_euclid`, not `%`: a negative timestamp
/// would otherwise shift the bucket forward.
pub(crate) fn bucketed_exp(now: Timestamp, ttl: i64) -> i64 {
	now.0 - now.0.rem_euclid(MINT_GRAIN) + ttl
}

/// A job's whole backoff run (8 attempts, 2^n seconds) fits inside one bucket. The trade is an
/// effective TTL of `ttl - MINT_GRAIN..= ttl`, so `expire_hours` stays true as a ceiling.
pub(crate) const MINT_GRAIN: i64 = 900;

/// Mints an activation token for `account`. Valid only while the account keeps the status
/// it has right now.
pub async fn mint(app: &App, account: &Account) -> ClResult<String> {
	let exp = bucketed_exp(Timestamp::now(), TTL_SECONDS);
	let payload = payload(account.uid.as_str(), account.status, exp);
	let key = pow::hmac_key(app, KEY_NAME).await?;
	let sig = mintworks_core::crypto::hmac_hex(&key, &payload)?;
	Ok(format!("{}.{sig}", B64.encode(payload.as_bytes())))
}

/// Opens `b64(purpose:…:exp).sig` — the shape both the activation and the password-reset
/// token use. Whatever sits between the uid and the deadline is ignored here: `recompute`
/// rebuilds the signed payload from the account's *current* state, so nothing the token
/// merely claims is ever trusted, and a token minted before any change to that state is
/// dead.
///
/// Every failure is `E-AUTH-TOKEN` with the same message: a bad token and an already-spent
/// one are indistinguishable, which is what makes replay uninteresting.
pub(crate) async fn open_signed(
	app: &App,
	store: &dyn AuthStore,
	presented: &str,
	purpose: &str,
	recompute: impl Fn(&Account, i64) -> String,
) -> ClResult<Account> {
	let (encoded, sig) = presented.split_once('.').ok_or_else(bad_token)?;
	let raw = B64.decode(encoded).map_err(|_| bad_token())?;
	let claimed = String::from_utf8(raw).map_err(|_| bad_token())?;
	let mut fields = claimed.split(':');
	let (Some(claimed_purpose), Some(uid), Some(exp)) =
		(fields.next(), fields.next(), fields.next_back())
	else {
		return Err(bad_token());
	};
	if claimed_purpose != purpose {
		return Err(bad_token());
	}
	verify(app, store, uid, exp, sig, recompute).await
}

/// The tail every signed token in this crate shares: check the deadline, load the account
/// the uid names, and compare the presented signature against one recomputed over the
/// account as it is now.
pub(crate) async fn verify(
	app: &App,
	store: &dyn AuthStore,
	uid: &str,
	exp: &str,
	sig: &str,
	recompute: impl Fn(&Account, i64) -> String,
) -> ClResult<Account> {
	let exp: i64 = exp.parse().map_err(|_| bad_token())?;
	if exp <= Timestamp::now().0 {
		return Err(bad_token());
	}
	let uid = AccountId::parse(uid).map_err(|_| bad_token())?;
	let account = store.account_by_uid(&uid).await?.ok_or_else(bad_token)?;

	let key = pow::hmac_key(app, KEY_NAME).await?;
	let expected = mintworks_core::crypto::hmac_hex(&key, &recompute(&account, exp))?;
	if !mintworks_core::crypto::ct_eq(expected.as_bytes(), sig.as_bytes()) {
		return Err(bad_token());
	}
	Ok(account)
}

async fn open(app: &App, store: &dyn AuthStore, token: &str) -> ClResult<Account> {
	open_signed(app, store, token, PURPOSE, |account, exp| {
		payload(account.uid.as_str(), account.status, exp)
	})
	.await
}

fn payload(uid: &str, status: AccountStatus, exp: i64) -> String {
	format!("{PURPOSE}:{uid}:{}:{exp}", status.as_str())
}

pub(crate) fn bad_token() -> Error {
	Error::coded(StatusCode::UNAUTHORIZED, "E-AUTH-TOKEN", "invalid or expired token")
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `exp` is the only time-varying input to either `mint`, so an identical `exp` is an
	/// identical token — which is what stops a retry putting a second live link in a relay log.
	#[test]
	fn two_mints_inside_one_grain_share_an_expiry() {
		let t = |s: i64| bucketed_exp(Timestamp(s), TTL_SECONDS);
		// 1_772_806_500 is a multiple of MINT_GRAIN, so the whole bucket is [base, base+899].
		assert_eq!(t(1_772_806_500), t(1_772_806_500 + MINT_GRAIN - 1));
		assert_ne!(t(1_772_806_500), t(1_772_806_500 + MINT_GRAIN));
		assert_eq!(t(-1), -MINT_GRAIN + TTL_SECONDS);
	}

	#[test]
	fn the_payload_binds_purpose_uid_status_and_expiry() {
		let p = payload("acc_01J", AccountStatus::Pending, 42);
		assert_eq!(p, "activate:acc_01J:PENDING:42");
		// Activation is what changes the status, so the signed string changes with it —
		// this is the whole single-use mechanism.
		assert_ne!(p, payload("acc_01J", AccountStatus::Active, 42));
	}
}

// vim: ts=4
