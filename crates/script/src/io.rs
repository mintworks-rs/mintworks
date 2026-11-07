// SPDX-License-Identifier: MPL-2.0
//! `fs`, `http`, `env` and `sys` as four **separately registrable** Rune modules.
//!
//! None of them is a member of the base module set, and none may become one. An app profile
//! registers all four; a future org-level script profile registers none, and an unregistered
//! module is a compile error in the script rather than a permission check at runtime. That
//! single property is what keeps org-level scripting reachable — moving `http` into the base
//! set the first time it is convenient is what forecloses it.

use std::{
	collections::HashMap,
	path::{Component, Path, PathBuf},
	sync::{Arc, OnceLock},
	time::Duration,
};

use mintworks_core::error::ClResult;
use rune::{ContextError, Module, Value};
use serde_json::{Value as Json, json};

use crate::{
	error::{self, R},
	tx,
	value::{ScriptError, from_json},
};

/// How long a script's own HTTP call may take. Not a setting: the `tx::with` deadline is the
/// one that protects the writer connection, and a script that needs a different budget is
/// better served by a job than by a knob.
const HTTP_DEADLINE: Duration = Duration::from_secs(30);

/// What `app.test_env(…)` declared. A cell because the `env` module is compiled in before
/// `main` runs; only `mintworks test` ever fills it, so a served app sees it empty.
pub(crate) type TestEnv = Arc<OnceLock<HashMap<String, String>>>;

/// What `fs::read` buffers at most. A script reads a template or a fixture, not a dataset, so
/// the cap is what keeps an unbounded `read_to_string` from being a memory-exhaustion
/// primitive in an application whose scripts are not all trusted equally.
const FS_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Which host modules an execution context is composed with, and how far each reaches.
///
/// The flags exist so a caller states its profile once, rather than every call site
/// remembering which modules are safe for which kind of script.
#[derive(Clone, Debug, PartialEq, Eq)]
// One flag per registrable module is the whole design: collapsing them into a level would put
// `sys` and `env` behind one switch, which is exactly what the split exists to prevent.
#[allow(clippy::struct_excessive_bools)]
pub struct IoProfile {
	pub fs: bool,
	pub http: bool,
	pub env: bool,
	/// `sys::escalate`. Gated like the other three and for the same reason: an org-level script
	/// that could re-actor its ctx as `Actor::System` would be no boundary at all.
	/// Also `jobs::` (an enqueued job runs as System) and `email::` (mail to any address).
	pub sys: bool,
	/// Every `fs` path resolves under this root. `None` with `fs: true` refuses to build — a
	/// profile with no root would confine nothing, which is worse than one that fails to load.
	pub fs_root: Option<PathBuf>,
	/// Cap on `fs::read`.
	pub fs_max_bytes: u64,
	/// Hosts `http` may reach. Empty means every public host — a script that needs arbitrary
	/// URLs. A host listed here may resolve to an internal address; an unlisted one may not.
	pub http_hosts: Vec<String>,
}

// Hand-written so `IoProfile { fs: true, .. Default::default() }` still carries a usable read
// cap; the derive would give it zero and refuse every read.
impl Default for IoProfile {
	fn default() -> Self {
		Self {
			fs: false,
			http: false,
			env: false,
			sys: false,
			fs_root: None,
			fs_max_bytes: FS_MAX_BYTES,
			http_hosts: Vec::new(),
		}
	}
}

impl IoProfile {
	/// What a consumer application's own scripts get: all four modules, `fs` confined to the
	/// root the caller supplies and `http` unrestricted until a deployment lists hosts.
	#[must_use]
	pub fn app(root: PathBuf) -> Self {
		Self { fs: true, http: true, env: true, sys: true, fs_root: Some(root), ..Self::default() }
	}

	/// What an org-level script would get, and the reason these are four modules and not one.
	#[must_use]
	pub fn sandboxed() -> Self {
		Self::default()
	}

	/// The modules this profile selects, installed into the compile `Context` beside the base set.
	///
	/// # Errors
	/// `E-SCRIPT-COMPILE` for `fs` without a root, or whatever Rune raises registering a module.
	pub(crate) fn modules(&self, test_env: &TestEnv) -> ClResult<Vec<Module>> {
		let ce = |e: ContextError| error::compile(e.to_string());
		let mut out = Vec::new();
		if self.fs {
			let root = self
				.fs_root
				.clone()
				.ok_or_else(|| error::compile("the fs module needs an fs_root"))?;
			out.push(fs::module(root, self.fs_max_bytes).map_err(ce)?);
		}
		if self.http {
			out.push(http::module(self.http_hosts.clone()).map_err(ce)?);
		}
		if self.env {
			out.push(env::module(Arc::clone(test_env)).map_err(ce)?);
			out.push(crate::secret::module(Arc::clone(test_env)).map_err(ce)?);
		}
		if self.sys {
			out.push(crate::sys::module().map_err(ce)?);
			out.push(crate::jobs::module().map_err(ce)?);
			out.push(crate::email::module().map_err(ce)?);
			#[cfg(feature = "ai")]
			out.push(crate::llm::sys_module().map_err(ce)?);
		}
		Ok(out)
	}
}

