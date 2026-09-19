//! One outbound HTTPS client for the whole process.
//!
//! Every caller here does the same five steps — build a connector, build a request, POST it
//! under a timeout, check the status, collect the body under a timeout — and the connector
//! is the expensive one: building it per call reparses the OS trust store and throws away
//! the connection pool, so filing one invoice loaded the native roots twice.
//!
//! The connector is `https_or_http` for the one caller that needs it: `saas-nav`'s base URL
//! is operator-settable and a loopback stand-in must work, which `saas_nav::auth::require_tls`
//! is the actual guard for. Every other caller passes a compile-time `https://` constant, and
//! the URI's scheme is what picks TLS.

use std::{sync::OnceLock, time::Duration};

use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, body::Bytes};
use hyper_util::{
	client::legacy::{Client, connect::HttpConnector},
	rt::TokioExecutor,
};
use tokio::time::timeout;

use crate::error::{ClResult, Error, StatusCode};

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

static CLIENT: OnceLock<HttpsClient> = OnceLock::new();

/// `OnceLock::get_or_try_init` is unstable, so a lost race just builds a second client and
/// drops it — harmless, and it happens at most once.
fn client() -> ClResult<&'static HttpsClient> {
	if let Some(c) = CLIENT.get() {
		return Ok(c);
	}
	let https = hyper_rustls::HttpsConnectorBuilder::new()
		.with_native_roots()
		.map_err(|e| Error::internal(format!("no native root CA certificates: {e}")))?
		.https_or_http()
		.enable_http1()
		.build();
	Ok(CLIENT.get_or_init(|| Client::builder(TokioExecutor::new()).build(https)))
}

/// The largest upstream reply this process will buffer.
///
/// `deadline` alone does not bound it: an upstream streaming at line rate for the whole
/// timeout is an OOM of the process, and every other resource in [`post`] is bounded. Sized
/// two orders of magnitude above the largest real reply — NAV's `queryInvoiceData` (tens of
/// KiB) and the MNB rate document over a long date range (a few hundred KiB).
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// POST `body` to `uri` and read the whole reply. `deadline` bounds the request and the body
/// read separately.
///
/// The status is returned rather than judged: NAV reads its own error document out of a 4xx,
/// where VIES treats any non-2xx as an outage. A transport failure is
/// [`Error::Unavailable`] with the detail logged, and every caller maps it onto its own
/// `errCode`.
///
/// The third element is `Retry-After` in **seconds**. Only the delta-seconds form is read —
/// the HTTP-date form falls back to `None` and the caller's own backoff, which is what NAV
/// sends anyway. One header rather than a `HeaderMap`: it is all any caller wants, and a map
/// would put `hyper` types in `saas-nav`'s face.
pub async fn post(
	uri: &str,
	headers: &[(&str, &str)],
	body: Vec<u8>,
	deadline: Duration,
) -> ClResult<(StatusCode, Option<u64>, Bytes)> {
	send(Method::POST, uri, headers, body, deadline).await
}

/// GET `uri`, with [`post`]'s connector, deadline split and error classification.
///
/// Here because a gateway that answers a state query on GET cannot be reached with [`post`]
/// at all, and the alternative was a second TLS connector and connection pool in the calling
/// adapter — which is the cost this module exists to avoid.
pub async fn get(
	uri: &str,
	headers: &[(&str, &str)],
	deadline: Duration,
) -> ClResult<(StatusCode, Option<u64>, Bytes)> {
	send(Method::GET, uri, headers, Vec::new(), deadline).await
}

async fn send(
	method: Method,
	uri: &str,
	headers: &[(&str, &str)],
	body: Vec<u8>,
	deadline: Duration,
) -> ClResult<(StatusCode, Option<u64>, Bytes)> {
	let mut builder = Request::builder().method(method).uri(uri);
	for (name, value) in headers {
		builder = builder.header(*name, *value);
	}
	let req = builder
		.body(Full::new(Bytes::from(body)))
		.map_err(|e| Error::internal(format!("malformed outbound request: {e}")))?;

	let res = timeout(deadline, client()?.request(req))
		.await
		.map_err(|_| timed_out(uri, "timed out"))?
		// Only a *connect* failure is safely "never reached": `hyper` also errors here after the
		// body was fully written, and classifying that as `Unavailable` had `nav::job::report`
		// resend a `manageInvoice` for an invoice number NAV had already taken.
		.map_err(|e| if e.is_connect() {
			failed(uri, &e.to_string())
		} else {
			incomplete(uri, &e.to_string())
		})?;
	let status = res.status();
	let retry_after = res
		.headers()
		.get(hyper::header::RETRY_AFTER)
		.and_then(|v| v.to_str().ok())
		.and_then(|v| v.trim().parse::<u64>().ok());
	let body = Limited::new(res.into_body(), MAX_RESPONSE_BYTES);
	let bytes = timeout(deadline, body.collect())
		.await
		.map_err(|_| timed_out(uri, "body read timed out"))?
		.map_err(|e| incomplete(uri, &e.to_string()))?
		.to_bytes();
	Ok((status, retry_after, bytes))
}

