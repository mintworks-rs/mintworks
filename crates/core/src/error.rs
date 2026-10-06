//! The framework error type and the single HTTP error envelope.
//!
//! Every non-2xx response is `{"error": {"errCode": "…", "errStr": "…"}}`, with an optional
//! `fields` member on field-level validation failures. `errCode` is stable and machine-readable;
//! `errStr` is prose in the request's locale and may change freely.
//!
//! 5xx errors log their detail and never expose it: the response carries a generic `errStr`
//! and the request id is what ties it back to the log line.

use std::collections::BTreeMap;

use axum::{
	http::{HeaderValue, header::RETRY_AFTER},
	response::{IntoResponse, Response},
};
use serde::Serialize;

// `Error::coded` takes a `StatusCode`, so every crate raising its own `errCode` needs the
// type. Re-exported so a store adapter does not depend on `axum` for two constants.
pub use axum::http::StatusCode;

pub type ClResult<T> = std::result::Result<T, Error>;

/// Field-level failure map: JSON field name -> its [`E_FORMAT`] / [`E_RANGE`] code.
///
/// A **code**, not prose: a client switches on the value, and the human wording belongs in
/// [`Error::ValidationFields`]'s first argument, which becomes `errStr`.
pub type FieldErrors = BTreeMap<String, &'static str>;

/// The value is the wrong shape — an address that is not an address, a malformed date.
pub const E_FORMAT: &str = "E-CORE-FORMAT";

/// The value is the right shape but outside its bounds — too short, too long, out of range.
pub const E_RANGE: &str = "E-CORE-RANGE";

/// Whether the job runner should try a failed unit of work again.
///
/// Two states by design. "The upstream may have processed it and we cannot tell" is a
/// property of a non-idempotent call, not of an error: it is fixed at the call — NAV's
/// `manageInvoice` carries `invoices.uid` as its `requestId` — rather than modelled here,
/// because modelling it institutionalises the defect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
	/// Try again after the backoff.
	Backoff,
	/// The next attempt fails identically. Give up now.
	Never,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// 400 `E-CORE-VALIDATION`
	#[error("{0}")]
	Validation(String),
	/// 400 `E-CORE-VALIDATION` carrying a per-field breakdown.
	#[error("{0}")]
	ValidationFields(String, FieldErrors),
	/// 404 `E-CORE-NOTFOUND` — no such resource, or not visible to this caller.
	#[error("resource not found")]
	NotFound,
	/// 409 `E-CORE-CONFLICT`
	#[error("{0}")]
	Conflict(String),
	/// 429 `E-CORE-RATELIMIT`, carrying the `Retry-After` value in seconds.
	#[error("rate limit exceeded, retry in {0}s")]
	RateLimit(u64),
	/// 400 `E-CORE-POW`
	#[error("{0}")]
	Pow(String),
	/// 400 `E-CORE-SETTING`
	#[error("{0}")]
	Setting(String),
	/// 415 `E-CORE-UNSUPPORTED` — the request body is not in a content type this route reads.
	#[error("{0}")]
	Unsupported(String),
	/// 503 `E-CORE-UNAVAILABLE` — an upstream demonstrably did not process the request:
	/// a refused connection, a 502/503 from its load balancer. Safe to retry.
	#[error("{0}")]
	Unavailable(String),
	/// 504 `E-CORE-TIMEOUT` — an upstream did not answer inside its deadline. Distinct from
	/// [`Error::Unavailable`] because the request may well have been processed, which is the
	/// difference between `mintworks-nav` retrying a filing and parking it as `UNKNOWN`.
	#[error("{0}")]
	Timeout(String),
	/// 500 `E-CORE-INTERNAL` — never exposed, always logged.
	#[error("{0}")]
	Internal(String),
	/// An `errCode` outside the `E-CORE-*` namespace, raised by a feature crate.
	/// `mintworks-core` cannot enumerate `E-AUTH-*`, `E-INV-*`, `E-PAY-*` or `E-NAV-*` without
	/// depending on the crates that own them, and dependencies point inward only.
	#[error("{msg}")]
	Coded { status: StatusCode, code: &'static str, msg: String, retry: Retry },
}