fn text(json: &Json) -> R<Value> {
	from_json(json).map_err(ScriptError)
}

mod fs {
	use tokio::io::AsyncReadExt;

	use super::*;

	fn failed(path: &str, e: &std::io::Error) -> ScriptError {
		// The detail is the operator's, not the caller's: these are all 5xx.
		ScriptError(error::runtime(format!("{path}: {e}")))
	}

	fn escaped(path: &str) -> ScriptError {
		ScriptError(error::runtime(format!("{path}: outside the script's fs root")))
	}

	/// `path`, resolved under the profile's root.
	///
	/// Lexical `..` is rejected before touching the filesystem, because `canonicalize` fails on
	/// a path that does not exist yet — which is exactly the `fs::write` case. The **parent** is
	/// then canonicalized, so a symlink along the way cannot point out of the root, and the leaf
	/// is refused outright if it is a symlink.
	pub(super) fn under_root(root: &Path, path: &str) -> R<PathBuf> {
		let p = Path::new(path);
		if p.is_absolute() || p.components().any(|c| c == Component::ParentDir) {
			return Err(escaped(path));
		}
		let Some(name) = p.file_name() else {
			return Err(escaped(path));
		};
		let root = root.canonicalize().map_err(|e| failed(path, &e))?;
		let joined = root.join(p);
		let parent = joined.parent().map_or_else(|| root.clone(), Path::to_path_buf);
		let parent = parent.canonicalize().map_err(|e| failed(path, &e))?;
		if !parent.starts_with(&root) {
			return Err(escaped(path));
		}
		let full = parent.join(name);
		// lstat, not the canonicalize above: that resolves the parent chain and leaves the leaf
		// alone, so a symlink there is followed straight out of the root by read, write and
		// exists alike.
		if full.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
			return Err(escaped(path));
		}
		Ok(full)
	}

	pub(super) async fn read_capped(file: &Path, path: &str, max: u64) -> R<String> {
		let f = tokio::fs::File::open(file).await.map_err(|e| failed(path, &e))?;
		let len = f.metadata().await.map_err(|e| failed(path, &e))?.len();
		if len > max {
			return Err(ScriptError(error::runtime(format!(
				"{path}: {len} bytes is over the {max} byte fs::read cap"
			))));
		}
		let mut out = String::new();
		// `take` as well as the stat: a file growing between the two would otherwise be read whole.
		f.take(max).read_to_string(&mut out).await.map_err(|e| failed(path, &e))?;
		Ok(out)
	}

	/// A running suite case's root, else the one the module was built with. A suite compiles once,
	/// so the built-in root is the discovery build's.
	fn root_of(built: &Path) -> PathBuf {
		crate::testing::fs_root().unwrap_or_else(|| built.to_path_buf())
	}

	/// Closures rather than `#[rune::function]` free functions: the root has to reach the call,
	/// and a process-global would be the one property `IoProfile::sandboxed()` exists to deny.
	pub fn module(root: PathBuf, max: u64) -> Result<Module, ContextError> {
		let mut m = Module::with_item(["fs"])?;
		let r = root.clone();
		m.function("read", move |path: String| {
			let root = root_of(&r);
			async move {
				tx::outside_tx("fs::")?;
				let file = under_root(&root, &path)?;
				read_capped(&file, &path, max).await
			}
		})
		.build()?;
		let r = root.clone();
		m.function("write", move |path: String, body: String| {
			let root = root_of(&r);
			async move {
				tx::outside_tx("fs::")?;
				let file = under_root(&root, &path)?;
				tokio::fs::write(&file, body).await.map_err(|e| failed(&path, &e))
			}
		})
		.build()?;
		let r = root.clone();
		// No size cap: the handle is served by `resp::stream`, never buffered.
		m.function("open", move |path: String| {
			let root = root_of(&r);
			async move {
				tx::outside_tx("fs::")?;
				let file = under_root(&root, &path)?;
				tokio::fs::metadata(&file).await.map_err(|e| failed(&path, &e))?;
				Ok::<_, ScriptError>(crate::stream::FileHandle {
					path: file,
					mime: None,
					name: None,
					attachment: false,
				})
			}
		})
		.build()?;
		m.function("exists", move |path: String| {
			let root = root_of(&root);
			async move {
				tx::outside_tx("fs::")?;
				let file = under_root(&root, &path)?;
				Ok::<_, ScriptError>(tokio::fs::metadata(&file).await.is_ok())
			}
		})
		.build()?;
		Ok(m)
	}
}

