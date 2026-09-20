//! The HS256 token pair and the login body every credential-presenting route returns.
//!
//! `saas_core::auth_mw` verifies tokens; this module is the only place that signs them.
//! The claim set is [`saas_core::auth_mw::Claims`] — there is deliberately no second
//! definition of it, so the signer and the verifier cannot drift apart.
//!
//! There is no session table. A token stays valid until it expires; the levers that
//! really revoke are `accounts.token_epoch` (one account) and rotating the
//! `auth.jwt_key` secret (everyone).

use std::sync::LazyLock;

use axum::Json;
use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use saas_core::app::App;
use saas_core::auth_mw::{Claims, JWT_SECRET_KEY};
use saas_core::ctx::Ctx;
use saas_core::gencache::GenCache;
use saas_core::prelude::*;
use serde::Serialize;
use serde_json::json;

use crate::store::{Account, AccountOrg, AuthStore, LegalKind, OrgKind, OrgStatus, Role};
use crate::{pow, routes};

/// Access tokens are short because nothing can revoke one early.
pub const ACCESS_TTL_SECONDS: i64 = 900;
/// Sliding: every refresh mints a new one.
pub const REFRESH_TTL_SECONDS: i64 = 7 * 24 * 3600;

/// Cookie names. The body carries the same values, so machine clients ignore these.
/// The access one lives in `saas-core` because `auth_mw` reads it and cannot depend on us.
pub use saas_core::auth_mw::ACCESS_COOKIE;
pub const REFRESH_COOKIE: &str = "refresh_token";

// Only the two documents `register` already makes mandatory gate the API. The
// other `LegalKind`s are contextual to invoicing and would lock every account out of the
// app on day one. Widen this list if a third document ever becomes a precondition.
pub(crate) const GATING_KINDS: [LegalKind; 2] = [LegalKind::Tos, LegalKind::Privacy];

// ---------------------------------------------------------------- wire

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountBody {
	pub uid: String,
	pub email: String,
	pub name: Option<String>,
	pub locale: String,
	pub is_operator: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgBody {
	pub uid: String,
	pub name: String,
	pub kind: OrgKind,
	pub role: Role,
	pub billing_currency: Option<CurrencyCode>,
}

/// One entry of login's `orgs`. Deliberately not [`crate::org::OrgSummary`], which
/// also carries `status`: the login body carries these four fields and no more.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginOrg {
	pub uid: String,
	pub name: String,
	pub kind: OrgKind,
	pub role: Role,
}

/// `GET /api/auth/me` is this body minus the three token fields, which is why they are
/// optional rather than a separate struct.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginBody {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub access_token: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub refresh_token: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub expires_in: Option<i64>,
	pub account: AccountBody,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub org: Option<OrgBody>,
	pub orgs: Vec<LoginOrg>,
	/// Kinds whose current version this account has not accepted. While this is non-empty
	/// every gated route answers `403 E-AUTH-CONSENT-REQUIRED` — see
	/// [`crate::consent::gate`], which enforces it, and [`crate::routes::authenticated`],
	/// which lists the handful of routes that stay reachable so the client can resolve it.
	pub consents_required: Vec<LegalKind>,
}

// ---------------------------------------------------------------- signing

/// Signs a claim set with `secrets["auth.jwt_key"]`, minted on first use.
async fn sign(app: &App, claims: &Claims) -> ClResult<String> {
	let key = pow::hmac_key(app, JWT_SECRET_KEY).await?;
	jsonwebtoken::encode(
		&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
		claims,
		&jsonwebtoken::EncodingKey::from_secret(&key),
	)
	.map_err(|e| Error::internal(format!("jwt encode: {e}")))
}

