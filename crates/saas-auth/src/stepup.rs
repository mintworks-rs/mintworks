//! Step-up re-authentication: the first of the two mitigations that stand in for the session table
//! this design does not have.
//!
//! A destructive route requires that the credential was presented recently — `auth_at`
//! within `auth.stepup_window` — rather than merely that the token is unexpired.

use axum::extract::State;
use axum::response::Response;
use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::service_api::Auth;
use crate::token;

#[derive(Debug, Deserialize)]
pub struct StepUpRequest {
	#[serde(default)]
	pub password: Option<String>,
	#[serde(default)]
	pub code: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepUpResponse {
	pub access_token: String,
	pub expires_in: i64,
}

/// `POST /api/auth/step-up` — every factor `login` would demand (the password, plus a TOTP
/// code where one is confirmed) buys a new access token with `auth_at = now`. The refresh
/// token is untouched, so this cannot extend a session.
pub async fn step_up(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<StepUpRequest>,
) -> ClResult<Response> {
	let out = Auth::new(app).step_up(&ctx, req.password, req.code.as_deref()).await?;
	let access = out.access_token.clone();
	token::respond_access(out, &access)
}

// vim: ts=4
