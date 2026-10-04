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

/// `test::llm_requests()` → `[#{messages, tools}]`: what the `fake` provider was sent since the
/// last call, oldest first. `messages` are in the OpenAI wire shape (`role`, `content`,
/// `tool_calls[].function.{name,arguments}`, `tool_call_id`); `tools` holds names.
#[cfg(feature = "ai")]
#[rune::function]
fn llm_requests() -> Result<Value, ScriptError> {
	use saas_llm::LlmState;
	let h = harness()?;
	let state = h.app.extensions.get::<LlmState>().ok_or_else(|| {
		ScriptError(error::runtime("test::llm_requests needs app.feature(\"llm\")"))
	})?;
	let out = state
		.fake_queue()
		.take_requests()
		.iter()
		.map(|r| {
			Ok(json!({
				"messages": serde_json::to_value(&r.messages)
					.map_err(|e| ScriptError(error::runtime(e.to_string())))?,
				"tools": r.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
			}))
		})
		.collect::<Result<Vec<Json>, ScriptError>>()?;
	from_json(&Json::Array(out)).map_err(ScriptError)
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

/// The scripted `fake` gateway `saas-run test` registers (`recurring: true`): `start`,
/// `fetch_state` and `charge_recurring` each pop the next state `test::payments` queued, and
/// answer `PENDING` once the queue is empty. Refunds always succeed in full.
#[derive(Default)]
pub struct FakePayments {
	queue: std::sync::Mutex<std::collections::VecDeque<saas_billing::provider::PaymentState>>,
	seq: std::sync::atomic::AtomicU64,
}

impl FakePayments {
	fn pop(&self) -> saas_billing::provider::PaymentState {
		self.queue
			.lock()
			.ok()
			.and_then(|mut q| q.pop_front())
			.unwrap_or(saas_billing::provider::PaymentState::Pending)
	}

	fn started(&self, redirect: Option<String>) -> saas_billing::provider::StartedPayment {
		let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		saas_billing::provider::StartedPayment {
			provider_ref: format!("fake-{n}"),
			redirect_url: redirect,
			state: self.pop(),
		}
	}
}

#[async_trait::async_trait]
impl saas_billing::provider::PaymentProvider for FakePayments {
	fn id(&self) -> &'static str {
		"fake"
	}

	fn capabilities(&self) -> saas_billing::provider::ProviderCaps {
		saas_billing::provider::ProviderCaps {
			reservation: false,
			recurring: true,
			partial_refund: true,
		}
	}

	async fn start(
		&self,
		req: &saas_billing::provider::StartPayment,
	) -> ClResult<saas_billing::provider::StartedPayment> {
		Ok(self.started(Some(req.redirect_url.clone())))
	}

	async fn fetch_state(
		&self,
		_provider_ref: &str,
	) -> ClResult<saas_billing::provider::PaymentState> {
		Ok(self.pop())
	}

	async fn refund(
		&self,
		_provider_ref: &str,
		amount: saas_core::money::Money,
		_request_id: &str,
	) -> ClResult<saas_billing::provider::RefundResult> {
		Ok(saas_billing::provider::RefundResult {
			refunded: amount,
			state: saas_billing::provider::PaymentState::Succeeded,
		})
	}

	async fn charge_recurring(
		&self,
		_token: &str,
		_req: &saas_billing::provider::StartPayment,
	) -> ClResult<saas_billing::provider::StartedPayment> {
		Ok(self.started(None))
	}

	fn parse_callback(
		&self,
		_headers: &axum::http::HeaderMap,
		body: &[u8],
	) -> ClResult<saas_billing::provider::CallbackRef> {
		Ok(saas_billing::provider::CallbackRef {
			provider_ref: String::from_utf8_lossy(body).into_owned(),
		})
	}
}

