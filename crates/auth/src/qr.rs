//! QR login: a device that is already signed in approves a new browser session.
//!
//! The pending session lives in this process's memory, like [`crate::pow`]'s spent salts and
//! [`crate::webauthn`]'s spent `jti`s. Nothing reaches the database, so an approval that never
//! arrives costs one map entry and one sweep.
//!
//! The initiating browser is anonymous and stays anonymous: the session carries no account, so
//! there is nothing to check at init and no address to enumerate. Its 32-byte secret never
//! enters the QR and the long poll presents it as `x-qr-secret`, which is what stops a bystander
//! who photographs the QR from collecting the tokens. On a *forwarded* QR the attacker is the
//! initiator, and there the match code and the device details are the human-facing defence.

use std::collections::HashMap;
use std::net::IpAddr;
use std::pin::pin;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::token::Tokens;

/// Two minutes: long enough to pick up a phone and compare a code, short enough that a QR left
/// on a shared screen expires before anyone can photograph it and act on it.
pub const TTL_SECONDS: i64 = 120;

/// How long `GET …/status` holds a request open before answering `pending`, so the desktop
/// learns of an approval without polling on a timer.
pub const POLL_DEFAULT_SECONDS: u64 = 15;

/// The ceiling on `?wait=`. Without it a caller holds a connection open for the whole TTL.
pub const POLL_MAX_SECONDS: u64 = 30;

/// Sweep expired sessions once the map grows past this. `qr.init` is rate-limited, so the map
/// only reaches it under a distributed flood — the sweep is what keeps the memory bounded then.
const CAPACITY: usize = 4096;

/// No `0`/`O`/`1`/`I`/`L`: the match code is compared by eye between two screens, and read aloud
/// when the two devices are not in the same room.
const MATCH_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Sessions by id, whether pending or answered.
//
// Process-global, matching the single-process deployment this framework targets. Running several
// processes needs this behind a shared store; nothing else changes.
static SESSIONS: LazyLock<Mutex<HashMap<String, Session>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

struct Session {
	/// SHA-256 of the secret the initiating browser holds. The map never carries the credential
	/// itself, so nothing that can read this process's memory can approve someone's login.
	secret_hash: [u8; 32],
	/// Copied at init so the approving phone can say *which* browser it is letting in.
	user_agent: Option<String>,
	ip: Option<IpAddr>,
	match_code: String,
	expires_at: i64,
	state: State,
	notify: Arc<Notify>,
}

enum State {
	Pending,
	/// The minted pair, waiting for the initiating browser to collect it. Held until the poll
	/// takes it, so the desktop is the only party that ever receives the tokens.
	Approved(Box<Tokens>),
	Denied,
}

/// What `POST /api/auth/qr/init` returns. The SPA renders `{BASE_URL}/qr/{sessionId}` as the QR;
/// the server never builds that URL, because only the client knows where it is served.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitResponse {
	pub session_id: String,
	/// Shown once, to the initiating browser only: `x-qr-secret` on every status poll.
	pub secret: String,
	/// Six characters, compared by the human on both screens.
	pub match_code: String,
}

/// What `GET …/details` shows the approving phone. No match code: telling the phone the code
/// would let anyone who saw the QR type it back, so the code travels one way only — desktop
/// screen to phone keyboard.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrDetails {
	pub browser: String,
	pub ip: Option<String>,
}

/// What a status poll resolved to.
#[derive(Debug)]
pub enum Status {
	/// The window elapsed with no answer; the client asks again.
	Pending,
	Denied,
	Approved(Box<Tokens>),
}

/// The long-poll query on `GET /api/auth/qr/{sessionId}/status`.
#[derive(Debug, Deserialize)]
pub struct QrStatusQuery {
	#[serde(default)]
	pub wait: Option<u64>,
}

/// `POST /api/auth/qr/{sessionId}/respond`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QrRespondBody {
	pub approved: bool,
	/// Typed off the initiating screen. `QrDetails` does not carry it, so holding the session id
	/// is not enough to answer.
	pub match_code: String,
}