/// Mints the access/refresh pair. `auth_at` is passed in rather than taken as `now`, so a
/// refresh can carry the original value forward and never manufacture step-up. `None` stays
/// `None`: an impersonation token has no `auth_at`, which is what keeps every step-up route
/// unreachable while impersonating.
pub(crate) async fn mint_pair(
	app: &App,
	account: &Account,
	org: Option<(&OrgId, Role)>,
	auth_at: Option<i64>,
) -> ClResult<(String, String)> {
	let now = Timestamp::now().0;
	let access = Claims {
		sub: account.uid.as_str().to_owned(),
		org: org.map(|(uid, _)| uid.as_str().to_owned()),
		rol: org.map(|(_, role)| role.as_str().to_owned()),
		opr: account.is_root_admin,
		ep: account.token_epoch,
		auth_at,
		imp: None,
		typ: None,
		iat: now,
		exp: now + ACCESS_TTL_SECONDS,
	};
	// The refresh token **does** carry the org: blanked, `refresh` fell back to `pick_org`
	// and an account that switched into org B came back scoped to their personal one, allocating
	// the next invoice number in the wrong org. Revocation happens at the mint instead —
	// `issue_in` re-resolves against a live membership, so the claim is a preference, not an
	// authority.
	let refresh = Claims {
		typ: Some("refresh".to_owned()),
		exp: now + REFRESH_TTL_SECONDS,
		..access.clone()
	};
	Ok((sign(app, &access).await?, sign(app, &refresh).await?))
}

/// The active org on login: the personal one, unless the account has exactly one non-personal
/// membership.
///
/// An unaccepted invitation (`accepted_at IS NULL`) is not a candidate. Any org admin can
/// post any address to `POST /api/org/members`, and without this filter a victim whose
/// only organisation is the attacker's would start creating data inside it on their next
/// login, having agreed to nothing. Switching into it stays available and is an explicit act.
///
/// The root org is never a candidate either: landing an operator on the platform root on every
/// login is not the intent, and every other org inherits from it anyway.
pub(crate) fn pick_org(orgs: &[AccountOrg]) -> Option<&AccountOrg> {
	let live = || {
		orgs.iter().filter(|t| {
			t.status == OrgStatus::Active && t.accepted_at.is_some() && t.kind != OrgKind::Root
		})
	};
	let mut shared = live().filter(|t| t.kind == OrgKind::Shared);
	match (shared.next(), shared.next()) {
		(Some(only), None) => Some(only),
		_ => live().find(|t| t.kind == OrgKind::Personal).or_else(|| live().next()),
	}
}

/// `current_legal_doc` per `(database, kind, locale)`: the consent gate runs one per
/// [`GATING_KINDS`] on every gated request — up to two statements each on the locale-fallback
/// path — and the answer only moves when an operator publishes. Only the version pair is held;
/// `LegalDoc::body` is a megabyte nothing here reads.
///
/// Keyed by `config.db_path` because this is a `static` and a process may hold more than one
/// `App` — the test suite does, and without it one suite's documents gated another's accounts.
static LEGAL_DOCS: LazyLock<GenCache<Option<(String, String)>>> = LazyLock::new(GenCache::new);

/// Called by `Auth::publish_legal_document`. Whole-cache, not per key: `current_legal_doc` falls
/// back across locales, so publishing one moves the answer for every locale of that kind.
pub(crate) fn invalidate_legal_docs() {
	LEGAL_DOCS.clear();
}

/// `(version, sha256)` of the current document, through [`LEGAL_DOCS`]. `None` is cached too:
/// `consents_required` fails closed on an unpublished kind, and that path is the common one on
/// a deployment that publishes in one locale only.
async fn current_doc_version(
	app: &App,
	store: &dyn AuthStore,
	kind: LegalKind,
	locale: &str,
	now: Timestamp,
) -> ClResult<Option<(String, String)>> {
	let key = format!("{}/{}/{locale}", app.config.db_path, kind.as_str());
	let miss = match LEGAL_DOCS.lookup(&key) {
		Ok(hit) => return Ok(hit),
		Err(miss) => miss,
	};
	let doc = store.current_legal_doc(kind, locale, now).await?.map(|d| (d.version, d.sha256));
	LEGAL_DOCS.store(&key, miss, doc.clone());
	Ok(doc)
}

