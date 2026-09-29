//! `#[test]` discovery and the in-process client behind `saas-run test`.
//!
//! This lives inside the crate rather than in the binary because `ScriptApp::context` and
//! `Script`'s fields are private: nothing under `bin/` can build a unit or reach the module set
//! a bundle compiles against.

use std::sync::Arc;

use axum::http::Request;
use http_body_util::BodyExt as _;
use rune::{
	ContextError, Hash, Module, Value,
	compile::{CompileVisitor, MetaError, MetaRef, meta},
};
use saas_core::{AppBuilder, error::ClResult};
use serde_json::{Value as Json, json};

use crate::{
	Script, ScriptApp, error,
	value::{ScriptError, from_json, to_json},
};

/// One discovered `#[test]` function.
pub struct TestFn {
	/// The entry point [`Script::invoke`] takes.
	pub hash: Hash,
	/// The Rune item path, which is also what the filter matches and the runner prints.
	pub name: String,
}

/// Collects `#[test]` functions during compilation.
///
/// rune 0.14's `Options::test` is a no-op, so the attribute survives only as
/// `meta::Kind::Function { is_test: true }` passing through a visitor. Modelled on rune's own
/// `FunctionVisitor` (`src/cli/visitor.rs`), which is private to its CLI.
#[derive(Default)]
pub struct TestVisitor {
	found: Vec<TestFn>,
}

impl TestVisitor {
	#[must_use]
	pub fn into_tests(self) -> Vec<TestFn> {
		self.found
	}
}

impl CompileVisitor for TestVisitor {
	fn register_meta(&mut self, meta: MetaRef<'_>) -> Result<(), MetaError> {
		if let meta::Kind::Function { is_test: true, .. } = &meta.kind {
			self.found.push(TestFn { hash: meta.hash, name: meta.item.to_string() });
		}
		Ok(())
	}
}

/// What one test case can reach: the router the application actually serves, and whatever the
/// harness seeded into its own database before the case ran.
pub struct Harness {
	pub router: axum::Router,
	/// The built app, for helpers that seed an extension rather than a request (`llm_script`).
	pub app: saas_core::App,
	/// Handed to script as `test::session()`. The harness owns its shape; the runner in
	/// `bin/saas-run/src/test.rs` fills in `token`, `accountUid` and `orgUid`.
	pub session: Json,
}

tokio::task_local! {
	/// A Rune host function is a free `fn` that captures nothing, so the harness travels the
	/// same way an open transaction does — by task. One invocation is one VM on one task.
	static HARNESS: Arc<Harness>;
}

/// Runs `f` with `harness` reachable from `test::` host functions.
pub async fn with<F: Future>(harness: Arc<Harness>, f: F) -> F::Output {
	HARNESS.scope(harness, f).await
}

impl ScriptApp {
	/// [`install`](ScriptApp::install), plus the compiled bundle and its `#[test]` functions.
	///
	/// The router is *not* returned here: `AppBuilder::into_service` composes the same stack
	/// production serves, so a test meets the real rate limiter and the real error envelope.
	///
	/// # Errors
	/// As [`install`](ScriptApp::install).
	pub async fn install_tests<F>(
		self,
		builder: AppBuilder,
		features: F,
	) -> ClResult<(AppBuilder, Arc<Script>, Vec<TestFn>)>
	where
		F: FnOnce(AppBuilder, &std::collections::BTreeSet<String>) -> ClResult<AppBuilder>,
	{
		let mut visitor = TestVisitor::default();
		let (builder, script) = self.install_visited(builder, features, Some(&mut visitor)).await?;
		Ok((builder, script, visitor.into_tests()))
	}
}

/// Runs one case with `harness` in scope and maps its outcome.
///
/// The convention is rune's own (`src/cli/tests.rs`): a case that returns `None` failed, and
/// `Err((code, msg))` has already become a framework error inside [`Script::invoke`]. Anything
/// else — including the usual unit — passed.
///
/// # Errors
/// Whatever the case raised, or `E-SCRIPT-RUNTIME` for a `None` return.
pub async fn run_test(script: &Script, test: &TestFn, harness: Arc<Harness>) -> ClResult<()> {
	let value: Value = with(harness, script.invoke(test.hash, ())).await?;
	match rune::from_value::<Option<Value>>(value) {
		Ok(None) => Err(error::runtime("the test returned None")),
		_ => Ok(()),
	}
}

fn harness() -> Result<Arc<Harness>, ScriptError> {
	HARNESS
		.try_with(Arc::clone)
		.map_err(|_| ScriptError(error::runtime("test:: is only available under `saas-run test`")))
}

