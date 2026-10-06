// SPDX-License-Identifier: MPL-2.0
//! `Auth` — the service handle a consumer application creates accounts, orgs and sessions
//! through.
//!
//! It exists because the **Rust service API is the framework's interface** and the routes are
//! opt-in adapters over it: a consumer registers an account or invites a member from its own
//! code, or from a job, without constructing an axum request.
//!
//! Every method takes `&Ctx` first and derives its permission from `ctx.actor`, never from
//! which middleware the request passed.
//!
//! ## Where the line runs
//!
//! The handle owns **validation, authorization, transaction boundaries and audit**. The
//! route bundle owns **transport**: parsing the body, minting cookies, and
//! rendering a [`LoginOutcome`] as either a `200` body or the `401` second-factor challenge.
//!
//! **Rate limiting is not here.** It is three middleware tiers plus the per-route
//! `route_layer` exceptions in [`crate::routes`] (`mintworks_core::ratelimit`), so a consumer
//! calling these methods from its own Rust is not throttled — it already holds the database.
//!
//! One thing that looks like a gate does live on the handle rather than in the handler,
//! because it is a decision and not parsing:
//!
//! * **Proof of work.** [`Auth::register`] and [`Auth::login`] take an optional
//!   [`pow::Proof`] and decide themselves whether one is required — on `login` that decision
//!   reads the IP-keyed [`mintworks_core::ratelimit::AUTH_FAILED`] bucket the auth middleware
//!   charges on a 401. It escalates, it never denies, which is why it is not a limit.
//!   A proof is demanded only of a caller that came off a socket
//!   (`ctx.ip.is_some()`), on the same reasoning
//!   [`mintworks_core::auth_mw::require_stepup`] uses to exempt `Actor::System` (never
//!   `Actor::Public`, which is what an unauthenticated route carries): the application's
//!   own code already holds the database, and admission control against it buys nothing.
//!
//! ## The whole HTTP surface goes through here
//!
//! Every route in [`crate::routes`] is a body-and-a-call over one method on this handle, so a
//! consumer can switch orgs, change a member's role, enrol TOTP or a passkey, mint an API key,
//! or change a password from Rust.
//!
//! One consequence of taking the org from `ctx.org_id` rather than the token's `org`
//! claim: `auth_mw::verify` resolves that field through the accepted-membership join, so a
//! caller whose membership is gone arrives with no org instead of one this handle then
//! refuses. `active_membership` still answers `E-AUTH-ORG` for the race between the two
//! reads; a token minted before the removal simply carries no org into the next call.

use std::sync::Arc;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use mintworks_core::app::App;
use mintworks_core::auth_mw::{require_operator, require_role};
use mintworks_core::ctx::Ctx;
use mintworks_core::event::{Event, emit};
use mintworks_core::prelude::*;
use mintworks_core::refs::{CreateRef, Ref, Refs};
use mintworks_email::SendEmail;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::consent::ConsentBody;
use crate::org::{MemberBody, OrgDetail, OrgPatch, OrgSummary, SwitchResponse};
use crate::stepup::StepUpResponse;
use crate::store::{
	Account, AccountStatus, ApiKey, AuthStore, LegalKind, NewAccount, NewApiKey, NewConsent,
	NewLegalDoc, NewWebauthnCredential, Org, OrgKind, OrgStatus, Role, WebauthnCredential,
};
use crate::token::Tokens;
use crate::webauthn::{self, PasskeyView};
use crate::{
	activate, apikey, consent, gdpr, login, pow, qr, register, reset, routes, token, totp,
};
use webauthn_rs::prelude::PublicKeyCredential;

/// What [`Auth::register`] takes. `user_agent` is transport evidence copied onto the consent rows
/// beside `ctx.ip`; both are stored because a consent record has to be defensible years later.
///
/// **No password field.** It is chosen at activation — see the `register` module doc.
#[derive(Clone, Debug, Default)]
pub struct Registration {
	pub email: String,
	pub name: Option<String>,
	pub locale: Option<String>,
	pub consents: Vec<register::ConsentInput>,
	pub user_agent: Option<String>,
	/// Required of a caller that came off a socket; see the module doc.
	pub pow: Option<pow::Proof>,
	/// A ref code, resolved here and redeemed at activation. An unknown one is dropped silently.
	pub ref_code: Option<String>,
}

/// What [`Auth::login`] takes.
#[derive(Clone, Debug)]
pub struct Credentials {
	pub email: String,
	pub password: String,
	/// Required once the caller's address has spent `settings['auth.pow_after_failures']`
	/// tokens from the `auth.failed` bucket — which escalates rather than denying, so an
	/// attacker cannot lock an account out by parking on it.
	pub pow: Option<pow::Proof>,
}

/// Which second factor a caller presented. Private: the choice is the service's, and both
/// public entry points take the two as separate `Option<&str>` arguments.
enum Factor<'a> {
	Totp(&'a str),
	Recovery(&'a str),
}

/// The two ways a correct password ends.
#[derive(Debug)]
pub enum LoginOutcome {
	/// Signed in. The route bundle renders this with [`token::respond`].
	Signed(Box<Tokens>),
	/// The account has a confirmed second factor, rendered as a `401 E-AUTH-TOTP-REQUIRED`.
	///
	/// The ticket is present on the login path, where `POST /api/auth/login/totp` spends it,
	/// and `None` on the reset path, where the caller re-submits the mailed reset token with
	/// `code` or `recoveryCode` — no ticket is redeemable there.
	TotpRequired { totp_token: Option<String> },
}

/// What [`Auth::record_consent`] takes.
#[derive(Clone, Debug)]
pub struct ConsentGrant {
	pub kind: LegalKind,
	/// Must be the version currently in force: an account cannot consent to superseded text.
	pub version: String,
	/// The `sha256` of the [`consent::LegalBody`] actually presented. **Required**, and
	/// checked against the row being recorded: one version can exist in several locales, so
	/// the version alone let a `hu` account that read the English text store the Hungarian
	/// row's hash — evidence about wording the user never saw, which is exactly what the
	/// version check exists to prevent. `Option` only so that omitting it answers
	/// `E-CORE-VALIDATION`; see [`Auth::record_consent`].
	pub doc_sha256: Option<String>,
	/// The org the consent is given on behalf of, if any. The caller must be an accepted
	/// member of it.
	pub org_uid: Option<String>,
	pub user_agent: Option<String>,
}

/// What [`Auth::erase_account`] answers with. Erasure is anonymization — see [`crate::gdpr`].
#[derive(Clone, Debug)]
pub struct Erasure {
	pub status: AccountStatus,
	pub anonymized_at: Timestamp,
	pub retained_until: String,
	pub retained_because: String,
}

/// The locale an account starts in when the caller names none — an invitee has no
/// per-org locale column to read, and changes it on first login — and the one a public
/// legal-document reader gets.
// A constant, not a setting. Give it a `SETTINGS` key when a deployment needs a
// different default.
pub(crate) const DEFAULT_LOCALE: &str = "en";

/// Bound for an account or org name, matching `mintworks-invoice`'s `MAX_PARTY_NAME` reasoning:
/// text that is displayed everywhere needs a ceiling somewhere.
pub(crate) const MAX_NAME_CHARS: usize = 200;

/// The member-listing ceiling. `AuthStore::members` had no `LIMIT` at all, and an org admin
/// grows the table by inviting; a cursor belongs here only once a real org needs a second
/// page.
pub const MAX_MEMBERS: i64 = 500;

/// Required, trimmed, and bounded in **characters** — a Hungarian name is multi-byte, and
/// `len()` would refuse a legal one.
///
/// Without one, axum's 2 MB body limit is the only ceiling: an org admin could `PATCH
/// /api/org` a 2 MB name that every member's login, `GET /api/auth/me` and `GET /api/orgs`
/// then carries — one caller amplifying onto everyone else's responses.
pub(crate) fn bounded(what: &str, s: &str, max: usize) -> ClResult<()> {
	// Here rather than per call site: `{{name}}` renders through `no_escape` in the `.txt.hbs`
	// templates, so a `\n` in any name-like field injects lines into a text/plain mail.
	if s.chars().any(char::is_control) {
		return Err(Error::validation(format!("{what} contains a control character")));
	}
	match s.trim().chars().count() {
		0 => Err(Error::validation(format!("{what} is required"))),
		n if n > max => Err(Error::validation(format!("{what} is too long"))),
		_ => Ok(()),
	}
}

/// As [`bounded`], but a document body legitimately contains line breaks. Tab, CR and LF stay
/// legal; every other control character does not — `body` reaches no `no_escape` template, but
/// the C0 range has no business in stored evidence text.
pub(crate) fn bounded_multiline(what: &str, s: &str, max: usize) -> ClResult<()> {
	if s.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
		return Err(Error::validation(format!("{what} contains a control character")));
	}
	match s.trim().chars().count() {
		0 => Err(Error::validation(format!("{what} is required"))),
		n if n > max => Err(Error::validation(format!("{what} is too long"))),
		_ => Ok(()),
	}
}

/// The `GET /api/org` shape, shared by the read and the patch.
fn org_detail(org: &Org, role: Role) -> OrgDetail {
	OrgDetail {
		uid: org.uid.as_str().to_owned(),
		kind: org.kind,
		name: org.name.clone(),
		status: org.status,
		billing_currency: org.billing_currency.clone(),
		slug: org.slug.clone(),
		role,
		created_at: org.created_at,
	}
}

/// The one `E-AUTH-FORBIDDEN` for "this caller has no business with this org". Deliberately
/// the same prose whether the membership is missing or the org is another one's: the answer
/// must not say which.
pub(crate) fn forbidden() -> Error {
	Error::coded(StatusCode::FORBIDDEN, "E-AUTH-FORBIDDEN", "no access to this org")
}

/// Concurrent [`Auth::export_account`] dumps. Two, leaving three of the adapter's five reader
/// connections for everything else.
///
/// ponytail: a process-wide constant assuming that five. If the reader count ever becomes
/// configurable, size this from it — or move the permit into the adapter, which knows it.
static EXPORTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Clone)]
pub struct Auth {
	app: App,
	/// Resolved once here rather than on each `self.store()?` call: `App` is immutable after
	/// `build()`, so the extension map cannot answer differently later.
	store: Option<Arc<dyn AuthStore>>,
}

impl Auth {
	pub fn new(app: App) -> Self {
		let store = app.extensions.get::<Arc<dyn AuthStore>>().cloned();
		Self { app, store }
	}

	fn store(&self) -> ClResult<Arc<dyn AuthStore>> {
		self.store.clone().ok_or_else(routes::no_store)
	}

	/// The account this call acts as. `Actor::System` has none, which is why every method
	/// below that speaks for a person refuses it rather than guessing.
	async fn actor_account(&self, ctx: &Ctx) -> ClResult<Account> {
		let id = ctx.actor.account_id().ok_or_else(|| {
			Error::coded(
				StatusCode::UNAUTHORIZED,
				"E-AUTH-TOKEN",
				"this call has to be made as a signed-in account",
			)
		})?;
		// Deliberately no `login::ensure_usable`: a consumer creating an org from Rust acts
		// for a still-`PENDING` account. A method that must not serve one says so itself.
		self.store()?.account_by_id(id).await?.ok_or_else(forbidden)
	}

	/// `ctx.org_id` resolved to the row, plus the caller's re-read role in it.
	///
	/// The role comes from `memberships`, never from the token's `rol` claim, and the org has to
	/// be `ACTIVE`: [`Auth::switch_org`] refuses to enter a suspended org, and without this a
	/// caller already inside one keeps every route.
	async fn active_of(&self, ctx: &Ctx) -> ClResult<(Account, Org, Role)> {
		let account = self.actor_account(ctx).await?;
		let store = self.store()?;
		// The *subject* is the org, so one the caller cannot see reads as absent, never as
		// forbidden. Lacking the role in an org is the different case.
		let org = store.org_by_id(ctx.org()?).await?.ok_or(Error::NotFound)?;
		if org.status != OrgStatus::Active {
			return Err(Error::coded(
				StatusCode::FORBIDDEN,
				"E-AUTH-SUSPENDED",
				"this org is suspended",
			));
		}
		// The *effective* role — a membership on this org or on any ancestor of it — which is
		// why this is not `accepted_membership_role`, which sees only a direct row.
		let role = self.app.store.org_role(account.id, org.id).await?.ok_or(Error::NotFound)?;
		Ok((account, org, role))
	}

	/// The `org-admin` level: [`Auth::active_of`] and `memberships.role IN ('OWNER','ADMIN')`,
	/// read from the database on every call. That is the second of the two mitigations standing
	/// in for the session table this design does not have.
	///
	/// The role is the **effective** one, so a membership inherited from an ancestor passes;
	/// [`Auth::owner_of`] deliberately does not inherit.
	async fn admin_of(&self, ctx: &Ctx) -> ClResult<(Account, Org, Role)> {
		let out = self.active_of(ctx).await?;
		if out.2 < Role::Admin {
			return Err(forbidden());
		}
		Ok(out)
	}