/// The `consentsRequired` list: gating kinds whose current document in the account's
/// locale has not been granted, or was granted against a superseded version.
pub(crate) async fn consents_required(
	app: &App,
	store: &dyn AuthStore,
	account: &Account,
) -> ClResult<Vec<LegalKind>> {
	let now = Timestamp::now();
	let mut out = Vec::new();
	for kind in GATING_KINDS {
		// Fail closed: nothing to consent to is not a waiver, so a gating kind published in **no**
		// locale gates the whole router rather than reporting the account as owing nothing.
		// Deliberate, and what the `warn!` is for.
		let Some((version, sha256)) =
			current_doc_version(app, store, kind, &account.locale, now).await?
		else {
			tracing::warn!(
				kind = ?kind,
				locale = %account.locale,
				"no legal document published in this locale; consent required by default"
			);
			out.push(kind);
			continue;
		};
		let accepted = store.latest_consent(account.id, kind, None).await?.is_some_and(|c| {
			// `doc_sha256` too: `current_legal_doc` falls back to any published locale, so
			// a consent recorded against that fallback text satisfied the version alone
			// once the account's own locale was published under it — different wording.
			c.granted
				&& c.withdrawn_at.is_none()
				&& c.doc_version == version
				&& c.doc_sha256 == sha256
		});
		if !accepted {
			out.push(kind);
		}
	}
	Ok(out)
}

// ---------------------------------------------------------------- responses

fn cookie(name: &str, value: &str, max_age: i64, path: &str) -> String {
	format!("{name}={value}; HttpOnly; Secure; SameSite=Lax; Path={path}; Max-Age={max_age}")
}

/// The refresh cookie is read in exactly one place, `login::refresh`, so that route is the
/// widest scope it ever needed — at `Path=/` the strongest credential in a system with no
/// denylist rode along with every API call and every proxy log. Must match at both call
/// sites, or the clearing cookie misses the one the browser holds.
const REFRESH_PATH: &str = "/api/auth/refresh";

fn set_cookie(resp: &mut Response, raw: &str) -> ClResult<()> {
	let value =
		HeaderValue::from_str(raw).map_err(|e| Error::internal(format!("cookie header: {e}")))?;
	resp.headers_mut().append(SET_COOKIE, value);
	Ok(())
}

/// Clears both token cookies. Logout is client-side; this only removes the browser's copy.
pub(crate) fn clear_cookies(resp: &mut Response) -> ClResult<()> {
	set_cookie(resp, &cookie(ACCESS_COOKIE, "", 0, "/"))?;
	set_cookie(resp, &cookie(REFRESH_COOKIE, "", 0, REFRESH_PATH))
}

/// A freshly minted pair plus the §4.2 body, as data.
///
/// The data half of `login`/`activate`/`refresh`: [`crate::service_api::Auth`] returns this,
/// and [`respond`] is what turns it into the HTTP answer with its two cookies. A consumer
/// calling `Auth::login` from Rust gets the tokens without an axum `Response` in the way.
#[derive(Debug)]
pub struct Tokens {
	pub access_token: String,
	pub refresh_token: String,
	pub expires_in: i64,
	pub body: LoginBody,
}

/// [`Tokens`] as the §4.2 HTTP answer: the body, plus both `HttpOnly` cookies.
pub fn respond(tokens: Tokens) -> ClResult<Response> {
	let mut resp = (StatusCode::OK, Json(tokens.body)).into_response();
	set_cookie(&mut resp, &cookie(ACCESS_COOKIE, &tokens.access_token, ACCESS_TTL_SECONDS, "/"))?;
	set_cookie(
		&mut resp,
		&cookie(REFRESH_COOKIE, &tokens.refresh_token, REFRESH_TTL_SECONDS, REFRESH_PATH),
	)?;
	Ok(resp)
}

/// A QR poll's outcome as the SPA reads it: an approval is the login body and both cookies —
/// the shape `POST /api/auth/login` already answers — and the other two are a bare status.
pub(crate) fn respond_qr(status: crate::qr::Status) -> ClResult<Response> {
	Ok(match status {
		crate::qr::Status::Pending => Json(json!({ "status": "pending" })).into_response(),
		crate::qr::Status::Denied => Json(json!({ "status": "denied" })).into_response(),
		crate::qr::Status::Approved(tokens) => respond(*tokens)?,
	})
}

