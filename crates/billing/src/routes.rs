//! The payment handlers and the three route bundles this crate exposes.
//!
//! **Handlers decide nothing.** Each one deserializes, calls exactly one function in
//! [`crate::allocate`] or [`crate::webhook`], and serializes. Authorization comes from
//! `ctx.actor`, never from which bundle a request arrived through.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use mintworks_core::app::App;
use mintworks_core::auth_mw::RouteGate;
use mintworks_core::ctx::Ctx;
use mintworks_core::error::StatusCode;
use mintworks_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::allocate::{self, Allocation, ManualPayment, StartRequest};
use crate::store::Payment;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

// ---------------------------------------------------------------- wire types

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
	pub items: Vec<T>,
	pub next_cursor: Option<String>,
}

/// Every field is optional and they are ANDed. serde drops unknown query parameters silently,
/// so a documented filter missing from this struct is not rejected — it is accepted and
/// answered with everything.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
	#[serde(default)]
	pub cursor: Option<PaymentId>,
	#[serde(default)]
	pub limit: Option<i64>,
	#[serde(default)]
	pub status: Option<String>,
	#[serde(default)]
	pub kind: Option<String>,
	#[serde(default)]
	pub provider: Option<String>,
	#[serde(default)]
	pub invoice_uid: Option<InvoiceId>,
	#[serde(default)]
	pub received_from: Option<Timestamp>,
	#[serde(default)]
	pub received_to: Option<Timestamp>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AllocationView {
	pub invoice_uid: InvoiceId,
	/// `null` on a draft, which has no number yet.
	pub invoice_number: Option<String>,
	pub amount: MoneyWire,
	pub allocated_at: Timestamp,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentView {
	pub uid: PaymentId,
	pub kind: String,
	pub provider: Option<String>,
	pub provider_ref: Option<String>,
	/// What "Continue payment" navigates to: a resumed payment carries the URL it was opened
	/// with, so the SPA needs no second POST.
	pub redirect_url: Option<String>,
	/// When the gateway must have given up, our own clock at the moment it accepted: what the
	/// SPA counts down. `null` for a manual entry and for a row written before the window
	/// existed.
	pub expires_at: Option<Timestamp>,
	pub request_id: Option<String>,
	pub status: String,
	pub amount: MoneyWire,
	pub refunded_amount: MoneyWire,
	pub received_at: Option<Timestamp>,
	pub ext_ref: Option<String>,
	pub note: Option<String>,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
	pub allocations: Vec<AllocationView>,
}

impl PaymentView {
	fn new(p: Payment, allocations: Vec<AllocationView>) -> Self {
		Self {
			uid: p.uid,
			kind: p.kind,
			provider: p.provider,
			provider_ref: p.provider_ref,
			redirect_url: p.redirect_url,
			expires_at: p.expires_at,
			request_id: p.request_id,
			status: p.status.as_str().to_string(),
			amount: p.amount.to_wire(&p.currency),
			refunded_amount: p.refunded_amount.to_wire(&p.currency),
			received_at: p.received_at,
			ext_ref: p.ext_ref,
			note: p.note,
			created_at: p.created_at,
			updated_at: p.updated_at,
			allocations,
		}
	}
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayBody {
	pub provider: String,
	#[serde(default)]
	pub request_id: Option<String>,
	pub return_url: String,
	#[serde(default)]
	pub locale: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayResponse {
	pub payment: PaymentView,
	pub redirect_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllocationBody {
	pub invoice_uid: InvoiceId,
	pub amount: MoneyWire,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefundBody {
	/// Omitted refunds the whole remaining `amount - refundedAmount`.
	#[serde(default)]
	pub amount: Option<MoneyWire>,
	#[serde(default)]
	pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualBody {
	pub org_uid: OrgId,
	pub kind: String,
	pub amount: MoneyWire,
	pub received_at: Timestamp,
	#[serde(default)]
	pub ext_ref: Option<String>,
	#[serde(default)]
	pub note: Option<String>,
	#[serde(default)]
	pub allocations: Vec<AllocationBody>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
	pub id: String,
	pub caps: crate::provider::ProviderCaps,
}

// ---------------------------------------------------------------- handlers

fn amount_in(w: &MoneyWire) -> ClResult<(Money, CurrencyCode)> {
	Ok((Money::parse(&w.amount)?, w.currency.clone()))
}

fn view(a: crate::store::PaymentAllocation, currency: &CurrencyCode) -> AllocationView {
	AllocationView {
		invoice_uid: a.invoice_uid,
		invoice_number: a.invoice_number,
		amount: a.amount.to_wire(currency),
		allocated_at: a.allocated_at,
	}
}

async fn hydrate(app: &App, ctx: &Ctx, p: Payment) -> ClResult<PaymentView> {
	let rows = allocate::allocations_of(app, ctx, std::slice::from_ref(&p)).await?;
	let currency = p.currency.clone();
	let allocations = rows.into_iter().map(|a| view(a, &currency)).collect();
	Ok(PaymentView::new(p, allocations))
}

/// One read for the whole page, rather than [`hydrate`] per row. Grouped before the view is
/// built: an allocation is formatted in its *payment's* currency.
fn hydrate_all(
	rows: Vec<Payment>,
	allocations: Vec<crate::store::PaymentAllocation>,
) -> Vec<PaymentView> {
	let mut by_payment: HashMap<i64, Vec<crate::store::PaymentAllocation>> = HashMap::new();
	for a in allocations {
		by_payment.entry(a.payment_id).or_default().push(a);
	}
	rows.into_iter()
		.map(|p| {
			let currency = p.currency.clone();
			let allocations = by_payment
				.remove(&p.id)
				.unwrap_or_default()
				.into_iter()
				.map(|a| view(a, &currency))
				.collect();
			PaymentView::new(p, allocations)
		})
		.collect()
}

async fn pay(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<InvoiceId>,
	axum::Json(body): axum::Json<PayBody>,
) -> ClResult<(StatusCode, axum::Json<PayResponse>)> {
	let (payment, redirect_url) = allocate::start(
		&app,
		&ctx,
		&uid,
		StartRequest {
			provider: body.provider,
			request_id: body.request_id,
			return_url: body.return_url,
			locale: body.locale,
		},
	)
	.await?;
	let payment = hydrate(&app, &ctx, payment).await?;
	Ok((StatusCode::CREATED, axum::Json(PayResponse { payment, redirect_url })))
}

async fn list(
	State(app): State<App>,
	ctx: Ctx,
	Query(q): Query<ListQuery>,
) -> ClResult<axum::Json<Page<PaymentView>>> {
	let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
	// Mapped here, not in `FromStr`: this is the one place the value is client text, and
	// `PaymentState::FromStr` answers `Error::internal` — a 500 on a typed query string.
	let status = q
		.status
		.as_deref()
		.map(|s| s.parse().map_err(|_| Error::validation("unknown payment status")))
		.transpose()?;
	let filter = crate::store::PaymentFilter {
		before: q.cursor.as_ref(),
		limit,
		status,
		kind: q.kind.as_deref(),
		provider: q.provider.as_deref(),
		invoice_uid: q.invoice_uid.as_ref(),
		received_from: q.received_from,
		received_to: q.received_to,
	};
	let (rows, allocations) = allocate::list(&app, &ctx, &filter).await?;
	// The `uid`, never `p.id`: ids are global rather than per org, so a sequential cursor
	// handed any org a cross-org row-volume oracle (`mintworks_core::ids`).
	let next_cursor = (i64::try_from(rows.len()).unwrap_or(i64::MAX) == limit)
		.then(|| rows.last().map(|p| p.uid.to_string()))
		.flatten();
	Ok(axum::Json(Page { items: hydrate_all(rows, allocations), next_cursor }))
}

async fn get_payment(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<PaymentId>,
) -> ClResult<axum::Json<PaymentView>> {
	let payment = allocate::payment(&app, &ctx, &uid).await?;
	Ok(axum::Json(hydrate(&app, &ctx, payment).await?))
}

async fn invoice_payments(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<InvoiceId>,
) -> ClResult<axum::Json<Page<PaymentView>>> {
	let rows = allocate::for_invoice(&app, &ctx, &uid).await?;
	let allocations = allocate::allocations_of(&app, &ctx, &rows).await?;
	Ok(axum::Json(Page { items: hydrate_all(rows, allocations), next_cursor: None }))
}

async fn list_providers(
	State(app): State<App>,
	_ctx: Ctx,
) -> ClResult<axum::Json<Page<ProviderView>>> {
	let items = crate::provider::list_providers(&app)?
		.into_iter()
		.map(|(id, caps)| ProviderView { id, caps })
		.collect();
	Ok(axum::Json(Page { items, next_cursor: None }))
}

async fn manual(
	State(app): State<App>,
	ctx: Ctx,
	axum::Json(body): axum::Json<ManualBody>,
) -> ClResult<(StatusCode, axum::Json<PaymentView>)> {
	let (amount, currency) = amount_in(&body.amount)?;
	let mut allocations = Vec::with_capacity(body.allocations.len());
	for a in &body.allocations {
		let (amount, currency) = amount_in(&a.amount)?;
		allocations.push(Allocation { invoice_uid: a.invoice_uid.clone(), amount, currency });
	}
	let payment = allocate::manual(
		&app,
		&ctx,
		ManualPayment {
			org_uid: body.org_uid,
			kind: body.kind,
			amount,
			currency,
			received_at: body.received_at,
			ext_ref: body.ext_ref,
			note: body.note,
			allocations,
		},
	)
	.await?;
	Ok((StatusCode::CREATED, axum::Json(hydrate(&app, &ctx, payment).await?)))
}

async fn add_allocation(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<PaymentId>,
	axum::Json(body): axum::Json<AllocationBody>,
) -> ClResult<StatusCode> {
	let (amount, currency) = amount_in(&body.amount)?;
	allocate::allocate(
		&app,
		&ctx,
		&uid,
		&Allocation { invoice_uid: body.invoice_uid, amount, currency },
	)
	.await?;
	Ok(StatusCode::CREATED)
}

async fn refund(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<PaymentId>,
	axum::Json(body): axum::Json<RefundBody>,
) -> ClResult<axum::Json<PaymentView>> {
	let amount = body.amount.as_ref().map(amount_in).transpose()?;
	let payment = crate::refund::refund(&app, &ctx, &uid, amount, body.reason).await?;
	Ok(axum::Json(hydrate(&app, &ctx, payment).await?))
}

// ---------------------------------------------------------------- bundles

/// The gateway's callback. Public and unauthenticated by design — the body is untrusted and
/// the truth is fetched back over the provider's own channel.
pub fn public() -> Router<App> {
	Router::new().route(
		"/api/webhook/{provider}",
		post(crate::webhook::callback).layer(axum::middleware::from_fn_with_state(
			"webhook",
			mintworks_core::ratelimit::scoped_ip_mw,
		)),
	)
}

/// Starting a payment and reading one back, for the org that owns it.
pub fn org(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/invoices/{uid}/pay", post(pay))
		.route("/api/invoices/{uid}/payments", get(invoice_payments))
		.route("/api/payments", get(list))
		.route("/api/payments/{uid}", get(get_payment))
		.route("/api/payment-providers", get(list_providers))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

/// Manual entry and allocation: how a bank transfer, or a mis-allocation, is settled by a
/// human.
///
/// Neither gate is here: `require_operator` and `require_stepup` are the first two lines of
/// each service function, so a consumer that leaves this bundle unmounted and calls the
/// handles directly is still gated.
pub fn operator(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/admin/payments", post(manual))
		.route("/api/admin/payments/{uid}/allocations", post(add_allocation))
		.route("/api/admin/payments/{uid}/refund", post(refund))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

// vim: ts=4
