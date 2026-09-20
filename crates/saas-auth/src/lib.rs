//! Accounts, orgs, memberships, API keys, passkeys, TOTP credentials and consent records.
//!
//! Authentication is pure stateless JWT: there is no session table, no token denylist and
//! no activation- or reset-token table. Activation and reset tokens are HMAC-signed and
//! single-use because their signature covers the state the action changes
//! (`accounts.status`, `accounts.pwd_hash`); a live token pair is revoked by bumping
//! `accounts.token_epoch`, or globally by rotating the `auth.jwt_key` secret.

#![forbid(unsafe_code)]

// The crate's interface is [`Auth`], the route bundles and the store trait; the private
// modules below are handlers over one `Auth` method each, so nothing outside names them.
pub(crate) mod activate;
pub(crate) mod apikey;
pub mod consent;
pub mod gdpr;
pub mod job;
pub(crate) mod login;
pub mod org;
pub mod pow;
pub(crate) mod qr;
pub mod register;
pub(crate) mod reset;
pub mod routes;
pub mod service_api;
pub(crate) mod stepup;
pub mod store;
pub(crate) mod token;
pub(crate) mod totp;
pub(crate) mod webauthn;

pub use apikey::{ApiKeyView, MintedKey};
pub use consent::{ConsentBody, PublishLegalDoc};
pub use job::{LinkMail, render};
pub use org::{MemberBody, OrgDetail, OrgPatch, OrgSummary, SwitchResponse};
pub use pow::{Challenge, Proof};
// `LoginBody`/`Status` would collide with `token::LoginBody` and read as something else at the
// crate root, so the two that clash are named for what they authenticate.
pub use qr::{InitResponse, QrDetails, Status as QrStatus};
pub use routes::{authenticated, consent_gated_router, public};
pub use service_api::{Auth, ConsentGrant, Credentials, Erasure, LoginOutcome, Registration};
pub use stepup::StepUpResponse;
pub use store::{AuthStore, LegalKind, OrgKind, Role};
pub use token::{AccountBody, LoginBody, LoginOrg, OrgBody, Tokens};
pub use totp::{Enrolment, RecoveryCodes};
pub use webauthn::{
	LoginBody as PasskeyLogin, PasskeyView, RegisterBody as PasskeyRegistration, StepUpProof,
};

use saas_core::settings::SettingDef;

/// This crate's declared settings, registered with `AppBuilder::settings(saas_auth::SETTINGS)`.
pub static SETTINGS: &[SettingDef] = &[
	// How long a session may be renewed for, from its `auth_at`. With no session table and no
	// denylist, without this a captured refresh token renewed itself indefinitely. An hour is
	// the floor because below the refresh TTL it is a logout.
	SettingDef::int(
		"auth.session_max_seconds",
		"2592000",
		"Seconds a session may be renewed for, from its auth_at.",
	)
	.range(3_600, 31_536_000),
	// Bounded at 20: `RateLimiter::consumed` reads the `AUTH_FAILED` bucket, which
	// `saas_core::ratelimit` fixes at `20/5min/ip`. A higher threshold is unreachable and
	// silently disables the gate.
	SettingDef::int(
		"auth.pow_after_failures",
		"3",
		"Failed logins from an address before proof-of-work is demanded.",
	)
	.range(0, 20),
	// Bounded because `totp::confirm` argon2id-hashes each one in a loop while holding a
	// `HASH_SLOTS` permit: an unbounded count hung `POST /api/auth/totp/verify` and starved
	// every other password hash in the process with it.
	SettingDef::int("auth.recovery_codes", "8", "Recovery codes minted when TOTP is enrolled.")
		.range(1, 64),
	// Whether `POST /api/auth/register` accepts new accounts. Login, activation and reset
	// stay up when it is off, so an operator can close signups during an abuse wave without
	// locking out the accounts that already exist. See `Auth::register`.
	SettingDef::flag("auth.registration_open", "1", "Whether new accounts may register."),
	SettingDef::int("pow.difficulty.", "18", "Proof-of-work leading zero bits, per scope.")
		.range(1, 32)
		.family(),
	SettingDef::int(
		"jobs.max_attempts.AUTH_LINK_EMAIL",
		"14",
		"Attempts before an activation or reset link mail is lost.",
	)
	.range(0, 1_000),
	// The two caps the mint path enforces. Unbounded, a key outlives the employment it was
	// minted for, and an org's key list becomes a page nobody can read.
	SettingDef::int("auth.api_keys_max", "20", "Live API keys one org may hold at once.")
		.range(1, 10_000),
	SettingDef::int(
		"auth.api_key_max_days",
		"365",
		"Furthest `expiresAt` a new API key may carry, in days from now.",
	)
	.range(1, 3_650),
	// The relying party. Blank means "derive from `BASE_URL`": a deployment that serves the
	// SPA from one origin configures nothing, and one that splits the SPA onto another
	// writes three rows rather than rebuilding.
	SettingDef::text(
		"auth.webauthn.rp_id",
		"",
		"WebAuthn relying-party id (registrable host); blank takes BASE_URL's.",
	),
	SettingDef::text(
		"auth.webauthn.origin",
		"",
		"WebAuthn origin as scheme://host[:port]; blank takes BASE_URL.",
	),
	SettingDef::text(
		"auth.webauthn.rp_name",
		"",
		"WebAuthn relying-party display name; blank takes the rp_id.",
	),
	// A passkey is a first factor, so this cap keeps the management list readable rather than
	// bounding an attack — the same reasoning as `auth.api_keys_max`.
	SettingDef::int("auth.webauthn_max", "10", "Passkeys one account may hold at once.")
		.range(1, 1_000),
];

/// Declared so the unprefixed environment namespace cannot collide with a setting's variable.
/// `auth.jwt_key` belongs to `saas-core`, which is what verifies the token.
pub static SECRETS: &[&str] = &["auth.token_key", "pow.hmac_key", "auth.webauthn_key"];

// vim: ts=4
