//! TOTP enrolment, verification and recovery codes (RFC 6238).
//!
//! The shared secret lives in `totp_credentials.secret_enc`, AES-256-GCM under
//! `HKDF(MASTER_KEY, "totp")` — not in the `secrets` table, which is for one-of-a-kind
//! deployment keys rather than a row per account.
//!
//! Codes are HMAC-**SHA-256**, declared as `algorithm=SHA256` in the `otpauth://` URI.
//! RFC 6238's default is SHA-1; authenticators that ignore the `algorithm` parameter
//! (Google Authenticator has historically done so) will compute SHA-1 and their codes
//! will simply not verify at enrolment.
//
// Switching to SHA-1 later needs a `sha1` dependency and an `algorithm` column
// on `totp_credentials`, since already-enrolled secrets would keep their SHA-256 codes.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng as AeadRng};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::extract::State;
use axum::http::StatusCode;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::register;
use crate::service_api::Auth;
use crate::store::{Account, AuthStore, NewTotpCredential};

const DIGITS: i64 = 6;
const PERIOD: i64 = 30;
const SECRET_BYTES: usize = 20;
/// Steps either side of the current one that are still accepted, for clock drift.
const SKEW_STEPS: i64 = 1;
/// Characters per recovery code, drawn from the base32 alphabet.
const RECOVERY_CHARS: usize = 10;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

#[derive(Debug, Deserialize)]
pub struct CodeRequest {
	pub code: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Enrolment {
	pub secret: String,
	pub otpauth_uri: String,
	pub digits: i64,
	pub period: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryCodes {
	pub recovery_codes: Vec<String>,
}

pub(crate) fn bad_code() -> Error {
	Error::coded(StatusCode::UNAUTHORIZED, "E-AUTH-TOTP-INVALID", "invalid or reused code")
}

fn already_enrolled() -> Error {
	Error::coded(StatusCode::CONFLICT, "E-AUTH-TOTP-ENROLLED", "a second factor is already set up")
}

// ---------------------------------------------------------------- crypto

/// RFC 4648 base32, no padding. Encode only — the secret is generated here and never
/// parsed back from its printable form.
fn base32(data: &[u8]) -> String {
	let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
	let (mut acc, mut bits) = (0u32, 0u32);
	for &byte in data {
		acc = (acc << 8) | u32::from(byte);
		bits += 8;
		while bits >= 5 {
			bits -= 5;
			out.push(char::from(BASE32[((acc >> bits) & 31) as usize]));
		}
	}
	if bits > 0 {
		out.push(char::from(BASE32[((acc << (5 - bits)) & 31) as usize]));
	}
	out
}

/// The per-deployment TOTP key. Derived from `MASTER_KEY`, so it rotates with it and
/// nothing else, and the store never sees it.
fn cipher(app: &App) -> ClResult<Aes256Gcm> {
	let mut key = [0u8; 32];
	Hkdf::<Sha256>::new(None, &app.config.master_key)
		.expand(b"totp", &mut key)
		.map_err(|e| Error::internal(format!("totp hkdf: {e}")))?;
	Aes256Gcm::new_from_slice(&key).map_err(|e| Error::internal(format!("totp aes key: {e}")))
}

fn seal(app: &App, secret: &[u8]) -> ClResult<(Vec<u8>, Vec<u8>)> {
	let nonce = Aes256Gcm::generate_nonce(&mut AeadRng);
	let enc = cipher(app)?
		.encrypt(&nonce, secret)
		.map_err(|e| Error::internal(format!("totp encrypt: {e}")))?;
	Ok((nonce.to_vec(), enc))
}

fn open(app: &App, nonce: &[u8], enc: &[u8]) -> ClResult<Vec<u8>> {
	cipher(app)?
		.decrypt(Nonce::from_slice(nonce), enc)
		.map_err(|_| Error::internal("totp secret does not decrypt under the current master key"))
}

/// RFC 3986 percent-encoding: everything outside the unreserved set. Hand-rolled because the
/// workspace has no URL crate and this is the only place that needs one.
fn pct(s: &str) -> String {
	use std::fmt::Write as _;

	let mut out = String::with_capacity(s.len());
	for b in s.bytes() {
		match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
				out.push(char::from(b));
			}
			// Writing to a `String` is infallible.
			_ => drop(write!(out, "%{b:02X}")),
		}
	}
	out
}

/// One RFC 6238 code, with RFC 4226 dynamic truncation.
fn code_at(secret: &[u8], step: i64, digits: i64) -> ClResult<String> {
	let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
		.map_err(|e| Error::internal(format!("totp hmac: {e}")))?;
	mac.update(&step.to_be_bytes());
	let tag = mac.finalize().into_bytes();

	// The offset is four low bits, and the tag is 32 bytes, so `off + 3` is always inside.
	let off = usize::from(tag[tag.len() - 1] & 0x0f);
	let bin = u32::from_be_bytes([tag[off] & 0x7f, tag[off + 1], tag[off + 2], tag[off + 3]]);
	// Clamped on the *value*, not just the conversion: `totp_credentials.digits` has no `CHECK`,
	// so a tampered column reached `10u32.pow(10)`, which panics under `overflow-checks`.
	// RFC 4226 allows 6-8; 9 still fits a `u32`.
	let digits = digits.clamp(6, 9);
	let width = usize::try_from(digits).unwrap_or(6);
	Ok(format!("{:0width$}", bin % 10u32.pow(u32::try_from(digits).unwrap_or(6)), width = width))
}

