//! Stateless HS256 bearer-token middleware: verifies the token, re-reads the account from the DB,
//! and inserts the [`Ctx`] every service method takes.
//!
//! There is no session table. `accounts.token_epoch` is the only revocation lever, so the
//! account row is re-read on every authenticated request — that read also supplies
//! `accounts.id` (the token carries the uid) and `accounts.is_operator`, both of which must
//! come from the DB rather than from the token.
//!
//! Enforcement is declared by the route bundle, not by whether a handler happens to extract
//! `Ctx`: [`require_auth`] answers `401 E-AUTH-TOKEN` itself when no usable token is present,
//! [`optional_auth`] lets the request through unauthenticated. The `accounts`, `tenants` and
//! `memberships` tables belong to `saas-auth`, so these are plain string queries — `saas-core`
//! takes no crate dependency on it.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{
	HeaderMap, StatusCode,
	header::{AUTHORIZATION, COOKIE},
};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::ctx::{Actor, Ctx};
use crate::error::{ClResult, Error};
use crate::log::RequestId;
use crate::types::Timestamp;

/// `secrets` key holding the HS256 signing key. Rotating it signs everyone out.
pub const JWT_SECRET_KEY: &str = "auth.jwt_key";

/// The access-token cookie `saas_auth::token::issue` sets. The name lives here because the
/// middleware that reads it cannot depend on `saas-auth`.
pub const ACCESS_COOKIE: &str = "access_token";

/// The consent gate, passed to a route bundle as a value it cannot be built without.
///
/// Every authenticated bundle outside `saas-auth`'s own used to be gated only if the
/// composition root remembered to wrap it, and nothing at boot noticed when it did not: an
/// account owing a newly published ToS was refused `/api/tenants` and could still issue a
/// numbered legal invoice. Making it an argument moves that from a convention to a compile
/// error. It lives here because `saas-invoice` cannot depend on `saas-auth`.
#[derive(Clone)]
pub struct RouteGate(
	#[allow(clippy::type_complexity)]
	Option<std::sync::Arc<dyn Fn(axum::Router<App>) -> axum::Router<App> + Send + Sync>>,
);

impl RouteGate {
	/// Wrap a router transform. `saas_auth::routes::consent_gate` is the framework's only one.
	pub fn new<F>(f: F) -> Self
	where
		F: Fn(axum::Router<App>) -> axum::Router<App> + Send + Sync + 'static,
	{
		Self(Some(std::sync::Arc::new(f)))
	}

	/// No gate — for a deployment that publishes no legal documents. Deliberate and visible at
	/// the call site, which is the whole reason the gate is a parameter.
	#[must_use]
	pub fn none() -> Self {
		Self(None)
	}

	/// Apply it to a bundle.
	pub fn apply(&self, router: axum::Router<App>) -> axum::Router<App> {
		match &self.0 {
			Some(gate) => gate(router),
			None => router,
		}
	}
}

impl std::fmt::Debug for RouteGate {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(if self.0.is_some() { "RouteGate(gated)" } else { "RouteGate::none()" })
	}
}

/// Access- and refresh-token claims. `saas-auth` signs these; this module verifies them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
	/// `accounts.uid`.
	pub sub: String,
	/// Active `tenants.uid`. Absent on a refresh token and before a tenant is chosen.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tnt: Option<String>,
	/// `memberships.role` for `tnt`. Advisory — a tenant-admin path re-reads it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rol: Option<String>,
	/// `accounts.is_operator`. Advisory — this module takes the DB's answer.
	#[serde(default)]
	pub opr: bool,
	/// `accounts.token_epoch` at issue time. A mismatch fails verification.
	pub ep: i64,
	/// When the credential was last presented; the step-up clock. Unset on an
	/// impersonation token, which is why no step-up route is reachable while
	/// impersonating.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub auth_at: Option<i64>,
	/// Impersonating operator's `accounts.uid`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub imp: Option<String>,
	/// `"refresh"` on a refresh token; absent on an access token.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub typ: Option<String>,
	pub iat: i64,
	pub exp: i64,
}

