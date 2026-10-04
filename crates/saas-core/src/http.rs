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

use std::{
	net::{IpAddr, Ipv4Addr, SocketAddr},
	pin::Pin,
	sync::OnceLock,
	task::{Context, Poll},
	time::Duration,
};

use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, body::Bytes};
use hyper_util::{
	client::legacy::{
		Client,
		connect::{
			Connect, HttpConnector,
			dns::{GaiResolver, Name},
		},
	},
	rt::TokioExecutor,
};
use tokio::time::timeout;

use crate::error::{ClResult, Error, StatusCode};

type HttpsClient<R = GaiResolver> =
	Client<hyper_rustls::HttpsConnector<HttpConnector<R>>, Full<Bytes>>;

// Process-global, so tests must not reuse a mock server's port — see CLAUDE.md, Testing.
static CLIENT: OnceLock<HttpsClient> = OnceLock::new();
static EXTERNAL: OnceLock<HttpsClient<NoInternal>> = OnceLock::new();
/// A script's call to a host the bundle's `http_hosts` names: unfiltered, but not the
/// framework's pool.
static LISTED: OnceLock<HttpsClient> = OnceLock::new();

tokio::task_local! {
	static NO_REMOTE: ();
}

/// The `errCode` of an outbound call made inside [`without_remote`].
pub const E_REMOTE_IN_TX: &str = "E-CORE-REMOTE-IN-TX";

/// Runs `f` with every outbound call refused as `409 E-CORE-REMOTE-IN-TX`: a caller holding
/// the only writer connection must not wait out an upstream's timeout while holding it.
pub async fn without_remote<F: Future>(f: F) -> F::Output {
	NO_REMOTE.scope((), f).await
}

/// Whether the calling task is inside [`without_remote`].
#[must_use]
pub fn remote_forbidden() -> bool {
	NO_REMOTE.try_with(|()| ()).is_ok()
}

/// `OnceLock::get_or_try_init` is unstable, so a lost race just builds a second client and
/// drops it — harmless, and it happens at most once.
fn client(cell: &'static OnceLock<HttpsClient>) -> ClResult<&'static HttpsClient> {
	if let Some(c) = cell.get() {
		return Ok(c);
	}
	let https = https_builder()?.build();
	Ok(cell.get_or_init(|| Client::builder(TokioExecutor::new()).build(https)))
}

/// [`client`]'s twin whose resolver is [`NoInternal`]: its own pool, so a connection opened
/// for a framework call is never handed to a script's request, or the reverse.
fn external_client() -> ClResult<&'static HttpsClient<NoInternal>> {
	if let Some(c) = EXTERNAL.get() {
		return Ok(c);
	}
	let mut http = HttpConnector::new_with_resolver(NoInternal(GaiResolver::new()));
	http.enforce_http(false);
	let https = https_builder()?.wrap_connector(http);
	Ok(EXTERNAL.get_or_init(|| Client::builder(TokioExecutor::new()).build(https)))
}

fn https_builder()
-> ClResult<hyper_rustls::HttpsConnectorBuilder<hyper_rustls::builderstates::WantsProtocols2>> {
	Ok(hyper_rustls::HttpsConnectorBuilder::new()
		.with_native_roots()
		.map_err(|e| Error::internal(format!("no native root CA certificates: {e}")))?
		.https_or_http()
		.enable_http1())
}

