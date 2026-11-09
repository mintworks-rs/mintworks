// SPDX-License-Identifier: MPL-2.0
//! `GET`/`PUT /api/account/settings/{key}` reads and writes the caller's own value of an `app.*`
//! key the application declared with `Scope::Account`, written only if declared `user_writable`.
//! `PATCH /api/account` sets the caller's handle, display name and locale.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use mintworks_core::settings::SettingValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::service_api::Auth;
use crate::token::AccountBody;

/// Handles nobody may choose, on top of [`mintworks_core::refs::RESERVED_SLUGS`]: they read as
/// the platform speaking.
pub const RESERVED_HANDLES: &[&str] = &[
	"root",
	"system",
	"support",
	"me",
	"operator",
	"staff",
	"help",
	"official",
	"anonymous",
	"null",
	"undefined",
];

/// `PATCH /api/account`. `handle: null` clears it; `name` and `locale` can only be replaced.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountPatch {
	#[serde(default)]
	pub handle: Patch<String>,
	/// Trimmed; 1..=200 characters, no control characters.
	#[serde(default)]
	pub name: Option<String>,
	/// A bare tag like `hu` or `en-GB`: it picks the account's email templates.
	#[serde(default)]
	pub locale: Option<String>,
}

/// A display name as `PATCH /api/account` stores it: trimmed, 1..=200 characters, no control
/// characters (a name lands in email headers and greetings).
///
/// # Errors
/// `E-CORE-VALIDATION` otherwise.
pub fn validate_name(raw: &str) -> ClResult<String> {
	let name = raw.trim();
	let len = name.chars().count();
	if len == 0 || len > 200 || name.chars().any(char::is_control) {
		return Err(Error::validation("name must be 1-200 characters without control characters"));
	}
	Ok(name.to_owned())
}

/// One entry of `Auth::handles`: public by definition, so never an email or a name.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleBody {
	pub uid: AccountId,
	pub handle: String,
}

/// `refs::validate_slug` (trimmed, lowercased, `^[a-z0-9][a-z0-9-]{2,31}$`, not a reserved slug)
/// plus [`RESERVED_HANDLES`].
///
/// # Errors
/// `E-CORE-SLUG-INVALID`.
pub fn validate_handle(raw: &str) -> ClResult<String> {
	let h = mintworks_core::refs::validate_slug(raw)?;
	if RESERVED_HANDLES.contains(&h.as_str()) {
		return Err(Error::coded(
			StatusCode::UNPROCESSABLE_ENTITY,
			"E-CORE-SLUG-INVALID",
			"that handle is reserved",
		));
	}
	Ok(h)
}

#[derive(Serialize)]
pub struct SettingBody {
	pub value: SettingValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingRequest {
	/// `null` (or absent) deletes the caller's row, so the read falls back to the global value.
	#[serde(default)]
	pub value: Option<Value>,
}

/// `GET /api/account/settings/{key}`.
pub async fn get_setting(
	State(app): State<App>,
	Path(key): Path<String>,
	ctx: Ctx,
) -> ClResult<Json<SettingBody>> {
	Ok(Json(SettingBody { value: Auth::new(app).account_setting(&ctx, &key).await? }))
}

/// `PUT /api/account/settings/{key}` — body `{"value": …|null}`.
pub async fn put_setting(
	State(app): State<App>,
	Path(key): Path<String>,
	ctx: Ctx,
	Json(req): Json<SettingRequest>,
) -> ClResult<StatusCode> {
	Auth::new(app).set_account_setting(&ctx, &key, req.value.as_ref()).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/account` — body `{"handle": "…"|null}`; answers the updated account.
pub async fn patch(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<AccountPatch>,
) -> ClResult<Json<AccountBody>> {
	Ok(Json(Auth::new(app).update_account(&ctx, &req).await?))
}

// vim: ts=4
