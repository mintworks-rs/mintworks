//! `POST /api/auth/register`.
//!
//! **Registration takes no password.** The password is set at activation, by whoever proves
//! they read the mail — the only step that establishes the address belongs to the person
//! choosing the credential. It also makes every `PENDING` account carry `pwd_hash = NULL` *by
//! construction*, so the invited (`Auth::add_member`) and self-registered cases stop being
//! states `activate` has to tell apart, and a submitted password can no longer be silently
//! thrown away down the duplicate-address path.
//!
//! The response is identical whether or not the address is already registered: `204` with no
//! body on every branch.
//!
//! **The timing is not identical.** With no password there is nothing expensive left to run,
//! so the duplicate path is measurably cheaper — accepted, because the route is PoW-gated and
//! limited to `3/h/ip`. The upgrade path is a "someone tried to register your address" notice
//! on the already-`ACTIVE` branch, which equalizes the three branches with a real feature
//! rather than a decoy hash; it needs four new `templates/email/` files.
//!
//! **No body**, and in particular no uid: a prefixed ULID opens with a millisecond timestamp,
//! so decoding one off the duplicate path would say exactly when the address was first
//! registered — the one thing every other line here goes out of its way not to say. Nothing
//! needs it; the client's next step is the emailed activation link.

use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng;
use argon2::{Argon2, PasswordHasher};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::USER_AGENT;
use saas_core::app::App;
use saas_core::auth_mw::ClientIp;
use saas_core::prelude::*;
use serde::Deserialize;
use std::sync::LazyLock;
use tokio::sync::Semaphore;

use saas_core::ctx::Ctx;

use crate::pow;
use crate::service_api::{Auth, Registration};
use crate::store::{Account, AccountStatus, AuthStore, LegalDoc, LegalKind};

/// Shortest password accepted. Length is the only rule: composition rules push people
/// towards worse passwords, and argon2id carries the rest. Enforced at activation and at
/// reset — the two routes that actually take a password.
const MIN_PASSWORD_CHARS: usize = 10;

#[derive(Clone, Debug, Deserialize)]
pub struct ConsentInput {
	pub kind: LegalKind,
	pub version: String,
}

#[derive(Debug, Deserialize)]
pub struct Request {
	pub email: String,
	pub name: Option<String>,
	pub locale: Option<String>,
	#[serde(default)]
	pub consents: Vec<ConsentInput>,
	pub pow: pow::Proof,
}

/// `POST /api/auth/register`. Creates the account, its personal org and the owning
/// membership in one transaction, records the consents, and queues the activation mail.
/// No token comes back: activation has to happen first, and no uid either — see the module
/// doc.
pub async fn register(
	State(app): State<App>,
	ClientIp(ip): ClientIp,
	headers: HeaderMap,
	Json(req): Json<Request>,
) -> ClResult<StatusCode> {
	let ctx = Ctx::public("auth.register").with_ip(ip);
	Auth::new(app)
		.register(
			&ctx,
			&Registration {
				email: req.email,
				name: req.name,
				locale: req.locale,
				consents: req.consents,
				user_agent: headers
					.get(USER_AGENT)
					.and_then(|v| v.to_str().ok())
					.map(str::to_owned),
				pow: Some(req.pow),
			},
		)
		.await?;
	Ok(StatusCode::NO_CONTENT)
}

/// The duplicate-address path: same status, same body shape, same cost. The only
/// difference is invisible to the caller — a still-pending account gets its activation
/// mail again, an active one gets nothing.
pub(crate) async fn already_registered(
	app: &App,
	store: &dyn AuthStore,
	email: &str,
) -> ClResult<()> {
	let Some(account) = store.account_by_email(email).await? else {
		// Lost a race against a concurrent delete, so the row that caused the conflict is
		// already gone. There is nothing left to conceal.
		return Err(Error::coded(
			StatusCode::CONFLICT,
			"E-AUTH-EMAIL-TAKEN",
			"that address is already registered",
		));
	};
	if account.status == AccountStatus::Pending {
		send_activation(app, &account).await?;
	}
	Ok(())
}