impl Error {
	/// Raise a feature crate's own `errCode`.
	pub fn coded(status: StatusCode, code: &'static str, msg: impl Into<String>) -> Self {
		Self::Coded { status, code, msg: msg.into(), retry: Retry::Never }
	}

	/// Raise a feature crate's own `errCode` for a transport-shaped failure: the upstream was
	/// momentarily not there, and sending the same request again is worth doing.
	pub fn coded_retry(status: StatusCode, code: &'static str, msg: impl Into<String>) -> Self {
		Self::Coded { status, code, msg: msg.into(), retry: Retry::Backoff }
	}

	pub fn validation(msg: impl Into<String>) -> Self {
		Self::Validation(msg.into())
	}

	pub fn conflict(msg: impl Into<String>) -> Self {
		Self::Conflict(msg.into())
	}

	pub fn internal(msg: impl Into<String>) -> Self {
		Self::Internal(msg.into())
	}

	/// The HTTP status and stable `errCode` this error maps to.
	pub fn parts(&self) -> (StatusCode, &'static str) {
		match self {
			Self::Validation(_) | Self::ValidationFields(..) => {
				(StatusCode::BAD_REQUEST, "E-CORE-VALIDATION")
			}
			Self::NotFound => (StatusCode::NOT_FOUND, "E-CORE-NOTFOUND"),
			Self::Conflict(_) => (StatusCode::CONFLICT, "E-CORE-CONFLICT"),
			Self::RateLimit(_) => (StatusCode::TOO_MANY_REQUESTS, "E-CORE-RATELIMIT"),
			Self::Pow(_) => (StatusCode::BAD_REQUEST, "E-CORE-POW"),
			Self::Setting(_) => (StatusCode::BAD_REQUEST, "E-CORE-SETTING"),
			Self::Unsupported(_) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "E-CORE-UNSUPPORTED"),
			Self::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "E-CORE-UNAVAILABLE"),
			Self::Timeout(_) => (StatusCode::GATEWAY_TIMEOUT, "E-CORE-TIMEOUT"),
			Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "E-CORE-INTERNAL"),
			Self::Coded { status, code, .. } => (*status, code),
		}
	}

	/// Whether the job runner should try this again.
	///
	/// One answer, read by `Runner::tick`. A handler says "give up" by returning a `Never`
	/// error rather than by re-deriving the question at its own call site.
	pub fn retry(&self) -> Retry {
		match self {
			Self::Unavailable(_) | Self::Timeout(_) | Self::RateLimit(_) => Retry::Backoff,
			Self::Validation(_)
			| Self::ValidationFields(..)
			| Self::NotFound
			| Self::Conflict(_)
			| Self::Pow(_)
			| Self::Setting(_)
			| Self::Unsupported(_)
			| Self::Internal(_) => Retry::Never,
			Self::Coded { retry, .. } => *retry,
		}
	}

	/// The upstream's own delay, in seconds, when it named one. Read by both the `Retry-After`
	/// response header and the job runner's backoff, so the two cannot disagree.
	#[must_use]
	pub fn retry_after(&self) -> Option<u64> {
		match self {
			Self::RateLimit(secs) => Some(*secs),
			_ => None,
		}
	}
}

