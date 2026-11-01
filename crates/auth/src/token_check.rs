// SPDX-License-Identifier: MPL-2.0
//! `POST /api/auth/tokens/check`: whether a mailed signed token would open, without spending
//! it, so a page can refuse a dead link before the user fills in its form.

use axum::extract::State;
use mintworks_core::app::App;
use mintworks_core::auth_mw::ClientIp;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::service_api::Auth;

/// Every signed token this crate mints; a new one is a new arm.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenPurpose {
	Activate,
	Reset,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCheck {
	/// `reset` only: the account has a confirmed TOTP, so the reset will ask for a code.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub totp_required: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct CheckBody {
	pub purpose: TokenPurpose,
	pub token: String,
}

/// POST so the token stays out of URLs and request logs, as on `/api/auth/activate`.
pub async fn check(
	State(app): State<App>,
	ClientIp(ip): ClientIp,
	Json(req): Json<CheckBody>,
) -> ClResult<Json<TokenCheck>> {
	let ctx = Ctx::public("auth.tokens.check").with_ip(ip);
	Ok(Json(Auth::new(app).check_token(&ctx, req.purpose, &req.token).await?))
}

// vim: ts=4
