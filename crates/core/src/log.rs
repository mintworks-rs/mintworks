// SPDX-License-Identifier: MPL-2.0
//! Log format and request-id plumbing.
//!
//! [`request_id_mw`] opens a `request` span carrying the id's last four characters, and
//! `tracing_subscriber`'s default full format prints it in front of every event of that
//! request:
//!
//! ```text
//! 2026-09-03T12:00:00.123456Z  INFO request{id=a1b2}: message body
//! ```
//!
//! It also emits the access line for every `/api` request — one event per request, at a level
//! that follows the status code, naming the `errCode` on a failure.

use axum::extract::Request;
use axum::http::{HeaderValue, header::HeaderName};
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// The full request id, put in the request extensions by [`request_id_mw`] and copied
/// into `Ctx::request_id` and `audit_logs.request_id`.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

/// Installs the subscriber. Called by `AppBuilder::new`; safe to call twice — a second
/// call is a no-op rather than a panic.
pub fn init() {
	use tracing_subscriber::layer::SubscriberExt;
	use tracing_subscriber::util::SubscriberInitExt;

	let filter = tracing_subscriber::EnvFilter::try_from_default_env()
		.or_else(|_| tracing_subscriber::EnvFilter::try_new("info,hyper=warn,tower=warn,sqlx=warn"))
		.unwrap_or_default();
	let fmt_layer = tracing_subscriber::fmt::layer()
		.with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339());
	let _ = tracing_subscriber::registry().with(filter).with(fmt_layer).try_init();
}

/// Outermost middleware: mints a request id, opens the `request` span every log line in
/// this request is tagged with, and echoes the id back in `x-request-id`.
///
/// The id is always generated here — a client-supplied header would let a caller forge
/// the correlation key that ties audit rows to log lines.
pub async fn request_id_mw(mut req: Request, next: Next) -> Response {
	let id = ulid::Ulid::generate().to_string();
	let short = id[id.len().saturating_sub(4)..].to_owned();
	req.extensions_mut().insert(RequestId(id.clone()));

	let method = req.method().clone();
	let path = req.uri().path().to_owned();
	let started = std::time::Instant::now();

	// ERROR level so the span stays in scope even under `RUST_LOG=error`.
	let span = tracing::error_span!("request", id = %short);
	let mut res = next.run(req).instrument(span.clone()).await;
	if let Ok(v) = HeaderValue::from_str(&id) {
		res.headers_mut().insert(REQUEST_ID_HEADER, v);
	}

	// `/api` only: this is the outermost layer, so the SPA fallback's every asset would
	// otherwise be a line. Mute the successes with `RUST_LOG=mintworks_core::log=warn`.
	if path.starts_with("/api") {
		let status = res.status().as_u16();
		// `as_millis` is u128 and `Option<&str>` is not a `tracing::Value`; both have to be
		// narrowed here rather than in the macro.
		let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
		let err_code =
			res.extensions().get::<crate::error::ErrCode>().map(|c| c.0).unwrap_or_default();
		span.in_scope(|| match status {
			500.. => tracing::error!(%method, %path, status, err_code, ms, "request"),
			400.. => tracing::warn!(%method, %path, status, err_code, ms, "request"),
			_ => tracing::info!(%method, %path, status, ms, "request"),
		});
	}
	res
}

// vim: ts=4