mod http {
	use super::*;

	/// The status is returned rather than judged, the way `mintworks_core::http` returns it: a
	/// script reading an error document out of a 4xx is the common case.
	fn reply(status: mintworks_core::error::StatusCode, body: &[u8]) -> R<Value> {
		text(&json!({
			"status": status.as_u16(),
			"body": String::from_utf8_lossy(body),
		}))
	}

	/// An empty allowlist allows everything: a deployment opts into confinement, it is not
	/// opted in for it, or every existing bundle would stop reaching its own gateway. `true`
	/// when the host is listed, which lets it resolve to an internal address.
	pub(super) fn allowed(hosts: &[String], url: &str) -> R<bool> {
		if hosts.is_empty() {
			return Ok(false);
		}
		let uri = url.parse::<axum::http::Uri>().ok();
		let host = uri.as_ref().and_then(axum::http::Uri::host);
		match host {
			Some(h) if hosts.iter().any(|a| a.eq_ignore_ascii_case(h)) => Ok(true),
			_ => Err(ScriptError(error::runtime(format!(
				"{}: host is not in the http allowlist",
				mintworks_core::http::redact(url)
			)))),
		}
	}

	/// `Validation` is the `_external` internal-address refusal, raised the way [`allowed`] raises
	/// an unlisted host.
	pub(super) fn refused(url: &str, e: mintworks_core::Error) -> ScriptError {
		match e {
			mintworks_core::Error::Validation(why) => {
				ScriptError(error::runtime(format!("{}: {why}", mintworks_core::http::redact(url))))
			}
			e => ScriptError(e),
		}
	}

	/// The checks every `http::` call runs first; `true` when the host is listed.
	fn gate(hosts: &[String], req: &Req) -> R<bool> {
		tx::outside_tx("http::")?;
		allowed(hosts, &req.url)
	}

	/// `get`, `post` and `request` share this one path; `shape` builds each one's reply.
	pub(super) async fn send<T>(
		hosts: &[String],
		req: Req,
		shape: impl FnOnce(
			mintworks_core::error::StatusCode,
			mintworks_core::http::HeaderList,
			&[u8],
		) -> R<T>,
	) -> R<T> {
		let listed = gate(hosts, &req)?;
		let (status, headers, body) = mintworks_core::http::request_external(
			&req.method,
			&req.url,
			&header_refs(&req.headers),
			req.body,
			HTTP_DEADLINE,
			listed,
		)
		.await
		.map_err(|e| refused(&req.url, e))?;
		shape(status, headers, &body)
	}

	/// `maxRedirects`, 0 when absent. Not on [`Req`]: `get`, `post` and `stream` never follow.
	pub(super) fn redirect_limit(opts: &Value) -> R<u64> {
		match crate::value::to_json(opts).map_err(ScriptError)?.get("maxRedirects") {
			None | Some(Json::Null) => Ok(0),
			Some(n) => n.as_u64().ok_or_else(|| {
				let why = "http: maxRedirects must be a non-negative int";
				ScriptError(mintworks_core::Error::validation(why))
			}),
		}
	}

	/// Repoints `req` at the hop `location` names, by the Fetch standard's rules, except that a
	/// cross-origin hop is stricter: [`CROSS_ORIGIN_HEADERS`] only, and no hop that still carries a
	/// body.
	pub(super) fn redirect(req: &mut Req, status: u16, location: &str) -> R<()> {
		let shown = mintworks_core::http::redact(location);
		let bad = |why: String| ScriptError(error::runtime(format!("{shown}: {why}")));
		let from = url::Url::parse(&req.url).map_err(|e| bad(e.to_string()))?;
		let to = from.join(location).map_err(|e| bad(e.to_string()))?;
		if !["http", "https"].contains(&to.scheme()) {
			return Err(bad("redirect to a scheme other than http(s)".into()));
		}
		if status == 303 || (matches!(status, 301 | 302) && req.method == "POST") {
			req.method = "GET".into();
			req.body.clear();
			req.headers.retain(|(k, _)| !k.starts_with("content-"));
		}
		// Dropped for good: a later hop back to the first origin does not restore them.
		if to.origin() != from.origin() {
			if !req.body.is_empty() {
				return Err(bad("the redirect would resend the body to another origin".into()));
			}
			req.headers.retain(|(k, _)| CROSS_ORIGIN_HEADERS.contains(&k.as_str()));
		}
		req.url = to.into();
		Ok(())
	}