fn token_error(msg: &'static str) -> Error {
	Error::coded(StatusCode::UNAUTHORIZED, "E-AUTH-TOKEN", msg)
}

/// Reads the `Claims` the middleware put in the request extensions, exactly as the [`Ctx`]
/// extractor does. Handlers took `Extension<Claims>` before, whose axum rejection on a
/// token-less request is a plain-text `500` — outside the single error envelope, and a
/// misleading one: the request is unauthenticated, not broken.
impl<S: Send + Sync> FromRequestParts<S> for Claims {
	type Rejection = Error;

	async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		parts
			.extensions
			.get::<Claims>()
			.cloned()
			.ok_or_else(|| AuthDenied::or(parts, || token_error("authentication required")))
	}
}

/// The address every per-IP rate-limit bucket keys on, resolved once by
/// [`client_ip_mw`] and read back by any handler that needs it.
///
/// Handlers used to build this themselves out of `ConnectInfo<SocketAddr>`, which ignored
/// `settings['http.trusted_proxy']` entirely — behind a proxy `register`, `login.ip` and
/// `pow` all keyed on the proxy and collapsed into one bucket.
#[derive(Clone, Copy, Debug)]
pub struct ClientIp(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
	type Rejection = Error;

	async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		parts
			.extensions
			.get::<ClientIp>()
			.copied()
			.ok_or_else(|| Error::internal("client_ip middleware is not mounted"))
	}
}

/// The address the per-IP rate-limit buckets key on.
///
/// `X-Forwarded-For` is honoured **only** when the direct peer is listed in
/// `settings['http.trusted_proxy']`; otherwise the peer wins and the header is ignored. Trusted
/// unconditionally, a client could forge a fresh bucket per request; ignored behind a real
/// proxy, `pow`, `login` and `register` all collapse onto the proxy's one bucket.
///
/// The chosen entry is the right-most one that is not itself a trusted proxy: everything to
/// its right was appended by infrastructure this deployment controls, and everything to its
/// left is client-supplied and forgeable. All lines are flattened, not just the first — see
/// `an_appended_second_forwarded_line_wins_over_a_forged_first_one`.
fn client_ip(peer: Option<IpAddr>, headers: &HeaderMap, trusted: &str) -> Option<IpAddr> {
	let is_trusted = |ip: &IpAddr| {
		trusted
			.split(',')
			.map(str::trim)
			.filter(|s| !s.is_empty())
			// `to_canonical` on both sides, or a dual-stack listener's `::ffff:10.0.0.4` peer
			// compares false against a configured `10.0.0.4` and the proxy is never trusted.
			.any(|t| {
				t.parse::<IpAddr>().is_ok_and(|t| t.to_canonical() == ip.to_canonical())
			})
	};
	let peer = peer?;
	if !is_trusted(&peer) {
		return Some(peer);
	}
	headers
		.get_all("x-forwarded-for")
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|line| line.split(','))
		.filter_map(|s| s.trim().parse::<IpAddr>().ok())
		.rfind(|ip| !is_trusted(ip))
		.or(Some(peer))
}

/// `http.trusted_proxy` as a `SettingDef::check` hook: [`client_ip`] discards an unparseable
/// entry silently, so a mistyped setting was inert — every per-IP bucket on the proxy's own
/// address, `/readyz` green, nothing logged. The message names CIDR because that is the shape
/// an operator reaches for first.
pub(crate) fn check_trusted_proxy(raw: &str) -> ClResult<()> {
	for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
		if entry.parse::<IpAddr>().is_err() {
			return Err(Error::validation(format!(
				"http.trusted_proxy: '{entry}' is not an IP address — list each proxy's \
				 address separately; CIDR ranges are not supported"
			)));
		}
	}
	Ok(())
}