/// `axum::Json`, but a rejection is the framework error envelope rather than axum's
/// plain-text default.
///
/// *Every* non-2xx response carries `{"error":{"errCode":…,"errStr":…}}`, and a bare
/// `axum::Json<T>` breaks that for the two commonest client mistakes: a malformed body (422,
/// plain text) and a missing
/// `Content-Type: application/json` (415, plain text). A client parsing `body.error.errCode`
/// gets a parse failure instead of a code it can act on.
///
/// Extractor position only — a response is still built with `axum::Json`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<T, S> axum::extract::FromRequest<S> for Json<T>
where
	T: serde::de::DeserializeOwned,
	S: Send + Sync,
{
	type Rejection = Error;

	async fn from_request(
		req: axum::extract::Request,
		state: &S,
	) -> std::result::Result<Self, Self::Rejection> {
		use axum::extract::rejection::JsonRejection;
		match axum::Json::<T>::from_request(req, state).await {
			Ok(axum::Json(value)) => Ok(Self(value)),
			Err(JsonRejection::MissingJsonContentType(e)) => Err(Error::Unsupported(e.body_text())),
			Err(e) => Err(Error::Validation(e.body_text())),
		}
	}
}

/// `Option<Json<T>>`, for the two routes whose body is genuinely optional (`auth/refresh`,
/// `storno`). Delegates to axum's own optional impl so "absent" and "malformed" stay
/// distinct — only the second is an error.
impl<T, S> axum::extract::OptionalFromRequest<S> for Json<T>
where
	T: serde::de::DeserializeOwned,
	S: Send + Sync,
{
	type Rejection = Error;

	async fn from_request(
		req: axum::extract::Request,
		state: &S,
	) -> std::result::Result<Option<Self>, Self::Rejection> {
		match axum::Json::<T>::from_request(req, state).await {
			Ok(v) => Ok(v.map(|axum::Json(value)| Self(value))),
			Err(e) => Err(Error::Validation(e.body_text())),
		}
	}
}

impl<T: Serialize> IntoResponse for Json<T> {
	fn into_response(self) -> Response {
		axum::Json(self.0).into_response()
	}
}

#[derive(Serialize)]
struct Envelope {
	error: Body,
}

/// The `errCode` of a response built from [`Error`], put in the response extensions so
/// `log::request_id_mw` can name it on the access line without re-reading the body.
#[derive(Clone, Copy, Debug)]
pub struct ErrCode(pub &'static str);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Body {
	err_code: &'static str,
	err_str: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	fields: Option<FieldErrors>,
}

impl IntoResponse for Error {
	fn into_response(self) -> Response {
		let (status, err_code) = self.parts();
		let retry_after = self.retry_after();
		let fields = match &self {
			Self::ValidationFields(_, f) => Some(f.clone()),
			_ => None,
		};
		// 5xx detail is logged, never returned, with no exemption: `Unavailable` and `Timeout`
		// are built from driver and `lettre` text. Fixed per variant so an `errStr` cannot
		// contradict its `errCode`; the log *level* stays per variant so routine blips do not
		// drown `error!`.
		let err_str = if status.is_server_error() {
			let fixed = match &self {
				Self::Unavailable(_) => "service unavailable",
				Self::Timeout(_) => "upstream timed out",
				_ => "internal error",
			};
			if matches!(self, Self::Unavailable(_) | Self::Timeout(_)) {
				tracing::warn!(err_code, error = %self, "request failed");
			} else {
				tracing::error!(err_code, error = %self, "request failed");
			}
			fixed.to_owned()
		} else {
			self.to_string()
		};

		let body = Envelope { error: Body { err_code, err_str, fields } };
		let mut response = (status, axum::Json(body)).into_response();
		response.extensions_mut().insert(ErrCode(err_code));
		if let Some(secs) = retry_after
			&& let Ok(value) = HeaderValue::try_from(secs.to_string())
		{
			response.headers_mut().insert(RETRY_AFTER, value);
		}
		response
	}
}

#[cfg(test)]
mod tests {
	use axum::{Router, body::Body, http::Request, routing::post};
	use http_body_util::BodyExt;
	use tower::ServiceExt;

	use super::*;

	#[derive(serde::Deserialize)]
	struct Body_ {
		#[allow(dead_code)]
		a: i32,
	}