/// Loopback, unspecified, RFC 1918, link-local, CGNAT `100.64/10` and ULA `fc00::/7`, plus a
/// NAT64 `64:ff9b::/96` address embedding any of those. Link-local and the metadata addresses
/// inside CGNAT (`100.100.100.200`) and ULA (`fd00:ec2::254`) hand out cloud credentials; the
/// rest is the host's own network. A local API is reached by naming it in `http_hosts`.
/// Also `192.0.0/24`, benchmarking `198.18/15`, reserved `240/4` and a 6to4 `2002::/16` wrap.
fn is_internal(ip: IpAddr) -> bool {
	match ip.to_canonical() {
		IpAddr::V4(v4) => is_internal_v4(v4),
		IpAddr::V6(v6) => {
			let s = v6.segments();
			v6.is_loopback()
				|| v6.is_unspecified()
				|| (s[0] & 0xffc0) == 0xfe80
				|| (s[0] & 0xfe00) == 0xfc00
				|| (s[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
					&& is_internal_v4(Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7]))))
				|| (s[0] == 0x2002
					&& is_internal_v4(Ipv4Addr::from((u32::from(s[1]) << 16) | u32::from(s[2]))))
		}
	}
}

fn is_internal_v4(v4: Ipv4Addr) -> bool {
	let [a, b, c, _] = v4.octets();
	v4.is_loopback()
		|| v4.is_private()
		|| v4.is_link_local()
		|| a == 0
		|| (a == 100 && (b & 0xc0) == 64)
		|| (a == 192 && b == 0 && c == 0)
		|| (a == 198 && (b & 0xfe) == 18)
		|| a >= 240
}

/// The resolver error [`send`] finds in a connect failure's source chain to tell a refusal
/// from an outage.
#[derive(Debug)]
struct InternalRefused;

impl std::fmt::Display for InternalRefused {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("the host resolves only to internal addresses")
	}
}

impl std::error::Error for InternalRefused {}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn routable(
	addrs: impl Iterator<Item = SocketAddr>,
) -> Result<std::vec::IntoIter<SocketAddr>, BoxError> {
	let kept: Vec<_> = addrs.filter(|a| !is_internal(a.ip())).collect();
	if kept.is_empty() {
		return Err(Box::new(InternalRefused));
	}
	Ok(kept.into_iter())
}

/// `GaiResolver` minus [`is_internal`] addresses. Filtering the resolved addresses rather than
/// checking the name up front is what makes it rebinding-safe: the addresses checked are the
/// ones connected to. An IP-literal host never reaches a resolver, so [`send`] checks those.
#[derive(Clone)]
pub struct NoInternal(GaiResolver);

impl tower_service::Service<Name> for NoInternal {
	type Response = std::vec::IntoIter<SocketAddr>;
	type Error = BoxError;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, BoxError>> + Send>>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
		tower_service::Service::poll_ready(&mut self.0, cx).map_err(Into::into)
	}

	fn call(&mut self, name: Name) -> Self::Future {
		let resolving = tower_service::Service::call(&mut self.0, name);
		Box::pin(async move { routable(resolving.await?) })
	}
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

/// [`post`] for a URL the framework did not choose — a script's. A target that is, or
/// resolves only to, an [`is_internal`] address is [`Error::Validation`], unless
/// `allow_internal`: the caller vouches the host was named explicitly.
pub async fn post_external(
	uri: &str,
	headers: &[(&str, &str)],
	body: Vec<u8>,
	deadline: Duration,
	allow_internal: bool,
) -> ClResult<(StatusCode, Option<u64>, Bytes)> {
	if allow_internal {
		return send_via(client(&LISTED)?, Method::POST, uri, headers, body, deadline).await;
	}
	refuse_internal_literal(uri)?;
	send_via(external_client()?, Method::POST, uri, headers, body, deadline).await
}

/// [`post_external`] whose body is handed back unread, for a reply that arrives as a stream
/// (an LLM's server-sent events). `deadline` bounds only the status line; each chunk after it
/// must arrive within `idle`, and the whole body is still capped at [`MAX_RESPONSE_BYTES`].
pub async fn post_external_stream(
	uri: &str,
	headers: &[(&str, &str)],
	body: Vec<u8>,
	deadline: Duration,
	idle: Duration,
	allow_internal: bool,
) -> ClResult<(StatusCode, Option<u64>, BodyStream)> {
	let (status, retry_after, res) = if allow_internal {
		request_via(client(&LISTED)?, Method::POST, uri, headers, body, deadline).await?
	} else {
		refuse_internal_literal(uri)?;
		request_via(external_client()?, Method::POST, uri, headers, body, deadline).await?
	};
	let body = Limited::new(res.into_body(), MAX_RESPONSE_BYTES);
	Ok((status, retry_after, BodyStream { body, idle, uri: uri.to_owned() }))
}