	/// `http::request`: [`send`], once per hop, so every hop passes the gate on its own.
	pub(super) async fn follow(hosts: &[String], mut req: Req, mut left: u64) -> R<Value> {
		loop {
			let (status, headers, body) =
				send(hosts, req.clone(), |s, h, b| Ok((s.as_u16(), h, b.to_vec()))).await?;
			// The limit first: a 3xx past it is answered without its `Location` being read.
			if left > 0
				&& matches!(status, 301 | 302 | 303 | 307 | 308)
				&& let Some((_, location)) = headers.iter().find(|(k, _)| k == "location")
			{
				redirect(&mut req, status, location)?;
				left -= 1;
				continue;
			}
			return text(&json!({
				"status": status,
				"headers": header_map(headers),
				"body": String::from_utf8_lossy(&body),
				"url": req.url,
			}));
		}
	}

	pub fn module(hosts: Vec<String>) -> Result<Module, ContextError> {
		let mut m = Module::with_item(["http"])?;
		let h = hosts.clone();
		m.function("get", move |url: String| {
			let hosts = h.clone();
			async move {
				let req = Req { method: "GET".into(), url, headers: Vec::new(), body: Vec::new() };
				send(&hosts, req, |status, _, body| reply(status, body)).await
			}
		})
		.build()?;
		let p = hosts.clone();
		m.function("post", move |url: String, body: String| {
			let hosts = p.clone();
			async move {
				let headers = vec![("content-type".into(), "application/json".into())];
				let req = Req { method: "POST".into(), url, headers, body: body.into_bytes() };
				send(&hosts, req, |status, _, body| reply(status, body)).await
			}
		})
		.build()?;
		let h = hosts.clone();
		m.function("request", move |opts: Value| {
			let (hosts, req, limit) = (h.clone(), Req::parse(&opts), redirect_limit(&opts));
			async move { follow(&hosts, req?, limit?).await }
		})
		.build()?;
		m.function("stream", move |opts: Value| {
			let (hosts, req) = (hosts.clone(), Req::parse(&opts));
			async move {
				let req = req?;
				let listed = gate(&hosts, &req)?;
				// `HTTP_DEADLINE` bounds the status line, then each chunk as the idle timeout.
				let (status, headers, body) = mintworks_core::http::stream_external(
					&req.method,
					&req.url,
					&header_refs(&req.headers),
					req.body,
					HTTP_DEADLINE,
					HTTP_DEADLINE,
					listed,
				)
				.await
				.map_err(|e| refused(&req.url, e))?;
				Ok::<_, ScriptError>(crate::stream::HttpStream::new(
					status.as_u16(),
					header_map(headers),
					body,
				))
			}
		})
		.build()?;
		Ok(m)
	}

	/// Repeated names joined with `", "` (RFC 9110 §5.3) rather than the last one winning;
	/// `set-cookie` cannot be comma-joined, so its values are joined with `"\n"`, which no
	/// header value contains.
	pub(super) fn header_map(list: mintworks_core::http::HeaderList) -> HashMap<String, String> {
		let mut out: HashMap<String, String> = HashMap::new();
		for (k, v) in list {
			let sep = if k.eq_ignore_ascii_case("set-cookie") { "\n" } else { ", " };
			out.entry(k).and_modify(|cur| *cur = format!("{cur}{sep}{v}")).or_insert(v);
		}
		out
	}

	/// Set by the connection, not the script: a forged `host` or `content-length` desyncs it.
	const REFUSED_HEADERS: &[&str] = &[
		"host",
		"connection",
		"keep-alive",
		"proxy-connection",
		"transfer-encoding",
		"te",
		"trailer",
		"upgrade",
		"content-length",
	];

	/// All a redirect may carry to another origin. A keep-list, stricter than the Fetch standard:
	/// a denylist cannot name an `X-Api-Key`-style secret.
	const CROSS_ORIGIN_HEADERS: &[&str] =
		&["accept", "accept-language", "content-language", "content-type", "user-agent"];

	/// `#{method, url, headers, body}`, parsed before the call so no `rune::Value` is held
	/// across an await.
	#[derive(Clone)]
	pub(super) struct Req {
		method: String,
		url: String,
		headers: Vec<(String, String)>,
		body: Vec<u8>,
	}