	async fn handler(Json(_): Json<Body_>) -> &'static str {
		"ok"
	}

	/// Whatever the client got wrong: one envelope, one `errCode`, never axum's plain text.
	async fn envelope(req: Request<Body>) -> (StatusCode, serde_json::Value) {
		let app = Router::new().route("/", post(handler));
		let res = app.oneshot(req).await.unwrap();
		let status = res.status();
		let bytes = res.into_body().collect().await.unwrap().to_bytes();
		(status, serde_json::from_slice(&bytes).expect("the body must be the JSON envelope"))
	}

	#[tokio::test]
	async fn a_rejected_body_is_the_envelope() {
		for (content_type, body, want_status, want_code) in [
			("application/json", "{ not json", StatusCode::BAD_REQUEST, "E-CORE-VALIDATION"),
			("", r#"{"a":1}"#, StatusCode::UNSUPPORTED_MEDIA_TYPE, "E-CORE-UNSUPPORTED"),
		] {
			let req = Request::post("/");
			let req = if content_type.is_empty() {
				req
			} else {
				req.header("content-type", content_type)
			};
			let (status, rendered) = envelope(req.body(Body::from(body)).unwrap()).await;
			assert_eq!(status, want_status, "{content_type:?} {body}");
			assert_eq!(rendered["error"]["errCode"], want_code, "{content_type:?} {body}");
		}
	}

	/// An `errStr` must not contradict its `errCode`, but letting `Unavailable`/`Timeout`
	/// render their own message bought that by leaking driver and `lettre` text. Every 5xx
	/// body is a fixed per-variant string instead; the detail goes to the log alone.
	#[tokio::test]
	async fn every_5xx_body_is_a_fixed_message() {
		let (status, body) =
			rendered(Error::Unavailable("SMTP: smtp://user:pw@host refused".to_owned())).await;
		assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
		assert_eq!(body["error"]["errCode"], "E-CORE-UNAVAILABLE");
		assert_eq!(body["error"]["errStr"], "service unavailable");

		let (status, body) = rendered(Error::Timeout("NAV gave no answer".to_owned())).await;
		assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
		assert_eq!(body["error"]["errCode"], "E-CORE-TIMEOUT");
		assert_eq!(body["error"]["errStr"], "upstream timed out");

		let (status, body) =
			rendered(Error::internal("connection string is bad: user:pw@host")).await;
		assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
		assert_eq!(body["error"]["errStr"], "internal error");

		let (_, body) = rendered(Error::coded(
			StatusCode::BAD_GATEWAY,
			"E-NAV-CREDENTIALS",
			"secret 'x' is bad",
		))
		.await;
		assert_eq!(body["error"]["errCode"], "E-NAV-CREDENTIALS");
		assert_eq!(body["error"]["errStr"], "internal error");
	}

	/// The classification the job runner reads, at the ends that are easy to get wrong: a
	/// timeout retries, a `Coded` defaults to terminal, and `coded_retry` is the opt-in.
	#[test]
	fn retry_class_follows_the_variant() {
		assert_eq!(Error::Timeout("x".to_owned()).retry(), Retry::Backoff);
		assert_eq!(Error::internal("x").retry(), Retry::Never);
		assert_eq!(
			Error::coded(StatusCode::BAD_GATEWAY, "E-NAV-BUSINESS", "x").retry(),
			Retry::Never
		);
		assert_eq!(
			Error::coded_retry(StatusCode::BAD_GATEWAY, "E-NAV-UNAVAILABLE", "x").retry(),
			Retry::Backoff
		);
	}

	#[test]
	fn err_code_reaches_the_response_extensions() {
		let res =
			Error::coded(StatusCode::UNAUTHORIZED, "E-AUTH-CREDENTIALS", "nope").into_response();
		assert_eq!(res.extensions().get::<ErrCode>().unwrap().0, "E-AUTH-CREDENTIALS");
	}

	/// The rendered envelope of one error.
	async fn rendered(err: Error) -> (StatusCode, serde_json::Value) {
		let res = err.into_response();
		let status = res.status();
		let bytes = res.into_body().collect().await.unwrap().to_bytes();
		(status, serde_json::from_slice(&bytes).unwrap())
	}
}

// vim: ts=4