	/// The caller's *active* org, with the **effective** role — a membership on this org or on
	/// any ancestor of it.
	///
	/// Not `token::pick_org`, which picks a *default* — the sole active organisation, else
	/// the personal org. A step-up would then hand someone working in organisation B a
	/// token scoped to their personal org, and the destructive operation step-up was
	/// gating would run against the wrong org.
	///
	/// A `ctx.org_id` the account is no longer a member of fails rather than falling back:
	/// silently substituting another org is the same hole from the other side. Reachable
	/// only as a race — `auth_mw::verify` resolves `org_id` through the same accepted
	/// membership — which is exactly why it is an error and not a silent `None`.
	async fn active_membership(&self, ctx: &Ctx) -> ClResult<Option<(OrgId, Role)>> {
		let Some(id) = ctx.org_id else {
			return Ok(None);
		};
		let org = self.store()?.org_by_id(id).await?.ok_or_else(forbidden)?;
		// The effective role, not a direct membership row's: a direct `MEMBER` who inherits
		// `OWNER` would otherwise report `MEMBER` here while `GET /api/org` says `OWNER`. A
		// direct-but-unaccepted row cannot occur — `ctx.org_id` is only ever an accepted org.
		let account_id = ctx.actor.account_id().ok_or_else(forbidden)?;
		let role = self.app.store.org_role(account_id, org.id).await?.ok_or_else(|| {
			Error::coded(
				StatusCode::FORBIDDEN,
				"E-AUTH-ORG",
				"no longer a member of the active org",
			)
		})?;
		Ok(Some((org.uid, role)))
	}

	/// A proof of work, demanded only of a caller that came off a socket. See the module doc.
	async fn require_pow(
		&self,
		ctx: &Ctx,
		scope: &str,
		proof: Option<&pow::Proof>,
	) -> ClResult<()> {
		if ctx.ip.is_none() {
			return Ok(());
		}
		let proof = proof.ok_or_else(|| Error::Pow("proof of work required".into()))?;
		pow::verify(&self.app, scope, proof).await
	}

	/// How many auth failures the caller's address has accrued, as `auth_mw` counts them on
	/// every 401 it or a public handler answers. Read, never spent: this handle charges no
	/// bucket at all.
	///
	/// A call with no `ctx.ip` never came off a socket, so there is no bucket and the gate
	/// does not fire — the same fail-open `ratelimit::default_mw` takes, for the same reason.
	fn auth_failures(&self, ctx: &Ctx) -> i64 {
		match ctx.ip {
			Some(ip) => {
				let key = mintworks_core::ratelimit::bucket_key(ip);
				self.app.limits.consumed(mintworks_core::ratelimit::AUTH_FAILED, &key)
			}
			None => 0,
		}
	}

	// ------------------------------------------------------------ accounts

	/// Create an account, its personal org and the owning membership in one transaction,
	/// record the consents, and queue the activation mail.
	///
	/// **No password.** The account is created with `pwd_hash = NULL` and the password is set
	/// at activation — see the `register` module doc for why, and for the timing property that
	/// change gives up.
	///
	/// **Answers the same way whether or not the address is already registered.** Both paths queue
	/// an activation mail for a still-`PENDING` account, and nothing about the return value
	/// distinguishes them — which is why it returns `()` and not the new uid.
	///
	/// # Errors
	/// `403 E-AUTH-CLOSED` under `auth.registration = closed`, `403 E-AUTH-INVITE-REQUIRED`
	/// under `invite`, when no ref is given or the given one is unknown, spent or of a type the
	/// mode does not admit. An addressed ref does not restrict the registering email.
	pub async fn register(&self, ctx: &Ctx, req: &Registration) -> ClResult<()> {
		let mode = Self::registration_mode(&self.app).await?;
		let refused = || match mode.as_str() {
			"closed" => {
				Some(Error::coded(StatusCode::FORBIDDEN, "E-AUTH-CLOSED", "registration is closed"))
			}
			"invite" => Some(activate::invite_required()),
			_ => None,
		};
		// First, before the proof of work: with signups closed there is nothing to spend that
		// work on. A refusal here says nothing about a code, because none was given.
		let ref_code = req.ref_code.as_deref().map(str::trim).filter(|c| !c.is_empty());
		if ref_code.is_none()
			&& let Some(e) = refused()
		{
			return Err(e);
		}
		self.require_pow(ctx, "register", req.pow.as_ref()).await?;

		let email = req.email.trim().to_lowercase();
		register::validate(&email)?;
		// Registration is the one body that sets both `accounts.name` and `orgs.name`.
		if let Some(name) = &req.name {
			bounded("name", name, MAX_NAME_CHARS)?;
		}
		let locale = req.locale.clone().unwrap_or_else(|| DEFAULT_LOCALE.to_owned());
		// The locale is interpolated into an email-template path
		// (`mintworks_email::template::load`) and `Path::join` resolves `..`. `accounts.locale` has
		// no CHECK, so this is the gate.
		if !register::is_locale(&locale) {
			return Err(Error::validation("unsupported locale"));
		}
		let store = self.store()?;

		// Consent is checked before the account exists, so a client holding a superseded
		// document version cannot register at all.
		let docs = register::check_consents(store.as_ref(), &req.consents, &locale).await?;

		let new = NewAccount {
			org_name: req.name.clone().unwrap_or_else(|| register::local_part(&email)),
			email: email.clone(),
			// Set at activation, by whoever proves they read the mail. Every `PENDING`
			// account has a NULL hash, self-registered and invited alike.
			pwd_hash: None,
			name: req.name.clone(),
			locale,
		};

		// Built before the insert and written *inside* its transaction, so the account and
		// its consents commit or fail together. `account_id` is filled in by the store.
		let consents: Vec<NewConsent> = docs
			.iter()
			.map(|doc| NewConsent {
				account_id: 0,
				// Registration consent is given by the person, not by an org.
				org_id: None,
				kind: doc.kind,
				legal_doc_id: Some(doc.id),
				doc_version: doc.version.clone(),
				doc_sha256: doc.sha256.clone(),
				granted: true,
				ip: ctx.ip.map(|ip| ip.to_string()),
				user_agent: req.user_agent.clone(),
			})
			.collect();

		// Open mode drops an unknown code silently; the others refuse it here, before any
		// account exists, so a bogus code never leaves a PENDING account behind.
		let pending_ref = match ref_code {
			Some(code) => {
				let r = mintworks_core::refs::Refs::from_app(&self.app)?.by_code(code).await?;
				let root = self.app.store.root_org_id().await?;
				let ok = r.as_ref().is_some_and(|r| {
					r.is_redeemable(Timestamp::now()) && activate::admits(&mode, root, r)
				});
				if !ok && let Some(e) = refused() {
					return Err(e);
				}
				r.map(|r| r.id)
			}
			None => None,
		};
		match store.create_account(&new, &consents).await {
			Ok((account, _personal_org)) => {
				if let Some(ref_id) = pending_ref {
					store.set_pending_ref(account.id, ref_id).await?;
				}
				// `as_user` because a public route's `Ctx` is `System` and `export_account`
				// selects on `account_id`. Ignored on failure: `register` must answer the
				// same way whether or not the address is already registered.
				mintworks_core::audit::log(
					&self.app.store,
					&ctx.clone().as_user(account.id),
					"account",
					Some(account.uid.as_str()),
					"REGISTERED",
					None,
				)
				.await;
				// Outside the transaction: holding the one writer connection for an email
				// stalls every other write. Non-fatal — a 500 would hide a created account.
				if let Err(e) = register::send_activation(&self.app, &account).await {
					tracing::error!(
						error = %e,
						account = account.uid.as_str(),
						"could not queue the activation mail; the account needs a resend"
					);
				}
			}
			Err(Error::Conflict(_)) => {
				// Timing parity only: the same UPDATE, matching no row (`-1`); never write here.
				if let Some(ref_id) = pending_ref {
					store.set_pending_ref(-1, ref_id).await?;
				}
				// Swallowed like the branch above, and for the reason that branch names: the
				// two must fail identically, or the status code answers the question this
				// route refuses to. The address is deliberately not logged.
				if let Err(e) =
					register::already_registered(&self.app, store.as_ref(), &email).await
				{
					tracing::error!(error = %e, "could not queue the already-registered notice");
				}
			}
			Err(e) => return Err(e),
		}
		Ok(())
	}

	/// Spend an activation token: `PENDING` -> `ACTIVE`, `activated_at` stamped, welcome mail
	/// queued, and a fresh token pair so the user lands signed in.
	///
	/// The ref registered with is judged and redeemed here (`activate::redeem`); emits
	/// `AccountActivated` and, for an `org_invite`, `MembershipAccepted`.
	pub async fn activate(
		&self,
		ctx: &Ctx,
		token: &str,
		password: Option<String>,
	) -> ClResult<Tokens> {
		let done = activate::redeem(&self.app, token, password).await?;
		let account = done.account;
		// The same gap `login_totp` had: activation establishes an identity and mints a
		// session, so it belongs in the subject's own history — `export_account`'s `auditLog`
		// dump selects on `account_id`, and a public route's `Ctx` is `System`.
		mintworks_core::audit::log(
			&self.app.store,
			&ctx.clone().as_user(account.id),
			"account",
			Some(account.uid.as_str()),
			"ACTIVATED",
			None,
		)
		.await;
		emit(
			&self.app,
			Event::AccountActivated {
				account: account.uid.clone(),
				org: done.personal,
				ref_uid: done.ref_uid,
			},
		);
		if let Some(org) = done.joined {
			emit(&self.app, Event::MembershipAccepted { org, account: account.uid.clone() });
		}
		self.issue_tokens(&account).await
	}

	/// `auth.registration`, or `closed` while the deprecated `AUTH_REGISTRATION_OPEN` is off
	/// and the setting was left at `open`.
	pub(crate) async fn registration_mode(app: &App) -> ClResult<String> {
		static WARNED: std::sync::Once = std::sync::Once::new();
		let mode = app.settings.text("auth.registration").await?;
		// ponytail: one release of the old variable; delete it with the next release.
		let old = std::env::var("AUTH_REGISTRATION_OPEN").unwrap_or_default();
		let old = old.trim().to_lowercase();
		if old.is_empty() {
			return Ok(mode);
		}
		WARNED.call_once(|| {
			tracing::warn!("AUTH_REGISTRATION_OPEN is deprecated; set AUTH_REGISTRATION instead");
		});
		if mode == "open" && matches!(old.as_str(), "0" | "false" | "no" | "off") {
			return Ok("closed".to_owned());
		}
		Ok(mode)
	}

	/// Verify a password and, unless a confirmed second factor intervenes, mint a session.
	///
	/// One bucket is charged here: `login.email`, the per-account half, **after** the
	/// proof-of-work gate. `login.ip` is charged by the route layer before the body is even
	/// parsed, and the `auth.failed` bucket the proof-of-work gate reads is charged by
	/// `auth_mw::optional_auth` on the 401 this method's failures become.
	pub async fn login(&self, ctx: &Ctx, req: &Credentials) -> ClResult<LoginOutcome> {
		let email = req.email.trim().to_lowercase();

		// **The PoW decision must not read per-account state**: off `accounts.failed_logins` it
		// was an enumeration oracle, and reading it at all restores one. The signal is the
		// caller's own address — `auth_mw::optional_auth` charges `AUTH_FAILED` on every 401.
		let mut pow_done = false;
		if self.auth_failures(ctx) >= self.app.settings.int("auth.pow_after_failures").await? {
			// `Error::Pow` is 400, the registered status for `E-CORE-POW`; the hand-rolled 403
			// here was the only place that code answered anything else.
			self.require_pow(ctx, "login", req.pow.as_ref()).await?;
			pow_done = true;
		}

		// **Counts but never denies**: a bucket keyed on a caller-supplied address is a lockout
		// dial the moment it can answer 429, so exhaustion escalates to proof-of-work instead.
		// After the gate, not before — charging first drained a victim's budget for free.
		if self.app.limits.check(&self.app.settings, "login.email", &email).await.is_err()
			&& !pow_done
		{
			self.require_pow(ctx, "login", req.pow.as_ref()).await?;
		}

		let store = self.store()?;
		let account = store.account_by_email(&email).await?;

		let Some(account) = account else {
			// Spend the same argon2id pass **and the same writer round-trip** as the real path;
			// the hash alone did not equalize them. `NO_ACCOUNT` matches no row.
			register::hash_password(req.password.clone()).await?;
			login::record_failure(store.as_ref(), login::NO_ACCOUNT).await?;
			return Err(login::bad_credentials());
		};

		if !login::verify_password(account.pwd_hash.clone(), req.password.clone()).await? {
			login::record_failure(store.as_ref(), account.id).await?;
			return Err(login::bad_credentials());
		}

		// After the verify and **paying the same side effect**: answering early confirmed a
		// correct password to an unauthenticated guesser. Only `SUSPENDED` reaches here —
		// `PENDING`/`ANONYMIZED` have `pwd_hash = NULL`.
		if login::ensure_usable(&account).is_err() {
			login::record_failure(store.as_ref(), account.id).await?;
			return Err(login::bad_credentials());
		}

		// A confirmed second factor stops here and hands out a ticket, never a token pair.
		if let Some(totp_token) = login::totp_ticket(&self.app, store.as_ref(), &account).await? {
			return Ok(LoginOutcome::TotpRequired { totp_token: Some(totp_token) });
		}

		store.record_login_success(account.id, Timestamp::now()).await?;

		// Successes only: `accounts.failed_logins` already counts failures, and auditing those
		// would swamp the table with what a credential-stuffing run generates. `as_user` because
		// a public route's `Ctx` is `System`, whose `account_id()` is NULL.
		mintworks_core::audit::log(
			&self.app.store,
			&ctx.clone().as_user(account.id),
			"account",
			Some(account.uid.as_str()),
			"LOGIN",
			None,
		)
		.await;
		Ok(LoginOutcome::Signed(Box::new(self.issue_tokens(&account).await?)))
	}