/// Verifies a code against the account's credential and consumes its time step.
///
/// The replay guard is the store's, not a comparison here: `advance_totp_step` rejects a
/// step that is not greater than the stored one, so the same code cannot be spent twice
/// even by two requests racing.
pub(crate) async fn check_code(
	app: &App,
	store: &dyn AuthStore,
	account: &Account,
	code: &str,
) -> ClResult<()> {
	let cred = store.totp_by_account(account.id).await?.ok_or_else(bad_code)?;
	let secret = open(app, &cred.secret_nonce, &cred.secret_enc)?;
	let now_step = Timestamp::now().0 / cred.period.max(1);

	let code = code.trim();
	let mut accepted = None;
	for delta in -SKEW_STEPS..=SKEW_STEPS {
		let step = now_step + delta;
		if crate::pow::ct_eq(code_at(&secret, step, cred.digits)?.as_bytes(), code.as_bytes()) {
			accepted = Some(step);
			break;
		}
	}
	let step = accepted.ok_or_else(bad_code)?;
	if !store.advance_totp_step(account.id, step).await? {
		return Err(bad_code());
	}
	Ok(())
}

/// Matches a recovery code against the stored hashes and removes the one it spends, in a
/// single compare-and-swap so a code can never be spent twice.
pub(crate) async fn spend_recovery(
	store: &dyn AuthStore,
	account: &Account,
	presented: &str,
) -> ClResult<()> {
	let cred = store.totp_by_account(account.id).await?.ok_or_else(bad_code)?;
	let hashes: Vec<String> =
		serde_json::from_str(&cred.recovery_hashes).map_err(|_| bad_code())?;
	let presented = presented.trim().to_uppercase();

	// Argon2id against each stored hash in turn — at most `auth.recovery_codes`
	// (8) passes on a blocking thread, on a path taken once in an account's lifetime.
	// Index the codes by a cheap prefix if that ever shows up in a profile.
	let candidates = hashes.clone();
	let hit = crate::register::hash_blocking(move || {
		Ok(candidates.iter().position(|h| {
			PasswordHash::new(h).is_ok_and(|parsed| {
				Argon2::default().verify_password(presented.as_bytes(), &parsed).is_ok()
			})
		}))
	})
	.await?;

	let idx = hit.ok_or_else(bad_code)?;
	let mut left = hashes;
	left.remove(idx);
	let encoded =
		serde_json::to_string(&left).map_err(|e| Error::internal(format!("recovery: {e}")))?;

	// Conditional on the array this call read, as `advance_totp_step` guards the time step: a
	// plain overwrite let two requests with the same code both succeed, and two with different
	// codes clobber each other back into a spent one.
	if store.swap_totp_recovery(account.id, &cred.recovery_hashes, &encoded).await? {
		Ok(())
	} else {
		Err(bad_code())
	}
}

// ---------------------------------------------------------------- enrolment

/// Mints a secret, stores it unconfirmed, and returns what the authenticator needs.
/// [`crate::service_api::Auth::enrol_totp`] owns the step-up gate in front of this.
pub(crate) async fn begin_enrolment(
	app: &App,
	store: &dyn AuthStore,
	account: &Account,
) -> ClResult<Enrolment> {
	// A fast path, not the guard — that is `put_totp`'s `confirmed_at IS NULL` below. Read-then-
	// upsert was a TOCTOU: a `confirm_totp` landing between the two was wiped, leaving 2FA off
	// and the user holding printed recovery codes that matched nothing.
	if store
		.totp_by_account(account.id)
		.await?
		.is_some_and(|c| c.confirmed_at.is_some())
	{
		return Err(already_enrolled());
	}

	let mut secret = [0u8; SECRET_BYTES];
	OsRng.fill_bytes(&mut secret);
	let (nonce, enc) = seal(app, &secret)?;
	if !store
		.put_totp(&NewTotpCredential {
			account_id: account.id,
			secret_nonce: nonce,
			secret_enc: enc,
			digits: DIGITS,
			period: PERIOD,
			recovery_hashes: "[]".to_owned(),
		})
		.await?
	{
		return Err(already_enrolled());
	}

	let printable = base32(&secret);
	// Both halves are percent-encoded: `register::shape_ok` rejects only whitespace, so an
	// address carrying `?issuer=` rewrote the URI's query and let the registrant choose the
	// issuer the authenticator displays. `printable` is base32 and needs no encoding.
	let issuer = pct(&issuer(app));
	let label = pct(&account.email);
	Ok(Enrolment {
		otpauth_uri: format!(
			"otpauth://totp/{issuer}:{label}?secret={printable}&issuer={issuer}\
			 &algorithm=SHA256&digits={DIGITS}&period={PERIOD}"
		),
		secret: printable,
		digits: DIGITS,
		period: PERIOD,
	})
}

