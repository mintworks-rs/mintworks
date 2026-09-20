//! Passkeys: WebAuthn registration, usernameless login, and an assertion as a step-up proof.
//!
//! No challenge row is ever written. The challenge state — a `PasskeyRegistration` or
//! `DiscoverableAuthentication`, which must never reach the client — is serialized into an
//! HMAC-signed blob the client hands back, exactly as [`crate::pow`] signs its challenge. The
//! blob's key is the persisted secret bound to a per-process nonce, which is what keeps the
//! process-local spent set sufficient: a restart invalidates every challenge still outstanding.
//! Only the spent `jti`s are remembered, and only for as long as a challenge lives, so there is
//! no `webauthn_challenges` table and no sweep job.
//!
//! Step-up takes the *same* blob the public login challenge mints. The challenge carries
//! no account, so the caller is established the way login establishes one: the assertion names
//! a credential, and the credential names an account — which `Auth::step_up_passkey` then
//! requires to be the calling account.

use std::sync::LazyLock;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use parking_lot::Mutex;
use saas_core::app::App;
use saas_core::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use webauthn_rs::prelude::{
	CreationChallengeResponse, CredentialID, DiscoverableAuthentication, DiscoverableKey, Passkey,
	PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential, Url, Uuid, Webauthn,
	WebauthnBuilder,
};

use crate::pow;
use crate::store::WebauthnCredential;

/// How long a challenge stays usable. Matches the 300 s interaction timeout the library puts on
/// the options it sends, so a blob cannot outlive the prompt the browser showed.
const TTL_SECONDS: i64 = 300;

/// The secret the challenge signature is keyed on, minted on first use.
const KEY_NAME: &str = "auth.webauthn_key";

/// Drop spent `jti`s once the set grows past this, as [`crate::pow::Spent`] does.
const SWEEP_AT: usize = 4096;

/// Challenges already spent, so one blob cannot be replayed — the authenticator's signature
/// counter cannot catch that on its own, because Apple and Google synced passkeys always
/// report 0.
//
// Process-global, matching the single-process deployment this framework targets. Running
// several processes needs this behind a shared store; nothing else changes.
static SPENT: LazyLock<Mutex<pow::Spent>> = LazyLock::new(|| Mutex::new(pow::Spent::new(SWEEP_AT)));

/// Mixed into the blob's signing key so a restart invalidates every outstanding challenge:
/// without it a captured blob would outlive `SPENT`, which a restart clears.
static PROCESS_NONCE: LazyLock<[u8; 32]> = LazyLock::new(|| {
	let mut nonce = [0u8; 32];
	OsRng.fill_bytes(&mut nonce);
	nonce
});

/// `POST /api/auth/wa/login`.
#[derive(Debug, Deserialize)]
pub struct LoginBody {
	pub blob: String,
	pub assertion: PublicKeyCredential,
}

/// `POST /api/auth/wa/register`. `name` is optional because the `User-Agent` default is better
/// than asking a user to name a credential they have not used yet.
#[derive(Debug, Deserialize)]
pub struct RegisterBody {
	pub blob: String,
	pub registration: RegisterPublicKeyCredential,
	#[serde(default)]
	pub name: Option<String>,
}

/// `PATCH /api/auth/wa/credentials/{credentialId}`.
#[derive(Debug, Deserialize)]
pub struct RenameBody {
	pub name: String,
}

/// The passkey half of `POST /api/auth/step-up`.
#[derive(Debug)]
pub struct StepUpProof {
	pub blob: String,
	pub assertion: PublicKeyCredential,
}

/// One row of `GET /api/auth/wa/credentials`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyView {
	pub credential_id: String,
	pub name: String,
	pub created_at: Timestamp,
	pub last_used_at: Option<Timestamp>,
}

