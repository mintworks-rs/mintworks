//! The example's own route bundle: handlers deserialize, call exactly one [`Bookings`] method
//! and serialize. Every decision is in `crate::bookings`.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::prelude::*;
use saas_invoice::Invoices;
use saas_invoice::routes::{InvoiceView, ListQuery, Page};
use saas_invoice::store::Invoice;
use saas_nav::submission::NavSubmission;

use crate::bookings::{BookRequest, Bookings};
use crate::store::Booking;

/// Every `/api` route this application serves, in the order `main` mounts them. It lives here
/// rather than inline in the composition root so `tests/flow.rs` can drive the real thing.
pub fn api() -> Router<App> {
	let gate = saas_auth::routes::consent_gate();
	saas_auth::routes::public()
		.merge(saas_auth::routes::authenticated())
		// `tenant_invoices()` is not mounted because this application wraps issue and storno in
		// its own `Bookings` handle, which also settles the bookings behind them —
		// `demo_buyer_invoice_routes()` exposes both, under the same rate-limit tier.
		.merge(saas_invoice::routes::tenant_read(&gate))
		.merge(saas_invoice::routes::tenant_parties(&gate))
		.merge(bookings())
		// One line to delete, which is the point — see the doc comment below.
		.merge(demo_buyer_invoice_routes())
		// Before the SPA fallback: an unmatched /api path is a 404 in the error envelope, not
		// index.html with a 200 the client parses as an empty success. Axum prefers the static
		// routes above, so every registered one still wins.
		.route("/api/{*rest}", axum::routing::any(|| async { Error::NotFound }))
}

/// Authenticated and consent-gated in one call: `consent_gated_router` layers `require_auth`.
pub fn bookings() -> Router<App> {
	saas_auth::routes::consent_gated_router(
		Router::new()
			.route("/api/bookings", get(list).post(book))
			.route("/api/bookings/checkout", post(checkout))
			.route("/api/invoices/{uid}/nav", get(nav))
			// The same account-keyed tier `saas_invoice::routes::tenant_invoices` carries:
			// `checkout` drafts an invoice, and `consent_gated_router` layers auth and
			// consent only.
			.layer(axum::middleware::from_fn_with_state(
				saas_core::ratelimit::AUTHENTICATED,
				saas_core::ratelimit::scoped_account_mw,
			)),
	)
}

/// **Demo only. A real consumer must not mount this.**
///
/// Three routes that let the invoice's own payer act on a numbered legal document: `confirm`
/// mints one under `sellers.id = 1` — the operator's own taxpayer id — and `payment` is one-way,
/// so the payer can permanently foreclose their own storno. The framework deliberately mounts no
/// route for `Invoices::mark_paid`: marking an invoice paid is the payment provider's report, not
/// the payer's claim. The example does it because it seeds no operator account and would
/// otherwise have no payment step at all.
///
/// Split out of [`bookings`] so the warning survives the copy-paste that a comment inside the
/// registration block does not.
pub fn demo_buyer_invoice_routes() -> Router<App> {
	tracing::warn!(
		"demo routes are mounted: POST /api/invoices/{{uid}}/confirm, /payment and /cancel let \
		 the invoice's own payer issue, foreclose and storno it"
	);
	saas_auth::routes::consent_gated_router(
		Router::new()
			.route("/api/invoices/{uid}/confirm", post(confirm))
			.route("/api/invoices/{uid}/payment", post(record_payment))
			.route("/api/invoices/{uid}/cancel", post(cancel))
			.layer(axum::middleware::from_fn_with_state(
				saas_core::ratelimit::AUTHENTICATED,
				saas_core::ratelimit::scoped_account_mw,
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

/// 204 when there was nothing to bill, so the SPA can say so without inspecting a body.
async fn checkout(State(app): State<App>, ctx: Ctx) -> ClResult<Response> {
	let Some(invoice) = Bookings::new(app.clone()).checkout(&ctx).await? else {
		return Ok(StatusCode::NO_CONTENT.into_response());
	};
	Ok(Json(view(&app, &ctx, invoice).await?).into_response())
}

async fn confirm(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<InvoiceView>> {
	let issued = Bookings::new(app.clone()).confirm(&ctx, &uid).await?;
	Ok(Json(view(&app, &ctx, issued).await?))
}

/// The body is the amount the client believes it is paying, in the wire shape the invoice
/// itself is served with. `Bookings::record_payment` is what compares it.
async fn record_payment(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(expected): Json<MoneyWire>,
) -> ClResult<Json<InvoiceView>> {
	let paid = Bookings::new(app.clone()).record_payment(&ctx, &uid, &expected).await?;
	Ok(Json(view(&app, &ctx, paid).await?))
}

#[derive(Debug, serde::Deserialize)]
struct CancelBody {
	reason: String,
}

/// The body is required: a `POST` that merely omits `content-type` used to file a STORNO whose
/// reason landed blank in an immutable counter-invoice. `Bookings::cancel` rejects a blank one.
async fn cancel(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(body): Json<CancelBody>,
) -> ClResult<Json<InvoiceView>> {
	let storno = Bookings::new(app.clone()).cancel(&ctx, &uid, &body.reason).await?;
	Ok(Json(view(&app, &ctx, storno).await?))
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
			verdict: s.verdict.map(saas_nav::NavVerdict::as_str),
			submitted_at: s.done_at,
			message,
		}
	}
}

/// `saas-nav` mounts no routes and `InvoiceView` carries no NAV field, so the example serves
/// the filing record itself. 204 means nothing has been filed — which, with no NAV credentials
/// configured, is the state the demo stays in.
async fn nav(State(app): State<App>, ctx: Ctx, Path(uid): Path<String>) -> ClResult<Response> {
	let Some(submission) = saas_nav::Nav::new(app).filing(&ctx, &uid).await? else {
		return Ok(StatusCode::NO_CONTENT.into_response());
	};
	Ok(Json(NavSubmissionView::of(submission)).into_response())
}

/// `saas_invoice::store::Invoice` carries no serde. `hydrate` plus `InvoiceView::of` is the
/// body the framework's own `GET /api/invoices/{uid}` serves, and `InvoiceView::of` is public
/// precisely so a consumer mounting none of those bundles can serve the same shape.
async fn view(app: &App, ctx: &Ctx, invoice: Invoice) -> ClResult<InvoiceView> {
	let invoices = Invoices::new(app.clone());
	let full = invoices.hydrate(ctx, invoice, true).await?;
	Ok(InvoiceView::of(full))
}

// vim: ts=4