/// `test::request(method, path, token, body)` — one request against the app's own router.
///
/// Returns `#{status, body}`; `body` is `()` when the response carries none, which is what a
/// 204 answers with. A body that is not JSON also comes back as `text`. A non-2xx is *not* an error: asserting on the envelope is the point.
#[rune::function]
async fn request(
	method: String,
	path: String,
	token: Option<String>,
	body: Value,
) -> Result<Value, ScriptError> {
	let h = harness()?;
	let body = match to_json(&body)? {
		Json::Null => Vec::new(),
		json => json.to_string().into_bytes(),
	};

	let mut req = Request::builder().method(method.as_str()).uri(&path);
	if let Some(token) = token {
		req = req.header("authorization", format!("Bearer {token}"));
	}
	let req = req
		.header("content-type", "application/json")
		.body(axum::body::Body::from(body))
		.map_err(|e| ScriptError(error::runtime(format!("{method} {path}: {e}"))))?;

	let res = tower::ServiceExt::oneshot(h.router.clone(), req)
		.await
		.map_err(|e| ScriptError(error::runtime(format!("{method} {path}: {e}"))))?;
	let status = res.status();
	let bytes = res
		.into_body()
		.collect()
		.await
		.map_err(|e| ScriptError(error::runtime(format!("{method} {path}: {e}"))))?
		.to_bytes();
	// A non-JSON body (an SSE stream) is also handed back raw, as `text`.
	let out = match serde_json::from_slice::<Json>(&bytes) {
		Ok(body) => json!({ "status": i64::from(status.as_u16()), "body": body }),
		Err(_) => json!({
			"status": i64::from(status.as_u16()),
			"body": Json::Null,
			"text": String::from_utf8_lossy(&bytes),
		}),
	};
	from_json(&out).map_err(ScriptError)
}

/// `test::session()` — the account, org and bearer token the harness seeded for this case.
#[rune::function]
fn session() -> Result<Value, ScriptError> {
	from_json(&harness()?.session).map_err(ScriptError)
}

/// `test::llm_script(["text", #{toolCalls: [#{id, name, arguments}]}, …])` — queues canned
/// completions for every `fake`-kind provider, consumed one per provider call in order.
#[cfg(feature = "ai")]
#[rune::function]
fn llm_script(items: Value) -> Result<(), ScriptError> {
	use saas_llm::{Canned, LlmState, ToolCall};
	let h = harness()?;
	let state = h.app.extensions.get::<LlmState>().ok_or_else(|| {
		ScriptError(error::runtime("test::llm_script needs app.feature(\"llm\")"))
	})?;
	let Json::Array(items) = to_json(&items)? else {
		return Err(ScriptError(error::runtime("test::llm_script takes an array")));
	};
	for item in items {
		let canned = match item {
			Json::String(text) => Canned::text(text),
			Json::Object(mut o) => {
				let calls = o.remove("toolCalls").unwrap_or(Json::Null);
				let calls = calls.as_array().cloned().unwrap_or_default();
				let calls = calls
					.iter()
					.map(|c| ToolCall {
						id: c["id"].as_str().unwrap_or_default().to_owned(),
						name: c["name"].as_str().unwrap_or_default().to_owned(),
						arguments: c["arguments"].as_str().unwrap_or_default().to_owned(),
					})
					.collect();
				let mut canned = Canned::tool_calls(calls);
				o.get("text")
					.and_then(Json::as_str)
					.unwrap_or_default()
					.clone_into(&mut canned.text);
				canned
			}
			other => {
				return Err(ScriptError(error::runtime(format!(
					"test::llm_script: an item is a string or #{{text, toolCalls}}, not {other}"
				))));
			}
		};
		state.fake_queue().push(canned);
	}
	Ok(())
}

/// `test::search_fixture(#{searches: [[#{title, url, snippet}, …], …], pages: [#{url, title, text}]})`
/// — feeds the `fake` search provider (one hit list per search, in order) and fetcher (by url).
#[cfg(feature = "ai")]
#[rune::function]
fn search_fixture(fixture: Value) -> Result<(), ScriptError> {
	use saas_search::{Fixtures, Page, SearchHit};
	#[derive(serde::Deserialize)]
	#[serde(deny_unknown_fields)]
	struct PageArg {
		url: String,
		#[serde(default)]
		title: String,
		text: String,
	}
	#[derive(serde::Deserialize)]
	#[serde(default, deny_unknown_fields)]
	#[derive(Default)]
	struct Arg {
		searches: Vec<Vec<SearchHit>>,
		pages: Vec<PageArg>,
	}
	let h = harness()?;
	let fixtures = h.app.extensions.get::<Fixtures>().ok_or_else(|| {
		ScriptError(error::runtime("test::search_fixture needs app.feature(\"search\")"))
	})?;
	let arg: Arg = serde_json::from_value(to_json(&fixture)?)
		.map_err(|e| ScriptError(error::runtime(format!("test::search_fixture: {e}"))))?;
	arg.searches.into_iter().for_each(|hits| fixtures.push_search(hits));
	for p in arg.pages {
		fixtures.put_page(Page { url: p.url, title: p.title, text: p.text, tokens: None });
	}
	Ok(())
}

/// Installed only when compiling for tests, so a served bundle cannot reach it.
///
/// # Errors
/// Whatever Rune raises registering the functions.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["test"])?;
	m.function_meta(request)?;
	m.function_meta(session)?;
	#[cfg(feature = "ai")]
	m.function_meta(llm_script)?;
	#[cfg(feature = "ai")]
	m.function_meta(search_fixture)?;
	Ok(m)
}

// vim: ts=4