	impl Req {
		pub(super) fn parse(opts: &Value) -> R<Self> {
			let bad = |msg: String| ScriptError(mintworks_core::Error::validation(msg));
			let json = crate::value::to_json(opts).map_err(ScriptError)?;
			let method = json.get("method").and_then(Json::as_str).unwrap_or("GET");
			let method = method.to_ascii_uppercase();
			if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&method.as_str()) {
				return Err(bad(format!("http: unsupported method {method}")));
			}
			let url = json
				.get("url")
				.and_then(Json::as_str)
				.ok_or_else(|| bad("http: url is required".into()))?
				.to_owned();
			let mut headers = Vec::new();
			if let Some(map) = json.get("headers").and_then(Json::as_object) {
				for (name, value) in map {
					let lower = name.to_ascii_lowercase();
					if REFUSED_HEADERS.contains(&lower.as_str()) {
						return Err(bad(format!("http: header {name} is set by the connection")));
					}
					let Some(value) = value.as_str() else {
						return Err(bad(format!("http: header {name} must be a string")));
					};
					headers.push((lower, value.to_owned()));
				}
			}
			let body = match json.get("body") {
				None | Some(Json::Null) => Vec::new(),
				Some(Json::String(s)) => s.clone().into_bytes(),
				Some(other) => other.to_string().into_bytes(),
			};
			Ok(Self { method, url, headers, body })
		}
	}

	fn header_refs(headers: &[(String, String)]) -> Vec<(&str, &str)> {
		headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
	}
}

mod env {
	use super::*;

	/// `env::get`. No `tx::with` guard: reading the process environment is not I/O and holds
	/// nothing. Only `APP_*` is visible: `mintworks` loads the app's `.env` into the process, so
	/// the framework's secrets (`MASTER_KEY`, `NAV_SIGN_KEY`, …) sit in the same environment.
	/// Under a suite a declared `app.test_env` value wins over the process: `mintworks test`
	/// still loads `.env`, whose blank `KEY=` would otherwise hide it. `main` never sees one.
	fn get(name: &str, test_env: &TestEnv) -> R<Value> {
		let test = test_env.get().filter(|_| !crate::testing::is_declaring());
		match lookup(name, test, |n| std::env::var(n).ok()) {
			Some(v) => text(&Json::String(v)),
			// `Value::from(())`, not `Value::empty()`: rune's `Inline::Empty` is not unit, so it
			// matches no `()` arm, is filtered by no `is_unit`, and cannot be serialized.
			None => Ok(Value::from(())),
		}
	}

	/// The lookup is a parameter because `set_var` is `unsafe` in edition 2024 and this crate
	/// forbids `unsafe`, so a test cannot plant a variable.
	pub(super) fn read(name: &str, var: impl FnOnce(&str) -> Option<String>) -> Option<String> {
		name.starts_with("APP_").then(|| var(name)).flatten()
	}

	pub(super) fn lookup(
		name: &str,
		test: Option<&HashMap<String, String>>,
		var: impl FnOnce(&str) -> Option<String>,
	) -> Option<String> {
		read(name, |n| test.and_then(|t| t.get(n).cloned()).or_else(|| var(n)))
	}