/// A reply body read chunk by chunk; see [`post_external_stream`].
pub struct BodyStream {
	body: Limited<hyper::body::Incoming>,
	idle: Duration,
	uri: String,
}

impl BodyStream {
	/// The next data chunk, `None` at the end of the body. A stall past `idle`, a dropped
	/// connection or a body past [`MAX_RESPONSE_BYTES`] is [`Error::Timeout`].
	pub async fn next_chunk(&mut self) -> ClResult<Option<Bytes>> {
		loop {
			let frame = timeout(self.idle, self.body.frame())
				.await
				.map_err(|_| timed_out(&self.uri, "stream idle timeout"))?;
			match frame {
				None => return Ok(None),
				Some(Err(e)) => return Err(incomplete(&self.uri, &e.to_string())),
				// Trailers carry no data.
				Some(Ok(f)) => {
					if let Ok(data) = f.into_data() {
						return Ok(Some(data));
					}
				}
			}
		}
	}
}

/// [`get`] with [`post_external`]'s internal-address refusal.
pub async fn get_external(
	uri: &str,
	headers: &[(&str, &str)],
	deadline: Duration,
	allow_internal: bool,
) -> ClResult<(StatusCode, Option<u64>, Bytes)> {
	if allow_internal {
		return send_via(client(&LISTED)?, Method::GET, uri, headers, Vec::new(), deadline).await;
	}
	refuse_internal_literal(uri)?;
	send_via(external_client()?, Method::GET, uri, headers, Vec::new(), deadline).await
}

/// `HttpConnector` connects to an IP-literal host without asking its resolver, so
/// [`NoInternal`] never sees one.
fn refuse_internal_literal(uri: &str) -> ClResult<()> {
	let uri = uri.parse::<hyper::Uri>().ok();
	let host = uri.as_ref().and_then(hyper::Uri::host).unwrap_or_default();
	match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
		Ok(ip) if is_internal(ip) => Err(Error::Validation(format!("{ip} is an internal address"))),
		_ => Ok(()),
	}
}

/// Refuses a URL whose host is, or resolves to, an internal address — for a target another
/// server (a self-hosted reader) fetches on our behalf, where [`NoInternal`] never runs.
pub async fn refuse_internal_target(url: &str) -> ClResult<()> {
	refuse_internal_literal(url)?;
	let uri = url
		.parse::<hyper::Uri>()
		.map_err(|e| Error::Validation(format!("invalid url {url}: {e}")))?;
	let host = uri.host().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
	if host.is_empty() {
		return Err(Error::Validation(format!("{url} has no host")));
	}
	let port = uri
		.port_u16()
		.unwrap_or(if uri.scheme_str() == Some("http") { 80 } else { 443 });
	let addrs = tokio::net::lookup_host((host, port))
		.await
		.map_err(|e| Error::Validation(format!("{host} does not resolve: {e}")))?;
	for a in addrs {
		if is_internal(a.ip()) {
			return Err(Error::Validation(format!("{host} resolves to an internal address")));
		}
	}
	Ok(())
}

/// `hyper-rustls` and `HttpConnector` each wrap the resolver's error, so it is found by walking
/// the source chain rather than at a fixed depth.
fn refused_by_resolver(e: &hyper_util::client::legacy::Error) -> bool {
	let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
	while let Some(err) = cur {
		if err.is::<InternalRefused>() {
			return true;
		}
		cur = err.source();
	}
	false
}

/// What goes out when the caller names nothing: an absent `User-Agent` is a standard bot rule at
/// an edge in front of a gateway, and nothing in this workspace named one.
const USER_AGENT: &str = concat!("saas-framework/", env!("CARGO_PKG_VERSION"));

