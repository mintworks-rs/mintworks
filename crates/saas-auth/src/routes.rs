//! `saas-auth`'s router, and the one piece of wiring it needs from the application.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path as UriPath, Query, State};
use axum::http::StatusCode;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::Response;
use axum::routing::{delete, get, patch, post};
use saas_core::app::App;
use saas_core::auth_mw::{RouteGate, optional_auth, require_auth};
use saas_core::prelude::*;
use saas_core::ratelimit::{AUTHENTICATED, scoped_account_mw, scoped_ip_mw};
use serde_json::{Value, json};

use crate::org::Items;
use crate::store::AuthStore;
use crate::{
	activate, apikey, consent, gdpr, login, org, pow, qr, register, reset, stepup, token, totp,
	webauthn,
};

/// The routes that need no credential. The PoW challenge is specified as `saas-core`'s, but it
/// has no callers there and every one of them is here, so it ships in this router at its
/// documented path.
///
/// Layered with `optional_auth`, not left bare: `GET /api/legal/{kind}` reads `Option<Ctx>` to
/// mark which documents the caller has already accepted, and `POST /api/auth/logout` is here
/// because clearing your own cookies must not need a credential — see [`login::logout`].
///
/// Eight routes carry a named rate limit of their own, on top of the blanket `ratelimit.default`
/// tier `AppBuilder::run` applies. All eight key on IP, because the account they concern lives in
/// the request body or behind a ticket, neither of which a layer can read. The per-account half
/// on login is the `login.email` scope `Auth::login` charges itself.
pub fn public() -> Router<App> {
	Router::new()
		.route("/api/auth/logout", post(login::logout))
		.route(
			"/api/pow/challenge",
			get(pow::challenge).layer(from_fn_with_state("pow", scoped_ip_mw)),
		)
		.route(
			"/api/auth/register",
			post(register::register).layer(from_fn_with_state("register", scoped_ip_mw)),
		)
		.route("/api/auth/activate", post(activate::activate))
		// Shares the `register` bucket with self-service registration: both send mail to an
		// address the caller chose, and both cost the same to abuse.
		.route(
			"/api/auth/resend-activation",
			post(activate::resend).layer(from_fn_with_state("register", scoped_ip_mw)),
		)
		.route(
			"/api/auth/login",
			post(login::login).layer(from_fn_with_state("login.ip", scoped_ip_mw)),
		)
		.route(
			"/api/auth/login/totp",
			post(login::login_totp).layer(from_fn_with_state("login.totp", scoped_ip_mw)),
		)
		// Public because a passkey login is unauthenticated by definition. The challenge fires on
		// every login-page view — conditional UI calls it before the user acts — so it carries its
		// own generous bucket: sharing `login.ip`'s 10/5min would lose the login page itself for
		// an office behind one NAT.
		.route(
			"/api/auth/wa/login/challenge",
			get(wa_login_challenge).layer(from_fn_with_state("wa.challenge", scoped_ip_mw)),
		)
		.route("/api/auth/wa/login", post(wa_login))
		// The desktop's own bucket, not `wa.challenge`'s 60: this is a deliberate act, and every
		// init mints a session a phone might later approve.
		.route(
			"/api/auth/qr/init",
			post(qr_init).layer(from_fn_with_state("qr.init", scoped_ip_mw)),
		)
		.route("/api/auth/qr/{sessionId}/status", get(qr_status))
		.route("/api/auth/refresh", post(login::refresh))
		.route(
			"/api/auth/password/reset-request",
			post(reset::request).layer(from_fn_with_state("password_reset", scoped_ip_mw)),
		)
		// The ticket's second-factor branch is the same six digits `login/totp` guards, so it
		// gets the same bucket: a stolen reset ticket plus unbounded guesses would otherwise
		// walk past TOTP with nothing in the way.
		.route(
			"/api/auth/password/reset",
			post(reset::reset).layer(from_fn_with_state("login.totp", scoped_ip_mw)),
		)
		.route(
			"/api/legal/{kind}",
			get(consent::legal).layer(from_fn_with_state("legal", scoped_ip_mw)),
		)
		.layer(from_fn(optional_auth))
}

