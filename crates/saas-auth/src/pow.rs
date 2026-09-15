//! The stateless proof-of-work captcha (`api-surface.md` §3.3).
//!
//! No challenge row is ever written. The challenge carries its own HMAC, so the server has
//! to remember only which salts have already been spent, and only for as long as a
//! challenge lives. Losing that set on a restart is harmless: every outstanding challenge
//! expires within [`TTL_SECONDS`] anyway.

use std::collections::HashMap;
use std::sync::LazyLock;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::Json;
use axum::extract::{Query, State};
use hmac::Mac;
use parking_lot::Mutex;
use saas_core::app::App;
use saas_core::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type Hmac256 = hmac::Hmac<Sha256>;

/// How long a challenge stays solvable: long enough for a slow phone to grind 2^18 hashes
/// in a Web Worker, short enough that the spent-salt set below stays small.
pub const TTL_SECONDS: i64 = 300;

/// The secret the challenge signature is keyed on, minted on first use.
const KEY_NAME: &str = "pow.hmac_key";

/// The scopes `?scope=` accepts, per `api-surface.md` §3.3.
const SCOPES: &[&str] = &["register", "resend-activation", "password-reset", "login"];

/// Drop expired salts once the spent set grows past this.
const SWEEP_AT: usize = 8192;

/// Salts already spent, so one solution cannot be replayed. Entries die with the challenge.
//
// Process-global, matching the single-process deployment this framework targets.
// Running several processes needs this behind a shared store; nothing else changes.
static SPENT: LazyLock<Mutex<Spent>> =
	LazyLock::new(|| Mutex::new(Spent { map: HashMap::new(), next_sweep: SWEEP_AT }));

struct Spent {
	map: HashMap<String, i64>,
	/// Map size at which the next sweep runs. Not `0`: that would sweep on the first request.
	next_sweep: usize,
}

/// What `GET /api/pow/challenge` returns.
#[derive(Clone, Debug, Serialize)]
pub struct Challenge {
	pub salt: String,
	pub difficulty: i64,
	/// ISO-8601, matching every other timestamp on the wire.
	pub exp: String,
	pub sig: String,
}

/// What a protected request carries back as its `pow` member.
#[derive(Clone, Debug, Deserialize)]
pub struct Proof {
	pub salt: String,
	pub exp: String,
	pub sig: String,
	pub nonce: u64,
}

#[derive(Debug, Deserialize)]
pub struct ChallengeQuery {
	pub scope: String,
}

/// `GET /api/pow/challenge`
pub async fn challenge(
	State(app): State<App>,
	Query(q): Query<ChallengeQuery>,
) -> ClResult<Json<Challenge>> {
	Ok(Json(issue(&app, &q.scope).await?))
}

/// Mints a challenge for `scope`. The difficulty comes from `pow.difficulty.<scope>`, which
/// resolves through the `pow.difficulty.` setting family, so it can be raised per scope
/// under pressure.
pub async fn issue(app: &App, scope: &str) -> ClResult<Challenge> {
	if !SCOPES.contains(&scope) {
		return Err(Error::validation(format!("unknown pow scope `{scope}`")));
	}
	let difficulty = app.settings.int(&format!("pow.difficulty.{scope}")).await?;
	let mut raw = [0u8; 16];
	OsRng.fill_bytes(&mut raw);
	let salt = hex::encode(raw);
	let exp = Timestamp(Timestamp::now().0 + TTL_SECONDS)
		.to_rfc3339()
		.ok_or_else(|| Error::internal("pow: challenge expiry is not representable"))?;
	let key = hmac_key(app, KEY_NAME).await?;
	let sig = hmac_hex(&key, &format!("{scope}|{salt}|{difficulty}|{exp}"))?;
	Ok(Challenge { salt, difficulty, exp, sig })
}