/// Resolves [`ClientIp`] for **every** request, mounted outside the auth layers so a public,
/// token-less route sees it too. Skipped silently when nothing resolves: a bucket with no
/// key is not a client error.
pub async fn client_ip_mw(State(app): State<App>, mut req: Request, next: Next) -> Response {
	let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
	// An absent row is legitimately "no proxy configured"; a *store error* is not, and
	// `unwrap_or_default` made the two the same — every request behind a configured proxy then
	// bucketed on the proxy's own address and `default_mw` 429'd all of it, silently.
	let trusted = app.settings.text("http.trusted_proxy").await.unwrap_or_else(|e| {
		tracing::warn!(error = %e, "client_ip: trusted_proxy read failed; trusting no proxy");
		String::new()
	});
	if let Some(ip) = client_ip(peer, req.headers(), &trusted) {
		req.extensions_mut().insert(ClientIp(ip));
	}
	next.run(req).await
}

/// The three account-state errCodes, in one place. Both the arm that lets them run on
/// anonymously and the arm that decides they are free of the [`crate::ratelimit::AUTH_FAILED`]
/// charge ask this question, and the two must not drift apart.
fn is_account_state_denial(code: &str) -> bool {
	matches!(code, "E-AUTH-SUSPENDED" | "E-AUTH-PENDING" | "E-AUTH-ANONYMIZED")
}

/// Why [`optional_auth`] declined a token that was otherwise valid, kept for the `Ctx`/`Claims`
/// extractors to answer with.
///
/// A suspended, pending or anonymized account is a *403 with its own code* and telling those apart
/// is part of the contract — a client told only "log in again" cannot see that logging in fixes
/// none of them. Answering it from the middleware failed *every* route for a browser still holding
/// the cookie, including the logout that would clear it, so the request runs on anonymously and
/// this rides along.
#[derive(Clone, Debug)]
pub struct AuthDenied {
	pub status: StatusCode,
	pub code: &'static str,
	pub msg: String,
}

impl AuthDenied {
	/// The reason as an [`Error`], or `fallback` when the request carried no denial.
	pub fn or(parts: &Parts, fallback: impl FnOnce() -> Error) -> Error {
		match parts.extensions.get::<Self>() {
			Some(d) => Error::coded(d.status, d.code, d.msg.clone()),
			None => fallback(),
		}
	}
}

/// Requires a usable access token: rejects an absent, expired, forged or not-yet-usable one
/// with the same response the [`Ctx`] extractor used to produce, and otherwise inserts `Ctx`
/// and `Claims`. Layer it on an authenticated route bundle.
///
/// Mount it inside [`crate::log::request_id_mw`] (request id), inside [`client_ip_mw`]
/// ([`ClientIp`]) and inside the `Extension<App>` layer `AppBuilder::run` applies.
///
/// Every rejection charges the IP-keyed [`crate::ratelimit::AUTH_FAILED`] bucket and becomes a
/// 429 once that is empty. Layer [`crate::ratelimit::AUTHENTICATED`] inside this one for the
/// account-keyed tier the accepted requests charge.
pub async fn require_auth(req: Request, next: Next) -> Response {
	authenticate(req, next, true).await
}

/// Populates `Ctx` and `Claims` when a token happens to be present and lets the request
/// through when it is not. Layer it on a public bundle with a handler that reads
/// `Option<Ctx>`, such as `GET /api/legal/{kind}`.
///
/// On a public bundle the *handler* is the credential check, so a 401 coming back out of it
/// charges the IP-keyed [`crate::ratelimit::AUTH_FAILED`] bucket exactly as a [`require_auth`]
/// rejection does. Keying that bucket on the caller's address rather than the submitted one is
/// what closes the account-enumeration oracle behind `saas_auth`'s proof-of-work gate: all
/// three of `login`'s failure branches are the same 401.
pub async fn optional_auth(req: Request, next: Next) -> Response {
	authenticate(req, next, false).await
}