/// The routes that need a valid access token.
///
/// The `org-admin` routes are **not** a separate router: the re-check needs the store and
/// the active org, so the [`crate::service_api::Auth`] method behind each of those routes
/// opens with `admin_of`, which reloads `memberships.role` from the database.
///
/// This bundle carries its own `saas_core::auth_mw::require_auth`, layered outside the consent
/// gate so the gate sees the `Claims` it reads. The application mounts it as-is and layers no
/// auth middleware of its own; doing so anyway costs nothing but is redundant, because
/// `require_auth` returns early when an outer copy already inserted the `Ctx`.
///
/// Inside that sits `saas_core::ratelimit::AUTHENTICATED`, the account-keyed tier every
/// authenticated call charges. The three routes that re-prove a credential or read the whole
/// database carry a tighter named limit of their own on top of it.
///
/// **The consent gate this applies covers only the routes in this function.** Every other
/// authenticated bundle needs its own: `saas_invoice::routes`' four take a [`RouteGate`]
/// argument ([`consent_gate`]), and a bundle the consumer writes goes through
/// [`consent_gated_router`].
pub fn authenticated() -> Router<App> {
	// Applied to the gated half only, so a route added to `consent_gated` later is gated by
	// construction. A path allowlist inside one middleware would have to be kept in step.
	consent_exempt()
		.merge(consent_gated().layer(from_fn(consent::gate)))
		.layer(from_fn_with_state(AUTHENTICATED, scoped_account_mw))
		.layer(from_fn(require_auth))
}

/// Applies the consent gate to a router the consumer assembled from another crate's bundles.
///
/// `saas-invoice` cannot depend on this crate — dependencies point inward and `saas-auth`
/// owns the `legal_docs` and `consents` tables — so the gate has to be layered at the
/// composition root. This is the one call that does it.
///
/// Every authenticated bundle outside [`authenticated`] must go through this, in particular
/// all four of `saas_invoice::routes`' bundles (`org_read`, `org_parties`,
/// `org_invoices`, `org_services`).
///
/// It re-applies `require_auth` *outside* the gate, because [`consent::gate`] reads `Claims`
/// that only an auth layer above it can have inserted; the bundle's own inner copy then
/// returns early on the `Ctx` this one inserted, so it costs no second `verify`. It does
/// **not** re-apply the authenticated rate-limit tier — that would charge each call twice.
///
/// `saas_invoice::routes`' four bundles take a [`RouteGate`] argument instead, so they cannot
/// be built ungated by accident; [`consent_gate`] is what to hand them. This function stays for
/// a bundle the consumer writes itself.
///
/// ```ignore
/// let app_routes = saas_auth::routes::authenticated()
///     .merge(saas_invoice::routes::org_read(saas_auth::routes::consent_gate()))
///     .merge(saas_auth::routes::consent_gated_router(my_own_bundle()));
/// ```
pub fn consent_gated_router(router: Router<App>) -> Router<App> {
	router.layer(from_fn(consent::gate)).layer(from_fn(require_auth))
}

/// [`consent_gated_router`] as a value, for the bundles that take one.
///
/// `RouteGate::none()` is the explicit opt-out, for a deployment that publishes no legal
/// documents at all.
#[must_use]
pub fn consent_gate() -> RouteGate {
	RouteGate::new(consent_gated_router)
}