#[derive(Serialize, Deserialize)]
struct Blob {
	state: State,
	exp: i64,
	jti: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum State {
	Register(Box<PasskeyRegistration>),
	Login(Box<DiscoverableAuthentication>),
}

/// The relying party, from `auth.webauthn.*` falling back to `BASE_URL`. Built per call: it is
/// three setting reads and a parse, and a cache would have to notice an operator moving the
/// origin mid-process.
async fn rp(app: &App) -> ClResult<Webauthn> {
	let base = Url::parse(app.config.base_url.trim_end_matches('/'))
		.map_err(|e| Error::internal(format!("BASE_URL is not a URL: {e}")))?;
	let host = base
		.host_str()
		.ok_or_else(|| Error::internal("BASE_URL has no host for the webauthn rp_id"))?
		.to_owned();

	let rp_id = non_blank(&app.settings.text("auth.webauthn.rp_id").await?).unwrap_or(host);
	let origin = match non_blank(&app.settings.text("auth.webauthn.origin").await?) {
		Some(raw) => Url::parse(&raw)
			.map_err(|e| Error::internal(format!("auth.webauthn.origin is not a URL: {e}")))?,
		None => base,
	};
	let name = non_blank(&app.settings.text("auth.webauthn.rp_name").await?)
		.unwrap_or_else(|| rp_id.clone());

	// A misconfigured rp_id or origin is a deployment error, not a caller's: `errStr` stays
	// generic and the library's reason goes to the log, as every other 5xx does.
	WebauthnBuilder::new(&rp_id, &origin)
		.and_then(|b| b.rp_name(&name).build())
		.map_err(|e| Error::internal(format!("webauthn relying party: {e}")))
}

fn non_blank(raw: &str) -> Option<String> {
	let trimmed = raw.trim();
	(!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The WebAuthn user handle: the 16 bytes an authenticator stores beside a discoverable
/// credential. Derived from `accounts.uid` so it is stable across devices and re-registrations
/// with no column to keep in sync — and it is never used to look an account up, because the
/// assertion names the credential and the credential names the account.
pub fn user_handle(account_uid: &str) -> Uuid {
	let digest = Sha256::digest(account_uid.as_bytes());
	let mut bytes = [0u8; 16];
	bytes.copy_from_slice(&digest[..16]);
	Uuid::from_bytes(bytes)
}

/// The credential id as the browser spells it — base64url, unpadded — which is the
/// `webauthn_credentials.credential_id` handle.
pub fn credential_id_str(id: &CredentialID) -> String {
	B64.encode(id)
}

/// A stored credential id back into the library's type, for `excludeCredentials`.
fn credential_id_from_str(raw: &str) -> ClResult<CredentialID> {
	B64.decode(raw).map(CredentialID::from).map_err(|e| {
		Error::internal(format!("webauthn: stored credential id is not base64url: {e}"))
	})
}

/// A stored serialized `Passkey`.
pub fn passkey(serialized: &str) -> ClResult<Passkey> {
	serde_json::from_str(serialized)
		.map_err(|e| Error::internal(format!("webauthn: stored credential does not parse: {e}")))
}

/// The credential ids already registered to this account, so a device that is already enrolled
/// is refused by the authenticator instead of becoming a second row for one credential.
pub fn exclude_ids(rows: &[WebauthnCredential]) -> ClResult<Vec<CredentialID>> {
	rows.iter().map(|row| credential_id_from_str(&row.credential_id)).collect()
}

/// A default credential name from the `User-Agent`: without one the management list is five
/// rows of "Passkey" and nobody can tell which to revoke. Deliberately crude — it is a label
/// the user can rename, not a fingerprint.
pub fn name_from_user_agent(user_agent: Option<&str>) -> String {
	let ua = user_agent.unwrap_or_default();
	// `Edg/` before `Chrome`, and `Chrome` before `Safari`: every Edge UA contains "Chrome",
	// and Chrome on iOS contains "Safari".
	let browser = if ua.contains("Firefox") {
		"Firefox"
	} else if ua.contains("Edg/") {
		"Edge"
	} else if ua.contains("Chrome") {
		"Chrome"
	} else if ua.contains("Safari") {
		"Safari"
	} else {
		"Browser"
	};
	let platform = if ua.contains("Windows") {
		"Windows"
	} else if ua.contains("Android") {
		"Android"
	} else if ua.contains("iPhone") || ua.contains("iPad") {
		"iOS"
	} else if ua.contains("Mac OS X") {
		"macOS"
	} else if ua.contains("Linux") {
		"Linux"
	} else {
		"device"
	};
	format!("{browser} on {platform}")
}

/// `POST /api/auth/wa/register/challenge` — the options plus the blob that carries their state.
pub(crate) async fn registration_challenge(
	app: &App,
	account_uid: &str,
	email: &str,
	display: &str,
	exclude: Vec<CredentialID>,
) -> ClResult<(Value, String)> {
	let (options, state): (CreationChallengeResponse, PasskeyRegistration) = rp(app)
		.await?
		// `start_passkey_registration`, not the attested variant: the attested one needs a
		// non-empty attestation CA list and refuses synchronised authenticators, i.e. every
		// Apple and Google passkey. Residence is asked for below instead.
		.start_passkey_registration(
			user_handle(account_uid),
			email,
			display,
			(!exclude.is_empty()).then_some(exclude),
		)
		.map_err(|e| rejected("registration could not start", e))?;
	let blob = seal(app, State::Register(Box::new(state))).await?;
	let mut options = to_value(options)?;
	require_discoverable(&mut options);
	Ok((options, blob))
}

/// `start_passkey_registration` hard-codes `residentKey: discouraged`, and a passkey manager may
/// take that literally: Bitwarden stores such a credential with `discoverable: false`, and its
/// picker offers nothing for the empty `allowCredentials` of a usernameless login. The library's
/// registration state ignores `require_resident_key` when verifying (`webauthn-rs-core`,
/// `register_credential`), so overriding the options is the whole change.
fn require_discoverable(options: &mut Value) {
	if let Some(sel) = options.pointer_mut("/publicKey/authenticatorSelection") {
		sel["residentKey"] = Value::from("required");
		sel["requireResidentKey"] = Value::Bool(true);
	}
}

pub(crate) async fn finish_registration(
	app: &App,
	blob: &str,
	registration: &RegisterPublicKeyCredential,
) -> ClResult<Passkey> {
	let State::Register(state) = open(app, blob).await? else {
		return Err(challenge_error());
	};
	rp(app)
		.await?
		.finish_passkey_registration(registration, &state)
		.map_err(|e| rejected("the passkey could not be registered", e))
}

/// `GET /api/auth/wa/login/challenge` — usernameless, so there is nothing to key it on and
/// `allowCredentials` stays empty. Also the blob a step-up uses.
pub(crate) async fn login_challenge(app: &App) -> ClResult<(Value, String)> {
	let (options, state) = rp(app)
		.await?
		.start_discoverable_authentication()
		.map_err(|e| rejected("the passkey challenge could not start", e))?;
	let blob = seal(app, State::Login(Box::new(state))).await?;
	Ok((to_value(options)?, blob))
}

/// Verifies an assertion against `passkey` and returns the stored credential re-serialized.
///
/// The library's `AuthenticationResult` carries the signature counter and the backup flags,
/// which live *inside* the serialized credential, so a caller must write this back together
/// with `last_used_at` — one statement, or the two drift.
pub(crate) async fn finish_login(
	app: &App,
	state: State,
	assertion: &PublicKeyCredential,
	passkey: &Passkey,
) -> ClResult<String> {
	let State::Login(state) = state else {
		return Err(challenge_error());
	};
	let key = DiscoverableKey::from(passkey);
	let result = rp(app)
		.await?
		.finish_discoverable_authentication(assertion, *state, std::slice::from_ref(&key))
		.map_err(|e| rejected("the passkey assertion was refused", e))?;
	let mut updated = passkey.clone();
	updated.update_credential(&result);
	serde_json::to_string(&updated).map_err(|e| Error::internal(e.to_string()))
}

/// The persisted secret bound to this process, so a blob cannot outlive the `SPENT` set that
/// records it as used.
async fn blob_key(app: &App) -> ClResult<Vec<u8>> {
	let mut hasher = Sha256::new();
	hasher.update(pow::hmac_key(app, KEY_NAME).await?);
	hasher.update(*PROCESS_NONCE);
	Ok(hasher.finalize().to_vec())
}

/// Signs the challenge state into an opaque `base64url(json).hmac` blob.
async fn seal(app: &App, state: State) -> ClResult<String> {
	let mut jti = [0u8; 16];
	OsRng.fill_bytes(&mut jti);
	let blob = Blob { state, exp: Timestamp::now().0 + TTL_SECONDS, jti: hex::encode(jti) };
	let raw = serde_json::to_vec(&blob).map_err(|e| Error::internal(e.to_string()))?;
	let payload = B64.encode(raw);
	let key = blob_key(app).await?;
	let sig = pow::hmac_hex(&key, &payload)?;
	Ok(format!("{payload}.{sig}"))
}

/// Verifies a blob and spends its `jti`. The `jti` is spent whether or not the assertion then
/// verifies, so a refused challenge cannot be retried with a different assertion — callers open
/// *before* resolving the credential, or an assertion naming an unknown credential id is not
/// refused at all and its blob stays replayable.
pub(crate) async fn open(app: &App, blob: &str) -> ClResult<State> {
	let (payload, sig) = blob.split_once('.').ok_or_else(challenge_error)?;
	let key = blob_key(app).await?;
	let expected = pow::hmac_hex(&key, payload)?;
	if !pow::ct_eq(expected.as_bytes(), sig.as_bytes()) {
		return Err(challenge_error());
	}
	let raw = B64.decode(payload).map_err(|_| challenge_error())?;
	let blob: Blob = serde_json::from_slice(&raw).map_err(|_| challenge_error())?;
	let now = Timestamp::now().0;
	if blob.exp <= now {
		return Err(challenge_error());
	}
	spend(&blob.jti, blob.exp, now)?;
	Ok(blob.state)
}

fn spend(jti: &str, exp: i64, now: i64) -> ClResult<()> {
	if SPENT.lock().spend(jti, exp, now) { Ok(()) } else { Err(challenge_error()) }
}

/// The options as the browser's own JSON. Nothing is stripped here: `webauthn-rs` omits the
/// optional members itself (`allow_credentials` is a `Vec`, `mediation` an `Option`), and the
/// browser rejects a `null` where one of them belongs.
fn to_value<T: Serialize>(options: T) -> ClResult<Value> {
	serde_json::to_value(options).map_err(|e| Error::internal(e.to_string()))
}

fn challenge_error() -> Error {
	Error::coded(
		StatusCode::BAD_REQUEST,
		"E-AUTH-CHALLENGE",
		"the passkey challenge is missing, expired, forged or already used",
	)
}

/// A rejected assertion is `401`, and the library's reason is never the `errStr`: which check
/// failed is what an attacker probing assertions is after.
fn rejected(what: &str, reason: impl std::fmt::Display) -> Error {
	tracing::warn!("webauthn: {what}: {reason}");
	Error::coded(StatusCode::UNAUTHORIZED, "E-AUTH-WEBAUTHN", what.to_owned())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_user_handle_is_stable_and_sixteen_bytes() {
		let a = user_handle("acc_01J0000000000000000000000");
		assert_eq!(a, user_handle("acc_01J0000000000000000000000"));
		assert_ne!(a, user_handle("acc_01J0000000000000000000001"));
	}

	#[test]
	fn a_jti_can_only_be_spent_once() {
		let now = Timestamp::now().0;
		assert!(spend("jti-spend-test", now + 60, now).is_ok());
		assert!(spend("jti-spend-test", now + 60, now).is_err());
	}

	#[test]
	fn the_default_name_separates_edge_and_chrome() {
		let edge =
			"Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 Chrome/120 Safari/537.36 Edg/120";
		assert_eq!(name_from_user_agent(Some(edge)), "Edge on Windows");
		let chrome = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120 Safari/537.36";
		assert_eq!(name_from_user_agent(Some(chrome)), "Chrome on Linux");
		assert_eq!(name_from_user_agent(None), "Browser on device");
	}

	#[test]
	fn an_unpadded_base64url_id_round_trips() {
		// Credential ids arrive unpadded from the browser, and the stored handle is the string
		// the login lookup is keyed on — a padding difference would be a silent miss.
		let id = credential_id_from_str("AAAAAAAAAAAAAAAAAAAAAA").unwrap();
		assert_eq!(credential_id_str(&id), "AAAAAAAAAAAAAAAAAAAAAA");
	}

	#[test]
	fn registration_asks_for_a_discoverable_credential() {
		let mut options = serde_json::json!({
			"publicKey": { "authenticatorSelection": { "residentKey": "discouraged", "requireResidentKey": false } }
		});
		require_discoverable(&mut options);
		let sel = &options["publicKey"]["authenticatorSelection"];
		assert_eq!(sel["residentKey"], "required");
		assert_eq!(sel["requireResidentKey"], true);
	}
}

// vim: ts=4