/// Inserted beside the `Ctx` [`authenticate`] verified. Private, so a `Ctx` that reached the
/// extensions any other way — `Ctx::system` is public — no longer reads as "already
/// authenticated", turning every route behind [`require_auth`] into an operator path.
#[derive(Clone, Copy)]
struct Verified;

/// The shared body. `required` is the only difference between the two entry points — keeping
/// one body is why a cookie fix or a claim check cannot land on one and miss the other.
async fn authenticate(mut req: Request, next: Next, required: bool) -> Response {
	// Idempotent: a bundle that ends up layered under an outer `require_auth` — which is what
	// `saas_auth::routes::consent_gated_router` does, so the consent gate runs with `Claims`
	// already present — must not pay a second `verify` and its two queries.
	if req.extensions().get::<Verified>().is_some() {
		return next.run(req).await;
	}
	// A `Router<App>` is built before any `App` exists, so a route bundle cannot use
	// `from_fn_with_state`; `AppBuilder::run` puts the state in the extensions instead.
	let Some(app) = req.extensions().get::<App>().cloned() else {
		return Error::internal("auth middleware mounted without the App extension")
			.into_response();
	};
	// Read before the bearer check: a request with no token at all is a countable auth
	// failure too, and it is the cheapest one to send in bulk.
	let ip = req.extensions().get::<ClientIp>().map(|c| c.0);
	let Some(token) = bearer(&req) else {
		if required {
			let denial = token_error("authentication required").into_response();
			return crate::ratelimit::charge_auth_failure(&app, ip, denial).await;
		}
		return run_public(&app, ip, req, next).await;
	};
	let request_id = req.extensions().get::<RequestId>().map_or_else(String::new, |r| r.0.clone());

	match verify(&app, &token, ip, request_id).await {
		Ok((claims, ctx)) => {
			req.extensions_mut().insert(ctx);
			req.extensions_mut().insert(claims);
			req.extensions_mut().insert(Verified);
			if required { next.run(req).await } else { run_public(&app, ip, req, next).await }
		}
		// The auth-failed tier's charge point — uncharged, a forged token costs only the blanket
		// 120/min/ip. Both guards are load-bearing: without `is_client_error` a reader-pool 5xx
		// drains the bucket and masks itself as `E-CORE-RATELIMIT`; without
		// [`is_account_state_denial`] one suspended user's cookie drains the NAT's shared budget.
		Err(e) if required && e.parts().0.is_client_error() => {
			let free = is_account_state_denial(e.parts().1);
			let response = e.into_response();
			if free {
				response
			} else {
				crate::ratelimit::charge_auth_failure(&app, ip, response).await
			}
		}
		// On an optional bundle a bad or expired token is not an error: failing the request made
		// `POST /api/auth/refresh` unreachable for any client attaching `Authorization` globally,
		// and a stale cookie 403'd the very `logout` that would clear it. The reason rides along
		// in [`AuthDenied`] so an authenticated route still answers the specific code.
		Err(e) if is_account_state_denial(e.parts().1) => {
			let (status, code) = e.parts();
			tracing::debug!(error = %e, "account not usable; continuing unauthenticated");
			req.extensions_mut().insert(AuthDenied { status, code, msg: e.to_string() });
			run_public(&app, ip, req, next).await
		}
		Err(e) if e.parts().0 == StatusCode::UNAUTHORIZED => {
			tracing::debug!(error = %e, "token verification failed; continuing unauthenticated");
			run_public(&app, ip, req, next).await
		}
		// Everything left is infrastructure failure, required bundle included — what the
		// `is_client_error` guard above lets through. A reader-pool 500 stays a 500 and
		// uncharged; a silent 401 storm or a drained-bucket 429 would hide it.
		Err(e) => e.into_response(),
	}
}