/// Reachable while the account owes a consent, because these are what let the user resolve or
/// escape the situation: read who you are, read and accept the outstanding documents, take
/// your data, or close the account. (`GET /api/legal/{kind}` and `POST /api/auth/logout` need
/// no token at all and live in [`public`].)
///
/// `/api/auth/step-up` is here for the same reason, one level down: both destructive exempt
/// routes call `saas_core::auth_mw::require_stepup`, so gating step-up would make the escape unreachable for any
/// session older than `auth.stepup_window`. It is safe — step-up re-proves a credential, it
/// widens no authority, and every other consent-gated route stays gated.
fn consent_exempt() -> Router<App> {
	Router::new()
		.route("/api/auth/me", get(token::me))
		// Re-proves the password: the same budget as login, keyed on the account uid the
		// enclosing `require_auth` put in the `Claims`.
		.route(
			"/api/auth/step-up",
			post(stepup::step_up).layer(from_fn_with_state("step_up", scoped_account_mw)),
		)
		.route("/api/consents", get(consent::list).post(consent::record))
		// A whole-database read per call — the one cost-based exception.
		.route(
			"/api/account/export",
			get(gdpr::export).layer(from_fn_with_state("account_export", scoped_account_mw)),
		)
		.route("/api/account/delete", post(gdpr::delete))
		// Exempt, not gated: these are the two steps `erase_account`'s `E-AUTH-OWNER-ERASURE`
		// demands, so gating them made closing an account conditional on accepting a new ToS.
		// Both still require `owner_of` plus step-up — this removes a precondition, not a check.
		.route("/api/orgs/{uid}", delete(org::delete))
		.route("/api/org/transfer-ownership", post(org::transfer_ownership))
		// Deliberately unscoped, both here and in [`consent_gated`]: `auth_mw` fails an
		// `Actor::Key` closed on a route carrying no `ScopePrefix`, so no key can mint, rename
		// or revoke a key. The 403 is the containment, not a forgotten annotation.
		.route("/api/api-keys", get(apikey::list))
		.route("/api/api-keys/scopes", get(apikey::scopes))
		.route("/api/api-keys/{uid}", patch(apikey::rename).delete(apikey::revoke))
		// Revoking is the emergency: shutting down a leaked key must not wait on a ToS the
		// user has not accepted. Minting stays gated.
		// Passkeys are account-scoped, unlike keys: they are the user's own factor, not an org's
		// machine credential. Listing and renaming grant nothing, so they are exempt; enrolling
		// and removing are in [`consent_gated`].
		.route("/api/auth/wa/credentials", get(wa_credentials))
		.route("/api/auth/wa/credentials/{credentialId}", patch(wa_rename))
		// Login-adjacent, so exempt: approving a login must not be blocked by a ToS published
		// since the phone last accepted one. The approving device is authenticated; the
		// initiating browser never reaches these two. `details` is a named bucket (`qr.details`),
		// because it discloses the initiator's address.
		.route(
			"/api/auth/qr/{sessionId}/details",
			get(qr_details).layer(from_fn_with_state("qr.details", scoped_account_mw)),
		)
		.route("/api/auth/qr/{sessionId}/respond", post(qr_respond))
}

/// Everything else: refused with `403 E-AUTH-CONSENT-REQUIRED` until the outstanding ToS and
/// privacy versions are accepted. See [`consent::gate`].
fn consent_gated() -> Router<App> {
	Router::new()
		// Presenting the current password *is* the step-up here (`reset::change`), so it
		// charges the step-up budget rather than one of its own.
		.route(
			"/api/auth/password",
			post(reset::change).layer(from_fn_with_state("step_up", scoped_account_mw)),
		)
		.route("/api/auth/totp", post(totp::enrol).delete(totp::remove))
		// Confirming enrolment is a second-factor guess, so it shares that budget — but keyed
		// on the account, like its siblings above: the route is authenticated, and the IP-keyed
		// `login.totp` it used to charge is shared with the *public* login and reset routes.
		.route(
			"/api/auth/totp/verify",
			post(totp::verify).layer(from_fn_with_state("login.totp.account", scoped_account_mw)),
		)
		.route("/api/auth/switch-org", post(org::switch))
		.route("/api/orgs", get(org::list).post(org::create))
		.route("/api/org", get(org::get).patch(org::patch))
		// Its own `invite` bucket, not `register`'s 3/h/ip: this layer runs before `add_member`'s
		// `admin_of` check, so sharing let three unauthorized invitations drain self-service
		// registration for a whole address. The listing read stays untouched.
		.route(
			"/api/org/members",
			get(org::members)
				// The DELETE mails nobody, so it carries no `invite` layer.
				.merge(delete(org::remove_member_by_email))
				.merge(post(org::add_member).layer(from_fn_with_state("invite", scoped_ip_mw))),
		)
		.route("/api/org/invites", get(org::invites))
		.route("/api/org/invites/{code}/accept", post(org::accept_invite))
		.route(
			"/api/auth/signup-refs",
			post(org::create_signup_ref).layer(from_fn_with_state("invite", scoped_ip_mw)),
		)
		.route(
			"/api/org/members/{accountUid}",
			patch(org::set_role).delete(org::remove_member),
		)
		.route("/api/consents/{kind}", delete(consent::withdraw))
		.route("/api/api-keys", post(apikey::create))
		.route("/api/auth/wa/register/challenge", post(wa_register_challenge))
		.route("/api/auth/wa/register", post(wa_register))
		.route("/api/auth/wa/credentials/{credentialId}", delete(wa_remove))
}

