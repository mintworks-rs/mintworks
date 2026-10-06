// SPDX-License-Identifier: MPL-2.0
//! `GET /api/plans/offers` (public); `POST /api/plans/quote` / `/checkout`,
//! `GET /api/plans/subscriptions` and `POST …/subscriptions/{uid}/cancel|resume|cancel-change`
//! (authenticated, scoped `plans`); operator: `GET /api/admin/subscriptions`,
//! `POST /api/admin/subscriptions/{uid}/cancel`, `POST /api/admin/offers/{code}/reprice`.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use mintworks_core::app::{App, RouterScopeExt, Scoped};
use mintworks_core::auth_mw::{RouteGate, require_auth};
use mintworks_core::ctx::Ctx;
use mintworks_core::error::{ClResult, Json};
use mintworks_core::ratelimit::{AUTHENTICATED, scoped_account_mw, scoped_ip_mw};
use serde_json::{Value, json};

use crate::admin::{AdminCancelReq, RepriceReq, SubsFilter};
use crate::checkout::{Checkout, CheckoutReq};
use crate::quote::{Quote, QuoteReq};
use crate::service::Plans;
use crate::store::Subscription;

pub fn routes(gate: &RouteGate) -> Scoped {
	let public = Router::new().route(
		"/api/plans/offers",
		get(offers).layer(from_fn_with_state("plans.offers", scoped_ip_mw)),
	);
	let user = gate
		.apply(
			Router::new()
				.route("/api/plans/quote", post(quote))
				.route("/api/plans/checkout", post(checkout))
				.route("/api/plans/subscriptions", get(subscriptions))
				.route("/api/plans/subscriptions/{uid}/cancel", post(cancel))
				.route("/api/plans/subscriptions/{uid}/resume", post(resume))
				.route("/api/plans/subscriptions/{uid}/cancel-change", post(cancel_change)),
		)
		.layer(from_fn_with_state(AUTHENTICATED, scoped_account_mw))
		.layer(from_fn(require_auth));
	let admin = gate
		.apply(
			Router::new()
				.route("/api/admin/subscriptions", get(admin_subscriptions))
				.route("/api/admin/subscriptions/{uid}/cancel", post(admin_cancel))
				.route("/api/admin/offers/{code}/reprice", post(reprice)),
		)
		.layer(from_fn_with_state(AUTHENTICATED, scoped_account_mw))
		.layer(from_fn(require_auth));
	Scoped::from(public).merge(user.scope("plans")).merge(Scoped::from(admin))
}

async fn offers(State(app): State<App>) -> ClResult<Json<Value>> {
	let items = Plans::from_app(&app)?.offers(&Ctx::system("plans")).await?;
	Ok(Json(json!({ "items": items })))
}

async fn quote(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<QuoteReq>,
) -> ClResult<Json<Quote>> {
	Ok(Json(Plans::from_app(&app)?.quote(&ctx, &req).await?))
}

async fn checkout(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<CheckoutReq>,
) -> ClResult<Json<Checkout>> {
	Ok(Json(Plans::from_app(&app)?.checkout(&ctx, &req).await?))
}

async fn subscriptions(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Value>> {
	let items = Plans::from_app(&app)?.subscriptions(&ctx).await?;
	Ok(Json(json!({ "items": items })))
}

async fn cancel(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<Subscription>> {
	Ok(Json(Plans::from_app(&app)?.cancel(&ctx, &uid).await?))
}

async fn resume(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<Subscription>> {
	Ok(Json(Plans::from_app(&app)?.resume(&ctx, &uid).await?))
}

async fn cancel_change(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<Subscription>> {
	Ok(Json(Plans::from_app(&app)?.cancel_change(&ctx, &uid).await?))
}

async fn admin_subscriptions(
	State(app): State<App>,
	ctx: Ctx,
	Query(filter): Query<SubsFilter>,
) -> ClResult<Json<Value>> {
	let items = Plans::from_app(&app)?.admin_subscriptions(&ctx, &filter).await?;
	Ok(Json(json!({ "items": items })))
}

async fn admin_cancel(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(req): Json<AdminCancelReq>,
) -> ClResult<Json<Subscription>> {
	Ok(Json(Plans::from_app(&app)?.admin_cancel(&ctx, &uid, &req).await?))
}

async fn reprice(
	State(app): State<App>,
	ctx: Ctx,
	Path(code): Path<String>,
	Json(req): Json<RepriceReq>,
) -> ClResult<Json<Value>> {
	let items = Plans::from_app(&app)?.reprice(&ctx, &code, &req).await?;
	Ok(Json(json!({ "items": items })))
}

// vim: ts=4