/// Queues the activation mail. Shared with `resend-activation` and `Auth::add_member`.
///
/// The token is **not** minted here — see [`crate::job`]. The queued row names the account
/// and the link kind, and the handler mints when it renders, so a `FAILED` job's retained
/// payload is a diagnostic rather than a live account-takeover credential.
pub async fn send_activation(app: &App, account: &Account) -> ClResult<()> {
	crate::job::enqueue(&app.store, account, crate::job::LinkKind::Activation).await
}

/// What to call the account in an email when it never gave a name.
pub(crate) fn display_name(account: &Account) -> String {
	account.name.clone().unwrap_or_else(|| local_part(&account.email))
}

/// `TOS` and `PRIVACY` are mandatory, and every entry must name the version that is
/// current for the caller's locale. Anything else is `E-AUTH-CONSENT-REQUIRED`.
pub(crate) async fn check_consents(
	store: &dyn AuthStore,
	given: &[ConsentInput],
	locale: &str,
) -> ClResult<Vec<LegalDoc>> {
	let now = Timestamp::now();
	let mut docs = Vec::with_capacity(given.len());
	let mut seen: Vec<LegalKind> = Vec::with_capacity(given.len());
	for input in given {
		// One consent per kind: a repeated `TOS` wrote two identical `consents` rows inside
		// the registration transaction, and the GDPR export then showed the same grant twice.
		if seen.contains(&input.kind) {
			return Err(consent_required());
		}
		seen.push(input.kind);
		let doc = store
			.current_legal_doc(input.kind, locale, now)
			.await?
			.filter(|d| d.version == input.version)
			.ok_or_else(consent_required)?;
		docs.push(doc);
	}
	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		if !given.iter().any(|c| c.kind == kind) {
			return Err(consent_required());
		}
	}
	Ok(docs)
}

fn consent_required() -> Error {
	Error::coded(
		StatusCode::FORBIDDEN,
		"E-AUTH-CONSENT-REQUIRED",
		"the current TOS and PRIVACY documents must both be accepted",
	)
}

/// argon2id at the crate default costs 19 MiB per concurrent pass, and both the login miss
/// branch (`service_api::login`) and [`crate::login::verify_password`]'s no-hash branch spend
/// a full pass on unauthenticated input — deliberately, to equalize timing. tokio's blocking
/// pool is 512 threads by default, so unbounded that is ~10 GiB of RSS for a burst of logins.
///
/// Every argon2 site in this crate queues here, so the two login branches still cost the
/// same and the timing equalization survives the bound.
// Fixed permit count. Make it a `settings` key if a deployment needs to tune it.
const HASH_CONCURRENCY: usize = 8;
static HASH_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(HASH_CONCURRENCY));

/// Run one argon2id job on a blocking thread, at most [`HASH_CONCURRENCY`] at a time.
pub(crate) async fn hash_blocking<T: Send + 'static>(
	job: impl FnOnce() -> ClResult<T> + Send + 'static,
) -> ClResult<T> {
	let _permit = HASH_SLOTS
		.acquire()
		.await
		.map_err(|e| Error::Unavailable(format!("hash permits unavailable: {e}")))?;
	tokio::task::spawn_blocking(job)
		.await
		.map_err(|e| Error::internal(format!("password hashing did not finish: {e}")))?
}

/// argon2id at the crate's default parameters, on a blocking thread.
pub async fn hash_password(password: String) -> ClResult<String> {
	hash_blocking(move || {
		let salt = SaltString::generate(&mut OsRng);
		Argon2::default()
			.hash_password(password.as_bytes(), &salt)
			.map(|h| h.to_string())
			.map_err(|e| Error::internal(format!("argon2: {e}")))
	})
	.await
}

/// A BCP-47-shaped tag we are willing to put in a filename: two ASCII letters, optionally
/// `-` plus two more. Nothing here can traverse out of `email.template_dir`.
pub(crate) fn is_locale(s: &str) -> bool {
	let (lang, region) = s.split_once('-').map_or((s, None), |(l, r)| (l, Some(r)));
	let two_alpha = |v: &str| v.len() == 2 && v.bytes().all(|b| b.is_ascii_alphabetic());
	two_alpha(lang) && region.is_none_or(two_alpha)
}