/// The revocation levers, operator-only and step-up — both gates are
/// [`crate::service_api::Auth`]'s, derived from `ctx.actor`, never from this mounting.
///
/// Not merged into [`authenticated`]: like `saas_invoice::routes::org_services` it is a separate
/// bundle a deployment chooses to expose, and it is deliberately *not* consent-gated — an
/// operator suspending a compromised account must not be blocked by an unaccepted ToS.
pub fn operator() -> Router<App> {
	Router::new()
		.route("/api/admin/accounts/{uid}/status", post(admin_set_status))
		.route("/api/admin/accounts/{uid}/revoke", post(admin_revoke))
		.layer(from_fn_with_state(AUTHENTICATED, scoped_account_mw))
		.layer(from_fn(require_auth))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusBody {
	pub status: crate::store::AccountStatus,
}

/// `POST /api/admin/accounts/{uid}/status`
async fn admin_set_status(
	State(app): State<App>,
	ctx: saas_core::ctx::Ctx,
	UriPath(uid): UriPath<String>,
	Json(body): Json<StatusBody>,
) -> ClResult<StatusCode> {
	crate::service_api::Auth::new(app)
		.set_account_status(&ctx, &uid, body.status)
		.await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/admin/accounts/{uid}/revoke`
async fn admin_revoke(
	State(app): State<App>,
	ctx: saas_core::ctx::Ctx,
	UriPath(uid): UriPath<String>,
) -> ClResult<StatusCode> {
	crate::service_api::Auth::new(app).revoke_tokens(&ctx, &uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- passkeys

/// `GET /api/auth/wa/login/challenge` — reachable signed out, so the `Ctx` may be absent.
async fn wa_login_challenge(
	State(app): State<App>,
	ctx: Option<saas_core::ctx::Ctx>,
) -> ClResult<Json<Value>> {
	let ctx = ctx.unwrap_or_else(|| saas_core::ctx::Ctx::public("wa.login.challenge"));
	let (options, blob) = crate::service_api::Auth::new(app).begin_passkey_login(&ctx).await?;
	Ok(Json(json!({ "options": options, "blob": blob })))
}

/// `POST /api/auth/wa/login`
async fn wa_login(
	State(app): State<App>,
	ctx: Option<saas_core::ctx::Ctx>,
	Json(body): Json<webauthn::LoginBody>,
) -> ClResult<Response> {
	let ctx = ctx.unwrap_or_else(|| saas_core::ctx::Ctx::public("wa.login"));
	let tokens = crate::service_api::Auth::new(app).passkey_login(&ctx, body).await?;
	token::respond(tokens)
}

/// `POST /api/auth/wa/register/challenge`
async fn wa_register_challenge(
	State(app): State<App>,
	ctx: saas_core::ctx::Ctx,
) -> ClResult<Json<Value>> {
	let (options, blob) =
		crate::service_api::Auth::new(app).begin_passkey_registration(&ctx).await?;
	Ok(Json(json!({ "options": options, "blob": blob })))
}

/// `POST /api/auth/wa/register`
async fn wa_register(
	State(app): State<App>,
	ctx: saas_core::ctx::Ctx,
	headers: axum::http::HeaderMap,
	Json(body): Json<webauthn::RegisterBody>,
) -> ClResult<(StatusCode, Json<webauthn::PasskeyView>)> {
	let user_agent = headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok());
	let view = crate::service_api::Auth::new(app)
		.finish_passkey_registration(&ctx, body, user_agent)
		.await?;
	Ok((StatusCode::CREATED, Json(view)))
}

/// `GET /api/auth/wa/credentials`
async fn wa_credentials(
	State(app): State<App>,
	ctx: saas_core::ctx::Ctx,
) -> ClResult<Json<Items<webauthn::PasskeyView>>> {
	Ok(Json(Items { items: crate::service_api::Auth::new(app).list_passkeys(&ctx).await? }))
}

/// `PATCH /api/auth/wa/credentials/{credentialId}`
async fn wa_rename(
	State(app): State<App>,
	ctx: saas_core::ctx::Ctx,
	UriPath(credential_id): UriPath<String>,
	Json(body): Json<webauthn::RenameBody>,
) -> ClResult<StatusCode> {
	crate::service_api::Auth::new(app)
		.rename_passkey(&ctx, &credential_id, &body.name)
		.await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/auth/wa/credentials/{credentialId}`
async fn wa_remove(
	State(app): State<App>,
	ctx: saas_core::ctx::Ctx,
	UriPath(credential_id): UriPath<String>,
) -> ClResult<StatusCode> {
	crate::service_api::Auth::new(app).remove_passkey(&ctx, &credential_id).await?;
	Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- qr login

/// `POST /api/auth/qr/init` — the desktop. Anonymous by construction: the account is whoever
/// approves, so there is nothing to extract and nothing to bind.
async fn qr_init(
	State(app): State<App>,
	ctx: Option<saas_core::ctx::Ctx>,
	headers: axum::http::HeaderMap,
) -> ClResult<Json<qr::InitResponse>> {
	let ctx = ctx.unwrap_or_else(|| saas_core::ctx::Ctx::public("qr.init"));
	let user_agent = headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok());
	Ok(Json(crate::service_api::Auth::new(app).qr_init(&ctx, user_agent)?))
}

/// `GET /api/auth/qr/{sessionId}/status` — the desktop's long poll. The secret travels in a
/// header, never the query string: a URL lands in proxy logs and `Referer`.
//
// ponytail: one open connection per poller for up to 30 s. Fine on the single-process target;
// a fan-out of thousands wants SSE or a websocket instead.
async fn qr_status(
	State(app): State<App>,
	UriPath(session_id): UriPath<String>,
	headers: axum::http::HeaderMap,
	Query(q): Query<qr::QrStatusQuery>,
) -> ClResult<Response> {
	let secret = headers.get("x-qr-secret").and_then(|v| v.to_str().ok()).unwrap_or_default();
	token::respond_qr(
		crate::service_api::Auth::new(app)
			.qr_status(&session_id, secret, q.wait)
			.await?,
	)
}

/// `GET /api/auth/qr/{sessionId}/details` — what the approving phone is about to let in.
/// Authenticated in the service, not only by the bundle: a consumer calling this from Rust with
/// a public `Ctx` must not be able to read a stranger's device details.
async fn qr_details(
	State(app): State<App>,
	ctx: saas_core::ctx::Ctx,
	UriPath(session_id): UriPath<String>,
) -> ClResult<Json<qr::QrDetails>> {
	Ok(Json(crate::service_api::Auth::new(app).qr_details(&ctx, &session_id).await?))
}

/// `POST /api/auth/qr/{sessionId}/respond`
async fn qr_respond(
	State(app): State<App>,
	ctx: saas_core::ctx::Ctx,
	UriPath(session_id): UriPath<String>,
	Json(body): Json<qr::QrRespondBody>,
) -> ClResult<StatusCode> {
	crate::service_api::Auth::new(app)
		.qr_respond(&ctx, &session_id, body.approved, &body.match_code)
		.await?;
	Ok(StatusCode::NO_CONTENT)
}

/// Reaches the application's store.
///
/// The application registers it with
/// `AppBuilder::extension(Arc::new(store) as Arc<dyn AuthStore>)`; every handler in this
/// crate goes through here rather than being generic over the store.
pub fn store(app: &App) -> ClResult<Arc<dyn AuthStore>> {
	app.extensions.get::<Arc<dyn AuthStore>>().cloned().ok_or_else(no_store)
}

pub(crate) fn no_store() -> Error {
	Error::internal("saas-auth: no AuthStore was registered on the app")
}

// vim: ts=4
