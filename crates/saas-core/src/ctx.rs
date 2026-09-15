//! Who is acting, on which tenant, from where — the first parameter of every service
//! method (`claude-docs/rust-api.md` §2).
//!
//! `Ctx` is plain data: it holds no `Arc<AppState>`, so a unit test builds one as a
//! literal. HTTP requests get theirs from `crate::auth_mw`; jobs and consumer code use
//! `Ctx::system("…")`.

use std::net::IpAddr;

use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::{StatusCode, request::Parts};

use crate::error::{ClResult, Error};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
	User {
		account_id: i64,
	},
	Operator {
		account_id: i64,
	},
	/// The application's own code. `source` is `&'static str` so it cannot carry user
	/// input; it lands in `audit_logs.detail` as `{"source": …}`.
	System {
		source: &'static str,
	},
	/// Nobody yet: an unauthenticated HTTP caller, on a route that runs before any token
	/// exists — login, register, activate, password reset, the public legal documents.
	///
	/// Distinct from [`Actor::System`] on purpose: those handlers build their `Ctx` out of an
	/// anonymous request body, while `System` is the *most* privileged actor there is —
	/// [`crate::auth_mw::require_operator`] grants it unconditionally and
	/// [`crate::auth_mw::require_stepup`] exempts it. One public route calling a gated method
	/// would have been a silent operator bypass with no compile-time signal. `source` carries
	/// the same audit meaning as `System`'s.
	Public {
		source: &'static str,
	},
}

impl Actor {
	/// `None` for `System` and `Public` — `audit_logs.account_id` is NULL for both. A public
	/// route that learns whose account it is about upgrades through [`Ctx::as_user`].
	pub fn account_id(&self) -> Option<i64> {
		match self {
			Self::User { account_id } | Self::Operator { account_id } => Some(*account_id),
			Self::System { .. } | Self::Public { .. } => None,
		}
	}
}

#[derive(Clone, Debug)]
pub struct Ctx {
	pub actor: Actor,
	/// `None` before a tenant is chosen, and for `System` sweeps.
	pub tenant_id: Option<i64>,
	pub ip: Option<IpAddr>,
	/// When the caller last presented a credential — the step-up clock, from the token's
	/// `auth_at` claim. `None` for an impersonation token, which is why no step-up route is
	/// reachable while impersonating, and `None` for `System`, which
	/// [`crate::auth_mw::require_stepup`] exempts: the application's own code already holds
	/// the database and has no credential to re-present.
	pub auth_at: Option<i64>,
	/// Ties every audit row to a structured log line. Empty for non-HTTP callers.
	pub request_id: String,
}

impl Ctx {
	pub fn system(source: &'static str) -> Self {
		Self {
			actor: Actor::System { source },
			tenant_id: None,
			ip: None,
			auth_at: None,
			request_id: String::new(),
		}
	}

	/// The `Ctx` an unauthenticated route builds for itself. See [`Actor::Public`] for why
	/// this is not [`Ctx::system`].
	pub fn public(source: &'static str) -> Self {
		Self { actor: Actor::Public { source }, ..Self::system(source) }
	}

	/// The peer address a public route saw. `register`, `login` and `activate` run before
	/// any token exists, so they build their own `Ctx` from `ConnectInfo` — and `ctx.ip` is
	/// what the IP-keyed rate-limit buckets and the proof-of-work gate key on.
	#[must_use]
	pub fn with_ip(mut self, ip: IpAddr) -> Self {
		self.ip = Some(ip);
		self
	}

	/// Name the account this call turned out to be about.
	///
	/// A public route starts as [`Ctx::system`], whose `account_id()` is `None`, but by the
	/// time it writes its audit row the account *is* known. Left `None`, every login misses
	/// `idx_audit_log_account` and the subject's own login and password-reset history falls
	/// out of their GDPR export, which selects on `account_id`.
	#[must_use]
	pub fn as_user(mut self, account_id: i64) -> Self {
		self.actor = Actor::User { account_id };
		self
	}

	pub fn with_tenant(mut self, tenant_id: i64) -> Self {
		self.tenant_id = Some(tenant_id);
		self
	}

	/// The tenant this call is confined to. A method that needs one calls this rather
	/// than unwrapping `tenant_id`.
	pub fn tenant(&self) -> ClResult<i64> {
		self.tenant_id.ok_or_else(|| {
			Error::coded(StatusCode::FORBIDDEN, "E-AUTH-FORBIDDEN", "no tenant selected")
		})
	}
}

/// Reads the `Ctx` the auth middleware put in the request extensions. A handler never
/// assembles one itself; no `Ctx` present means no usable token was sent.
impl<S: Send + Sync> FromRequestParts<S> for Ctx {
	type Rejection = Error;

	async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		parts.extensions.get::<Ctx>().cloned().ok_or_else(|| {
			// `auth_mw::auth` no longer short-circuits a suspended, pending or anonymized
			// account — that 403 used to hit `/healthz` and `POST /api/auth/logout` too. It
			// leaves the reason behind instead, and this is where it becomes the answer.
			crate::auth_mw::AuthDenied::or(parts, || {
				Error::coded(StatusCode::UNAUTHORIZED, "E-AUTH-TOKEN", "authentication required")
			})
		})
	}
}

/// The same extension, for a **public** route that wants the caller's identity when a token
/// happens to be present. Never an error: no `Ctx` extension means an anonymous caller, which
/// is the normal case on these routes — unlike [`FromRequestParts`], where its absence is a
/// 401.
impl<S: Send + Sync> OptionalFromRequestParts<S> for Ctx {
	type Rejection = Error;

	async fn from_request_parts(
		parts: &mut Parts,
		_state: &S,
	) -> Result<Option<Self>, Self::Rejection> {
		Ok(parts.extensions.get::<Ctx>().cloned())
	}
}

// vim: ts=4