/// Trust-boundary validation of an address. Deliberately shallow — the activation mail is the
/// only check that proves it exists — but not skippable: `accounts.email` has no CHECK, so
/// this is all that stands between a request body and a stored `"a@b\r\nBcc: c@d"` that
/// `saas_email` rejects hours later inside a job. Every entry point that stores an address
/// goes through here: registration, `Auth::add_member`, and password reset.
pub(crate) fn validate(email: &str) -> ClResult<()> {
	let (local, domain) = email.split_once('@').unwrap_or(("", ""));
	if shape_ok(email, local, domain) {
		return Ok(());
	}
	let mut errs = FieldErrors::new();
	// The code, not the prose: `fields` is a map of field name to
	// `E-CORE-FORMAT`/`E-CORE-RANGE`, and a client switching on it never matched a sentence.
	// The human wording is the `errStr` beside it.
	errs.insert("email".to_owned(), E_FORMAT);
	Err(Error::ValidationFields("not an email address".to_owned(), errs))
}

/// The password rule on its own. Activation and reset are where a password is actually set.
pub(crate) fn validate_password(password: &str) -> ClResult<()> {
	if password.chars().count() >= MIN_PASSWORD_CHARS {
		return Ok(());
	}
	let mut errs = FieldErrors::new();
	// A length bound is a range failure, not a format one.
	errs.insert("password".to_owned(), E_RANGE);
	Err(Error::ValidationFields(
		format!("a password is at least {MIN_PASSWORD_CHARS} characters"),
		errs,
	))
}

fn shape_ok(email: &str, local: &str, domain: &str) -> bool {
	!local.is_empty()
		&& !domain.is_empty()
		&& domain.contains('.')
		&& !domain.starts_with('.')
		&& !domain.ends_with('.')
		&& !domain.contains('@')
		&& !email.contains(char::is_whitespace)
		&& email.len() <= 254
}

pub(crate) fn local_part(email: &str) -> String {
	email.split('@').next().unwrap_or(email).to_owned()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `locale` lands in an email-template path, and `Path::join` resolves `..`.
	/// `accounts.locale` has no CHECK, so `register` is the only gate.
	#[test]
	fn locale_is_a_bare_tag_or_nothing() {
		for ok in ["hu", "en", "pt-br", "PT-BR"] {
			assert!(is_locale(ok), "{ok}");
		}
		for bad in ["", "../../x", "english", "hu/en", "hu.", "h", "hu-", "hu-brazil"] {
			assert!(!is_locale(bad), "{bad}");
		}
	}

	#[test]
	fn validate_rejects_what_it_should() {
		assert!(validate("a@b.hu").is_ok());
		assert!(validate("nobody").is_err());
		assert!(validate("@b.hu").is_err());
		assert!(validate("a@b").is_err());
		assert!(validate("a@b@c.hu").is_err());
		assert!(validate("a b@c.hu").is_err());
		// `accounts.email` has no CHECK, so an unvalidated address reaches `Mailbox::parse`
		// inside a job hours later, or `orgs.name` forever.
		assert!(validate("a@b\r\nBcc: c@d.hu").is_err());
		assert!(validate(&format!("{}@b.hu", "x".repeat(100_000))).is_err());
	}

	/// `fields` is a map of field name to `errCode`, and both producers put a sentence in the value
	/// slot, so a client switching on it never matched.
	#[test]
	fn the_fields_member_carries_codes_and_not_prose() {
		let fields = |e: Error| match e {
			Error::ValidationFields(_, f) => f,
			other => panic!("expected a field breakdown, got {other:?}"),
		};
		assert_eq!(fields(validate("nobody").unwrap_err()).get("email"), Some(&E_FORMAT));
		assert_eq!(fields(validate_password("short").unwrap_err()).get("password"), Some(&E_RANGE));
	}
}

// vim: ts=4
