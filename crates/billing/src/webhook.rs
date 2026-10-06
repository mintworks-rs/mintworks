// SPDX-License-Identifier: MPL-2.0
//! `POST /api/webhook/{provider}` — public, unauthenticated, and the body is a ping, not news.
//!
//! Nothing here reads an amount, a status or a currency out of the request. The body is parsed
//! for one thing, the gateway's own id for a payment, that id is looked up in our own tables,
//! and only then is the truth fetched back over the provider's own authenticated channel.
//!
//! **It always answers 200**, for an unknown reference, a replay, a malformed body or a
//! provider it has never heard of. A gateway that receives an error retries and eventually
//! disables the hook, there is nothing useful to tell it, and a distinguishable error would
//! leak which references exist.

use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use mintworks_core::app::App;
use mintworks_core::error::StatusCode;
use mintworks_core::prelude::*;

use crate::allocate;
use crate::provider::providers;

pub async fn callback(
	State(app): State<App>,
	Path(provider): Path<String>,
	RawQuery(query): RawQuery,
	headers: HeaderMap,
	body: axum::body::Bytes,
) -> StatusCode {
	if let Err(e) = handle(&app, &provider, &headers, &body, query.as_deref()).await {
		// Logged and alerted, never returned: see the module note.
		tracing::warn!(error = %e, provider, "webhook callback");
	}
	StatusCode::OK
}

async fn handle(
	app: &App,
	provider_id: &str,
	headers: &HeaderMap,
	body: &[u8],
	query: Option<&str>,
) -> ClResult<()> {
	let provider = providers(app)?.get(provider_id).ok_or_else(|| {
		Error::coded(StatusCode::BAD_REQUEST, "E-PAY-PROVIDER", "unknown provider")
	})?;

	// Barion has been documented to send `paymentId` in the callback body *and* in the callback
	// URL's query string, and which one arrives has changed between their API versions. The
	// trait only hands the provider bytes, so the query is offered as a second body.
	let reference = match provider.parse_callback(headers, body) {
		Ok(r) => r,
		Err(e) => match query {
			Some(q) => provider.parse_callback(headers, q.as_bytes())?,
			None => return Err(e),
		},
	};

	// Ours first: this endpoint is public, and a reference we do not hold must not reach the
	// gateway.
	let Some(payment) = crate::store::store(app)?
		.payment_by_provider_ref(provider_id, &reference.provider_ref)
		.await?
	else {
		tracing::info!(provider = provider_id, "callback for an unknown payment reference");
		return Ok(());
	};
	let state = provider.fetch_state(&reference.provider_ref).await?;
	allocate::apply_state(app, &payment, state).await
}

// vim: ts=4
