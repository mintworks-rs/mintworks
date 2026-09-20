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
use crate::webauthn::StepUpProof;

#[derive(Debug, Deserialize)]
pub struct StepUpRequest {
	#[serde(default)]
	pub password: Option<String>,
	#[serde(default)]
	pub code: Option<String>,
	/// The passkey half: `{blob, assertion}` instead of `{password, code}`. Both members
	/// are optional so that sending neither is a `401`, not a `400` from serde.
	#[serde(default)]
	pub blob: Option<String>,
	#[serde(default)]
	pub assertion: Option<webauthn_rs::prelude::PublicKeyCredential>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepUpResponse {
	pub access_token: String,
	pub expires_in: i64,
}

/// `POST /api/auth/step-up` — every factor `login` would demand (the password, plus a TOTP
/// code where one is confirmed), or an assertion from a passkey of the calling account, buys a
/// new access token with `auth_at = now`. The refresh token is untouched, so this cannot extend
/// a session.
///
/// The passkey half takes the blob the **public login challenge** mints, not one of its own:
/// that challenge carries no account, and the account is established the way login establishes
/// it — the assertion names a credential, and `Auth::step_up_passkey` requires that credential
/// to belong to the caller.
pub async fn step_up(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<StepUpRequest>,
) -> ClResult<Response> {
	let StepUpRequest { password, code, blob, assertion } = req;
	let proof = match (blob, assertion) {
		(Some(blob), Some(assertion)) => Some(StepUpProof { blob, assertion }),
		(None, None) => None,
		_ => return Err(Error::validation("blob and assertion are sent together")),
	};
	let out = Auth::new(app).step_up(&ctx, password, code.as_deref(), proof).await?;
	let access = out.access_token.clone();
	token::respond_access(out, &access)
}

// vim: ts=4