/// `test::payments([#{state: "Succeeded"}, …])` — queues states for the `fake` gateway, one
/// per `start`/`fetch_state`/`charge_recurring` call. A state is `Succeeded` or `SUCCEEDED`.
#[rune::function]
fn payments(items: Value) -> Result<(), ScriptError> {
	let h = harness()?;
	let fake = h.app.extensions.get::<Arc<FakePayments>>().ok_or_else(|| {
		ScriptError(error::runtime("test::payments needs app.feature(\"billing\")"))
	})?;
	let Json::Array(items) = to_json(&items)? else {
		return Err(ScriptError(error::runtime("test::payments takes an array")));
	};
	let mut q = fake
		.queue
		.lock()
		.map_err(|_| ScriptError(error::runtime("test::payments: poisoned")))?;
	for item in items {
		let name = item["state"].as_str().unwrap_or_default();
		// `Succeeded` → `SUCCEEDED`, `AwaitingUser` → `AWAITING_USER`.
		let mut screaming = String::new();
		for (i, ch) in name.chars().enumerate() {
			if i > 0
				&& ch.is_ascii_uppercase()
				&& !name.chars().nth(i - 1).is_some_and(|p| p.is_ascii_uppercase() || p == '_')
			{
				screaming.push('_');
			}
			screaming.push(ch.to_ascii_uppercase());
		}
		let state = screaming.parse().map_err(|_| {
			ScriptError(error::runtime(format!("test::payments: unknown state '{name}'")))
		})?;
		q.push_back(state);
	}
	Ok(())
}

/// `test::signup(#{email, ref})` — registers and activates `email` carrying the ref code, as a
/// browser would, and answers `#{token, accountUid, orgUid}` for the new account's own org.
/// Consents name `v1`, the version the harness publishes when the app ships no `legal/`.
#[rune::function]
async fn signup(arg: Value) -> Result<Value, ScriptError> {
	let h = harness()?;
	let arg = to_json(&arg)?;
	let email = arg["email"].as_str().unwrap_or_default().to_owned();
	let fail = |e: saas_core::error::Error| ScriptError(e);
	let consents = serde_json::from_value(json!([
		{ "kind": "TOS", "version": "v1" },
		{ "kind": "PRIVACY", "version": "v1" },
	]))
	.map_err(|e| ScriptError(error::runtime(format!("test::signup: {e}"))))?;
	let auth = saas_auth::Auth::new(h.app.clone());
	// No `ip`: a caller that did not come off a socket owes no proof of work.
	let ctx = saas_core::ctx::Ctx::public("test.signup");
	auth.register(
		&ctx,
		&saas_auth::Registration {
			email: email.clone(),
			consents,
			ref_code: arg["ref"].as_str().map(str::to_owned),
			..saas_auth::Registration::default()
		},
	)
	.await
	.map_err(fail)?;
	let account = saas_auth::routes::store(&h.app)
		.map_err(fail)?
		.account_by_email(&email)
		.await
		.map_err(fail)?
		.ok_or_else(|| {
			ScriptError(error::runtime(format!("test::signup: {email} was not created")))
		})?;
	let token = saas_auth::activation_token(&h.app, &account).await.map_err(fail)?;
	let tokens = auth
		.activate(&ctx, &token, Some("correct horse battery".to_owned()))
		.await
		.map_err(fail)?;
	let body = serde_json::to_value(&tokens.body)
		.map_err(|e| ScriptError(error::runtime(format!("test::signup: {e}"))))?;
	from_json(&json!({
		"token": tokens.access_token,
		"accountUid": body["account"]["uid"],
		"orgUid": body["org"]["uid"],
	}))
	.map_err(ScriptError)
}

/// Installed only when compiling for tests, so a served bundle cannot reach it.
///
/// # Errors
/// Whatever Rune raises registering the functions.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["test"])?;
	m.function_meta(request)?;
	m.function_meta(session)?;
	m.function_meta(payments)?;
	m.function_meta(signup)?;
	#[cfg(feature = "ai")]
	m.function_meta(llm_script)?;
	#[cfg(feature = "ai")]
	m.function_meta(llm_requests)?;
	#[cfg(feature = "ai")]
	m.function_meta(search_fixture)?;
	Ok(m)
}

// vim: ts=4
