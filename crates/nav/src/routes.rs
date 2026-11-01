// SPDX-License-Identifier: MPL-2.0
//! The one route bundle: a tenant seller's NAV connection. Each handler makes one [`Nav`] call.

use axum::extract::State;
use axum::routing::{get, put};
use axum::{Json, Router};
use mintworks_core::auth_mw::RouteGate;
use mintworks_core::{App, ctx::Ctx, prelude::*};

use crate::service_api::{Nav, NavCredentials, NavCredentialsStatus};

/// `GET`/`PUT /api/nav/credentials` — Admin on the acting org's seller org; the `PUT` is
/// step-up gated and verified with NAV before anything is stored. `PUT /api/nav/live` ends
/// test mode: Owner, step-up, credentials connected.
pub fn org_credentials(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/nav/credentials", get(credentials_status).put(set_credentials))
		.route("/api/nav/live", put(go_live))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

async fn credentials_status(
	State(app): State<App>,
	ctx: Ctx,
) -> ClResult<Json<NavCredentialsStatus>> {
	Ok(Json(Nav::new(app).credentials_status(&ctx).await?))
}

async fn set_credentials(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<NavCredentials>,
) -> ClResult<Json<NavCredentialsStatus>> {
	Ok(Json(Nav::new(app).set_credentials(&ctx, &body).await?))
}

/// `PUT /api/nav/live` — no body; 204.
async fn go_live(State(app): State<App>, ctx: Ctx) -> ClResult<axum::http::StatusCode> {
	Nav::new(app).go_live(&ctx).await?;
	Ok(axum::http::StatusCode::NO_CONTENT)
}

// vim: ts=4