/// [`respond`] for the two routes that mint an **access** token only — `step-up` and
/// `switch-org`. The refresh cookie is deliberately untouched: neither route extends a
/// session. Without this a cookie-authenticated browser kept the stale `auth_at` and every
/// `require_stepup` route stayed unreachable, and kept operating in the previous org after
/// a switch.
pub fn respond_access<T: serde::Serialize>(body: T, access_token: &str) -> ClResult<Response> {
	let mut resp = (StatusCode::OK, Json(body)).into_response();
	set_cookie(&mut resp, &cookie(ACCESS_COOKIE, access_token, ACCESS_TTL_SECONDS, "/"))?;
	Ok(resp)
}

/// The §4.2 login body carrying an explicit `auth_at` — `refresh` passes the original
/// value through so refreshing can never satisfy a step-up check.
pub(crate) async fn issue(app: &App, account: &Account, auth_at: Option<i64>) -> ClResult<Tokens> {
	issue_in(app, account, auth_at, None).await
}

/// [`issue`], but minting against the org the caller was already working in.
///
/// `prefer` is the `org` uid off the spent refresh token. It is resolved against a **live**
/// membership — accepted, and in an `ACTIVE` org — and a `prefer` that no longer resolves
/// yields no org at all rather than falling back to `pick_org`: a revoked or suspended
/// membership must not be silently swapped for a different one. Only an absent `prefer`
/// re-picks the default.
pub(crate) async fn issue_in(
	app: &App,
	account: &Account,
	auth_at: Option<i64>,
	prefer: Option<&str>,
) -> ClResult<Tokens> {
	let store = routes::store(app)?;
	let orgs = store.orgs_for_account(account.id).await?;
	// `orgs` is direct memberships only, so `prefer` has to go through the ancestor walk: a
	// switch may have entered an org reached only through a role on an ancestor.
	let active: Option<(OrgId, Role)> = match prefer {
		Some(uid) => match app.store.org_membership_role(account.id, uid).await? {
			Some((id, role)) => store.org_by_id(id).await?.map(|o| (o.uid, role)),
			None => None,
		},
		None => pick_org(&orgs).map(|t| (t.uid.clone(), t.role)),
	};
	let (access, refresh) =
		mint_pair(app, account, active.as_ref().map(|(uid, role)| (uid, *role)), auth_at).await?;

	// The summary list has no billing currency, so the active org is re-read in full.
	let org = match active.as_ref() {
		Some((uid, role)) => store.org_by_uid(uid).await?.map(|full| OrgBody {
			uid: full.uid.into_string(),
			name: full.name,
			kind: full.kind,
			role: *role,
			billing_currency: full.billing_currency,
		}),
		None => None,
	};

	let body = LoginBody {
		access_token: Some(access.clone()),
		refresh_token: Some(refresh.clone()),
		expires_in: Some(ACCESS_TTL_SECONDS),
		account: account_body(account),
		org,
		orgs: summaries(orgs),
		consents_required: consents_required(app, store.as_ref(), account).await?,
	};
	Ok(Tokens {
		access_token: access,
		refresh_token: refresh,
		expires_in: ACCESS_TTL_SECONDS,
		body,
	})
}

pub(crate) fn account_body(account: &Account) -> AccountBody {
	AccountBody {
		uid: account.uid.as_str().to_owned(),
		email: account.email.clone(),
		name: account.name.clone(),
		locale: account.locale.clone(),
		is_operator: account.is_root_admin,
	}
}

pub(crate) fn summaries(orgs: Vec<AccountOrg>) -> Vec<LoginOrg> {
	orgs.into_iter()
		.map(|t| LoginOrg { uid: t.uid.into_string(), name: t.name, kind: t.kind, role: t.role })
		.collect()
}

/// `GET /api/auth/me` — the login body minus the tokens. The client's cheap
/// "who am I, what orgs, what consents are outstanding" call.
pub async fn me(State(app): State<App>, ctx: Ctx) -> ClResult<Json<LoginBody>> {
	Ok(Json(crate::service_api::Auth::new(app).me(&ctx).await?))
}

// vim: ts=4
