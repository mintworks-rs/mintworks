//! The run routes: the event stream and cancel. Starting a run is the application's decision (its
//! spec names tools and grants spaces), so it goes through `Agent::start`, not an HTTP route.

use std::convert::Infallible;

use axum::{
	Router,
	extract::{Path, State},
	http::{HeaderMap, StatusCode},
	response::sse::{Event, KeepAlive, Sse},
	routing::{get, post},
};
use futures_util::{Stream, StreamExt};
use mintworks_core::{App, ClResult, Ctx, auth_mw::RouteGate};

use crate::service::Agent;

/// `GET /api/agent/runs/{uid}/events` — SSE; each event's `id` is its `seq`, its `event` the kind,
/// its `data` the JSON payload. `Last-Event-ID` resumes after that seq.
pub async fn events(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	headers: HeaderMap,
) -> ClResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
	let after = headers
		.get("last-event-id")
		.and_then(|v| v.to_str().ok())
		.and_then(|v| v.trim().parse().ok())
		.unwrap_or(0);
	let stream = Agent::new(app).events(&ctx, &uid, after).await?;
	Ok(Sse::new(stream.map(|e| {
		Ok(Event::default().id(e.seq.to_string()).event(e.kind.as_str()).data(e.payload))
	}))
	.keep_alive(KeepAlive::default()))
}

/// `POST /api/agent/runs/{uid}/cancel`
pub async fn cancel(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<StatusCode> {
	Agent::new(app).cancel(&ctx, &uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// Authenticated, org-confined in [`Agent`], consent-gated by `gate`.
pub fn runs(gate: &RouteGate) -> Router<App> {
	let bundle = Router::new()
		.route("/api/agent/runs/{uid}/events", get(events))
		.route("/api/agent/runs/{uid}/cancel", post(cancel))
		.layer(axum::middleware::from_fn_with_state(
			mintworks_core::ratelimit::AUTHENTICATED,
			mintworks_core::ratelimit::scoped_account_mw,
		))
		.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth));
	gate.apply(bundle)
}

// vim: ts=4