/// Starts a session for an anonymous desktop. `ctx.ip` and the `User-Agent` are the only things
/// recorded about it — evidence for the human on the approving side, never used to authenticate.
pub fn init(ctx: &Ctx, user_agent: Option<&str>) -> ClResult<InitResponse> {
	let now = Timestamp::now().0;
	let mut secret = [0u8; 32];
	OsRng.fill_bytes(&mut secret);
	let mut id = [0u8; 16];
	OsRng.fill_bytes(&mut id);
	let session_id = hex::encode(id);
	let match_code = match_code();

	let mut sessions = SESSIONS.lock();
	if sessions.len() >= CAPACITY {
		sessions.retain(|_, s| s.expires_at > now);
		if sessions.len() >= CAPACITY {
			return Err(Error::Unavailable("too many pending QR logins".to_owned()));
		}
	}
	sessions.insert(
		session_id.clone(),
		Session {
			secret_hash: digest(&secret),
			user_agent: user_agent.map(str::to_owned),
			ip: ctx.ip,
			match_code: match_code.clone(),
			expires_at: now + TTL_SECONDS,
			state: State::Pending,
			notify: Arc::new(Notify::new()),
		},
	);
	Ok(InitResponse { session_id, secret: B64.encode(secret), match_code })
}

/// Long-polls one session until it is answered or `wait_seconds` elapses.
///
/// No `Ctx`: the secret *is* the credential here, and the initiating browser has no account to
/// derive anything from. `Notify` is a latency optimisation and not the source of truth — every
/// poll re-reads the state, so a wake-up that arrives before the waiter registered costs one
/// window's delay and never an answer.
pub async fn status(session_id: &str, secret: &str, wait_seconds: u64) -> ClResult<Status> {
	let window = Duration::from_secs(wait_seconds.clamp(1, POLL_MAX_SECONDS));
	let deadline = tokio::time::Instant::now() + window;
	loop {
		let notify: Arc<Notify> = {
			let mut sessions = SESSIONS.lock();
			let now = Timestamp::now().0;
			let Some(session) = sessions.get(session_id) else {
				return Err(Error::NotFound);
			};
			// A wrong secret, a decode failure and an unknown session all answer `NotFound`: the
			// session id is printed in the QR, and a client that lost its secret needs the same
			// recovery as one whose session expired. `init` hashed the raw bytes, hence the decode.
			let Some(presented) = B64.decode(secret).ok().map(|raw| digest(&raw)) else {
				return Err(Error::NotFound);
			};
			if !mintworks_core::crypto::ct_eq(&presented, &session.secret_hash) {
				return Err(Error::NotFound);
			}
			if session.expires_at <= now {
				sessions.remove(session_id);
				// The contract is 404, as for a session the sweep has already dropped; the client
				// maps it to `expired`.
				return Err(Error::NotFound);
			}
			match &session.state {
				State::Pending => Arc::clone(&session.notify),
				State::Approved(_) | State::Denied => {
					let Some(session) = sessions.remove(session_id) else {
						return Err(Error::NotFound);
					};
					return Ok(match session.state {
						State::Approved(tokens) => Status::Approved(tokens),
						_ => Status::Denied,
					});
				}
			}
		};
		// `enable()` registers with the waiter list before the wait, so an answer arriving
		// between the read above and this line is not slept through.
		let mut notified = pin!(notify.notified());
		notified.as_mut().enable();
		tokio::select! {
			() = &mut notified => {}
			() = tokio::time::sleep_until(deadline) => return Ok(Status::Pending),
		}
	}
}

/// What the approving phone sees before it decides.
pub fn details(session_id: &str) -> ClResult<QrDetails> {
	let sessions = SESSIONS.lock();
	let session = sessions.get(session_id).ok_or(Error::NotFound)?;
	if session.expires_at <= Timestamp::now().0 {
		return Err(Error::NotFound);
	}
	Ok(QrDetails {
		browser: crate::webauthn::name_from_user_agent(session.user_agent.as_deref()),
		ip: session.ip.map(|ip| ip.to_string()),
	})
}