/// Put in a `401`'s extensions to say it is a **challenge**, not a failed credential, so
/// [`run_public`] does not charge it to [`crate::ratelimit::AUTH_FAILED`].
///
/// `saas_auth` renders its second-factor ticket as a `401` carrying a `totpToken` — the status
/// is fixed by the contract — so without this every *successful* first factor on a 2FA account
/// counted as a failure: an office NAT of 2FA users DoSed itself, and the accounts with the
/// strongest authentication were the ones penalised.
///
/// A marker rather than an `errCode` list, so `saas-core` still names no `E-AUTH-*` code.
#[derive(Clone, Copy, Debug)]
pub struct NotAnAuthFailure;

/// Runs a public bundle's handler and charges [`crate::ratelimit::AUTH_FAILED`] if it answers
/// 401. On such a bundle the credential check is the handler's, not the middleware's, so this
/// is the only place a wrong password can be counted — and counting it is what arms
/// `saas_auth`'s proof-of-work gate. A response carrying [`NotAnAuthFailure`] is exempt.
async fn run_public(app: &App, ip: Option<IpAddr>, req: Request, next: Next) -> Response {
	let resp = next.run(req).await;
	if resp.status() == StatusCode::UNAUTHORIZED
		&& resp.extensions().get::<NotAnAuthFailure>().is_none()
	{
		return crate::ratelimit::charge_auth_failure(app, ip, resp).await;
	}
	resp
}

fn bearer(req: &Request) -> Option<String> {
	let header = req.headers().get(AUTHORIZATION).and_then(|raw| {
		raw.to_str()
			.ok()?
			.strip_prefix("Bearer ")
			.map(|t| t.trim().to_owned())
			.filter(|t| !t.is_empty())
	});
	// Bearer wins; the cookie is the browser path — `token::issue` sets it `HttpOnly`, and
	// nothing read it before, so a cookie-only client was unauthenticated everywhere.
	header.or_else(|| cookie_value(req.headers(), ACCESS_COOKIE))
}

/// Reads one cookie out of a `Cookie:` header. `pub` because the auth crate's refresh path
/// needs the same parse and had a byte-identical copy of it.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
	let raw = headers.get(COOKIE)?.to_str().ok()?;
	raw.split(';')
		.filter_map(|pair| pair.trim().split_once('='))
		.find(|(k, _)| *k == name)
		.map(|(_, v)| v.to_owned())
}

async fn verify(
	app: &App,
	token: &str,
	ip: Option<IpAddr>,
	request_id: String,
) -> ClResult<(Claims, Ctx)> {
	// The signing key comes from `SecretStore`'s cache, so an authenticated request costs
	// no DB read for it. `secrets::set` invalidates the entry, so an operator's key
	// rotation still takes effect without a restart.
	let key = app
		.secrets
		.get(JWT_SECRET_KEY)
		.await?
		.ok_or_else(|| token_error("no signing key configured"))?;

	let validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
	let claims = jsonwebtoken::decode::<Claims>(
		token,
		&jsonwebtoken::DecodingKey::from_secret(&key),
		&validation,
	)
	.map_err(|_| token_error("invalid or expired token"))?
	.claims;

	if claims.typ.as_deref() == Some("refresh") {
		return Err(token_error("refresh token is not an access token"));
	}

	let Some(account) = app.store.account_for_token(&claims.sub).await? else {
		return Err(token_error("unknown account"));
	};
	let (account_id, epoch, is_operator, status) =
		(account.id, account.token_epoch, account.is_operator, account.status);
	if epoch != claims.ep {
		return Err(token_error("token superseded"));
	}
	match status.as_str() {
		"ACTIVE" => {}
		"PENDING" => {
			return Err(Error::coded(
				StatusCode::FORBIDDEN,
				"E-AUTH-PENDING",
				"activation not completed",
			));
		}
		"SUSPENDED" => {
			return Err(Error::coded(
				StatusCode::FORBIDDEN,
				"E-AUTH-SUSPENDED",
				"account suspended",
			));
		}
		_ => {
			return Err(Error::coded(
				StatusCode::FORBIDDEN,
				"E-AUTH-ANONYMIZED",
				"account anonymized",
			));
		}
	}

	let actor =
		if is_operator { Actor::Operator { account_id } } else { Actor::User { account_id } };
	// The membership join **is** the authorization check: every feature crate scopes on
	// `ctx.tenant()` and trusts it was earned, so resolving the `tnt` claim by uid alone let a
	// removed member keep access for the rest of the token's life. `accepted_at IS NOT NULL` at
	// the *read*, so a row inserted by other means confers nothing; `t.status = 'ACTIVE'`
	// re-read per request, or a suspended tenant's members keep issuing for 15 minutes.
	let tenant_id = match &claims.tnt {
		Some(uid) => app.store.tenant_membership(account_id, uid).await?,
		None => None,
	};

	let auth_at = claims.auth_at;
	Ok((claims, Ctx { actor, tenant_id, ip, auth_at, request_id }))
}