/// Verifies a submitted proof and spends its salt.
///
/// Every failure is `E-CORE-POW` with the same message: which check failed is not the
/// caller's business. Re-signing with the difficulty read *now* means that raising
/// `pow.difficulty.<scope>` invalidates the challenges already out there — the client
/// fetches a fresh one and retries, which is the intended behaviour under pressure.
pub async fn verify(app: &App, scope: &str, proof: &Proof) -> ClResult<()> {
	let now = Timestamp::now().0;
	let exp = Timestamp::parse_rfc3339(&proof.exp).ok_or_else(reject)?;
	if exp.0 <= now || exp.0 > now + TTL_SECONDS {
		return Err(reject());
	}
	let difficulty = app.settings.int(&format!("pow.difficulty.{scope}")).await?;
	let key = hmac_key(app, KEY_NAME).await?;
	let expected = hmac_hex(&key, &format!("{scope}|{}|{difficulty}|{}", proof.salt, proof.exp))?;
	if !ct_eq(expected.as_bytes(), proof.sig.as_bytes()) {
		return Err(reject());
	}
	if !solved(&proof.salt, proof.nonce, difficulty) {
		return Err(reject());
	}
	spend(&proof.salt, exp.0, now)
}

/// Reads an HMAC key from the secret store, minting it on first use so no deployment step
/// has to seed it.
///
/// [`saas_core::secrets::SecretStore::get_or_create`] makes the mint converge rather than
/// overwrite. That matters beyond PoW: `token::sign` reaches `auth.jwt_key` through here,
/// and a lost race there killed every session the loser had already signed.
pub(crate) async fn hmac_key(app: &App, name: &str) -> ClResult<Vec<u8>> {
	app.secrets.get_or_create(name, 32).await
}

/// HMAC-SHA256 of `msg` under `key`, hex-encoded.
pub(crate) fn hmac_hex(key: &[u8], msg: &str) -> ClResult<String> {
	let mut mac = <Hmac256 as Mac>::new_from_slice(key)
		.map_err(|e| Error::internal(format!("hmac key rejected: {e}")))?;
	mac.update(msg.as_bytes());
	Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Length-checked, branch-free comparison, for anything an attacker can retry.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
	a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn reject() -> Error {
	Error::Pow("proof of work missing, expired, replayed or unsolved".to_owned())
}

/// `leading_zero_bits(SHA256(salt || nonce)) >= difficulty`, both parts in their ASCII
/// form — the salt exactly as issued, the nonce in decimal — so a browser can build the
/// preimage with `TextEncoder` and Web Crypto alone, no WASM.
fn solved(salt: &str, nonce: u64, difficulty: i64) -> bool {
	let mut h = Sha256::new();
	h.update(salt.as_bytes());
	h.update(nonce.to_string().as_bytes());
	leading_zero_bits(&h.finalize()) >= difficulty
}

fn leading_zero_bits(digest: &[u8]) -> i64 {
	let mut bits = 0;
	for byte in digest {
		bits += i64::from(byte.leading_zeros());
		if *byte != 0 {
			break;
		}
	}
	bits
}

fn spend(salt: &str, exp: i64, now: i64) -> ClResult<()> {
	let s = &mut *SPENT.lock();
	if s.map.len() >= s.next_sweep {
		s.map.retain(|_, e| *e > now);
		// A map whose entries are all still inside `TTL_SECONDS` drops nothing, so a fixed
		// threshold re-scans on every later `spend` under this lock. Doubling makes the scans
		// logarithmic in the map's lifetime size, as `saas_core::ratelimit::take` does.
		s.next_sweep = s.map.len().saturating_mul(2).max(SWEEP_AT);
	}
	if s.map.insert(salt.to_owned(), exp).is_some() { Err(reject()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn zero_bits_counts_across_bytes() {
		assert_eq!(leading_zero_bits(&[0x00, 0x0f, 0xff]), 12);
		assert_eq!(leading_zero_bits(&[0xff]), 0);
		assert_eq!(leading_zero_bits(&[0x00, 0x00]), 16);
	}

	#[test]
	fn ct_eq_is_exact() {
		assert!(ct_eq(b"abc", b"abc"));
		assert!(!ct_eq(b"abc", b"abd"));
		assert!(!ct_eq(b"abc", b"ab"));
	}

	#[test]
	fn a_salt_can_only_be_spent_once() {
		let now = Timestamp::now().0;
		let salt = "0123456789abcdef-spend-test";
		assert!(spend(salt, now + 60, now).is_ok());
		assert!(spend(salt, now + 60, now).is_err());
	}
}

// vim: ts=4
