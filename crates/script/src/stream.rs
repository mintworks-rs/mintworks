// SPDX-License-Identifier: MPL-2.0
//! The opaque handles `http::stream` and `fs::open` return, and `resp::stream`, which hands one
//! to axum. The script never iterates a body: Rune 0.14 has no `Send` path for per-chunk
//! execution, and the script's deadline ends at `return` (`claude-docs/port-features.md` §3).

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::Mutex,
};

use axum::{
	body::Body,
	http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, header},
	response::Response,
};
use mintworks_core::{
	error::{ClResult, Error},
	http::BodyStream,
};
use rune::{
	Any, ContextError, Module, Value,
	runtime::{Protocol, Ref},
};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{
	error::{self, R},
	value::{ScriptError, from_json, to_json},
};

/// `http::stream`'s reply: `.status`, `.headers` and `.text()`, which consumes the body.
#[derive(Any)]
pub struct HttpStream {
	#[rune(get, copy)]
	pub status: u16,
	pub headers: HashMap<String, String>,
	body: Mutex<Option<BodyStream>>,
}

impl HttpStream {
	pub(crate) fn new(status: u16, headers: HashMap<String, String>, body: BodyStream) -> Self {
		Self { status, headers, body: Mutex::new(Some(body)) }
	}

	fn take(&self) -> R<BodyStream> {
		self.body
			.lock()
			.ok()
			.and_then(|mut b| b.take())
			.ok_or_else(|| ScriptError(error::runtime("the stream body was already consumed")))
	}
}

/// `stream.text()` — the whole body, still capped at `mintworks_core::http::MAX_RESPONSE_BYTES`.
#[rune::function(instance)]
async fn text(this: Ref<HttpStream>) -> R<String> {
	let body = this.take()?;
	drop(this);
	let buf = body.collect().await.map_err(ScriptError)?;
	Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// `fs::open`'s handle: a path already confined under the fs root.
#[derive(Any)]
pub struct FileHandle {
	pub(crate) path: PathBuf,
}

pub(crate) enum Source {
	/// Status, body and the upstream `content-encoding`, which the bytes still carry.
	Http(u16, BodyStream, Option<String>),
	File(PathBuf),
}

/// What `resp::stream` returns; `routes::Reply` takes the source out of it.
#[derive(Any)]
pub struct Streamed {
	source: Mutex<Option<Source>>,
	content_type: Option<String>,
}

impl Streamed {
	pub(crate) fn take(&self) -> Option<(Source, Option<String>)> {
		let source = self.source.lock().ok()?.take()?;
		Some((source, self.content_type.clone()))
	}
}

/// `resp::stream(handle, #{content_type})` — an `HttpStream` keeps its upstream status and, unless
/// overridden, its content type; a `FileHandle` is served with Range and conditional requests.
#[rune::function]
pub fn stream(handle: Value, opts: Value) -> R<Streamed> {
	let opts = to_json(&opts)?;
	let mut content_type =
		opts.get("content_type").and_then(serde_json::Value::as_str).map(str::to_owned);
	let source = if let Ok(h) = handle.borrow_ref::<HttpStream>() {
		content_type = content_type.or_else(|| h.headers.get("content-type").cloned());
		Source::Http(h.status, h.take()?, h.headers.get("content-encoding").cloned())
	} else if let Ok(f) = handle.borrow_ref::<FileHandle>() {
		Source::File(f.path.clone())
	} else {
		return Err(ScriptError(error::runtime(
			"resp::stream takes an http::stream or fs::open handle",
		)));
	};
	Ok(Streamed { source: Mutex::new(Some(source)), content_type })
}

fn with_type(mut res: Response, content_type: Option<&str>) -> Response {
	if let Some(v) = content_type.and_then(|ct| HeaderValue::from_str(ct).ok()) {
		res.headers_mut().insert(header::CONTENT_TYPE, v);
	}
	// Proxied or stored bytes are untrusted, and served from the app's own origin.
	let h = res.headers_mut();
	h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
	h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
	res
}

/// A trickling upstream otherwise holds the response open indefinitely: the idle timeout
/// bounds each chunk, this the whole body.
const STREAM_TOTAL: std::time::Duration = std::time::Duration::from_mins(10);

/// Upstream bytes as they arrive; each chunk waits at most the stream's idle timeout.
pub(crate) fn http_response(
	status: u16,
	content_type: Option<&str>,
	encoding: Option<&str>,
	body: BodyStream,
) -> Response {
	let start = tokio::time::Instant::now();
	let chunks = futures_util::stream::unfold(Some(body), move |state| async move {
		let mut body = state?;
		if start.elapsed() >= STREAM_TOTAL {
			return Some((Err(std::io::Error::other("upstream stream over its total cap")), None));
		}
		match body.next_chunk().await {
			Ok(Some(chunk)) => Some((Ok(chunk), Some(body))),
			Ok(None) => None,
			Err(e) => Some((Err(std::io::Error::other(e.to_string())), None)),
		}
	});
	let mut res = Response::new(Body::from_stream(chunks));
	// `@mintworks/client` reads a 401 as its own session ending, so an upstream's must not pass.
	let status = if matches!(status, 401 | 403) { 502 } else { status };
	*res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
	if let Some(v) = encoding.and_then(|e| HeaderValue::from_str(e).ok()) {
		res.headers_mut().insert(header::CONTENT_ENCODING, v);
	}
	with_type(res, content_type)
}

/// The request's own headers reach `ServeFile`, so `Range` and `If-None-Match` apply. The method
/// is forced to GET (HEAD kept): `ServeFile` answers 405 to anything else, and the route's own
/// method is the script's business.
pub(crate) async fn file_response(
	path: &Path,
	content_type: Option<&str>,
	method: &Method,
	uri: &Uri,
	headers: &HeaderMap,
) -> ClResult<Response> {
	let method = if method == Method::HEAD { Method::HEAD } else { Method::GET };
	let mut req = Request::builder()
		.method(method)
		.uri(uri.clone())
		.body(Body::empty())
		.map_err(|e| Error::internal(format!("file request: {e}")))?;
	*req.headers_mut() = headers.clone();
	// Re-checked at serve time; the remaining window needs a non-script writer in the fs root
	// (scripts cannot create symlinks).
	if path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
		return Err(Error::NotFound);
	}
	let res = ServeFile::new(path)
		.oneshot(req)
		.await
		.map_err(|e| Error::internal(format!("{}: {e}", path.display())))?;
	Ok(with_type(res.map(Body::new), content_type))
}