/// Operator-only master data. The flag is re-read from the DB per call rather than trusted from the
/// token; `System` is the application's own code and is trusted.
///
/// Here rather than in `saas-auth` for the same reason [`require_stepup`] is: this module
/// already reads `accounts` with a plain string query, and the operator-only routes are
/// spread across feature crates that must not take a dependency edge on `saas-auth`.
pub async fn require_operator(app: &App, ctx: &Ctx) -> ClResult<()> {
	let forbidden = || Error::coded(StatusCode::FORBIDDEN, "E-AUTH-FORBIDDEN", "operator only");
	match ctx.actor {
		Actor::System { .. } => Ok(()),
		// An unauthenticated caller is never an operator, whatever the route. `System` is
		// trusted because it *is* the application; `Public` is an anonymous request body.
		Actor::User { .. } | Actor::Public { .. } => Err(forbidden()),
		Actor::Operator { account_id } => {
			if app.store.is_operator(account_id).await? == Some(true) {
				Ok(())
			} else {
				Err(forbidden())
			}
		}
	}
}

/// The step-up guard: a destructive route requires that the credential was *presented* recently —
/// `auth_at` within `settings['auth.stepup_window']` — not merely that the token is unexpired. Call
/// it as the first line of any handler the step-up list covers.
///
/// In `saas-core`, not `saas-auth`: the step-up routes are spread across feature crates that
/// would each need a dependency edge on `saas-auth` for fifteen lines.
///
/// A claim set with no `auth_at` is an API key or an impersonation token, neither of which
/// can ever satisfy step-up: there is no credential behind them to re-present.
pub async fn require_stepup(app: &App, ctx: &Ctx) -> ClResult<()> {
	// The application's own code holds the database already and has no credential to
	// re-present, so a service handle called from a job must not be gated on one. `Public`
	// falls through to `E-AUTH-STEPUP-IMPOSSIBLE`: no `auth_at` and no way to acquire one.
	if matches!(ctx.actor, Actor::System { .. }) {
		return Ok(());
	}
	let Some(auth_at) = ctx.auth_at else {
		return Err(Error::coded(
			StatusCode::FORBIDDEN,
			"E-AUTH-STEPUP-IMPOSSIBLE",
			"this route needs a re-presented password or second factor",
		));
	};
	let window = app.settings.int("auth.stepup_window").await?;
	if Timestamp::now().0 - auth_at >= window {
		return Err(Error::coded(
			StatusCode::UNAUTHORIZED,
			"E-AUTH-STEPUP",
			"re-authentication required",
		));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ip(s: &str) -> IpAddr {
		s.parse().unwrap()
	}

	fn headers(xff: &str) -> HeaderMap {
		let mut h = HeaderMap::new();
		h.insert("x-forwarded-for", xff.parse().unwrap());
		h
	}

	/// The default is "trust nothing": a forged header from a direct client must not move
	/// which rate-limit bucket the request spends.
	#[test]
	fn a_forged_forwarded_header_is_ignored_when_nothing_is_trusted() {
		let peer = Some(ip("203.0.113.9"));
		assert_eq!(client_ip(peer, &headers("1.2.3.4"), ""), peer);
		assert_eq!(client_ip(peer, &headers("1.2.3.4"), "10.0.0.1"), peer, "peer not listed");
	}

	/// Behind a configured proxy the header is the client, or every bucket keys on the proxy.
	#[test]
	fn a_trusted_proxy_supplies_the_client_address() {
		let peer = Some(ip("10.0.0.1"));
		assert_eq!(client_ip(peer, &headers("198.51.100.7"), "10.0.0.1"), Some(ip("198.51.100.7")));

		// Two hops, both trusted: the right-most entry that is not infrastructure wins, and
		// the forgeable prefix to its left is discarded.
		let chain = headers("1.2.3.4, 198.51.100.7, 10.0.0.2");
		assert_eq!(client_ip(peer, &chain, "10.0.0.1, 10.0.0.2"), Some(ip("198.51.100.7")));
	}

	/// Traefik and several CDNs append a second `X-Forwarded-For` line instead of folding
	/// into the first, so `HeaderMap::get` — which returns only the first — handed back the
	/// attacker's own line and every request got a fresh bucket.
	#[test]
	fn an_appended_second_forwarded_line_wins_over_a_forged_first_one() {
		let mut h = HeaderMap::new();
		h.append("x-forwarded-for", "1.2.3.4".parse().unwrap());
		h.append("x-forwarded-for", "198.51.100.7".parse().unwrap());
		assert_eq!(client_ip(Some(ip("10.0.0.1")), &h, "10.0.0.1"), Some(ip("198.51.100.7")));
	}

	/// A trusted peer that sent no usable header still has an address of its own.
	#[test]
	fn a_trusted_peer_with_no_header_falls_back_to_itself() {
		let peer = Some(ip("10.0.0.1"));
		assert_eq!(client_ip(peer, &HeaderMap::new(), "10.0.0.1"), peer);
		assert_eq!(client_ip(peer, &headers("not-an-ip"), "10.0.0.1"), peer);
		assert_eq!(client_ip(None, &headers("1.2.3.4"), "10.0.0.1"), None);
	}

	/// Every test above passes a bare literal, which is why two realistic configurations were
	/// invisible: an unparseable entry is discarded silently, so the proxy is never trusted
	/// and every per-IP bucket collapses onto its address with `/readyz` green.
	#[test]
	fn a_configuration_that_cannot_be_honoured_is_refused_when_it_is_written() {
		// CIDR is the first thing an operator reaches for, and it never matched anything.
		assert!(check_trusted_proxy("10.0.0.0/8").is_err());
		assert!(check_trusted_proxy("127.0.0.1, 10.0.0.0/8").is_err());
		assert!(check_trusted_proxy("localhost").is_err());
		// The default and the documented shapes still pass.
		assert!(check_trusted_proxy("").is_ok());
		assert!(check_trusted_proxy("127.0.0.1, 10.0.0.4, ::1").is_ok());
	}

	/// A dual-stack listener reports an IPv4 peer as `::ffff:10.0.0.4`, which compared false
	/// against a configured `10.0.0.4` — `Ipv6Addr != Ipv4Addr` — so the proxy was untrusted.
	#[test]
	fn an_ipv4_mapped_peer_matches_a_bare_ipv4_configuration() {
		let peer = Some(ip("::ffff:10.0.0.1"));
		assert_eq!(client_ip(peer, &headers("198.51.100.7"), "10.0.0.1"), Some(ip("198.51.100.7")));
		// And the reverse spelling, in case the configuration is the mapped one.
		let peer = Some(ip("10.0.0.1"));
		assert_eq!(
			client_ip(peer, &headers("198.51.100.7"), "::ffff:10.0.0.1"),
			Some(ip("198.51.100.7"))
		);
	}
}

// vim: ts=4