/// Compares `code` with the one shown on the initiating screen, then records the answer and
/// wakes whoever is waiting on it.
///
/// `tokens` is `Some` for an approval and `None` for a denial, already minted by the caller: the
/// approve path must not hold the map lock across an `await`, and `Auth::qr_respond` is what
/// knows how to mint a pair with no `auth_at`.
pub fn resolve(session_id: &str, tokens: Option<Box<Tokens>>, code: &str) -> ClResult<()> {
	let now = Timestamp::now().0;
	let notify = {
		let mut sessions = SESSIONS.lock();
		let Some(session) = sessions.get_mut(session_id) else {
			return Err(Error::NotFound);
		};
		if session.expires_at <= now {
			sessions.remove(session_id);
			return Err(Error::NotFound);
		}
		// Constant time, though the 120 s TTL and the blanket rate limit are what make the code
		// unbrute-forceable; the check itself is what makes holding the session id insufficient.
		if !mintworks_core::crypto::ct_eq(
			code.trim().to_ascii_uppercase().as_bytes(),
			session.match_code.as_bytes(),
		) {
			return Err(code_mismatch());
		}
		// An approval and a late denial race by design — the phone may tap twice, or two tabs
		// may disagree. The first answer is the one the waiting browser was promised, and it
		// stays.
		if !matches!(session.state, State::Pending) {
			return Err(answered());
		}
		session.state = match tokens {
			Some(tokens) => State::Approved(tokens),
			None => State::Denied,
		};
		Arc::clone(&session.notify)
	};
	notify.notify_waiters();
	Ok(())
}

fn answered() -> Error {
	Error::coded(StatusCode::CONFLICT, "E-AUTH-QR-STATE", "this QR login has already been answered")
}

fn code_mismatch() -> Error {
	Error::coded(
		StatusCode::FORBIDDEN,
		"E-AUTH-QR-CODE",
		"the code does not match the one shown on the other screen",
	)
}

fn digest(bytes: &[u8]) -> [u8; 32] {
	Sha256::digest(bytes).into()
}

fn match_code() -> String {
	// Rejection sampling, not `% 31`: 256 is not a multiple of the alphabet, so the modulo made
	// 8 of its 31 characters likelier — entropy shed from the flow's only human-facing defence.
	let ceiling = 256 - (256 % MATCH_ALPHABET.len());
	let mut code = String::with_capacity(6);
	let mut buf = [0u8; 32];
	while code.len() < 6 {
		OsRng.fill_bytes(&mut buf);
		for b in buf {
			if usize::from(b) < ceiling {
				code.push(MATCH_ALPHABET[usize::from(b) % MATCH_ALPHABET.len()] as char);
				if code.len() == 6 {
					break;
				}
			}
		}
	}
	code
}

#[cfg(test)]
mod tests {
	use super::*;

	fn seed(id: &str, expires_at: i64) -> String {
		SESSIONS.lock().insert(
			id.to_owned(),
			Session {
				secret_hash: digest(b"secret"),
				user_agent: None,
				ip: None,
				match_code: "ABCDEF".to_owned(),
				expires_at,
				state: State::Pending,
				notify: Arc::new(Notify::new()),
			},
		);
		id.to_owned()
	}

	#[test]
	fn a_session_can_only_be_answered_once() {
		let id = seed("qr-test-once", Timestamp::now().0 + TTL_SECONDS);
		resolve(&id, None, "ABCDEF").unwrap();
		// A second tap, or a denial arriving after an approval, must not overwrite the answer
		// the waiting browser has already been promised.
		assert!(resolve(&id, None, "ABCDEF").is_err());
		SESSIONS.lock().remove(&id);
	}

	#[test]
	fn an_approval_needs_the_code_from_the_other_screen() {
		let id = seed("qr-test-code", Timestamp::now().0 + TTL_SECONDS);
		assert!(resolve(&id, None, "WRONG1").is_err());
		// The wrong code refused nothing permanent: the session is still pending, and the real
		// code answers it.
		assert!(resolve(&id, None, " abcdef ").is_ok(), "trimmed and case-folded");
		SESSIONS.lock().remove(&id);
	}

	#[test]
	fn an_expired_session_is_refused_and_dropped() {
		let now = Timestamp::now().0;
		let id = seed("qr-test-expired", now - 1);
		assert!(resolve(&id, None, "ABCDEF").is_err());
		assert!(!SESSIONS.lock().contains_key(&id));
	}

	#[test]
	fn a_match_code_is_six_unambiguous_characters() {
		let code = match_code();
		assert_eq!(code.len(), 6);
		assert!(!code.contains(['0', 'O', '1', 'I', 'L']));
		// Every character is from the alphabet, over enough draws to catch an off-by-one in the
		// rejection bound rather than only the happy path.
		for _ in 0..1_000 {
			let code = match_code();
			assert_eq!(code.len(), 6);
			assert!(code.bytes().all(|b| MATCH_ALPHABET.contains(&b)), "{code}");
		}
	}
}

// vim: ts=4