/// The `uri` goes to the log, never into the response: a NAV base URL is operator
/// configuration. The message does not reach a client either — `IntoResponse` renders a fixed
/// per-variant string for every 5xx — so what these three build is for the log and for the
/// variant `Error::retry` reads, not for a caller.
///
/// A *connect* failure means the request did not reach the application, which a caller may
/// safely retry; [`timed_out`] and [`incomplete`] mean it may have. Every other transport
/// error goes to [`incomplete`]: once the body is on the wire, a dropped connection says
/// nothing about whether the upstream processed it.
fn failed(uri: &str, why: &str) -> Error {
	tracing::warn!(uri = redact(uri), why, "outbound request failed");
	Error::Unavailable("upstream service unavailable".to_owned())
}

/// The request was written but no complete reply came back — the connection dropped before
/// the status line, dropped mid-body, or the reply ran past [`MAX_RESPONSE_BYTES`]. The upstream may
/// well have processed it, so this is [`Error::Timeout`] and not [`Error::Unavailable`]:
/// `saas_nav::job::report` parks a filing as `UNKNOWN` on exactly this distinction, and
/// classifying it as "never reached NAV" resent a statutory filing that had already landed.
fn incomplete(uri: &str, why: &str) -> Error {
	tracing::warn!(uri = redact(uri), why, "outbound reply was not received in full");
	Error::Timeout("upstream reply was not received in full".to_owned())
}

/// The deadline expired, so whether the upstream processed the request is unknowable. Kept
/// apart from [`failed`] because `saas-nav` decides between retrying a filing and parking it
/// as `UNKNOWN` on exactly this distinction.
fn timed_out(uri: &str, why: &str) -> Error {
	tracing::warn!(uri = redact(uri), why, "outbound request timed out");
	Error::Timeout("upstream service timed out".to_owned())
}

/// The path, without the query string: a query string is where a gateway puts its API key
/// (Barion's `GetPaymentState?POSKey=…`), and the path is the whole of what the log needs.
fn redact(uri: &str) -> &str {
	uri.split('?').next().unwrap_or(uri)
}

#[cfg(test)]
mod tests {
	use wiremock::matchers::method;
	use wiremock::{Mock, MockServer, ResponseTemplate};

	use super::*;

	#[test]
	fn redact_drops_the_query_string() {
		assert_eq!(
			redact("https://api.barion.com/v2/Payment/GetPaymentState?POSKey=s3cr3t"),
			"https://api.barion.com/v2/Payment/GetPaymentState"
		);
		assert_eq!(
			redact("https://api.barion.com/v2/Payment/Start"),
			"https://api.barion.com/v2/Payment/Start"
		);
		assert_eq!(redact("?a=b"), "");
	}

	/// The other half of the same distinction: a refused connection genuinely never reached
	/// the upstream, so it stays a clean `Unavailable` the caller may retry.
	#[tokio::test]
	async fn a_refused_connection_is_a_clean_failure() {
		// A port nothing is listening on: bind one, learn its number, drop it.
		let port = {
			let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
			l.local_addr().unwrap().port()
		};
		let uri = format!("http://127.0.0.1:{port}/");
		let err = post(&uri, &[], Vec::new(), Duration::from_secs(5)).await.unwrap_err();
		assert!(matches!(err, Error::Unavailable(_)), "{err}");
	}

	#[tokio::test]
	async fn an_oversized_response_is_indeterminate_not_a_clean_failure() {
		for size in [1024, MAX_RESPONSE_BYTES + 1] {
			let server = MockServer::start().await;
			Mock::given(method("POST"))
				.respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; size]))
				.mount(&server)
				.await;

			let got = post(&server.uri(), &[], Vec::new(), Duration::from_secs(30)).await;
			if size > MAX_RESPONSE_BYTES {
				// The status line was already in hand when the cap tripped, so the upstream may
				// well have processed the request: `saas_nav::job::report` parks a filing as
				// `UNKNOWN` on `Error::Timeout` and resends it on `Error::Unavailable`.
				assert!(matches!(got, Err(Error::Timeout(_))), "{size}: {got:?}");
			} else {
				let (status, retry_after, bytes) = got.unwrap();
				assert_eq!((status, retry_after, bytes.len()), (StatusCode::OK, None, size));
			}
		}
	}

	/// NAV answers a throttle with `Retry-After`, and the header used to be dropped on the
	/// floor: the job backed off `2^attempts` regardless of how long it was asked to wait.
	#[tokio::test]
	async fn a_retry_after_header_comes_back_with_the_status() {
		let cases = [
			(Some("30"), Some(30)),
			(None, None),
			// The HTTP-date form is deliberately not parsed: ~40 lines for a case nobody has
			// seen, and the caller's own backoff is the fallback.
			(Some("Wed, 21 Oct 2026 07:28:00 GMT"), None),
		];
		for (header, want) in cases {
			let server = MockServer::start().await;
			let mut template = ResponseTemplate::new(429);
			if let Some(header) = header {
				template = template.insert_header("retry-after", header);
			}
			Mock::given(method("POST")).respond_with(template).mount(&server).await;

			let (status, retry_after, _) =
				post(&server.uri(), &[], Vec::new(), Duration::from_secs(30)).await.unwrap();
			assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
			assert_eq!(retry_after, want, "{header:?}");
		}
	}
}

// vim: ts=4
