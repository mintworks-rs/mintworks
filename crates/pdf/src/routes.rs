// SPDX-License-Identifier: MPL-2.0
//! `GET /api/documents/{uid}` and its PDF. Handlers decide nothing; [`Documents`] confines
//! both to the caller's org.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::error::ClResult;

use crate::service::{DocView, Documents};

/// `GET /api/documents/{uid}`
pub async fn get_doc(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<axum::Json<DocView>> {
	Ok(axum::Json(Documents::from_app(&app)?.get(&ctx, &uid).await?))
}

/// `GET /api/documents/{uid}/pdf`
pub async fn pdf(State(app): State<App>, ctx: Ctx, Path(uid): Path<String>) -> ClResult<Response> {
	let bytes = Documents::from_app(&app)?.read(&ctx, &uid).await?;
	Ok(([(header::CONTENT_TYPE, "application/pdf")], bytes).into_response())
}

/// Authenticated; the mount decides any further gate.
pub fn routes() -> Router<App> {
	Router::new()
		.route("/api/documents/{uid}", get(get_doc))
		.route("/api/documents/{uid}/pdf", get(pdf))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth))
}

// vim: ts=4
