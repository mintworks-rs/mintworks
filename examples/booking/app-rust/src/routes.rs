//! The example's own route bundle: handlers deserialize, call exactly one [`Bookings`] method
//! and serialize. Every decision is in `crate::bookings`.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use mintworks_core::app::{App, RouterScopeExt, Scoped};
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use mintworks_invoice::Invoices;
use mintworks_invoice::routes::{InvoiceView, ListQuery, Page};
use mintworks_invoice::store::Invoice;
use mintworks_nav::submission::NavSubmission;

use crate::bookings::{BookRequest, Bookings, CheckoutRequest};
use crate::store::Booking;

/// Every `/api` route this application serves. It lives here rather than inline in the
/// composition root so `tests/flow.rs` can drive the real thing.
///
/// The three scoped bundles are the only ones a `sk_`-prefixed key can reach; `mintworks_auth`'s
/// two stay unscoped, which `auth_mw` reads as a fail-closed 403 for a key — that is what keeps a
/// leaked key out of the endpoints that mint other keys and start erasures.
pub fn api() -> Scoped {
	let gate = mintworks_auth::routes::consent_gate();
	Scoped::from(mintworks_auth::routes::public().merge(mintworks_auth::routes::authenticated()))
		// `org_invoices()` is not mounted: issuing is not something the customer asks for
		// here, it is what their own payment-method choice causes, and storno is an operator's.
		.merge(mintworks_invoice::routes::org_read(&gate).scope("invoice"))
		.merge(mintworks_invoice::routes::org_parties(&gate).scope("invoice"))
		.merge(bookings().scope("booking"))
		// The webhook is public by design; `operator` is mounted although the example seeds no
		// operator account, because with no gateway configured a hand-made operator token
		// recording a MANUAL payment is the only way a TRANSFER invoice reaches PAID.
		.merge(mintworks_billing::routes::public().scope("billing"))
		.merge(mintworks_billing::routes::org(&gate).scope("billing"))
		.merge(mintworks_billing::routes::operator(&gate).scope("billing"))
		// Before the SPA fallback: an unmatched /api path is a 404 in the error envelope, not
		// index.html with a 200 the client parses as an empty success. Axum matches by
		// specificity rather than registration order, so every static route above still wins.
		.merge(Router::<App>::new().route(
			"/api/{*rest}",
			axum::routing::any(|| async { Error::NotFound }),
		))
}

/// Authenticated and consent-gated in one call: `consent_gated_router` layers `require_auth`.
pub fn bookings() -> Router<App> {
	mintworks_auth::routes::consent_gated_router(
		Router::new()
			.route("/api/bookings", get(list).post(book))
			.route("/api/bookings/checkout", post(checkout))
			.route("/api/invoices/{uid}/pay-by-transfer", post(pay_by_transfer))
			.route("/api/invoices/{uid}", axum::routing::delete(discard))
			.route("/api/invoices/{uid}/nav", get(nav))
			// The same account-keyed tier `mintworks_invoice::routes::org_invoices` carries:
			// `checkout` drafts an invoice, and `consent_gated_router` layers auth and
			// consent only.
			.layer(axum::middleware::from_fn_with_state(
				mintworks_core::ratelimit::AUTHENTICATED,
				mintworks_core::ratelimit::scoped_account_mw,
			)),
	)
}

async fn list(
	State(app): State<App>,
	ctx: Ctx,
	Query(q): Query<ListQuery>,
) -> ClResult<Json<Page<Booking>>> {
	Ok(Json(Bookings::new(app).list(&ctx, q.cursor.as_deref(), q.limit).await?))
}

async fn book(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<BookRequest>,
) -> ClResult<(StatusCode, Json<Booking>)> {
	Ok((StatusCode::CREATED, Json(Bookings::new(app).book(&ctx, &req).await?)))
}

/// The invoice body the SPA already knows, plus the one field a gateway adds. Flattened, so
/// `redirectUrl` is the only difference from what `GET /api/invoices/{uid}` serves.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckoutView {
	#[serde(flatten)]
	invoice: InvoiceView,
	redirect_url: Option<String>,
}

/// 204 when there was nothing to bill, so the SPA can say so without inspecting a body.
async fn checkout(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<CheckoutRequest>,
) -> ClResult<Response> {
	let Some(done) = Bookings::new(app.clone()).checkout(&ctx, &req).await? else {
		return Ok(StatusCode::NO_CONTENT.into_response());
	};
	let invoice = view(&app, &ctx, done.invoice).await?;
	Ok(Json(CheckoutView { invoice, redirect_url: done.redirect_url }).into_response())
}

/// "Pay another way", and the way a TRANSFER checkout's invoice gets issued after a card
/// attempt was abandoned. No body: the method is in the path.
async fn pay_by_transfer(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<InvoiceView>> {
	let issued = Bookings::new(app.clone()).pay_by_transfer(&ctx, &uid).await?;
	Ok(Json(view(&app, &ctx, issued).await?))
}

/// Throws an unpaid draft away and returns its bookings to the unbilled set. A numbered
/// invoice answers `E-INV-NOT-DRAFT`.
async fn discard(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<StatusCode> {
	Bookings::new(app).discard(&ctx, &uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// The NAV filing record, read-only. The archived XML stays on the server — it is what a NAV
/// dispute is settled from, not something a browser needs — and is not even on this row: it
/// lives in `nav_submission_xml`, behind the operator-only `Nav::filing_archive`.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct NavSubmissionView {
	op: &'static str,
	verdict: Option<&'static str>,
	submitted_at: Option<Timestamp>,
	message: Option<String>,
}

impl NavSubmissionView {
	fn of(s: NavSubmission) -> Self {
		let message = match (s.error_code, s.error_msg) {
			(Some(code), Some(msg)) => Some(format!("{code}: {msg}")),
			(Some(code), None) => Some(code),
			(None, msg) => msg,
		};
		Self {
			op: s.op.as_str(),
			verdict: s.verdict.map(mintworks_nav::NavVerdict::as_str),
			submitted_at: s.done_at,
			message,
		}
	}
}

/// `mintworks-nav` mounts no routes and `InvoiceView` carries no NAV field, so the example serves
/// the filing record itself. 204 means nothing has been filed — which, with no NAV credentials
/// configured, is the state the demo stays in.
async fn nav(State(app): State<App>, ctx: Ctx, Path(uid): Path<String>) -> ClResult<Response> {
	let Some(submission) = mintworks_nav::Nav::new(app).filing(&ctx, &uid).await? else {
		return Ok(StatusCode::NO_CONTENT.into_response());
	};
	Ok(Json(NavSubmissionView::of(submission)).into_response())
}

/// `mintworks_invoice::store::Invoice` carries no serde. `hydrate` plus `InvoiceView::of` is the
/// body the framework's own `GET /api/invoices/{uid}` serves, and `InvoiceView::of` is public
/// precisely so a consumer mounting none of those bundles can serve the same shape.
async fn view(app: &App, ctx: &Ctx, invoice: Invoice) -> ClResult<InvoiceView> {
	let invoices = Invoices::new(app.clone());
	let full = invoices.hydrate(ctx, invoice, true).await?;
	Ok(InvoiceView::of(full))
}

// vim: ts=4