	/// A fresh access/refresh pair with `auth_at = now`, plus the login body.
	///
	/// `pub(crate)` and `Ctx`-free on purpose: it mints a session with immediate step-up
	/// authority over `DELETE /api/auth/totp` and `POST /api/account/delete` and verifies
	/// nothing, so **the caller must already have verified a credential** — `activate` and
	/// `login` have. It is not an authorized service method, and a consumer minting a session
	/// from an identity it established its own way wants one of those, with its own audit row.
	pub(crate) async fn issue_tokens(&self, account: &Account) -> ClResult<Tokens> {
		token::issue(&self.app, account, Some(Timestamp::now().0)).await
	}

	// ------------------------------------------------------------ orgs

	/// Every org the account belongs to, with its role in each.
	pub async fn list_orgs(&self, ctx: &Ctx) -> ClResult<Vec<OrgSummary>> {
		let account = self.actor_account(ctx).await?;
		Ok(self
			.store()?
			.orgs_for_account(account.id)
			.await?
			.into_iter()
			.map(|t| OrgSummary {
				uid: t.uid.into_string(),
				kind: t.kind,
				name: t.name,
				status: t.status,
				role: t.role,
			})
			.collect())
	}

	/// Enter `org_uid` and mint an **access** token scoped to it. The refresh token is
	/// untouched and `auth_at` is carried over, so switching can neither extend a session nor
	/// manufacture step-up.
	pub async fn switch_org(&self, ctx: &Ctx, org_uid: &str) -> ClResult<SwitchResponse> {
		let account = self.actor_account(ctx).await?;
		let store = self.store()?;
		let uid = OrgId::parse(org_uid)?;

		// Eligibility is the *effective* role — a membership on the org or on any ancestor of
		// it — read from the database, not from the list the client last saw. `NotFound`, not
		// `forbidden`: this route takes an arbitrary `org_` uid in its body, so a 403 would
		// confirm another org's row exists.
		let resolved = self.app.store.org_membership_role(account.id, uid.as_str()).await?;
		if resolved.is_none() {
			// The ancestor walk anchors on `status = 'ACTIVE'`, so a suspended org misses it
			// whatever the caller holds. Re-ask without that filter, or a member of a suspended
			// org gets `E-CORE-NOTFOUND` where they used to get `E-AUTH-SUSPENDED`.
			let full = store.org_by_uid(&uid).await?.ok_or(Error::NotFound)?;
			if full.status != OrgStatus::Active {
				if store.accepted_membership_role(full.id, account.id).await?.is_some() {
					return Err(Error::coded(
						StatusCode::FORBIDDEN,
						"E-AUTH-SUSPENDED",
						"this org is suspended",
					));
				}
				return Err(Error::NotFound);
			}
		}
		let Some((org_id, role)) = resolved else {
			return Err(Error::NotFound);
		};
		let full = store.org_by_id(org_id).await?.ok_or(Error::NotFound)?;

		let (access, _refresh) =
			token::mint_pair(&self.app, &account, Some((&full.uid, role)), ctx.auth_at).await?;
		// Names which org a later privileged row was performed in.
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"org",
			Some(full.uid.as_str()),
			"ORG_SWITCHED",
			None,
		)
		.await;
		Ok(SwitchResponse { access_token: access, expires_in: token::ACCESS_TTL_SECONDS })
	}

	/// The active org in full, with the caller's role in it.
	pub async fn org(&self, ctx: &Ctx) -> ClResult<OrgDetail> {
		let (_account, org, role) = self.active_of(ctx).await?;
		Ok(org_detail(&org, role))
	}

	/// Rename the active org or change its billing currency — org-admin.
	/// `billing_currency: Patch::Null` clears the column, which is how an org falls back to
	/// `settings['currency.base']`. `status` is operator-only and deliberately not patchable.
	pub async fn update_org(&self, ctx: &Ctx, patch: &OrgPatch) -> ClResult<OrgDetail> {
		let (_account, org, role) = self.admin_of(ctx).await?;
		let store = self.store()?;

		let name = match patch.name.as_deref().map(str::trim) {
			Some("") => return Err(Error::validation("an org needs a name")),
			Some(n) => {
				bounded("name", n, MAX_NAME_CHARS)?;
				Some(n)
			}
			other => other,
		};
		if let Patch::Value(code) = &patch.billing_currency
			&& !store.currency_enabled(code).await?
		{
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-INV-CURRENCY-DISABLED",
				"that billing currency is not enabled",
			));
		}
		// First, so a taken slug leaves the rest of the patch unapplied too.
		match &patch.slug {
			Patch::Value(raw) => {
				let slug = mintworks_core::refs::validate_slug(raw)?;
				store.set_org_slug(org.id, Some(&slug)).await?;
			}
			Patch::Null => store.set_org_slug(org.id, None).await?,
			Patch::Undefined => {}
		}
		store.update_org(org.id, name, patch.billing_currency.clone(), None).await?;

		let org = store.org_by_id(org.id).await?.ok_or_else(forbidden)?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"org",
			Some(org.uid.as_str()),
			"ORG_UPDATED",
			None,
		)
		.await;
		Ok(org_detail(&org, role))
	}

	/// The active org's members — org-admin.
	pub async fn members(&self, ctx: &Ctx) -> ClResult<Vec<MemberBody>> {
		let (_account, org, _role) = self.admin_of(ctx).await?;
		Ok(self
			.store()?
			.members(org.id, MAX_MEMBERS)
			.await?
			.into_iter()
			.map(|m| MemberBody {
				account_uid: Some(m.account_uid.into_string()),
				email: Some(m.email),
				name: m.name,
				role: m.role,
				status: Some(m.status),
				accepted: true,
				created_at: m.created_at,
			})
			.collect())
	}

	/// The caller's `OWNER` role on `org_uid`, read fresh from `memberships` — never from
	/// the token, and never from which router the request came through.
	///
	/// **Direct**, unlike [`Auth::admin_of`], which reads the effective role: the two routes
	/// this guards — transfer and delete — are irreversible, and inheriting would let a root
	/// `OWNER` move or delete any org in the deployment.
	///
	/// An org the caller has no membership on, including one that does not exist, is
	/// `E-CORE-NOTFOUND`: `403` would confirm another actor's organisation. A member who is
	/// simply not the owner gets the ordinary `E-AUTH-FORBIDDEN`, as `admin_of` gives.
	async fn owner_of(&self, ctx: &Ctx, org_uid: &str) -> ClResult<(Account, Org)> {
		let account = self.actor_account(ctx).await?;
		let store = self.store()?;
		let org = store.org_by_uid(&OrgId::parse(org_uid)?).await?.ok_or(Error::NotFound)?;
		let role = store
			.accepted_membership_role(org.id, account.id)
			.await?
			.ok_or(Error::NotFound)?;
		if role != Role::Owner {
			return Err(forbidden());
		}
		Ok((account, org))
	}

	/// Hand an organisation to another of its members — **owner-only**, and step-up, because it
	/// gives away every org-admin power the caller holds.
	///
	/// The caller stays on as `ADMIN`. The new owner must already be a member who has accepted
	/// their invitation: promoting a pending one would hand the organisation to somebody who has
	/// not agreed to have it.
	///
	/// Together with [`Auth::delete_org`] this is what makes `erase_account` reachable for an
	/// account that ever created an organisation — the error there names both.
	pub async fn transfer_ownership(
		&self,
		ctx: &Ctx,
		org_uid: &str,
		account_uid: &str,
	) -> ClResult<()> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let (caller, org) = self.owner_of(ctx, org_uid).await?;
		let store = self.store()?;
		// Same rule as `set_member_role`: the subject is an account uid, so one that does not
		// exist and one outside this org answer identically.
		let target = store
			.account_by_uid(&AccountId::parse(account_uid)?)
			.await?
			.ok_or(Error::NotFound)?;
		if target.id == caller.id {
			return Err(Error::conflict("this account already owns the organisation"));
		}
		if !store.transfer_org_ownership(org.id, caller.id, target.id).await? {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				"E-AUTH-TRANSFER-TARGET",
				"the new owner has to be a member who has accepted their invitation",
			));
		}
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			// The successor goes in `entity_id`, not `detail`: `detail` is exported verbatim
			// and another account's uid is not the subject's data. `entity_id` is dropped from
			// the export but kept in the database, so the successor stays auditable.
			"membership",
			Some(target.uid.as_str()),
			"ORG_OWNER_CHANGED",
			Some(json!({ "org": org.uid.as_str() })),
		)
		.await;
		Ok(())
	}

	/// Delete an organisation — **owner-only**, and step-up: it is irreversible.
	///
	/// Refused while any other member has accepted, or while the org still holds records the
	/// store must retain — invoices carry an eight-year statutory obligation, so an organisation
	/// that ever issued one is transferred rather than deleted.
	///
	/// The personal org is not deletable here: it goes with the account, through
	/// [`Auth::erase_account`]. Neither is the root org, which belongs to the deployment.
	pub async fn delete_org(&self, ctx: &Ctx, org_uid: &str) -> ClResult<()> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let (_caller, org) = self.owner_of(ctx, org_uid).await?;
		if org.kind == OrgKind::Personal {
			return Err(Error::conflict(
				"the personal org goes with the account; use POST /api/account/delete",
			));
		}
		// Its `owner_account_id` is `NULL`, so the store's member count cannot refuse it and the
		// `NOT IN ('PERSONAL','ROOT')` delete guard is the last word. A stated conflict, not
		// `E-AUTH-ORG-NOT-EMPTY`.
		if org.kind == OrgKind::Root {
			return Err(Error::conflict("the platform root org cannot be deleted"));
		}
		if !self.store()?.delete_org(org.id).await? {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				"E-AUTH-ORG-NOT-EMPTY",
				"the organisation still has members or records that have to be retained; \
				 remove the members, or transfer ownership instead",
			));
		}
		for hook in &self.app.account_data_hooks {
			if let Err(e) = hook.org_deleted(&org.uid).await {
				tracing::error!(hook = hook.name(), org = org.uid.as_str(), "org_deleted: {e}");
			}
		}
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"org",
			Some(org.uid.as_str()),
			"ORG_DELETED",
			None,
		)
		.await;
		Ok(())
	}

	/// Change a member's role — org-admin. `OWNER` is neither assignable nor removable
	/// here; [`Auth::transfer_ownership`] is the one route that moves it.
	pub async fn set_member_role(
		&self,
		ctx: &Ctx,
		account_uid: &str,
		role: Role,
	) -> ClResult<MemberBody> {
		let (_caller, org, _role) = self.admin_of(ctx).await?;
		if role == Role::Owner {
			return Err(Error::conflict("OWNER cannot be assigned through this route"));
		}
		let store = self.store()?;
		// The *subject* is an account uid, so absent and in-another-org answer identically as
		// `E-CORE-NOTFOUND`; a `403` would confirm the account.
		let target = store
			.account_by_uid(&AccountId::parse(account_uid)?)
			.await?
			.ok_or(Error::NotFound)?;
		let current = store.membership_role(org.id, target.id).await?.ok_or(Error::NotFound)?;
		if current == Role::Owner {
			return Err(Error::conflict("the owner's role cannot be changed"));
		}
		// A membership nobody has accepted discloses nothing about the invitee: otherwise
		// re-roling an invitation is an oracle.
		let accepted = store.accepted_membership_role(org.id, target.id).await?.is_some();

		if !store.put_membership(org.id, target.id, role).await? {
			return Err(Error::conflict("the owner's role cannot be changed"));
		}
		// Read back, not `Timestamp::now()`: the route used to report now for a membership
		// that may be years old.
		let created_at =
			store.membership_created_at(org.id, target.id).await?.ok_or(Error::NotFound)?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"membership",
			Some(target.uid.as_str()),
			"MEMBER_ROLE_CHANGED",
			Some(json!({ "org": org.uid.as_str(), "role": role.as_str() })),
		)
		.await;
		Ok(MemberBody {
			account_uid: accepted.then_some(target.uid).map(AccountId::into_string),
			email: accepted.then_some(target.email),
			name: accepted.then_some(target.name).flatten(),
			role,
			status: accepted.then_some(target.status),
			accepted,
			created_at,
		})
	}

	/// Remove a member — org-admin. Removing the `OWNER` is a conflict.
	///
	/// The removed member's access token stays valid until it expires, but every privileged
	/// route re-reads `memberships` and every ordinary read is scoped by `org_id`, so the
	/// window is bounded to reads they could already perform.
	pub async fn remove_member(&self, ctx: &Ctx, account_uid: &str) -> ClResult<()> {
		let (_caller, org, _role) = self.admin_of(ctx).await?;
		let store = self.store()?;
		// Same rule as `set_member_role`: an account outside this org is not found, not
		// forbidden.
		let target = store
			.account_by_uid(&AccountId::parse(account_uid)?)
			.await?
			.ok_or(Error::NotFound)?;
		let current = store.membership_role(org.id, target.id).await?.ok_or(Error::NotFound)?;
		if current == Role::Owner {
			return Err(Error::conflict("the owner's membership cannot be removed"));
		}

		if !store.remove_membership(org.id, target.id).await? {
			return Err(Error::conflict("the owner's membership cannot be removed"));
		}
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"membership",
			Some(target.uid.as_str()),
			"MEMBER_REMOVED",
			Some(json!({ "org": org.uid.as_str() })),
		)
		.await;
		Ok(())
	}

	/// Remove a membership by address — org-admin; the mirror of [`Auth::add_member`] for a
	/// caller holding the email, not the uid. A pending invitation is an `org_invite` ref,
	/// revoked through `DELETE /api/refs/{uid}`.
	///
	/// Answers `Ok(())` whether or not a membership existed, for `add_member`'s reason: any
	/// org admin may post any address, so a distinguishable answer is an existence oracle.
	/// Removing the `OWNER` is still `E-CORE-CONFLICT`.
	pub async fn remove_member_by_email(&self, ctx: &Ctx, email: &str) -> ClResult<()> {
		let (_caller, org, _role) = self.admin_of(ctx).await?;
		let email = email.trim().to_lowercase();
		register::validate(&email)?;
		let store = self.store()?;
		let Some(target) = store.account_by_email(&email).await? else {
			return Ok(());
		};
		match store.membership_role(org.id, target.id).await? {
			Some(Role::Owner) => {
				return Err(Error::conflict("the owner's membership cannot be removed"));
			}
			None => return Ok(()),
			Some(_) => {}
		}
		// `false` is not an error here: it means the row vanished concurrently, which is the
		// outcome the caller asked for.
		store.remove_membership(org.id, target.id).await?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"membership",
			Some(target.uid.as_str()),
			"MEMBER_REMOVED",
			Some(json!({ "org": org.uid.as_str() })),
		)
		.await;
		Ok(())
	}

	/// Create an organisation with the caller as its `OWNER`.
	pub async fn create_org(
		&self,
		ctx: &Ctx,
		name: &str,
		billing_currency: Option<&CurrencyCode>,
	) -> ClResult<Org> {
		let account = self.actor_account(ctx).await?;
		let name = name.trim();
		if name.is_empty() {
			return Err(Error::validation("an org needs a name"));
		}
		bounded("name", name, MAX_NAME_CHARS)?;
		let store = self.store()?;
		// Before `create_org`, which commits the org row *and* its OWNER membership:
		// checking after left a `400` beside an org with no route to delete it.
		if let Some(code) = billing_currency
			&& !store.currency_enabled(code).await?
		{
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-INV-CURRENCY-DISABLED",
				"that currency is not enabled",
			));
		}
		// A `PERSONAL` org is one person's scope: parenting a shared org under it hands its owner a
		// permanent inherited OWNER that `transfer_ownership` cannot take back. Only a `SHARED` or
		// `ROOT` org is a parent, and only for a caller who already administers it.
		let parent_id = match ctx.org_id {
			Some(id) => {
				let active = store.org_by_id(id).await?.ok_or(Error::NotFound)?;
				match active.kind {
					OrgKind::Personal => self.app.store.root_org_id().await?,
					OrgKind::Root | OrgKind::Shared => {
						mintworks_core::auth_mw::require_role_on(&self.app, ctx, id, Role::Admin)
							.await?;
						id
					}
				}
			}
			None => self.app.store.root_org_id().await?,
		};
		let org = store
			.create_org(OrgKind::Shared, parent_id, name, account.id, billing_currency)
			.await?;

		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"org",
			Some(org.uid.as_str()),
			"ORG_CREATED",
			Some(json!({ "name": org.name })),
		)
		.await;
		Ok(org)
	}

	/// Invite `email` into `ctx.org()` at `role`: mints an `org_invite` ref (`uses_left = 1`,
	/// `auth.invite_ttl_days`) and mails its code. **No account is read or created**, so the
	/// answer and its cost are the same whether or not the address is registered. A new
	/// address registers with the code; a registered one calls [`Auth::accept_invite`].
	pub async fn add_member(&self, ctx: &Ctx, email: &str, role: Role) -> ClResult<()> {
		// The `invite` bucket (30/h/ip) in [`crate::routes`] is spent *before* this authorization
		// check, so a non-admin hammering the route still burns it — which is why it is not the
		// 3/h/ip `register` bucket self-service registration uses.
		let (admin, org, _role) = self.admin_of(ctx).await?;
		// A personal org has exactly one membership, and `export_account`/`anonymize_account`
		// scope by `kind = 'P'` on that basis: a second member would land in the owner's GDPR
		// export and erasure. `set_member_role`/`remove_member` close the same invariant.
		if org.kind == OrgKind::Personal {
			return Err(Error::conflict("a personal org cannot have other members"));
		}
		if role == Role::Owner {
			return Err(Error::conflict("OWNER cannot be assigned through this route"));
		}
		let email = email.trim().to_lowercase();
		// The same validation `register` runs, not a `contains('@')` of its own: `accounts.email`
		// has no CHECK, so a header-injecting address stored fine and only failed at
		// `Mailbox::parse` inside the `SEND_EMAIL` job, long after the admin saw its `204`.
		register::validate(&email)?;
		let ttl = self.app.settings.int("auth.invite_ttl_days").await?;
		let req = CreateRef {
			ref_type: "org_invite".to_owned(),
			target: Some(org.uid.to_string()),
			email: Some(email.clone()),
			params: Some(json!({ "role": role.as_str() })),
			uses_left: Some(1),
			expires_at: Some(Timestamp(Timestamp::now().0 + ttl * 86_400)),
			..CreateRef::default()
		};
		let r = self.mint_gated(ctx, &req).await?;
		let mail = SendEmail {
			to: email,
			template: "org_invite".to_owned(),
			// The inviter's: the invitee has no stated preference yet.
			lang: admin.locale.clone(),
			vars: json!({
				"inviter": register::display_name(&admin),
				"org_name": org.name,
				"link": format!(
					"{}/invite/{}",
					self.app.config.base_url.trim_end_matches('/'),
					r.code
				),
			}),
		};
		// Logged, not propagated: a 500 would hide a minted invitation, which the admin can
		// see in `GET /api/org/invites` and revoke.
		if let Err(e) = mintworks_email::job::enqueue(&self.app.store, &mail).await {
			tracing::error!(error = %e, r#ref = r.uid.as_str(), "could not queue the invitation mail");
		}
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"ref",
			Some(r.uid.as_str()),
			"MEMBER_INVITED",
			Some(json!({ "org": org.uid.as_str(), "role": role.as_str() })),
		)
		.await;
		Ok(())
	}

	/// `POST /api/org/invites/{code}/accept`: an existing account redeems an `org_invite`
	/// addressed to it and becomes an accepted member. `E-AUTH-INVITE-EMAIL` (403) for someone
	/// else's invitation, `E-AUTH-INVITE-EXPIRED` (410) for a spent one.
	pub async fn accept_invite(&self, ctx: &Ctx, code: &str) -> ClResult<OrgId> {
		let account = self.actor_account(ctx).await?;
		let store = self.store()?;
		let refs = Refs::from_app(&self.app)?;
		let r = refs
			.by_code(code.trim())
			.await?
			.filter(|r| r.ref_type == "org_invite")
			.ok_or(Error::NotFound)?;
		if r.email.as_deref().is_some_and(|e| e != account.email) {
			return Err(Error::coded(
				StatusCode::FORBIDDEN,
				"E-AUTH-INVITE-EMAIL",
				"this invitation is addressed to another account",
			));
		}
		// Before the redeem, so the owner's refused join does not spend the invite.
		if store.membership_role(r.org_id, account.id).await? == Some(Role::Owner) {
			return Err(Error::conflict("the owner's role cannot be changed"));
		}
		let (personal_id, _) = activate::personal_org(store.as_ref(), account.id).await?;
		// A repeat use comes back as `(first, false)`: only a fresh use admits.
		if !matches!(refs.redeem(ctx, r.id, account.id, personal_id).await?, Some((_, true))) {
			return Err(Error::coded(
				StatusCode::GONE,
				"E-AUTH-INVITE-EXPIRED",
				"this invitation has expired",
			));
		}
		let org = activate::join(store.as_ref(), &r, &account).await?.ok_or(Error::NotFound)?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"membership",
			Some(org.as_str()),
			"MEMBER_ACCEPTED",
			Some(json!({ "ref": r.uid.as_str() })),
		)
		.await;
		emit(&self.app, Event::MembershipAccepted { org: org.clone(), account: account.uid });
		Ok(org)
	}

	/// The active org's `org_invite` refs — org-admin. Revoked through `DELETE /api/refs/{uid}`.
	pub async fn invites(&self, ctx: &Ctx) -> ClResult<Vec<Ref>> {
		Refs::from_app(&self.app)?.list(ctx, Some("org_invite")).await
	}

	/// `POST /api/auth/signup-refs`: a `signup` ref in `ctx.org()`, for whoever
	/// `auth.invite_by` names (operator, org admin, or any member).
	pub async fn create_signup_ref(&self, ctx: &Ctx, req: &CreateRef) -> ClResult<Ref> {
		match self.app.settings.text("auth.invite_by").await?.as_str() {
			"operator" => require_operator(&self.app, ctx).await?,
			"member" => require_role(&self.app, ctx, Role::Member).await?,
			_ => require_role(&self.app, ctx, Role::Admin).await?,
		}
		// Everyone owns a personal org: under `invite`, minting there would admit anyone's friends.
		if Self::registration_mode(&self.app).await? == "invite"
			&& !self.app.settings.flag("auth.invite_personal").await?
			&& self
				.store()?
				.org_by_id(ctx.org()?)
				.await?
				.is_some_and(|o| o.kind == OrgKind::Personal)
		{
			return Err(Error::coded(
				StatusCode::FORBIDDEN,
				"E-AUTH-FORBIDDEN",
				"insufficient role",
			));
		}
		let req = CreateRef { ref_type: "signup".to_owned(), ..req.clone() };
		self.mint_gated(ctx, &req).await
	}

	/// [`Refs::mint`] between the two [`InviteGate`](crate::InviteGate) calls. The caller has
	/// authorized.
	async fn mint_gated(&self, ctx: &Ctx, req: &CreateRef) -> ClResult<Ref> {
		let gate = crate::invite_gate::of(&self.app);
		gate.may_invite(&self.app, ctx, &req.ref_type).await?;
		let r = Refs::from_app(&self.app)?.mint(ctx, req).await?;
		if let Err(e) = gate.invited(&self.app, ctx, &r.uid).await {
			tracing::error!(error = %e, r#ref = r.uid.as_str(), "InviteGate::invited failed");
		}
		Ok(r)
	}

	// ------------------------------------------------------------ session

	/// Finish a login that stopped at the second factor: the ticket plus either a TOTP code
	/// or a recovery code.
	///
	/// `login.totp.account` is charged here, keyed on the account: the ticket already binds the
	/// request to one, so rotating source addresses cannot sidestep it — where the route
	/// layer's `login.totp` keys on an address masked to /64, and one routed /48 yields 65 536
	/// of those buckets against a six-digit code good for the ticket's whole 300 s.
	///
	/// Its own scope, not the password half's `login.email`: sharing that caller-supplied-key
	/// bucket let an attacker draining it at the password stage lock a 2FA user out here. This
	/// one is only reachable with a valid ticket, so unlike `login.email` it can hard-deny.
	pub async fn login_totp(
		&self,
		ctx: &Ctx,
		ticket: &str,
		code: Option<&str>,
		recovery_code: Option<&str>,
	) -> ClResult<Tokens> {
		let store = self.store()?;
		let account =
			login::open_ticket(&self.app, store.as_ref(), ticket, login::TICKET_PURPOSE).await?;
		self.app
			.limits
			.check(&self.app.settings, "login.totp.account", account.uid.as_str())
			.await?;

		let outcome = match (code, recovery_code) {
			(Some(code), _) => totp::check_code(&self.app, store.as_ref(), &account, code).await,
			(None, Some(recovery)) => {
				totp::spend_recovery(store.as_ref(), &account, recovery).await
			}
			(None, None) => Err(totp::bad_code()),
		};
		if let Err(e) = outcome {
			login::record_failure(store.as_ref(), account.id).await?;
			return Err(e);
		}

		store.record_login_success(account.id, Timestamp::now()).await?;
		// This path wrote nothing, so the accounts with the *strongest* authentication had no
		// login history at all. A distinct action keeps the two paths separable in the log;
		// `as_user` for the same reason as the password path.
		mintworks_core::audit::log(
			&self.app.store,
			&ctx.clone().as_user(account.id),
			"account",
			Some(account.uid.as_str()),
			"LOGIN_TOTP",
			None,
		)
		.await;
		token::issue(&self.app, &account, Some(Timestamp::now().0)).await
	}

	/// Spend a refresh token for a fresh pair. Sliding: a new pair every time, `auth_at`
	/// carried over unchanged so refreshing can never manufacture step-up.
	///
	/// The org is carried over too, for the reason `step_up` states below. It comes
	/// off the spent token's own `org` claim rather than `ctx` — this route is reached with an
	/// expired access token, so there is no `ctx.org_id` to read — and `issue_in`
	/// re-resolves it against a live membership, so a revoked one drops the caller to no
	/// org instead of silently switching them to the default.
	///
	// Known gap: `switch_org`/`step_up` discard the refresh token they mint, so a refresh
	// after a switch reverts to the org active at *login*. Bounded by the 15-minute access
	// TTL. Closing it means rotating on switch, which does extend the session.
	pub async fn refresh(&self, _ctx: &Ctx, refresh_token: &str) -> ClResult<Tokens> {
		let claims = login::open_refresh(&self.app, refresh_token).await?;
		let store = self.store()?;
		let account = login::account_from_claims(store.as_ref(), &claims).await?;
		token::issue_in(&self.app, &account, claims.auth_at, claims.org.as_deref()).await
	}

	/// Re-present a credential for a new access token with `auth_at = now`. The refresh token
	/// is untouched, so this cannot extend a session.
	///
	/// The active org is carried through unchanged, never re-picked: stepping up would
	/// otherwise hand someone working in organisation B a token scoped to their personal
	/// org, and the destructive operation step-up was gating would run against the wrong
	/// one.
	pub async fn step_up(
		&self,
		ctx: &Ctx,
		password: Option<String>,
		code: Option<&str>,
		passkey: Option<webauthn::StepUpProof>,
	) -> ClResult<StepUpResponse> {
		let account = self.actor_account(ctx).await?;
		let store = self.store()?;

		// The `step_up` budget rides on the route layer in [`crate::routes`], keyed on the
		// account — this route is authenticated, so unlike `login` the key is readable from a
		// layer and no second charge is needed here.
		let outcome = match (password, passkey) {
			// **Every factor `login` would demand, not just the first presented.** Ignoring the
			// code when a password was there granted strictly more than `login` on identical
			// credentials. `bad_credentials` either way, so this is no second-factor oracle.
			(Some(password), None) => {
				login::verify_all_factors(&self.app, store.as_ref(), &account, password, code).await
			}
			// A passkey assertion is a whole factor of its own and must belong to the calling
			// account, so it stands in for both members rather than adding to them.
			(None, Some(proof)) => self.step_up_passkey(&account, proof).await,
			// A password *and* an assertion, a code with neither, or neither: one factor fewer than
			// `login` demands, folded into the same answer. None of these counts as a failed
			// attempt — nothing was checked — though the bucket token is spent.
			_ => return Err(login::bad_credentials()),
		};
		// Both outcomes, unlike `login`, which audits successes only: this mints `auth_at = now`
		// and so authorizes the destructive routes. One account's budget, not login volume.
		if let Err(e) = outcome {
			login::record_failure(store.as_ref(), account.id).await?;
			mintworks_core::audit::detached(
				&self.app.store,
				ctx,
				"account",
				Some(account.uid.as_str()),
				"STEP_UP_FAILED",
				None,
			)
			.await;
			return Err(e);
		}

		let active = self.active_membership(ctx).await?;
		let (access, _refresh) = token::mint_pair(
			&self.app,
			&account,
			active.as_ref().map(|(uid, role)| (uid, *role)),
			Some(Timestamp::now().0),
		)
		.await?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(account.uid.as_str()),
			"STEP_UP",
			None,
		)
		.await;
		Ok(StepUpResponse { access_token: access, expires_in: token::ACCESS_TTL_SECONDS })
	}

	/// Who the caller is, what orgs they belong to, and what consents are outstanding —
	/// the login body minus the tokens.
	pub async fn me(&self, ctx: &Ctx) -> ClResult<token::LoginBody> {
		let account = self.actor_account(ctx).await?;
		let store = self.store()?;
		let orgs = store.orgs_for_account(account.id).await?;
		// `ctx.org_id`, not `token::pick_org`'s default: after `switch_org` the
		// caller is working in a different org, and `/me` reporting the default would name
		// one they are not in.
		let org = match self.active_membership(ctx).await? {
			Some((_, role)) => {
				let full = store.org_by_id(ctx.org()?).await?.ok_or_else(forbidden)?;
				Some(token::OrgBody {
					uid: full.uid.into_string(),
					name: full.name,
					kind: full.kind,
					role,
					billing_currency: full.billing_currency,
				})
			}
			None => None,
		};
		Ok(token::LoginBody {
			access_token: None,
			refresh_token: None,
			expires_in: None,
			account: token::account_body(&account),
			org,
			orgs: token::summaries(orgs),
			consents_required: token::consents_required(&self.app, store.as_ref(), &account)
				.await?,
		})
	}

	// ------------------------------------------------------------ passwords

	/// Mail a password-reset link, if the address belongs to a usable account. Answers the
	/// same way whatever the address, so it cannot be used to find out which are registered.
	pub async fn request_password_reset(
		&self,
		ctx: &Ctx,
		email: &str,
		proof: Option<&pow::Proof>,
	) -> ClResult<()> {
		// The proof of work comes first: everything below — the account lookup and the mail, or
		// the compensating writer round-trip — is work an unauthenticated caller naming any
		// address would otherwise get for free.
		self.require_pow(ctx, "password-reset", proof).await?;
		let email = email.trim().to_lowercase();
		// The `password_reset` bucket is charged by the route layer, keyed on IP rather than the
		// submitted address — which is why it may deny outright: no attacker can park on a
		// victim's address and close their way out.
		let store = self.store()?;
		if let Some(account) = store.account_by_email(&email).await?
			// A suspended or anonymized account gets no mail, and no different answer.
			&& login::ensure_usable(&account).is_ok()
		{
			// Swallowed, as `register` does: a `?` here makes a jobs-insert failure a 5xx on a
			// registered address and `204` on an unknown one, which answers what this route
			// refuses to. The address is deliberately not logged.
			if let Err(e) = reset::send_link(&self.app, &account).await {
				tracing::error!(
					error = %e,
					account = account.uid.as_str(),
					"could not queue the password-reset mail"
				);
			}
		} else {
			// The same writer round-trip the hit branch pays, on the same single connection, or
			// the two branches are distinguishable by latency — `NO_ACCOUNT` is 0, so the
			// UPDATE matches no row. The unusable-account branch lands here for the same reason.
			login::record_failure(store.as_ref(), login::NO_ACCOUNT).await?;
		}
		Ok(())
	}

	/// Spend a mailed reset token. Bumps `token_epoch`, signing out every live token, then
	/// signs the caller straight in — unless a confirmed second factor intervenes.
	pub async fn reset_password(
		&self,
		ctx: &Ctx,
		token: &str,
		code: Option<&str>,
		recovery_code: Option<&str>,
		new_password: String,
	) -> ClResult<LoginOutcome> {
		let store = self.store()?;
		let account = reset::open(&self.app, store.as_ref(), token).await?;
		login::ensure_usable(&account)?;
		register::validate_password(&new_password)?;

		// **Before the write, not after.** A mailed token is one factor; on a confirmed-TOTP
		// account the password must not change until the second is in hand, or a mailbox
		// compromise bumps `token_epoch` and locks the real owner out with no route back.
		if login::has_confirmed_totp(store.as_ref(), &account).await? {
			// A printed recovery code stands in for the authenticator, as in `login_totp`:
			// without it, losing the device *and* the password left no path at all. Terminal
			// once all 8 are spent — regenerating codes is a separate, step-up'd route.
			let second_factor = match (code, recovery_code) {
				(Some(code), _) => Factor::Totp(code),
				(None, Some(recovery)) => Factor::Recovery(recovery),
				(None, None) => {
					// No ticket: the caller re-submits the *mailed* token plus `code` or
					// `recoveryCode`, so there is nothing here for them to redeem.
					return Ok(LoginOutcome::TotpRequired { totp_token: None });
				}
			};
			// The mailed token rations nothing — the same 2-hour link re-opens for every guess,
			// so unbounded 6-digit tries would defeat this factor. Counts, never locks out: a
			// lockout here once refused the correct new password. Its own scope, because
			// sharing `login.totp.account` let a password-holder block the victim's reset.
			self.app
				.limits
				.check(&self.app.settings, "reset.totp.account", account.uid.as_str())
				.await?;
			let checked = match second_factor {
				Factor::Totp(code) => {
					totp::check_code(&self.app, store.as_ref(), &account, code).await
				}
				Factor::Recovery(recovery) => {
					totp::spend_recovery(store.as_ref(), &account, recovery).await
				}
			};
			if let Err(e) = checked {
				login::record_failure(store.as_ref(), account.id).await?;
				return Err(e);
			}
		}

		let hash = register::hash_password(new_password).await?;
		// Conditional on the epoch `reset::open` verified against, which is what makes the mailed
		// token single-use: both redemptions of a concurrent pair get past `open` on the same
		// snapshot. The loser sees the same "no longer valid" as any spent token.
		if !store.set_password(account.id, account.token_epoch, &hash).await? {
			return Err(activate::bad_token());
		}
		// `set_password` moves `pwd_hash` and `token_epoch` only, so clearing `failed_logins` and
		// stamping `last_login_at` is what makes this the sign-in it is.
		store.record_login_success(account.id, Timestamp::now()).await?;

		// The stored epoch just moved, so the row read above would mint a token that fails
		// verification. Re-read rather than patching the copy.
		let fresh = store.account_by_id(account.id).await?.ok_or_else(activate::bad_token)?;

		// `as_user` because a public route's `Ctx` is `System`, and an audit row with a NULL
		// `account_id` drops out of the subject's own GDPR export.
		mintworks_core::audit::log(
			&self.app.store,
			&ctx.clone().as_user(fresh.id),
			"account",
			Some(fresh.uid.as_str()),
			"PASSWORD_RESET",
			None,
		)
		.await;

		// The ticket on the other branch is deliberately **not** single-use: consuming it needs
		// a `token_epoch` bump or the session table this design does not have. The replay
		// protection that matters is the code's — `advance_totp_step` is monotonic.
		Ok(LoginOutcome::Signed(Box::new(
			token::issue(&self.app, &fresh, Some(Timestamp::now().0)).await?,
		)))
	}

	/// Change the password, presenting the current one — which *is* the step-up, so this
	/// needs no separate one.
	pub async fn change_password(
		&self,
		ctx: &Ctx,
		current: String,
		code: Option<&str>,
		new_password: String,
	) -> ClResult<Tokens> {
		let account = self.actor_account(ctx).await?;
		let store = self.store()?;
		// The same gate `step_up` applies: this mints `auth_at = now`, and verifying the password
		// alone granted what `step_up` refuses. One shared function keeps the two from drifting.
		if let Err(e) =
			login::verify_all_factors(&self.app, store.as_ref(), &account, current, code).await
		{
			login::record_failure(store.as_ref(), account.id).await?;
			return Err(e);
		}
		register::validate_password(&new_password)?;

		let hash = register::hash_password(new_password).await?;
		// The account was read at the top of this method and the epoch has not moved since;
		// a concurrent change losing the CAS is the same "your credential just moved under
		// you" as a stale session, so it re-authenticates.
		if !store.set_password(account.id, account.token_epoch, &hash).await? {
			return Err(activate::bad_token());
		}
		let fresh = store.account_by_id(account.id).await?.ok_or_else(activate::bad_token)?;

		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(fresh.uid.as_str()),
			"PASSWORD_CHANGED",
			None,
		)
		.await;
		// `issue`, not `issue_in`: `set_password` bumps `token_epoch` and invalidates every token
		// held, so this is a new session, not a continuation. `reset_password` matches.
		token::issue(&self.app, &fresh, Some(Timestamp::now().0)).await
	}

	/// Re-send the activation mail, if the address belongs to a `PENDING` account. Answers
	/// the same way whatever the address, so it cannot be used to enumerate accounts.
	pub async fn resend_activation(
		&self,
		ctx: &Ctx,
		email: &str,
		proof: Option<&pow::Proof>,
	) -> ClResult<()> {
		// Shares the `register` bucket with self-service registration, in [`crate::routes`]:
		// both are the same 3/h/ip budget for creating an account.
		self.require_pow(ctx, "resend-activation", proof).await?;

		let email = email.trim().to_lowercase();
		let store = self.store()?;
		if let Some(account) = store.account_by_email(&email).await?
			&& account.status == AccountStatus::Pending
		{
			// Swallowed for the same reason the branches below and in `register` are: a 5xx on
			// the hit branch alone is the enumeration answer. The address is not logged.
			if let Err(e) = register::send_activation(&self.app, &account).await {
				tracing::error!(
					error = %e,
					account = account.uid.as_str(),
					"could not queue the activation mail"
				);
			}
		} else {
			// Same reason as `request_password_reset`: the miss branch has to pay for the same
			// writer round-trip, or the latency answers what the response body will not.
			login::record_failure(store.as_ref(), login::NO_ACCOUNT).await?;
		}
		Ok(())
	}

	// ------------------------------------------------------------ second factor

	/// Begin TOTP enrolment. An unconfirmed credential is replaced; a confirmed one has to be
	/// deleted first.
	///
	/// Step-up, like [`Auth::remove_totp`]: an attacker holding a 15-minute stolen access
	/// token on an account with no TOTP could otherwise enrol their own authenticator, and
	/// every future login by the owner — who still knows the password — would be gated on the
	/// attacker's device. A transient token becoming a persistent lockout is the worse half.
	pub async fn enrol_totp(&self, ctx: &Ctx) -> ClResult<totp::Enrolment> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let account = self.actor_account(ctx).await?;
		totp::begin_enrolment(&self.app, self.store()?.as_ref(), &account).await
	}

	/// Confirm enrolment and hand out the recovery codes, which are shown once and stored
	/// only as argon2id hashes. Step-up, for the reason [`Auth::enrol_totp`] gives:
	/// confirming is the half that actually arms the factor.
	pub async fn confirm_totp(&self, ctx: &Ctx, code: &str) -> ClResult<totp::RecoveryCodes> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let account = self.actor_account(ctx).await?;
		let codes = totp::confirm(&self.app, self.store()?.as_ref(), &account, code).await?;

		// Never the secret or the codes themselves — only that the factor now exists.
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(account.uid.as_str()),
			"TOTP_ENROLLED",
			None,
		)
		.await;
		Ok(codes)
	}

	/// Remove the second factor — step-up required, because this is the exact move an
	/// attacker holding a stolen access token would make.
	pub async fn remove_totp(&self, ctx: &Ctx) -> ClResult<()> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let account = self.actor_account(ctx).await?;
		self.store()?.delete_totp(account.id).await?;

		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(account.uid.as_str()),
			"TOTP_REMOVED",
			None,
		)
		.await;
		Ok(())
	}

	// ------------------------------------------------------------ revocation

	/// Move an account's `status`. Operator-only, step-up.
	///
	/// `SUSPENDED` also bumps `token_epoch`, inside
	/// [`crate::store::AuthStore::set_account_status`]: `auth_mw` re-reads the epoch on
	/// privileged paths, but an ordinary read carries on until `exp` otherwise, and suspending
	/// an account is exactly the moment that window is not acceptable. Without this method nothing outside
	/// the tests could ever *reach* `SUSPENDED`, while `login` and `auth_mw` branched on it
	/// throughout. `ANONYMIZED` is refused here: it is erasure's outcome, not a status move.
	pub async fn set_account_status(
		&self,
		ctx: &Ctx,
		account_uid: &str,
		status: AccountStatus,
	) -> ClResult<()> {
		// Read before the transition guards, not after: the `PENDING -> ACTIVE` check below
		// needs the current status, and this is also what answers `E-CORE-NOTFOUND` for an
		// account the caller may not see, before any of them say anything about it.
		let target = self.operator_target(ctx, account_uid).await?;
		// Reachable here, this lever flipped the flag alone — irreversibly, with every personal
		// field intact, no owned-organisation guard and no GDPR receipt. `erase_account` owns it.
		if status == AccountStatus::Anonymized {
			return Err(Error::validation(
				"ANONYMIZED is set by account erasure, not by this route",
			));
		}
		// `activate` refuses a token for an account that already has a `pwd_hash` and `login`
		// refuses a non-ACTIVE one, so `ACTIVE -> PENDING` stranded the account with no route
		// back but another operator call.
		if status == AccountStatus::Pending {
			return Err(Error::validation("PENDING is set by registration, not by this route"));
		}
		// Keyed on `activated_at`, not `target.status == PENDING`: that tested one hop, and
		// `PENDING -> SUSPENDED -> ACTIVE` walked around it into a verified address with a NULL
		// `pwd_hash` and a dead activation token.
		if status == AccountStatus::Active && target.activated_at.is_none() {
			return Err(Error::validation(
				"a PENDING account is activated by its mail link, not by this route",
			));
		}
		// Keyed on `activated_at`, like the guard above: from SUSPENDED, PENDING and ACTIVE are
		// both refused and `activate::payload` signs the status, so the mailed link is dead too —
		// a never-activated account suspended here had no route back but SQL.
		if status == AccountStatus::Suspended && target.activated_at.is_none() {
			return Err(Error::validation(
				"a PENDING account cannot be suspended; it has no route back",
			));
		}
		let store = self.store()?;
		store.set_account_status(target.id, status).await?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(target.uid.as_str()),
			"STATUS",
			Some(json!({ "status": status.as_str() })),
		)
		.await;
		Ok(())
	}

	/// Invalidate every access and refresh token an account holds, leaving it usable: the
	/// password still works and the next login mints a token at the new epoch. Operator-only,
	/// step-up.
	pub async fn revoke_tokens(&self, ctx: &Ctx, account_uid: &str) -> ClResult<()> {
		let target = self.operator_target(ctx, account_uid).await?;
		self.store()?.bump_token_epoch(target.id).await?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(target.uid.as_str()),
			"REVOKE",
			None,
		)
		.await;
		Ok(())
	}

	/// The shared gate of the two methods above: operator, step-up, then the subject. An
	/// unknown uid is `E-CORE-NOTFOUND`, never `403` — the same rule `set_member_role` follows.
	async fn operator_target(&self, ctx: &Ctx, account_uid: &str) -> ClResult<Account> {
		mintworks_core::auth_mw::require_operator(&self.app, ctx).await?;
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		self.store()?
			.account_by_uid(&AccountId::parse(account_uid)?)
			.await?
			.ok_or(Error::NotFound)
	}

	// ------------------------------------------------------------ consent and GDPR

	/// Publish a legal document version. Operator-only **and step-up**: a new version makes
	/// `consents_required` non-empty for every account at once, so it gates the whole product
	/// behind the consent screen — not something a stolen access token gets to do.
	/// (`Actor::System` is exempt from step-up, so a consumer seeding the first TOS from its
	/// own boot code is unaffected.) This is the text every account's consent is evidence
	/// against.
	///
	/// **The framework does not seed one.** `register` refuses every account until a current
	/// `TOS` and `PRIVACY` exist (`register::check_consents`), so a fresh deployment must call
	/// this before anybody can sign up — what to publish, in which locales, and from when is
	/// the consumer application's decision, not the framework's, which is why this is a handle
	/// method with no route behind it.
	///
	/// `sha256` is computed here, over `body`, so a caller cannot record a hash that does not
	/// match the text it publishes. Re-publishing an existing `(kind, locale, version)` is a
	/// `409` — a shipped version is evidence and is never rewritten in place; supersede it
	/// with a new `version`.
	pub async fn publish_legal_document(
		&self,
		ctx: &Ctx,
		new: consent::PublishLegalDoc,
	) -> ClResult<consent::LegalDocSummary> {
		mintworks_core::auth_mw::require_operator(&self.app, ctx).await?;
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;

		// The locale is interpolated into an email-template path elsewhere and `legal_docs`
		// has no CHECK on it, so it goes through the same gate `register` uses.
		if !register::is_locale(&new.locale) {
			return Err(Error::validation("unsupported locale"));
		}
		bounded("version", &new.version, 40)?;
		bounded("title", &new.title, 200)?;
		bounded_multiline("body", &new.body, 1_000_000)?;

		let sha256 = hex::encode(Sha256::digest(new.body.as_bytes()));
		let store = self.store()?;
		store
			.insert_legal_doc(&NewLegalDoc {
				kind: new.kind,
				locale: new.locale.clone(),
				version: new.version.clone(),
				title: new.title,
				body: new.body,
				sha256: sha256.clone(),
				effective_from: new.effective_from,
			})
			.await?;
		crate::token::invalidate_legal_docs();

		// `legal_docs` has no `uid` column, so the identifying triple goes in `detail`.
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"legal_doc",
			None,
			"LEGAL_DOC_PUBLISHED",
			Some(serde_json::json!({
				"kind": new.kind,
				"locale": new.locale,
				"version": new.version,
			})),
		)
		.await;

		Ok(consent::LegalDocSummary {
			kind: new.kind,
			locale: new.locale,
			version: new.version,
			sha256,
			effective_from: new.effective_from,
		})
	}

	/// The version of a legal document currently in force: the highest whose `effective_from`
	/// has passed. Public — no `ctx.actor` is required.
	pub async fn legal_document(
		&self,
		ctx: &Ctx,
		kind: LegalKind,
		locale: Option<&str>,
	) -> ClResult<consent::LegalBody> {
		// A signed-in caller gets their **own** locale; only an anonymous one may choose.
		// `record_consent` anchors evidence on `account.locale` and versions collide across
		// locales, so `?locale=en` on a `hu` account recorded the wrong row's hash as evidence.
		let store = self.store()?;
		let account_locale = match ctx.actor.account_id() {
			Some(id) => store.account_by_id(id).await?.map(|a| a.locale),
			None => None,
		};
		let locale = account_locale.as_deref().or(locale).unwrap_or(DEFAULT_LOCALE);
		let doc = store
			.current_legal_doc(kind, locale, Timestamp::now())
			.await?
			// `Error::NotFound`, not a hand-rolled `E-CORE-NOT-FOUND`: `mintworks-core` owns the
			// `E-CORE-*` namespace and the registry carries no such spelling.
			.ok_or(Error::NotFound)?;
		Ok(consent::LegalBody {
			kind: doc.kind,
			locale: doc.locale,
			version: doc.version,
			title: doc.title,
			body: doc.body,
			sha256: doc.sha256,
			effective_from: doc.effective_from,
		})
	}

	/// The latest consent row per `(kind, org)` for the calling account — one row per
	/// scope, not one per kind. A grant carrying an `orgUid` used to be shadowed by any
	/// later grant of the same kind in another org, which made it invisible here and
	/// unreachable from `withdraw_consent`. See `AuthStore::latest_consent`.
	pub async fn list_consents(&self, ctx: &Ctx) -> ClResult<Vec<ConsentBody>> {
		let account = self.actor_account(ctx).await?;
		Ok(self
			.store()?
			.list_consents(account.id)
			.await?
			.into_iter()
			.map(|c| ConsentBody {
				kind: c.kind,
				doc_version: c.doc_version,
				doc_sha256: c.doc_sha256,
				granted: c.granted,
				at: c.at,
				withdrawn_at: c.withdrawn_at,
				org_uid: c.org_uid.map(OrgId::into_string),
			})
			.collect())
	}

	/// Stamp `withdrawn_at`. The row itself stays: it is evidence, and §8.2 keeps `consents`
	/// out of every erasure path.
	pub async fn withdraw_consent(&self, ctx: &Ctx, kind: LegalKind) -> ClResult<()> {
		let account = self.actor_account(ctx).await?;
		let store = self.store()?;
		if consent::UNWITHDRAWABLE.contains(&kind) {
			return Err(Error::conflict(
				"the terms and the privacy notice cannot be withdrawn while the account is \
				 active — delete the account instead",
			));
		}
		// The caller's current scope first, then the account-wide row. Without the org in the
		// lookup, withdrawing in B left A's grant in force with no way to reach it; without the
		// fallback, a caller inside any org could no longer withdraw an account-wide one.
		let scoped = match ctx.org_id {
			Some(t) => store.latest_consent(account.id, kind, Some(t)).await?,
			None => None,
		};
		let consent = match scoped {
			Some(c) => c,
			None => store
				.latest_consent(account.id, kind, None)
				.await?
				.ok_or_else(|| Error::conflict("that document was never accepted"))?,
		};
		if !store.withdraw_consent(consent.id, Timestamp::now()).await? {
			return Err(Error::conflict("that consent was already withdrawn"));
		}

		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"consent",
			Some(account.uid.as_str()),
			"CONSENT_WITHDRAWN",
			Some(json!({ "kind": kind })),
		)
		.await;
		Ok(())
	}

	/// Record an acceptance of the wording **currently in force**. A `version` that is not the
	/// one in force is a conflict: the row would otherwise be evidence about text nobody saw.
	pub async fn record_consent(&self, ctx: &Ctx, req: &ConsentGrant) -> ClResult<ConsentBody> {
		let account = self.actor_account(ctx).await?;
		let store = self.store()?;
		let now = Timestamp::now();

		let doc = store
			.current_legal_doc(req.kind, &account.locale, now)
			.await?
			.ok_or_else(|| Error::validation("no version of that document is in force"))?;
		if doc.version != req.version {
			return Err(Error::conflict("that version of the document is no longer in force"));
		}
		// Mandatory: the only thing tying the stored row to the wording actually read. Versions
		// collide across locales, so the version check alone passed on wrong-language text.
		// `Option` on the wire so a missing field is `E-CORE-VALIDATION`, not a 400 with no code.
		let Some(presented) = &req.doc_sha256 else {
			return Err(Error::validation("docSha256 is required"));
		};
		if presented != &doc.sha256 {
			return Err(Error::conflict(format!(
				"the document presented ({presented}) is not the {} text in force ({})",
				account.locale, doc.sha256
			)));
		}

		// `consents_required` reads `latest_consent(account, kind, None)`, so an org-scoped
		// `TOS` satisfied nothing and left every gated route `403` while `GET /api/consents`
		// reported it recorded. Read from `GATING_KINDS` so a third document cannot slip past.
		if req.org_uid.is_some() && crate::token::GATING_KINDS.contains(&req.kind) {
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-AUTH-CONSENT-SCOPE",
				"TOS and PRIVACY are account-wide and cannot be recorded against an org",
			));
		}

		let org_id = match &req.org_uid {
			Some(uid) => {
				// Both branches are `E-CORE-NOTFOUND`: a `400` for an unknown uid beside a
				// `403` for a real one let any user enumerate valid `org_` uids.
				let org = store.org_by_uid(&OrgId::parse(uid)?).await?.ok_or(Error::NotFound)?;
				store
					.accepted_membership_role(org.id, account.id)
					.await?
					.ok_or(Error::NotFound)?;
				Some(org.id)
			}
			None => None,
		};

		store
			.record_consent(
				&NewConsent {
					account_id: account.id,
					org_id,
					kind: req.kind,
					legal_doc_id: Some(doc.id),
					doc_version: doc.version.clone(),
					doc_sha256: doc.sha256.clone(),
					granted: true,
					ip: ctx.ip.map(|ip| ip.to_string()),
					user_agent: req.user_agent.clone(),
				},
				now,
			)
			.await?;

		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"consent",
			Some(account.uid.as_str()),
			"CONSENT_GRANTED",
			Some(json!({ "kind": req.kind, "version": doc.version })),
		)
		.await;
		Ok(ConsentBody {
			kind: doc.kind,
			doc_version: doc.version,
			doc_sha256: doc.sha256,
			granted: true,
			at: now,
			withdrawn_at: None,
			org_uid: req.org_uid.clone(),
		})
	}

	/// Every row concerning the caller, as one JSON document, plus the account uid the
	/// download is named after.
	///
	/// **Step-up and rate limited** — the `account_export` budget rides on the route layer in
	/// [`crate::routes`]. It is a whole-database read that was reachable with nothing but a stolen
	/// 15-minute access token. Scoped to the caller's *personal* org: an organisation the
	/// account merely owns holds other members' rows, which are not this data subject's to receive.
	///
	/// [`crate::gdpr::EXPORT`] names every section, its scope and its columns; the store
	/// returns one row array per section and knows nothing about the document.
	pub async fn export_account(&self, ctx: &Ctx) -> ClResult<(AccountId, Value)> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let account = self.actor_account(ctx).await?;
		// `export_account` holds one reader transaction for the whole dump and the pool has
		// five, while the rate limit is per *account* — five concurrent exporters starved every
		// other authenticated request into `E-CORE-UNAVAILABLE`.
		let rows = {
			let _permit = EXPORTS
				.acquire()
				.await
				.map_err(|e| Error::internal(format!("export semaphore closed: {e}")))?;
			self.store()?.export_account(account.id, gdpr::EXPORT).await?
		};
		let mut doc = gdpr::document(rows)?;
		for hook in &self.app.account_data_hooks {
			let section = hook.export(&account.uid).await?;
			let Some(obj) = doc.as_object_mut() else {
				return Err(Error::internal("export document is not an object"));
			};
			// A hook shadowing a framework section would silently drop rows from the export.
			if obj.insert(hook.name().to_owned(), section).is_some() {
				return Err(Error::internal(format!(
					"account data hook '{}' collides with an export section",
					hook.name()
				)));
			}
		}

		// `AUDIT_EXPORT`, the action `001_core.sql` names in its `audit_logs.action` comment:
		// dumping every invoice, payment and consent row for an account has to leave a trace.
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(account.uid.as_str()),
			"AUDIT_EXPORT",
			None,
		)
		.await;
		Ok((account.uid, doc))
	}

	/// Anonymize the caller's account. **Step-up**, and irreversible: the account can never
	/// authenticate again.
	///
	/// `confirm_email` must match the account's own address — a deliberate, typed
	/// confirmation rather than a boolean flag. What is erased is the closed column allowlist
	/// [`crate::gdpr::ERASURE`], which is also where an invoice row's absence from it is
	/// explained.
	///
	/// # Errors
	/// `409 E-AUTH-OWNER-ERASURE` while the account owns any organisation. The
	/// message names their uids; ownership must be transferred or the organisation deleted
	/// first. See [`crate::store::AuthStore::owned_shared_orgs`].
	pub async fn erase_account(&self, ctx: &Ctx, confirm_email: &str) -> ClResult<Erasure> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let account = self.actor_account(ctx).await?;

		if !confirm_email.trim().eq_ignore_ascii_case(&account.email) {
			return Err(Error::validation("confirmEmail must match the account's own address"));
		}
		// Erasure is one-way; [`crate::store::AuthStore::set_account_status`]'s doc is the other
		// half. No longer enforced in the database — `trg_account_no_unerase` is gone.
		if account.status == AccountStatus::Anonymized {
			return Err(Error::conflict("this account has already been anonymized"));
		}

		let store = self.store()?;
		// Refused rather than allowed to orphan the organisation: `anonymize_account` scrubs the
		// personal org only, so an org this account owns would keep pointing at the erased row.
		// `transfer_ownership` and `delete_org` are the two ways out, and the message names them.
		let owned = store.owned_shared_orgs(account.id).await?;
		if !owned.is_empty() {
			let uids = owned.iter().map(|t| t.uid.as_str()).collect::<Vec<_>>().join(", ");
			return Err(Error::coded(
				StatusCode::CONFLICT,
				"E-AUTH-OWNER-ERASURE",
				format!(
					"this account owns {uids}; POST /api/org/transfer-ownership or \
					 DELETE /api/orgs/{{uid}} before erasing the account"
				),
			));
		}

		// Before anonymising, never after: a failed hook then leaves a retryable account rather
		// than app-DB personal data behind an already-erased one. Hooks are idempotent.
		for hook in &self.app.account_data_hooks {
			hook.erase(&account.uid).await?;
		}

		let now = Timestamp::now();
		// The pre-check above is the message; this is the guarantee. It re-runs the same
		// predicate inside the write transaction, which the reader-pool check cannot.
		if !store.anonymize_account(account.id, now, &gdpr::ERASURE).await? {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				"E-AUTH-OWNER-ERASURE",
				"this account acquired an organisation while it was being erased",
			));
		}
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(account.uid.as_str()),
			"ACCOUNT_ANONYMIZED",
			None,
		)
		.await;

		Ok(Erasure {
			status: AccountStatus::Anonymized,
			anonymized_at: now,
			retained_until: gdpr::retained_until(now)?,
			// ponytail: the true floor is 8 years from the last invoice's *issue* year, which
			// lives in `mintworks-invoice` and this crate cannot reach. Erasure year is a safe
			// over-estimate; narrow it when a retention hook exists.
			retained_because: format!(
				"invoice retention (Számv. tv. 169. §), {} years from the end of the year of \
				 erasure — at or after the last invoice's issue year, so never below the \
				 statutory floor",
				gdpr::RETENTION_YEARS
			),
		})
	}

	// -- api keys

	/// Mints a key. Step-up required: a credential that outlives the session must not be
	/// creatable from a session that cannot re-prove itself.
	pub async fn create_api_key(
		&self,
		ctx: &Ctx,
		name: &str,
		scopes: &[String],
		expires_at: Option<Timestamp>,
	) -> ClResult<apikey::MintedKey> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		// `active_of`, not `admin_of`: any member may hold a key. The containment is the scope
		// set — a key carries no role of its own, and `auth_mw` re-reads the membership on every
		// request, so a key can never exceed the reach of the account that minted it.
		let (account, org, _) = self.active_of(ctx).await?;
		bounded("name", name, MAX_NAME_CHARS)?;
		let scopes = self.checked_scopes(scopes)?;
		let store = self.store()?;

		let max = self.app.settings.int("auth.api_keys_max").await?;

		let days = self.app.settings.int("auth.api_key_max_days").await?;
		let now = Timestamp::now();
		let ceiling = Timestamp(now.0 + days * 86_400);
		let expires_at = match expires_at {
			Some(at) if at.0 > ceiling.0 => {
				return Err(Error::validation(format!(
					"expiresAt is at most {days} days from now"
				)));
			}
			Some(at) if at.0 <= now.0 => {
				return Err(Error::validation("expiresAt is in the past"));
			}
			Some(at) => Some(at),
			None => Some(ceiling),
		};

		// Prefix and secret are independent: the prefix is public and is the lookup handle, so
		// deriving it from the secret would hand a prefix reader 48 of the 256 bits the digest
		// comparison rests on.
		let mut secret = [0u8; 32];
		let mut tag = [0u8; 6];
		OsRng.fill_bytes(&mut secret);
		OsRng.fill_bytes(&mut tag);
		let prefix = B64.encode(tag);
		let key = format!("sk_{prefix}_{}", B64.encode(secret));

		let Some(row) = store
			.create_api_key(
				&NewApiKey {
					org_id: org.id,
					account_id: account.id,
					name: name.trim().to_owned(),
					prefix,
					key_hash: hex::encode(Sha256::digest(key.as_bytes())),
					scopes: serde_json::to_string(&scopes)
						.map_err(|e| Error::internal(e.to_string()))?,
					expires_at,
				},
				max,
			)
			.await?
		else {
			return Err(Error::conflict(format!("at most {max} live API keys per org")));
		};

		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"api_key",
			Some(row.uid.as_str()),
			"API_KEY_CREATED",
			Some(json!({ "scopes": scopes, "expiresAt": expires_at.map(|t| t.0) })),
		)
		.await;

		Ok(apikey::MintedKey {
			uid: row.uid.as_str().to_owned(),
			name: row.name,
			prefix: row.prefix,
			key,
			scopes,
			created_at: row.created_at,
			expires_at: row.expires_at,
		})
	}

	/// The org's live keys, newest last. Revoked rows stay in the table — they are the trail — but
	/// never appear here. Org-wide **for an admin**; a plain member sees only their own, because a
	/// key list is a revocation surface.
	pub async fn list_api_keys(&self, ctx: &Ctx) -> ClResult<Vec<apikey::ApiKeyView>> {
		let (account, org, role) = self.active_of(ctx).await?;
		Ok(self
			.store()?
			.api_keys_for_org(org.id)
			.await?
			.into_iter()
			.filter(|k| role >= Role::Admin || k.account_id == account.id)
			// An expired key is already in the export and the audit log, so it goes the way of a
			// revoked one: these are the org's live keys.
			.filter(|k| {
				k.revoked_at.is_none() && k.expires_at.is_none_or(|e| e.0 > Timestamp::now().0)
			})
			.map(|k| apikey::ApiKeyView {
				uid: k.uid.as_str().to_owned(),
				name: k.name,
				prefix: k.prefix,
				scopes: serde_json::from_str(&k.scopes).unwrap_or_default(),
				last_used_at: k.last_used_at,
				expires_at: k.expires_at,
				created_at: k.created_at,
			})
			.collect())
	}

	/// The key `uid` names, once the caller is allowed to act on it. Its holder always may; anyone
	/// else needs `ADMIN` on the org, because a key is an org credential but revoking one is not
	/// every member's to do. A key of another org is `E-CORE-NOTFOUND`, never a 403.
	async fn own_or_admin_key(&self, ctx: &Ctx, uid: &ApiKeyId) -> ClResult<ApiKey> {
		let (account, org, role) = self.active_of(ctx).await?;
		let key = self
			.store()?
			.api_keys_for_org(org.id)
			.await?
			.into_iter()
			.find(|k| k.uid.as_str() == uid.as_str())
			.ok_or(Error::NotFound)?;
		if key.account_id != account.id && role < Role::Admin {
			return Err(Error::NotFound);
		}
		Ok(key)
	}

	/// What a mint may ask for: the prefixes the deployment registered through `.scope(…)`.
	/// Prefixes only — the verb follows the request method at the route.
	pub async fn api_key_scopes(&self, ctx: &Ctx) -> ClResult<Vec<String>> {
		self.active_of(ctx).await?;
		Ok(self.app.route_scopes.iter().map(|p| (*p).to_owned()).collect())
	}

	/// Renames a key. No step-up: it grants nothing. A key the caller is not allowed to touch is an
	/// `E-CORE-NOTFOUND`, never a 403, which would say the key exists.
	pub async fn rename_api_key(&self, ctx: &Ctx, uid: &str, name: &str) -> ClResult<()> {
		let (_, org, _) = self.active_of(ctx).await?;
		bounded("name", name, MAX_NAME_CHARS)?;
		let uid = ApiKeyId::parse(uid).map_err(|_| Error::NotFound)?;
		self.own_or_admin_key(ctx, &uid).await?;
		if !self.store()?.rename_api_key(org.id, &uid, name.trim()).await? {
			return Err(Error::NotFound);
		}
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"api_key",
			Some(uid.as_str()),
			"API_KEY_RENAMED",
			None,
		)
		.await;
		Ok(())
	}

	/// Revokes a key. **No** step-up: revocation is the emergency action, and gating it
	/// behind a re-auth keeps a leaked key live for the length of that re-auth.
	pub async fn revoke_api_key(&self, ctx: &Ctx, uid: &str) -> ClResult<()> {
		let (_, org, _) = self.active_of(ctx).await?;
		let uid = ApiKeyId::parse(uid).map_err(|_| Error::NotFound)?;
		self.own_or_admin_key(ctx, &uid).await?;
		if !self.store()?.revoke_api_key(org.id, &uid, Timestamp::now()).await? {
			return Err(Error::NotFound);
		}
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"api_key",
			Some(uid.as_str()),
			"API_KEY_REVOKED",
			None,
		)
		.await;
		Ok(())
	}

	/// One `<prefix>:<read|write>` per entry, each a prefix the deployment registered. An unknown
	/// prefix is refused rather than stored: a typo would otherwise mint a key dead on arrival.
	fn checked_scopes(&self, scopes: &[String]) -> ClResult<Vec<String>> {
		let mut out: Vec<String> = Vec::new();
		for scope in scopes {
			let (prefix, verb) = scope.split_once(':').ok_or_else(|| {
				Error::validation(format!("scope {scope:?} is not <prefix>:<verb>"))
			})?;
			if !matches!(verb, "read" | "write") {
				return Err(Error::validation(format!(
					"scope {scope:?} must end in :read or :write"
				)));
			}
			if !self.app.route_scopes.contains(prefix) {
				return Err(Error::validation(format!("no route prefix {prefix:?} is registered")));
			}
			if !out.iter().any(|seen| seen == scope) {
				out.push(scope.clone());
			}
		}
		if out.is_empty() {
			return Err(Error::validation("at least one scope is required"));
		}
		Ok(out)
	}

	// -- passkeys

	/// The options for enrolling a passkey, plus the blob carrying their challenge state. Step-up
	/// required, for the reason minting a key needs it: a passkey is a first factor.
	pub async fn begin_passkey_registration(&self, ctx: &Ctx) -> ClResult<(Value, String)> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let account = self.actor_account(ctx).await?;
		let existing = self.store()?.webauthn_for_account(account.id).await?;
		let max = self.app.settings.int("auth.webauthn_max").await?;
		if i64::try_from(existing.len()).unwrap_or(i64::MAX) >= max {
			return Err(Error::conflict(format!("at most {max} passkeys per account")));
		}
		let display = account.name.as_deref().unwrap_or(&account.email).to_owned();
		webauthn::registration_challenge(
			&self.app,
			account.uid.as_str(),
			&account.email,
			&display,
			webauthn::exclude_ids(&existing)?,
		)
		.await
	}

	/// Stores the credential a finished registration produced. The name defaults from the
	/// `User-Agent`: a user who has not used the credential yet has nothing to call it, and five
	/// rows called "Passkey" is a list nobody can act on.
	pub async fn finish_passkey_registration(
		&self,
		ctx: &Ctx,
		body: webauthn::RegisterBody,
		user_agent: Option<&str>,
	) -> ClResult<PasskeyView> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let account = self.actor_account(ctx).await?;
		let passkey =
			webauthn::finish_registration(&self.app, &body.blob, &body.registration).await?;
		let name = match body.name.as_deref().map(str::trim) {
			Some(given) if !given.is_empty() => given.to_owned(),
			_ => webauthn::name_from_user_agent(user_agent),
		};
		bounded("name", &name, MAX_NAME_CHARS)?;
		let new = NewWebauthnCredential {
			account_id: account.id,
			credential_id: webauthn::credential_id_str(passkey.cred_id()),
			credential: serde_json::to_string(&passkey)
				.map_err(|e| Error::internal(e.to_string()))?,
			name,
			created_at: Timestamp::now(),
		};
		// Re-read at the insert, not only at the challenge: the two ends of one registration are
		// separate requests, so the challenge-time check is an early refusal and this is the cap.
		let max = self.app.settings.int("auth.webauthn_max").await?;
		let Some(row) = self.store()?.put_webauthn_credential(&new, max).await? else {
			return Err(Error::conflict(format!("at most {max} passkeys per account")));
		};
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"passkey",
			Some(&new.credential_id),
			"PASSKEY_REGISTERED",
			None,
		)
		.await;
		Ok(passkey_view(&row))
	}

	/// The management list. `lastUsedAt` is what tells a user which of five credentials is the one
	/// they actually reach for.
	pub async fn list_passkeys(&self, ctx: &Ctx) -> ClResult<Vec<PasskeyView>> {
		let account = self.actor_account(ctx).await?;
		Ok(self
			.store()?
			.webauthn_for_account(account.id)
			.await?
			.iter()
			.map(passkey_view)
			.collect())
	}

	/// Renames a credential. No step-up: it grants nothing. Another account's credential id is an
	/// `E-CORE-NOTFOUND`, never a 403, which would confirm it exists.
	pub async fn rename_passkey(&self, ctx: &Ctx, credential_id: &str, name: &str) -> ClResult<()> {
		let account = self.actor_account(ctx).await?;
		bounded("name", name, MAX_NAME_CHARS)?;
		if !self.store()?.rename_webauthn(account.id, credential_id, name.trim()).await? {
			return Err(Error::NotFound);
		}
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"passkey",
			Some(credential_id),
			"PASSKEY_RENAMED",
			None,
		)
		.await;
		Ok(())
	}

	/// Removes a credential. Step-up required, unlike revoking an API key: a key is a machine
	/// credential whose leak is an emergency, while removing a passkey is the user taking away one
	/// of their own ways back in.
	pub async fn remove_passkey(&self, ctx: &Ctx, credential_id: &str) -> ClResult<()> {
		mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
		let account = self.actor_account(ctx).await?;
		if !self.store()?.delete_webauthn(account.id, credential_id).await? {
			return Err(Error::NotFound);
		}
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"passkey",
			Some(credential_id),
			"PASSKEY_REMOVED",
			None,
		)
		.await;
		Ok(())
	}

	/// `GET /api/auth/wa/login/challenge` — public, so `ctx` is `Actor::Public` and names nobody.
	/// Usernameless by design: answering "which credentials does this address have" before
	/// authenticating anyone is the enumeration surface the whole flow is built to avoid.
	pub async fn begin_passkey_login(&self, _ctx: &Ctx) -> ClResult<(Value, String)> {
		webauthn::login_challenge(&self.app).await
	}

	/// Completes a usernameless login. A passkey is a complete first factor, so this mints
	/// `auth_at = now` with no TOTP step behind it: the user verification the authenticator
	/// asserted *is* the second factor, and `verify_assertion` proves it server-side first.
	pub async fn passkey_login(&self, ctx: &Ctx, body: webauthn::LoginBody) -> ClResult<Tokens> {
		let (account, credential_id, credential) =
			self.verify_assertion(&body.assertion, &body.blob, None).await?;
		self.store()?
			.touch_webauthn(&credential_id, &credential, Timestamp::now())
			.await?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(account.uid.as_str()),
			"PASSKEY_LOGIN",
			Some(json!({ "credentialId": credential_id })),
		)
		.await;
		token::issue(&self.app, &account, Some(Timestamp::now().0)).await
	}

	/// The passkey half of `step_up`. Nothing is minted here: the caller's own assertion has proved
	/// the account, and `step_up` mints the token.
	async fn step_up_passkey(
		&self,
		account: &Account,
		proof: webauthn::StepUpProof,
	) -> ClResult<()> {
		let (_, credential_id, credential) =
			self.verify_assertion(&proof.assertion, &proof.blob, Some(account.id)).await?;
		self.store()?
			.touch_webauthn(&credential_id, &credential, Timestamp::now())
			.await?;
		Ok(())
	}

	/// Resolves an assertion to its account and verifies it, without a `Ctx`: the credential names
	/// the account, and `expect` — the calling account on the step-up path — is checked against it.
	/// Returns the account, the credential id, and the credential re-serialized for write-back.
	async fn verify_assertion(
		&self,
		assertion: &PublicKeyCredential,
		blob: &str,
		expect: Option<i64>,
	) -> ClResult<(Account, String, String)> {
		// The challenge is opened — and its `jti` spent — before the credential is resolved, so an
		// assertion naming an unknown credential id cannot leave its blob replayable.
		let state = webauthn::open(&self.app, blob).await?;
		let store = self.store()?;
		let credential_id = assertion.id.clone();
		let row = store
			.webauthn_by_credential_id(&credential_id)
			.await?
			.ok_or_else(assertion_refused)?;
		let account = store
			.account_by_id(row.account_id)
			.await?
			.filter(|a| a.status == AccountStatus::Active)
			.ok_or_else(assertion_refused)?;
		// An assertion for one account is not a step-up for another, and both refusals answer
		// alike so a stolen token cannot probe which credential ids exist.
		if expect.is_some_and(|id| id != account.id) {
			return Err(assertion_refused());
		}
		let passkey = webauthn::passkey(&row.credential)?;
		let credential = webauthn::finish_login(&self.app, state, assertion, &passkey).await?;
		Ok((account, credential_id, credential))
	}

	// -- qr login

	/// Start a QR login for an anonymous desktop. No `Ctx` use beyond the address the approving
	/// phone will be shown, and nothing about the caller is stored against an account: there is no
	/// account yet, and pre-binding one is the enumeration surface the flow avoids.
	pub fn qr_init(&self, ctx: &Ctx, user_agent: Option<&str>) -> ClResult<qr::InitResponse> {
		qr::init(ctx, user_agent)
	}

	/// Long-poll one QR login. Deliberately `Ctx`-free: the `x-qr-secret` the initiating browser
	/// holds is the whole credential, and that browser is anonymous until its session is approved,
	/// so there is no actor to derive a permission from.
	pub async fn qr_status(
		&self,
		session_id: &str,
		secret: &str,
		wait: Option<u64>,
	) -> ClResult<qr::Status> {
		qr::status(session_id, secret, wait.unwrap_or(qr::POLL_DEFAULT_SECONDS)).await
	}

	/// What the approving phone sees. Opens with `actor_account` so a caller with a public `Ctx`
	/// cannot read a stranger's device details — the bundle's `require_auth` is not the check.
	pub async fn qr_details(&self, ctx: &Ctx, session_id: &str) -> ClResult<qr::QrDetails> {
		self.actor_account(ctx).await?;
		qr::details(session_id)
	}

	/// Record the answer, minting for the **approving** account. `auth_at: None`, so a session born
	/// from a QR cannot reach a step-up route until the desktop re-proves itself: a QR photographed
	/// off a café screen cannot mint an API key or change a password.
	pub async fn qr_respond(
		&self,
		ctx: &Ctx,
		session_id: &str,
		approved: bool,
		match_code: &str,
	) -> ClResult<()> {
		let account = self.actor_account(ctx).await?;
		let tokens = if approved {
			Some(Box::new(token::issue(&self.app, &account, None).await?))
		} else {
			None
		};
		// Resolve before auditing: a `409 E-AUTH-QR-STATE` writes nothing, so the trail holds
		// only the answers that were accepted.
		qr::resolve(session_id, tokens, match_code)?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"account",
			Some(account.uid.as_str()),
			if approved { "QR_LOGIN_APPROVED" } else { "QR_LOGIN_DENIED" },
			None,
		)
		.await;
		Ok(())
	}
}