	pub fn module(test_env: TestEnv) -> Result<Module, ContextError> {
		let mut m = Module::with_item(["env"])?;
		m.function("get", move |name: String| get(&name, &test_env)).build()?;
		Ok(m)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tmp() -> PathBuf {
		let dir = std::env::temp_dir().join(format!("mintworks-script-io-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	#[test]
	fn the_sandboxed_profile_registers_nothing() {
		assert_eq!(IoProfile::sandboxed().modules(&TestEnv::default()).unwrap().len(), 0);
		let app = if cfg!(feature = "ai") { 8 } else { 7 };
		assert_eq!(IoProfile::app(tmp()).modules(&TestEnv::default()).unwrap().len(), app);
		assert!(!IoProfile::sandboxed().sys);
	}

	#[test]
	fn a_refused_url_drops_its_query_string() {
		let err =
			http::allowed(&["a.example".into()], "https://b.example/x?key=s3cr3t").unwrap_err();
		assert!(!format!("{:?}", err.0).contains("s3cr3t"));
	}

	#[test]
	fn fs_without_a_root_does_not_build() {
		let err = IoProfile { fs: true, ..IoProfile::default() }
			.modules(&TestEnv::default())
			.err()
			.unwrap();
		assert_eq!(err.parts().1, error::E_COMPILE);
	}

	#[test]
	fn a_path_escaping_the_root_is_refused() {
		let root = tmp();
		for path in ["../../etc/passwd", "/etc/passwd", "sub/../../out"] {
			assert!(fs::under_root(&root, path).is_err(), "{path} resolved");
		}
		assert!(fs::under_root(&root, "note.txt").is_ok());

		let link = root.join("link.txt");
		let _ = std::fs::remove_file(&link);
		std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
		assert!(fs::under_root(&root, "link.txt").is_err(), "a leaf symlink resolved");
		std::fs::remove_file(&link).unwrap();
	}

	#[test]
	fn a_path_through_a_symlinked_parent_directory_is_refused() {
		let root = tmp();
		let link = root.join("linkdir");
		let _ = std::fs::remove_file(&link);
		std::os::unix::fs::symlink("/etc", &link).unwrap();
		let got = fs::under_root(&root, "linkdir/passwd");
		std::fs::remove_file(&link).unwrap();
		assert!(got.is_err(), "a symlinked parent directory resolved");
	}

	#[test]
	fn env_get_sees_only_app_prefixed_names() {
		let planted = |_: &str| Some("value".to_owned());
		assert_eq!(env::read("MASTER_KEY", planted), None);
		assert_eq!(env::read("NAV_SIGN_KEY", planted), None);
		assert_eq!(env::read("APP_X", planted).as_deref(), Some("value"));
		// And against the real environment: `PATH` is set, and still unseen.
		assert!(std::env::var("PATH").is_ok());
		assert_eq!(env::read("PATH", |n| std::env::var(n).ok()), None);
	}

	#[tokio::test]
	async fn http_to_an_internal_address_is_refused_as_a_runtime_error() {
		for url in ["http://169.254.169.254/latest/meta-data/", "http://127.0.0.1/"] {
			let err = mintworks_core::http::get_external(url, &[], HTTP_DEADLINE, false)
				.await
				.unwrap_err();
			assert_eq!(http::refused(url, err).0.parts().1, error::E_RUNTIME);
		}
	}

	#[tokio::test]
	async fn a_read_over_the_cap_is_refused() {
		let root = tmp();
		std::fs::write(root.join("big.txt"), "x".repeat(64)).unwrap();
		let file = fs::under_root(&root, "big.txt").unwrap();
		assert!(fs::read_capped(&file, "big.txt", 16).await.is_err());
		assert_eq!(fs::read_capped(&file, "big.txt", 1024).await.unwrap().len(), 64);
	}

	#[test]
	fn an_unlisted_host_is_refused_when_the_allowlist_is_set() {
		let hosts = vec!["api.example.com".to_owned()];
		assert!(http::allowed(&hosts, "https://api.example.com/v1").unwrap());
		assert!(http::allowed(&hosts, "https://API.EXAMPLE.COM/v1").unwrap());
		assert!(http::allowed(&hosts, "https://evil.test/v1").is_err());
		assert!(http::allowed(&hosts, "not a url").is_err());
		assert!(!http::allowed(&[], "https://evil.test/v1").unwrap(), "unlisted is not internal");
	}

	#[test]
	fn set_cookie_joins_with_a_newline_and_other_headers_with_a_comma() {
		let h = |k: &str, v: &str| (k.to_owned(), v.to_owned());
		let m = http::header_map(vec![
			h("set-cookie", "a=1"),
			h("set-cookie", "b=2"),
			h("accept", "x"),
			h("accept", "y"),
		]);
		assert_eq!(m["set-cookie"], "a=1\nb=2");
		assert_eq!(m["accept"], "x, y");
	}

	fn one(name: &str, value: &str) -> HashMap<String, String> {
		HashMap::from([(name.to_owned(), value.to_owned())])
	}

	#[test]
	fn a_test_env_value_beats_a_blank_or_set_process_var() {
		let t = one("APP_X", "t");
		assert_eq!(env::lookup("APP_X", Some(&t), |_| Some(String::new())).as_deref(), Some("t"));
		assert_eq!(env::lookup("APP_X", Some(&t), |_| Some("real".into())).as_deref(), Some("t"));
	}

	#[test]
	fn without_a_test_env_the_process_var_is_read() {
		assert_eq!(env::lookup("APP_X", None, |_| Some("real".into())).as_deref(), Some("real"));
	}

	#[test]
	fn a_test_env_name_outside_app_stays_invisible() {
		assert_eq!(env::lookup("SECRET", Some(&one("SECRET", "x")), |_| None), None);
	}

	mod redirects {
		use wiremock::{
			Mock, MockServer, ResponseTemplate,
			matchers::{body_string, header, method, path, query_param},
		};

		use super::*;

		fn parse(opts: &Json) -> http::Req {
			http::Req::parse(&from_json(opts).unwrap()).unwrap()
		}

		async fn call(opts: &Json) -> R<Json> {
			let value = from_json(opts).unwrap();
			let (req, limit) = (http::Req::parse(&value)?, http::redirect_limit(&value)?);
			let reply = http::follow(&["127.0.0.1".into()], req, limit).await?;
			Ok(crate::value::to_json(&reply).unwrap())
		}

		fn to(location: &str) -> ResponseTemplate {
			ResponseTemplate::new(302).insert_header("location", location)
		}

		async fn serve(server: &MockServer, at: &str, reply: ResponseTemplate) {
			Mock::given(path(at)).respond_with(reply).mount(server).await;
		}

		fn done() -> ResponseTemplate {
			ResponseTemplate::new(200).set_body_string("done")
		}

		#[tokio::test]
		async fn a_location_joins_against_the_url_it_came_from() {
			let server = MockServer::builder().start().await;
			let authority = server.uri().replace("http:", "");
			let uri = server.uri();
			let cases = [
				("next".to_owned(), "/a/b/next", ""),
				("../up".to_owned(), "/a/up", ""),
				("/abs".to_owned(), "/abs", ""),
				(format!("{authority}/proto"), "/proto", ""),
				(format!("{uri}/full"), "/full", ""),
				("?q=2".to_owned(), "/a/b/start", "?q=2"),
			];
			for (location, want, query) in cases {
				server.reset().await;
				Mock::given(path("/a/b/start"))
					.and(query_param("x", "1"))
					.respond_with(to(&location))
					.mount(&server)
					.await;
				serve(&server, want, done()).await;
				let url = format!("{uri}/a/b/start?x=1");
				let got = call(&json!({"url": url, "maxRedirects": 1})).await.unwrap();
				assert_eq!(got["status"], 200, "{location}");
				assert_eq!(got["body"], "done", "{location}");
				assert_eq!(got["url"], format!("{uri}{want}{query}"), "{location}");
			}
		}

		#[tokio::test]
		async fn a_hop_to_an_unlisted_host_is_refused() {
			let server = MockServer::builder().start().await;
			serve(&server, "/start", to("http://169.254.169.254/")).await;
			let url = format!("{}/start", server.uri());
			let err = call(&json!({"url": url, "maxRedirects": 5})).await.unwrap_err();
			assert_eq!(err.0.parts().1, error::E_RUNTIME);
		}

		#[tokio::test]
		async fn a_hop_to_an_internal_address_is_refused() {
			for location in ["http://169.254.169.254/latest/", "http://2130706433/", "//[::1]/"] {
				let mut req = parse(&json!({"url": "http://example.com/a"}));
				http::redirect(&mut req, 302, location).unwrap();
				let err = http::send(&[], req, |_, _, _| Ok(())).await.unwrap_err();
				assert_eq!(err.0.parts().1, error::E_RUNTIME, "{location}");
			}
		}

		#[test]
		fn a_hop_off_http_is_refused_without_its_query_string() {
			for location in ["ftp://x/?k=s3cr3t", "file:///etc/passwd", "javascript:alert(1)"] {
				let mut req = parse(&json!({"url": "http://example.com/a"}));
				let err = http::redirect(&mut req, 302, location).unwrap_err();
				assert_eq!(err.0.parts().1, error::E_RUNTIME, "{location}");
				assert!(!format!("{:?}", err.0).contains("s3cr3t"));
			}
		}

		#[tokio::test]
		async fn the_last_redirect_past_the_limit_is_answered() {
			let server = MockServer::builder().start().await;
			serve(&server, "/1", to("/2")).await;
			serve(&server, "/2", to("/3")).await;
			Mock::given(path("/3")).respond_with(done()).expect(0).mount(&server).await;
			let url = format!("{}/1", server.uri());
			let got = call(&json!({"url": url, "maxRedirects": 1})).await.unwrap();
			assert_eq!(got["status"], 302);
			assert_eq!(got["headers"]["location"], "/3");
			assert_eq!(got["url"], format!("{}/2", server.uri()));
		}

		#[tokio::test]
		async fn by_default_a_redirect_is_not_followed_or_read() {
			let server = MockServer::builder().start().await;
			serve(&server, "/start", to("ftp://x/")).await;
			let url = format!("{}/start", server.uri());
			let got = call(&json!({"url": url})).await.unwrap();
			assert_eq!(got["status"], 302);
			assert_eq!(got["headers"]["location"], "ftp://x/");
			assert_eq!(got["url"], url);
		}

		#[test]
		fn max_redirects_must_be_a_non_negative_int() {
			let limit = |n: Json| {
				let opts = from_json(&json!({"url": "http://x/", "maxRedirects": n})).unwrap();
				http::redirect_limit(&opts).map_err(|_| ())
			};
			assert_eq!(limit(json!(3)), Ok(3));
			assert_eq!(limit(Json::Null), Ok(0));
			assert_eq!(limit(json!(-1)), Err(()));
			assert_eq!(limit(json!("3")), Err(()));
		}

		#[tokio::test]
		async fn only_the_keep_list_crosses_an_origin() {
			let (first, second) =
				(MockServer::builder().start().await, MockServer::builder().start().await);
			serve(&first, "/start", to("/same")).await;
			Mock::given(path("/same"))
				.and(header("authorization", "Bearer t"))
				.and(header("cookie", "a=1"))
				.respond_with(to(&format!("{}/other", second.uri())))
				.mount(&first)
				.await;
			serve(&second, "/other", to(&format!("{}/back", first.uri()))).await;
			serve(&first, "/back", done()).await;
			let got = call(&json!({
				"url": format!("{}/start", first.uri()),
				"maxRedirects": 3,
				"headers": {
					"Authorization": "Bearer t", "Cookie": "a=1", "X-Api-Key": "s", "Accept": "text/x",
				},
			}))
			.await
			.unwrap();
			assert_eq!(got["body"], "done");
			let seen = second.received_requests().await.unwrap();
			assert!(!seen[0].headers.contains_key("authorization"));
			assert!(!seen[0].headers.contains_key("cookie"));
			assert!(!seen[0].headers.contains_key("x-api-key"));
			assert_eq!(seen[0].headers["accept"], "text/x");
			let back = first.received_requests().await.unwrap();
			assert!(!back.last().unwrap().headers.contains_key("authorization"), "restored");
		}

		#[tokio::test]
		async fn a_303_turns_a_post_into_a_bodyless_get() {
			let server = MockServer::builder().start().await;
			let see_other = ResponseTemplate::new(303).insert_header("location", "/next");
			Mock::given(method("POST")).respond_with(see_other).mount(&server).await;
			Mock::given(method("GET"))
				.and(body_string(""))
				.respond_with(done())
				.mount(&server)
				.await;
			let got = call(&json!({
				"method": "POST",
				"url": format!("{}/start", server.uri()),
				"headers": {"Content-Type": "text/plain"},
				"body": "payload",
				"maxRedirects": 1,
			}))
			.await
			.unwrap();
			assert_eq!(got["body"], "done");
			let seen = server.received_requests().await.unwrap();
			assert!(!seen[1].headers.contains_key("content-type"));
		}

		#[tokio::test]
		async fn a_307_keeps_the_method_and_body() {
			let server = MockServer::builder().start().await;
			let temporary = ResponseTemplate::new(307).insert_header("location", "/next");
			serve(&server, "/start", temporary).await;
			Mock::given(method("POST"))
				.and(path("/next"))
				.and(body_string("payload"))
				.respond_with(done())
				.mount(&server)
				.await;
			let got = call(&json!({
				"method": "POST",
				"url": format!("{}/start", server.uri()),
				"body": "payload",
				"maxRedirects": 1,
			}))
			.await
			.unwrap();
			assert_eq!(got["body"], "done");
		}

		#[tokio::test]
		async fn a_cross_origin_307_with_a_body_is_refused() {
			let (first, second) =
				(MockServer::builder().start().await, MockServer::builder().start().await);
			let elsewhere = format!("{}/other", second.uri());
			let temporary = ResponseTemplate::new(307).insert_header("location", elsewhere);
			serve(&first, "/start", temporary).await;
			let got = call(&json!({
				"method": "POST",
				"url": format!("{}/start", first.uri()),
				"body": "payload",
				"maxRedirects": 1,
			}))
			.await;
			assert!(got.is_err());
			assert!(second.received_requests().await.unwrap().is_empty());
		}

		#[tokio::test]
		async fn a_cross_origin_301_on_a_put_with_a_body_is_refused() {
			let (first, second) =
				(MockServer::builder().start().await, MockServer::builder().start().await);
			let elsewhere = format!("{}/other", second.uri());
			let moved = ResponseTemplate::new(301).insert_header("location", elsewhere);
			serve(&first, "/start", moved).await;
			let got = call(&json!({
				"method": "PUT",
				"url": format!("{}/start", first.uri()),
				"body": "payload",
				"maxRedirects": 1,
			}))
			.await;
			assert!(got.is_err());
			assert!(second.received_requests().await.unwrap().is_empty());
		}

		#[tokio::test]
		async fn a_same_origin_301_on_a_put_keeps_its_body() {
			let server = MockServer::builder().start().await;
			let moved = ResponseTemplate::new(301).insert_header("location", "/next");
			serve(&server, "/start", moved).await;
			Mock::given(method("PUT"))
				.and(path("/next"))
				.and(body_string("payload"))
				.respond_with(done())
				.mount(&server)
				.await;
			let got = call(&json!({
				"method": "PUT",
				"url": format!("{}/start", server.uri()),
				"body": "payload",
				"maxRedirects": 1,
			}))
			.await
			.unwrap();
			assert_eq!(got["body"], "done");
		}
	}
}

// vim: ts=4
