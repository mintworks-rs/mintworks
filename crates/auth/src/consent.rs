//! Versioned legal texts and the acceptance record.
//!
//! A consent row is evidence about **specific wording**, so it copies the document's `version` and
//! `sha256` rather than only referencing `legal_docs`, and it captures the IP and user agent.
//! 45/2014. (II. 26.) Korm. rendelet 29. § (1) m) makes the withdrawal waiver a declaration that
//! the consumer *acknowledged* losing the 20. § right, and "the user ticked a box" is not evidence
//! unless the wording is recoverable years later.
//!
//! Consent rows are never deleted — not even by a GDPR erasure, see [`crate::gdpr`].

use axum::extract::{Path, Query, Request, State};
use axum::http::header::USER_AGENT;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use mintworks_core::app::App;
use mintworks_core::auth_mw::Claims;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::org::Items;
use crate::service_api::{Auth, ConsentGrant};
use crate::store::LegalKind;
use crate::{login, routes};

/// Every consentable kind, in the order `GET /api/consents` returns them.
pub const KINDS: [LegalKind; 4] = [
	LegalKind::Tos,
	LegalKind::Privacy,
	LegalKind::EInvoice,
	LegalKind::WithdrawalWaiver,
];

/// Withdrawing either of these would leave an active account using the service under no
/// terms and no privacy notice. The way out of those two is `POST /api/account/delete`.
pub(crate) const UNWITHDRAWABLE: [LegalKind; 2] = [LegalKind::Tos, LegalKind::Privacy];

// ---------------------------------------------------------------- wire

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegalBody {
	pub kind: LegalKind,
	pub locale: String,
	pub version: String,
	pub title: String,
	/// Verbatim Markdown, as presented — the consent record is about this text.
	pub body: String,
	pub sha256: String,
	pub effective_from: Timestamp,
}

/// What [`crate::service_api::Auth::publish_legal_document`] takes.
///
/// No `sha256`: it is computed over `body` inside the handle, so a caller cannot record a
/// hash that does not match the text it publishes — every consent row copies that hash as
/// evidence about specific wording.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishLegalDoc {
	pub kind: LegalKind,
	pub locale: String,
	pub version: String,
	pub title: String,
	/// Markdown, verbatim as it will be presented.
	pub body: String,
	pub effective_from: Timestamp,
}

/// What publishing answers with — the body is left out, the caller just sent it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegalDocSummary {
	pub kind: LegalKind,
	pub locale: String,
	pub version: String,
	pub sha256: String,
	pub effective_from: Timestamp,
}