/// Registers the handle types and `HttpStream::text`. In the base set: a type no function hands
/// out reaches nothing, so `fs`/`http` stay the gate.
///
/// # Errors
/// Whatever Rune raises registering a type or a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::new();
	m.ty::<HttpStream>()?;
	m.function_meta(text)?;
	// A field getter, not `#[rune(get)]`: that derive needs `TryClone`, which `HashMap` lacks.
	m.field_function(&Protocol::GET, "headers", |this: &HttpStream| -> R<Value> {
		from_json(&serde_json::json!(this.headers)).map_err(ScriptError)
	})?;
	m.ty::<FileHandle>()?;
	m.ty::<Streamed>()?;
	Ok(m)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_streamed_reply_is_never_sniffed_nor_scripted() {
		let res = with_type(Response::new(Body::empty()), Some("text/html"));
		assert_eq!(res.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
		assert_eq!(res.headers()[header::CONTENT_SECURITY_POLICY], "sandbox");
	}

	async fn upstream(status: u16, body: &str) -> (wiremock::MockServer, BodyStream, StatusCode) {
		use wiremock::{Mock, ResponseTemplate, matchers::any};
		let server = wiremock::MockServer::builder().start().await;
		Mock::given(any())
			.respond_with(
				ResponseTemplate::new(status)
					.insert_header("content-encoding", "gzip")
					.set_body_bytes(body.as_bytes()),
			)
			.mount(&server)
			.await;
		let d = std::time::Duration::from_secs(5);
		let url = format!("{}/x", server.uri());
		let (status, _, body) =
			mintworks_core::http::stream_external("GET", &url, &[], vec![], d, d, true)
				.await
				.unwrap();
		(server, body, status)
	}

	#[tokio::test]
	async fn a_proxied_body_streams_through_with_its_encoding() {
		let (_server, body, status) = upstream(200, "chunked body").await;
		let res = http_response(status.as_u16(), Some("text/plain"), Some("gzip"), body);
		assert_eq!(res.status(), StatusCode::OK);
		assert_eq!(res.headers()[header::CONTENT_ENCODING], "gzip");
		assert_eq!(res.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
		assert_eq!(res.headers()[header::CONTENT_SECURITY_POLICY], "sandbox");
		let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
		assert_eq!(&bytes[..], b"chunked body");
	}

	#[tokio::test]
	async fn collect_reads_the_whole_body() {
		let (_server, body, _) = upstream(200, "chunked body").await;
		assert_eq!(&body.collect().await.unwrap()[..], b"chunked body");
	}

	#[tokio::test]
	async fn an_upstream_401_is_a_bad_gateway() {
		let (_server, body, status) = upstream(401, "no").await;
		let res = http_response(status.as_u16(), None, None, body);
		assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
	}
}

// vim: ts=4