async fn send(
	method: Method,
	uri: &str,
	headers: &[(&str, &str)],
	body: Vec<u8>,
	deadline: Duration,
) -> ClResult<(StatusCode, Option<u64>, Bytes)> {
	send_via(client(&CLIENT)?, method, uri, headers, body, deadline).await
}

async fn send_via<C: Connect + Clone + Send + Sync + 'static>(
	client: &Client<C, Full<Bytes>>,
	method: Method,
	uri: &str,
	headers: &[(&str, &str)],
	body: Vec<u8>,
	deadline: Duration,
) -> ClResult<(StatusCode, Option<u64>, Bytes)> {
	let (status, retry_after, res) =
		request_via(client, method, uri, headers, body, deadline).await?;
	let body = Limited::new(res.into_body(), MAX_RESPONSE_BYTES);
	let bytes = timeout(deadline, body.collect())
		.await
		.map_err(|_| timed_out(uri, "body read timed out"))?
		.map_err(|e| incomplete(uri, &e.to_string()))?
		.to_bytes();
	Ok((status, retry_after, bytes))
}

/// Everything up to the status line; the body is the caller's to read.
async fn request_via<C: Connect + Clone + Send + Sync + 'static>(
	client: &Client<C, Full<Bytes>>,
	method: Method,
	uri: &str,
	headers: &[(&str, &str)],
	body: Vec<u8>,
	deadline: Duration,
) -> ClResult<(StatusCode, Option<u64>, hyper::Response<hyper::body::Incoming>)> {
	if remote_forbidden() {
		return Err(Error::coded(
			StatusCode::CONFLICT,
			E_REMOTE_IN_TX,
			"an outbound call cannot run while a write transaction is held",
		));
	}
	let mut builder = Request::builder().method(method).uri(uri);
	// A caller's own wins: the framework does not silently substitute its own.
	let mut named = false;
	for (name, value) in headers {
		named |= name.eq_ignore_ascii_case("user-agent");
		builder = builder.header(*name, *value);
	}
	if !named {
		builder = builder.header("user-agent", USER_AGENT);
	}
	let req = builder
		.body(Full::new(Bytes::from(body)))
		.map_err(|e| Error::internal(format!("malformed outbound request: {e}")))?;

	let res = timeout(deadline, client.request(req))
		.await
		.map_err(|_| timed_out(uri, "timed out"))?
		// Only a *connect* failure is safely "never reached": `hyper` also errors here after the
		// body was fully written, and classifying that as `Unavailable` had `nav::job::report`
		// resend a `manageInvoice` for an invoice number NAV had already taken.
		.map_err(|e| if refused_by_resolver(&e) {
			Error::Validation(InternalRefused.to_string())
		} else if e.is_connect() {
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
	Ok((status, retry_after, res))
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

/// RFC 3986 percent-encoding: everything outside the unreserved set. The workspace has no URL
/// crate.
pub fn pct(s: &str) -> String {
	use std::fmt::Write as _;

	let mut out = String::with_capacity(s.len());
	for b in s.bytes() {
		match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
				out.push(char::from(b));
			}
			// Writing to a `String` is infallible.
			_ => drop(write!(out, "%{b:02X}")),
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use wiremock::matchers::{header, method};
	use wiremock::{Mock, MockServer, ResponseTemplate};

	use super::*;

	#[test]
	fn reserved_and_embedded_ranges_are_internal() {
		for ip in ["192.0.0.8", "198.18.0.1", "198.19.255.255", "240.0.0.1", "255.255.255.255"]
			.into_iter()
			.chain(["2002:7f00:0001::1", "2002:a9fe:a9fe::1"])
		{
			assert!(is_internal(ip.parse().unwrap()), "{ip}");
		}
		for ip in ["8.8.8.8", "192.0.2.1", "198.20.0.1", "2002:0808:0808::1"] {
			assert!(!is_internal(ip.parse().unwrap()), "{ip}");
		}
	}

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
			let server = MockServer::builder().start().await;
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

	/// An absent `User-Agent` is a standard bot rule at an edge in front of a gateway, and one
	/// refusal of that kind looks exactly like the gateway refusing the account.
	#[tokio::test]
	async fn an_outbound_request_names_this_framework_unless_the_caller_says_otherwise() {
		for (supplied, want) in [(None, USER_AGENT), (Some("acme/2"), "acme/2")] {
			let server = MockServer::builder().start().await;
			Mock::given(method("GET"))
				.and(header("user-agent", want))
				.respond_with(ResponseTemplate::new(200))
				.mount(&server)
				.await;
			let headers = supplied.map(|ua| vec![("user-agent", ua)]).unwrap_or_default();

			let got = get(&server.uri(), &headers, Duration::from_secs(5)).await.unwrap();
			// wiremock answers an unmatched request 404, so the status is the assertion.
			assert_eq!(got.0, StatusCode::OK, "{supplied:?}");
		}
	}

	#[tokio::test]
	async fn an_external_call_to_an_internal_literal_is_refused() {
		for uri in [
			"http://169.254.169.254/latest/meta-data/",
			"http://[fe80::1]/",
			"http://[::ffff:169.254.169.254]/",
			"http://[fd00:ec2::254]/",
			"http://100.100.100.200/",
			"http://[::ffff:100.100.100.200]/",
			"http://127.0.0.1:1/",
			"http://[::1]/",
			"http://0.0.0.0/",
			"http://10.0.0.1/",
			"http://192.168.1.1/",
			"http://100.64.0.1/",
			"http://[fd00::1]/",
			"http://[64:ff9b::a9fe:a9fe]/",
		] {
			let err = get_external(uri, &[], Duration::from_secs(5), false).await.unwrap_err();
			assert!(matches!(err, Error::Validation(_)), "{uri}: {err}");
		}
		assert!(refuse_internal_literal("http://1.1.1.1/").is_ok());
		assert!(refuse_internal_literal("http://[64:ff9b::101:101]/").is_ok());
	}

	#[test]
	fn the_resolver_drops_internal_addresses_and_refuses_when_none_remain() {
		let a = |s: &str| s.parse::<SocketAddr>().unwrap();
		let addrs = [
			a("169.254.169.254:80"),
			a("10.0.0.5:80"),
			a("1.1.1.1:80"),
			a("[fe80::1]:80"),
			a("[fd00:ec2::254]:80"),
		];
		let kept: Vec<_> = routable(addrs.into_iter()).unwrap().collect();
		assert_eq!(kept, [a("1.1.1.1:80")]);
		let err = routable([a("127.0.0.1:80"), a("[fe80::2]:80")].into_iter()).unwrap_err();
		assert!(err.is::<InternalRefused>());
	}

	/// End to end through the real resolver: `localhost` is refused unless the caller vouches
	/// for the host, and then it connects.
	#[tokio::test]
	async fn an_external_call_to_localhost_needs_the_host_listed() {
		let server = MockServer::builder().start().await;
		Mock::given(method("GET"))
			.respond_with(ResponseTemplate::new(200))
			.mount(&server)
			.await;
		let uri = server.uri().replace("127.0.0.1", "localhost");
		let err = get_external(&uri, &[], Duration::from_secs(5), false).await.unwrap_err();
		assert!(matches!(err, Error::Validation(_)), "{err}");
		let got = get_external(&uri, &[], Duration::from_secs(5), true).await.unwrap();
		assert_eq!(got.0, StatusCode::OK);
	}

	#[tokio::test]
	async fn a_call_inside_without_remote_is_refused() {
		let err = without_remote(get("http://127.0.0.1:1/", &[], Duration::from_secs(5)))
			.await
			.unwrap_err();
		assert_eq!(err.parts().1, E_REMOTE_IN_TX);
		assert!(!remote_forbidden());
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
			let server = MockServer::builder().start().await;
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
