// SPDX-License-Identifier: MPL-2.0
//! The route bundles: a tenant seller's NAV connection and filing retry, and the operator's
//! filing queue. Each handler makes one [`Nav`] call.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use mintworks_core::auth_mw::RouteGate;
use mintworks_core::{App, ctx::Ctx, prelude::*};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::service_api::{Nav, NavCredentials, NavCredentialsStatus};
use crate::submission::{NavArchive, NavFiling};

/// `GET`/`PUT /api/nav/credentials` — Admin on the acting org's seller org; the `PUT` is
/// step-up gated and verified with NAV before anything is stored. `PUT /api/nav/live` ends
/// test mode: Owner, step-up, credentials connected. `POST /api/invoices/{uid}/nav/retry` is
/// [`Nav::retry`]: Admin on the seller's org, with the invoice's org selected, step-up gated.
pub fn org_credentials(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/nav/credentials", get(credentials_status).put(set_credentials))
		.route("/api/nav/live", put(go_live))
		.route("/api/invoices/{uid}/nav/retry", post(retry))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

/// The operator's filing queue and its actions. `{uid}` is the invoice's, except on
/// `nav-sellers`, where it is the seller's. `POST …/{submit,cancel,release-hold}` take no body,
/// `resolve` and `confirm-not-filed` take `{note}`, `attach` takes `{transactionId}`; all answer
/// 204.
///
/// No gate is here: `require_operator`, and `require_stepup` where it applies, open each
/// [`Nav`] method, so a consumer calling the handle directly is still gated.
pub fn operator(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/admin/nav-submissions", get(list_filings))
		.route("/api/admin/nav-submissions/{uid}/archives", get(filing_archives))
		.route("/api/admin/nav-submissions/{uid}/submit", post(submit))
		.route("/api/admin/nav-submissions/{uid}/cancel", post(cancel_filing))
		.route("/api/admin/nav-submissions/{uid}/resolve", post(resolve_filing))
		.route("/api/admin/nav-submissions/{uid}/attach", post(attach_transaction))
		.route("/api/admin/nav-submissions/{uid}/confirm-not-filed", post(confirm_not_filed))
		.route("/api/admin/nav-sellers/{uid}/release-hold", post(release_seller_hold))
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

/// `POST /api/invoices/{uid}/nav/retry` — no body; the filing record after the re-drive.
async fn retry(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<Option<serde_json::Value>>> {
	Ok(Json(Nav::new(app).retry(&ctx, &uid).await?.as_ref().map(filing_json)))
}

#[derive(Debug, Default, Deserialize)]
struct FilingQuery {
	state: Option<String>,
	after: Option<i64>,
	limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct NoteBody {
	note: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachBody {
	transaction_id: String,
}

/// `GET /api/admin/nav-submissions` — `{items, nextCursor}`, each item a filing plus its
/// `invoiceUid`. `nextCursor` goes back as `after`, and is null only on an empty page.
async fn list_filings(
	State(app): State<App>,
	ctx: Ctx,
	Query(q): Query<FilingQuery>,
) -> ClResult<Json<Value>> {
	let page = Nav::new(app).list_filings(&ctx, q.state.as_deref(), q.after, q.limit).await?;
	let items: Vec<Value> = page
		.items
		.iter()
		.map(|(uid, f)| {
			let mut item = filing_json(f);
			item["invoiceUid"] = json!(uid.as_str());
			item
		})
		.collect();
	Ok(Json(json!({ "items": items, "nextCursor": page.next_cursor })))
}

/// `GET /api/admin/nav-submissions/{uid}/archives` — `[{requestXml, responseXml}]`, newest first.
async fn filing_archives(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<Vec<NavArchive>>> {
	Ok(Json(Nav::new(app).filing_archives(&ctx, &uid).await?))
}

async fn submit(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<axum::http::StatusCode> {
	Nav::new(app).submit(&ctx, &uid).await?;
	Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn cancel_filing(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<axum::http::StatusCode> {
	Nav::new(app).cancel_filing(&ctx, &uid).await?;
	Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn resolve_filing(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(body): Json<NoteBody>,
) -> ClResult<axum::http::StatusCode> {
	Nav::new(app).resolve_filing(&ctx, &uid, &body.note).await?;
	Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn attach_transaction(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(body): Json<AttachBody>,
) -> ClResult<axum::http::StatusCode> {
	Nav::new(app).attach_transaction(&ctx, &uid, &body.transaction_id).await?;
	Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn confirm_not_filed(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(body): Json<NoteBody>,
) -> ClResult<axum::http::StatusCode> {
	Nav::new(app).confirm_not_filed(&ctx, &uid, &body.note).await?;
	Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn release_seller_hold(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<axum::http::StatusCode> {
	Nav::new(app).release_seller_hold(&ctx, &uid).await?;
	Ok(axum::http::StatusCode::NO_CONTENT)
}

/// A filing record on the wire — HTTP and the Rune `nav::filing`/`nav::retry` share it.
pub fn filing_json(f: &NavFiling) -> serde_json::Value {
	let (s, batch) = (&f.row, f.batch.as_ref());
	serde_json::json!({
		"state": f.state().as_str(),
		"op": s.op.as_str(),
		"transactionId": batch.and_then(|b| b.transaction_id.as_deref()),
		"idx": s.idx,
		"verdict": s.verdict.map(crate::NavVerdict::as_str),
		// The row's own first; a batch-level fault is all a held or oversize filing has.
		"errorCode": s.error_code.as_deref().or(batch.and_then(|b| b.error_code.as_deref())),
		"errorMsg": s.error_msg.as_deref().or(batch.and_then(|b| b.error_msg.as_deref())),
		"createdAt": s.created_at,
		"doneAt": s.done_at,
		// Empty is `Nav::filing`'s blanking: a tenant's batch stays on the filing for `state()`.
		"batchUid": batch.map(|b| b.uid.as_str()).filter(|uid| !uid.is_empty()),
		"resolvedAt": s.resolved_at,
	})
}

// vim: ts=4
