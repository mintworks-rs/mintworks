// SPDX-License-Identifier: MPL-2.0
//! The API-key management surface: mint, list, rename, revoke, and the scope listing the
//! mint UI renders.
//!
//! A key is `sk_<8-char prefix>_<43-char base64url>` — 256 bits of server-generated entropy,
//! stored as the SHA-256 hex of the whole string and looked up on the unique `prefix`. It is
//! shown here once and never again; nothing in the framework can recompute it.
//!
//! Every route in this module is deliberately **unscoped**: `/api/api-keys` carries no
//! `ScopePrefix`, and `auth_mw` fails closed on an `Actor::Key` reaching one, so no key can
//! mint, rename or revoke a key. That 403 is the containment, not an oversight.

use axum::Json;
use axum::extract::{Path as UriPath, State};
use axum::http::StatusCode;
use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::org::Items;
use crate::service_api::Auth;

/// `POST /api/api-keys` body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mint {
	pub name: String,
	/// `<prefix>:<read|write>` strings, each naming a route prefix the deployment registered.
	pub scopes: Vec<String>,
	/// Absent means `auth.api_key_max_days` from now; a later instant is refused.
	pub expires_at: Option<Timestamp>,
}

/// `PATCH /api/api-keys/{uid}` body. The key's scopes are frozen at mint — rebuild the key
/// rather than widening a live one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rename {
	pub name: String,
}

/// The mint response, and the only time the plaintext exists outside the caller's memory.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MintedKey {
	pub uid: String,
	pub name: String,
	pub prefix: String,
	pub key: String,
	pub scopes: Vec<String>,
	pub created_at: Timestamp,
	pub expires_at: Option<Timestamp>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyView {
	pub uid: String,
	pub name: String,
	pub prefix: String,
	pub scopes: Vec<String>,
	pub last_used_at: Option<Timestamp>,
	pub expires_at: Option<Timestamp>,
	pub created_at: Timestamp,
}

/// `GET /api/api-keys/scopes` — what a mint may ask for, for the UI to render toggles from.
///
/// Prefixes only: the verb follows the request method at the route.
#[derive(Debug, Serialize)]
pub struct RegisteredScopes {
	pub prefixes: Vec<String>,
}

/// `POST /api/api-keys` — mint. Step-up required.
pub async fn create(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<Mint>,
) -> ClResult<(StatusCode, Json<MintedKey>)> {
	let key = Auth::new(app)
		.create_api_key(&ctx, &req.name, &req.scopes, req.expires_at)
		.await?;
	Ok((StatusCode::CREATED, Json(key)))
}

/// `GET /api/api-keys` — the organisation's live keys.
pub async fn list(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Items<ApiKeyView>>> {
	Ok(Json(Items { items: Auth::new(app).list_api_keys(&ctx).await? }))
}

/// `GET /api/api-keys/scopes`
pub async fn scopes(State(app): State<App>, ctx: Ctx) -> ClResult<Json<RegisteredScopes>> {
	Ok(Json(RegisteredScopes { prefixes: Auth::new(app).api_key_scopes(&ctx).await? }))
}

/// `PATCH /api/api-keys/{uid}` — rename. No step-up: it grants nothing.
pub async fn rename(
	State(app): State<App>,
	ctx: Ctx,
	UriPath(uid): UriPath<String>,
	Json(req): Json<Rename>,
) -> ClResult<StatusCode> {
	Auth::new(app).rename_api_key(&ctx, &uid, &req.name).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/api-keys/{uid}` — revoke. **No** step-up: revocation is the emergency action,
/// and gating it behind a re-auth keeps a leaked key live for the length of it.
pub async fn revoke(
	State(app): State<App>,
	ctx: Ctx,
	UriPath(uid): UriPath<String>,
) -> ClResult<StatusCode> {
	Auth::new(app).revoke_api_key(&ctx, &uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

// vim: ts=4
