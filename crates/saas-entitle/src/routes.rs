//! `GET /api/entitlements` (authenticated, scoped `entitlements`), and the operator's
//! `POST`/`GET /api/admin/orgs/{uid}/grants`. The operator gate is in the service methods, so a
//! consumer calling the handle directly is gated the same.

use axum::Router;
use axum::extract::{Path, State};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::get;
use saas_core::app::{App, RouterScopeExt, Scoped};
use saas_core::auth_mw::{RouteGate, require_auth};
use saas_core::ctx::Ctx;
use saas_core::error::{ClResult, Json, StatusCode};
use saas_core::ratelimit::{AUTHENTICATED, scoped_account_mw};
use serde_json::{Value, json};

use crate::service::{AdminGrant, Entitle, Summary};
use crate::store::Grant;

pub fn routes(gate: &RouteGate) -> Scoped {
	let user = gate
		.apply(Router::new().route("/api/entitlements", get(summary)))
		.layer(from_fn_with_state(AUTHENTICATED, scoped_account_mw))
		.layer(from_fn(require_auth));
	let admin = gate
		.apply(Router::new().route("/api/admin/orgs/{uid}/grants", get(list).post(grant)))
		.layer(from_fn_with_state(AUTHENTICATED, scoped_account_mw))
		.layer(from_fn(require_auth));
	Scoped::from(admin).merge(user.scope("entitlements"))
}

async fn summary(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Summary>> {
	Ok(Json(Entitle::from_app(&app)?.summary(&ctx).await?))
}

async fn grant(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(req): Json<AdminGrant>,
) -> ClResult<(StatusCode, Json<Grant>)> {
	Ok((StatusCode::CREATED, Json(Entitle::from_app(&app)?.admin_grant(&ctx, &uid, &req).await?)))
}

async fn list(State(app): State<App>, ctx: Ctx, Path(uid): Path<String>) -> ClResult<Json<Value>> {
	let items = Entitle::from_app(&app)?.admin_grants(&ctx, &uid).await?;
	Ok(Json(json!({ "items": items })))
}

// vim: ts=4