/// The management-list shape. `credential` is absent on purpose: it is the whole serialized
/// public key, and the caller already knows which credential it asked about.
fn passkey_view(row: &WebauthnCredential) -> PasskeyView {
	PasskeyView {
		credential_id: row.credential_id.clone(),
		name: row.name.clone(),
		created_at: row.created_at,
		last_used_at: row.last_used_at,
	}
}

/// One message for every way an assertion can fail past the lookup — unknown credential id,
/// suspended account, another account's credential — so none of them is distinguishable.
fn assertion_refused() -> Error {
	Error::coded(StatusCode::UNAUTHORIZED, "E-AUTH-WEBAUTHN", "the passkey assertion was refused")
}

#[cfg(test)]
mod tests {
	use super::{MAX_NAME_CHARS, bounded, bounded_multiline};

	/// A `display_name` reaches `{{name}}`, which the `.txt.hbs` templates render through
	/// `register_escape_fn(handlebars::no_escape)`, so a newline injects lines into the mail.
	#[test]
	fn a_name_carrying_a_control_character_is_refused() {
		bounded("name", "Kis Béla\nBcc: attacker@example.com", MAX_NAME_CHARS).unwrap_err();
		bounded("name", "Kis\u{7}Béla", MAX_NAME_CHARS).unwrap_err();
		// Multi-byte is exactly what the character count exists for.
		bounded("name", "Árvíztűrő Tükörfúrógép", MAX_NAME_CHARS).unwrap();
	}

	/// Every realistic ToS body is multi-line, and `bounded` refused all of them — which
	/// `register::check_consents` turned into "a fresh deployment accepts no registration".
	#[test]
	fn a_legal_document_body_may_span_lines_but_not_carry_other_controls() {
		bounded_multiline("body", "1. §\r\n\tA feltételek.\n\n2. §\n", 1_000_000).unwrap();
		bounded_multiline("body", "A felt\u{7}ételek.", 1_000_000).unwrap_err();
		bounded_multiline("body", "\n \t", 1_000_000).unwrap_err();
	}
}

// vim: ts=4