/// The label an authenticator shows. The deployment's host, so two environments of the
/// same product do not collide in the app's list.
fn issuer(app: &App) -> String {
	app.config
		.base_url
		.rsplit("://")
		.next()
		.unwrap_or("saas")
		.trim_end_matches('/')
		.to_owned()
}

/// Confirms the pending credential against `code` and mints the recovery codes, which are
/// returned once and stored only as argon2id hashes.
pub(crate) async fn confirm(
	app: &App,
	store: &dyn AuthStore,
	account: &Account,
	code: &str,
) -> ClResult<RecoveryCodes> {
	// No read-then-write precondition: `confirm_totp` carries `confirmed_at IS NULL` in its own
	// `WHERE`, so two requests from adjacent skew steps cannot both mint a recovery-code set —
	// the loser is told the factor is armed, not handed codes the database did not keep.
	check_code(app, store, account, code).await?;

	// Everything fallible happens *before* the factor is armed: a failure between the
	// `confirmed_at` and `recovery_hashes` writes left 2FA on with no recovery codes, and
	// `enrol` refuses an already-confirmed credential — an unrecoverable lockout.
	let wanted = app.settings.int("auth.recovery_codes").await?;
	let mut plain = Vec::new();
	let mut hashes = Vec::new();
	for _ in 0..wanted.max(1) {
		let code = recovery_code();
		hashes.push(register::hash_password(code.clone()).await?);
		plain.push(code);
	}
	let encoded =
		serde_json::to_string(&hashes).map_err(|e| Error::internal(format!("recovery: {e}")))?;
	if !store.confirm_totp(account.id, Timestamp::now(), &encoded).await? {
		return Err(already_enrolled());
	}
	Ok(RecoveryCodes { recovery_codes: plain })
}

fn recovery_code() -> String {
	let mut raw = [0u8; RECOVERY_CHARS];
	OsRng.fill_bytes(&mut raw);
	raw.iter().map(|b| char::from(BASE32[usize::from(*b) % 32])).collect()
}

// ---------------------------------------------------------------- routes

/// `POST /api/auth/totp` — begins enrolment.
pub async fn enrol(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Enrolment>> {
	Ok(Json(Auth::new(app).enrol_totp(&ctx).await?))
}

/// `POST /api/auth/totp/verify` — confirms enrolment and hands out the recovery codes.
pub async fn verify(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<CodeRequest>,
) -> ClResult<Json<RecoveryCodes>> {
	Ok(Json(Auth::new(app).confirm_totp(&ctx, &req.code).await?))
}

/// `DELETE /api/auth/totp` — removes the second factor.
pub async fn remove(State(app): State<App>, ctx: Ctx) -> ClResult<StatusCode> {
	Auth::new(app).remove_totp(&ctx).await?;
	Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn base32_matches_rfc4648_vectors() {
		assert_eq!(base32(b""), "");
		assert_eq!(base32(b"f"), "MY");
		assert_eq!(base32(b"fo"), "MZXQ");
		assert_eq!(base32(b"foo"), "MZXW6");
		assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
	}

	/// `totp_credentials.digits` has no `CHECK`, so a tampered column used to reach
	/// `10u32.pow(10)` and panic under `overflow-checks`.
	#[test]
	fn a_tampered_digit_count_cannot_overflow() {
		for digits in [-1, 0, 6, 8, 10, i64::MAX] {
			let code = code_at(b"0123456789", 1, digits).unwrap();
			assert!((6..=9).contains(&code.len()), "{digits} -> {code}");
		}
	}

	/// An unencoded `?` or `&` in the label rewrites the query, so the authenticator shows an
	/// issuer the registrant chose.
	#[test]
	fn the_otpauth_label_is_percent_encoded() {
		assert_eq!(
			pct("evil?issuer=YourBank&x@attacker.tld"),
			"evil%3Fissuer%3DYourBank%26x%40attacker.tld"
		);
		assert_eq!(pct("a.b-c_d~e"), "a.b-c_d~e");
	}

	#[test]
	fn codes_are_six_digits_and_step_dependent() {
		let secret = b"12345678901234567890";
		let a = code_at(secret, 1, 6).unwrap();
		let b = code_at(secret, 2, 6).unwrap();
		assert_eq!(a.len(), 6);
		assert!(a.chars().all(|c| c.is_ascii_digit()));
		assert_ne!(a, b, "a different time step must give a different code");
		assert_eq!(a, code_at(secret, 1, 6).unwrap(), "the same step must be stable");
	}
}

// vim: ts=4
