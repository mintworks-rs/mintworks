//! Log format and request-id plumbing.
//!
//! [`request_id_mw`] opens a `request` span carrying the id's last four characters, and
//! `tracing_subscriber`'s default full format prints it in front of every event of that
//! request:
//!
//! ```text
//! 2026-09-03T12:00:00.123456Z  INFO request{id=a1b2}: message body
//! ```

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
	let id = ulid::Ulid::new().to_string();
	let short = id[id.len().saturating_sub(4)..].to_owned();
	req.extensions_mut().insert(RequestId(id.clone()));

	// ERROR level so the span stays in scope even under `RUST_LOG=error`.
	let span = tracing::error_span!("request", id = %short);
	let mut res = next.run(req).instrument(span).await;
	if let Ok(v) = HeaderValue::from_str(&id) {
		res.headers_mut().insert(REQUEST_ID_HEADER, v);
	}
	res
}

// vim: ts=4