/// `ip` and `user_agent` are stored as evidence and deliberately never returned.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentBody {
	pub kind: LegalKind,
	pub doc_version: String,
	pub doc_sha256: String,
	pub granted: bool,
	pub at: Timestamp,
	pub withdrawn_at: Option<Timestamp>,
	pub org_uid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LocaleQuery {
	#[serde(default)]
	pub locale: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentRequest {
	pub kind: LegalKind,
	pub version: String,
	/// The `sha256` of the document the client actually presented. **Required** — it is the
	/// only thing that ties the stored evidence to the wording the user read; see
	/// [`crate::service_api::ConsentGrant`]. `Option` on the wire so that omitting it is a
	/// clean `E-CORE-VALIDATION` from `record_consent` rather than a deserialisation failure
	/// with no `errCode`, exactly as [`parse_kind`] spells its path segment out.
	#[serde(default)]
	pub doc_sha256: Option<String>,
	#[serde(default)]
	pub org_uid: Option<String>,
}

/// Drop the `current_legal_doc` answers [`gate`] holds. [`crate::service_api::Auth::
/// publish_legal_document`] calls it itself; a consumer writing `legal_docs` through
/// [`crate::store::AuthStore::insert_legal_doc`] directly must, or the gate keeps serving the
/// superseded version for up to the cache TTL.
pub fn invalidate_document_cache() {
	crate::token::invalidate_legal_docs();
}

/// The path segment is spelled out rather than routed through serde, so an unknown kind is
/// a validation error instead of a deserialisation failure with no `errCode`.
pub(crate) fn parse_kind(s: &str) -> ClResult<LegalKind> {
	match s {
		"TOS" => Ok(LegalKind::Tos),
		"PRIVACY" => Ok(LegalKind::Privacy),
		"EINVOICE" => Ok(LegalKind::EInvoice),
		"WITHDRAWAL_WAIVER" => Ok(LegalKind::WithdrawalWaiver),
		_ => Err(Error::validation("unknown legal document kind")),
	}
}

// ---------------------------------------------------------------- handlers

/// `GET /api/legal/{kind}?locale=hu` — public. The row currently in force: the highest
/// version whose `effective_from` has passed.
/// The `Ctx` is optional so the route stays anonymous-reachable while still serving a
/// signed-in caller their own locale — `record_consent` anchors on `account.locale`, so
/// without it a `hu` account that omits `?locale=` reads English and stores the Hungarian
/// row's hash.
pub async fn legal(
	State(app): State<App>,
	ctx: Option<Ctx>,
	Path(kind): Path<String>,
	Query(q): Query<LocaleQuery>,
) -> ClResult<Json<LegalBody>> {
	let ctx = ctx.unwrap_or_else(|| Ctx::public("auth.legal"));
	let doc = Auth::new(app)
		.legal_document(&ctx, parse_kind(&kind)?, q.locale.as_deref())
		.await?;
	Ok(Json(doc))
}

/// `GET /api/consents` — the latest row per kind for the calling account.
pub async fn list(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Items<ConsentBody>>> {
	Ok(Json(Items { items: Auth::new(app).list_consents(&ctx).await? }))
}

/// `POST /api/consents` — records an acceptance of the wording currently in force. A
/// `version` that is not the one in force is a conflict: an account cannot consent to
/// superseded text, because the row would then be evidence about something it never saw.
pub async fn record(
	State(app): State<App>,
	ctx: Ctx,
	headers: HeaderMap,
	Json(req): Json<ConsentRequest>,
) -> ClResult<(StatusCode, Json<ConsentBody>)> {
	let grant = ConsentGrant {
		kind: req.kind,
		version: req.version,
		doc_sha256: req.doc_sha256,
		org_uid: req.org_uid,
		user_agent: headers.get(USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_owned),
	};
	let recorded = Auth::new(app).record_consent(&ctx, &grant).await?;
	Ok((StatusCode::CREATED, Json(recorded)))
}

/// `DELETE /api/consents/{kind}` — stamps `withdrawn_at`. The row itself stays: it is
/// evidence, and §8.2 keeps `consents` out of every erasure path.
pub async fn withdraw(
	State(app): State<App>,
	Path(kind): Path<String>,
	ctx: Ctx,
) -> ClResult<StatusCode> {
	Auth::new(app).withdraw_consent(&ctx, parse_kind(&kind)?).await?;
	Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- the gate

/// Refuses every gated route while the account owes a consent.
///
/// In `mintworks-auth`, not `mintworks-core`, because `legal_docs` and `consents` are ours. Mount
/// it *inside* `mintworks_core::auth_mw`, whose `Claims` and `App` extensions it reads. Both are
/// required: it used to fall through when either was missing, so mounting it outside the auth
/// layer silently removed the gate from every route behind it. (`App` comes from the
/// extension, not `State`, because a route bundle is built before any `App` exists.)
///
/// [`crate::routes::authenticated`] applies it to the gated sub-router only, so a route added
/// there later is gated by construction rather than by remembering to list it.
///
/// **That covers `mintworks-auth`'s own routes and nothing else.** It reads request extensions
/// only, so it layers onto any router — and every other authenticated bundle needs it, or an
/// account owing a newly published ToS is blocked from `/api/orgs` and can still issue a
/// numbered legal invoice. [`crate::routes::consent_gated_router`] is how a consumer does it.
///
/// Two `latest_consent` reads per gated request, plus the account re-read `account_from_claims`
/// does for `token_epoch` — that one is the revocation compensation, not overhead.
/// `current_legal_doc` is cached per `(kind, locale)` in [`crate::token`].
pub async fn gate(req: Request, next: Next) -> Response {
	let Some(app) = req.extensions().get::<App>().cloned() else {
		return Error::internal("consent gate mounted without the App extension").into_response();
	};
	let Some(claims) = req.extensions().get::<Claims>().cloned() else {
		return Error::coded(
			StatusCode::UNAUTHORIZED,
			"E-AUTH-TOKEN",
			"the consent gate is mounted outside an auth layer",
		)
		.into_response();
	};
	match outstanding(&app, &claims).await {
		Ok(kinds) if kinds.is_empty() => next.run(req).await,
		Ok(kinds) => Error::coded(
			StatusCode::FORBIDDEN,
			"E-AUTH-CONSENT-REQUIRED",
			// Named in the message so an operator reading a log knows which. The client's
			// machine-readable copy is `consentsRequired` on `GET /api/auth/me`, which is
			// exempt from this gate exactly so it stays reachable here.
			format!(
				"these documents must be accepted first: {}",
				kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")
			),
		)
		.into_response(),
		Err(e) => e.into_response(),
	}
}

async fn outstanding(app: &App, claims: &Claims) -> ClResult<Vec<LegalKind>> {
	let store = routes::store(app)?;
	let account = login::account_from_claims(store.as_ref(), claims).await?;
	crate::token::consents_required(app, store.as_ref(), &account).await
}

// vim: ts=4
