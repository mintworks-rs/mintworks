//! `Provider::OpenAi` against a `wiremock` stand-in for an OpenAI-compatible endpoint, and the
//! `fake` queue.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_core::Retry;
use mintworks_llm::{
	CallSpec, Canned, ChatEvent, ChatRequest, ChatStream, FakeQueue, Llm, LlmState, Message,
	OpenAi, Provider, Tool, ToolCall, Usage, provider,
};
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{body_partial_json, header, method, path},
};

fn sse(events: &[&str]) -> String {
	let mut out: String = events.iter().flat_map(|e| ["data: ", e, "\n\n"]).collect();
	out.push_str("data: [DONE]\n\n");
	out
}

async fn server(status: u16, body: String) -> MockServer {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v1/chat/completions"))
		.respond_with(ResponseTemplate::new(status).set_body_raw(body, "text/event-stream"))
		.mount(&server)
		.await;
	server
}

fn openai(server: &MockServer) -> Provider {
	let mut p = OpenAi::new(server.uri(), Some("sk-test".into()));
	p.allow_internal = true;
	Provider::OpenAi(p)
}

fn ask(text: &str) -> ChatRequest {
	ChatRequest { messages: vec![Message::User { content: text.into() }], ..Default::default() }
}

async fn drain(mut s: ChatStream) -> Vec<ChatEvent> {
	let mut out = Vec::new();
	while let Some(ev) = s.next().await.unwrap() {
		out.push(ev);
	}
	out
}

#[tokio::test]
async fn streamed_text() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v1/chat/completions"))
		.and(header("authorization", "Bearer sk-test"))
		.and(body_partial_json(serde_json::json!({"model": "m1", "stream": true})))
		.respond_with(ResponseTemplate::new(200).set_body_raw(
			sse(&[
				r#"{"choices":[{"delta":{"role":"assistant","content":"Hel"}}]}"#,
				r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":null}]}"#,
				r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
				r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}}"#,
			]),
			"text/event-stream",
		))
		.mount(&server)
		.await;
	let events = drain(openai(&server).stream("m1", &ask("hi")).await.unwrap()).await;
	assert_eq!(
		events,
		vec![
			ChatEvent::Text("Hel".into()),
			ChatEvent::Text("lo".into()),
			ChatEvent::Finish("stop".into()),
			ChatEvent::Usage(Usage { input: 7, output: 2, cached: 0 }),
		]
	);
}

#[tokio::test]
async fn streamed_tool_call_is_assembled() {
	let server = server(
		200,
		sse(&[
			r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"lookup","arguments":""}}]}}]}"#,
			r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":"}}]}}]}"#,
			r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"now","arguments":"{}"}}]}}]}"#,
			r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}"#,
			r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
		]),
	)
	.await;
	let req = ChatRequest {
		tools: vec![Tool {
			name: "lookup".into(),
			description: "Look something up.".into(),
			parameters: serde_json::json!({"type": "object"}),
		}],
		..ask("find x")
	};
	let events = drain(openai(&server).stream("m1", &req).await.unwrap()).await;
	assert_eq!(
		events,
		vec![
			ChatEvent::ToolCall(ToolCall {
				id: "c1".into(),
				name: "lookup".into(),
				arguments: r#"{"q":"x"}"#.into()
			}),
			ChatEvent::ToolCall(ToolCall {
				id: "c2".into(),
				name: "now".into(),
				arguments: "{}".into()
			}),
			ChatEvent::Finish("tool_calls".into()),
		]
	);
	let sent: serde_json::Value =
		serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
	assert_eq!(sent["tools"][0]["type"], "function");
	assert_eq!(sent["tools"][0]["function"]["name"], "lookup");
}

#[tokio::test]
async fn unknown_fields_and_nulls_are_ignored() {
	let server = server(
		200,
		// No trailing `[DONE]` either: body end closes the stream.
		concat!(
			": keepalive\n\n",
			"data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,",
			"\"delta\":{\"content\":null,\"reasoning_content\":\"thinking\",\"tool_calls\":null},",
			"\"logprobs\":null,\"finish_reason\":null}],\"usage\":null,\"system_fingerprint\":\"f\"}\n\n",
			"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
		)
		.into(),
	)
	.await;
	let events = drain(openai(&server).stream("m1", &ask("hi")).await.unwrap()).await;
	assert_eq!(events, vec![ChatEvent::Text("ok".into()), ChatEvent::Finish("stop".into())]);
}

#[tokio::test]
async fn a_cut_stream_is_an_upstream_error() {
	let server = server(
		200,
		"data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n".into(),
	)
	.await;
	let mut s = openai(&server).stream("m1", &ask("hi")).await.unwrap();
	let e = loop {
		match s.next().await {
			Ok(Some(_)) => {}
			Ok(None) => panic!("a cut stream ended cleanly"),
			Err(e) => break e,
		}
	};
	assert_eq!(e.parts().1, provider::E_UPSTREAM);
}

async fn refused(status: u16) -> mintworks_core::Error {
	let server = server(status, r#"{"error":{"message":"nope"}}"#.into()).await;
	match openai(&server).stream("m1", &ask("hi")).await {
		Ok(_) => panic!("{status} accepted"),
		Err(e) => e,
	}
}

#[tokio::test]
async fn rate_limit_and_server_errors_are_retryable() {
	for status in [429, 500, 502, 503] {
		let e = refused(status).await;
		assert_eq!(e.parts().1, provider::E_UPSTREAM, "{status}");
		assert_eq!(e.retry(), Retry::Backoff, "{status}");
	}
}

#[tokio::test]
async fn other_client_errors_are_not() {
	for status in [400, 401, 404] {
		let e = refused(status).await;
		assert_eq!(e.parts().1, provider::E_REJECTED, "{status}");
		assert_eq!(e.retry(), Retry::Never, "{status}");
	}
}

#[tokio::test]
async fn mid_stream_error_is_retryable() {
	let server = server(200, sse(&[r#"{"error":{"message":"overloaded"}}"#])).await;
	let mut s = openai(&server).stream("m1", &ask("hi")).await.unwrap();
	let e = s.next().await.unwrap_err();
	assert_eq!(e.retry(), Retry::Backoff);
}

#[tokio::test]
async fn unreachable_provider_is_retryable() {
	// A closed loopback port.
	let mut p = OpenAi::new("http://127.0.0.1:9", None);
	p.allow_internal = true;
	let Err(e) = Provider::OpenAi(p).stream("m1", &ask("hi")).await else { panic!("connected") };
	assert_eq!(e.retry(), Retry::Backoff);
}

#[tokio::test]
async fn fake_queue_is_fifo_and_shared() {
	let queue = FakeQueue::default();
	let fake = Provider::Fake(queue.clone());
	queue.push(Canned::text("first"));
	queue.push(Canned::tool_calls(vec![ToolCall {
		id: "c1".into(),
		name: "f".into(),
		arguments: "{}".into(),
	}]));
	let events = drain(fake.stream("any", &ask("12345678")).await.unwrap()).await;
	assert_eq!(
		events,
		vec![
			ChatEvent::Text("first".into()),
			ChatEvent::Finish("stop".into()),
			ChatEvent::Usage(Usage { input: 2, output: 2, cached: 0 }),
		]
	);
	let events = drain(fake.stream("any", &ask("x")).await.unwrap()).await;
	assert!(matches!(&events[0], ChatEvent::ToolCall(c) if c.name == "f"));
	assert_eq!(events[1], ChatEvent::Finish("tool_calls".into()));
	assert!(queue.is_empty());
	let Err(e) = fake.stream("any", &ask("x")).await else { panic!("empty queue answered") };
	assert_eq!(e.parts().1, provider::E_FAKE_EMPTY);
}

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("mintworks-llm-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn app(db: &TmpDb) -> mintworks_core::App {
	let config = mintworks_core::config::Config {
		master_key: [0; 32],
		db_path: db.0.join("test.db").to_string_lossy().into_owned(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	let store = mintworks_store_sqlite::SqliteStore::open(&config).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let ledger: std::sync::Arc<dyn mintworks_llm::LlmStore> = std::sync::Arc::new(store.clone());
	mintworks_core::AppBuilder::new()
		.config(config)
		.store(std::sync::Arc::new(store) as std::sync::Arc<dyn mintworks_core::store::CoreStore>)
		.settings(mintworks_llm::SETTINGS)
		.extension(LlmState::default())
		.extension(ledger)
		.build()
		.await
		.unwrap()
}

async fn set(app: &mintworks_core::App, pairs: &[(&str, &str)]) {
	for (k, v) in pairs {
		app.settings.set(k, v, None).await.unwrap();
	}
}

#[tokio::test]
async fn falls_back_on_5xx_and_honours_prefer() {
	let db = TmpDb::new("fallback");
	let app = app(&db).await;
	let down = server(503, String::new()).await;
	let up =
		server(200, sse(&[r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#]))
			.await;
	let (down_uri, up_uri) = (down.uri(), up.uri());
	set(
		&app,
		&[
			("llm.base_url.down", &down_uri),
			("llm.allow_internal.down", "1"),
			("llm.base_url.up", &up_uri),
			("llm.allow_internal.up", "1"),
			("llm.profile.writer", "down:m1, up:m2"),
		],
	)
	.await;
	let llm = Llm::new(app.clone());
	let ctx = mintworks_core::Ctx::system("test");
	let spec = CallSpec { role: "writer".into(), request: ask("hello"), ..Default::default() };

	let done = llm.complete(&ctx, &spec).await.unwrap();
	assert_eq!((done.target.provider.as_str(), done.text.as_str()), ("up", "hi"));
	assert_eq!(down.received_requests().await.unwrap().len(), 1);

	let spec = CallSpec { prefer: Some("up:m2".into()), ..spec };
	llm.complete(&ctx, &spec).await.unwrap();
	assert_eq!(down.received_requests().await.unwrap().len(), 1, "prefer skipped the dead entry");
}

#[tokio::test]
async fn fake_kind_uses_the_shared_queue() {
	let db = TmpDb::new("fake");
	let app = app(&db).await;
	set(&app, &[("llm.kind.f", "fake"), ("llm.profile.extract", "f:any")]).await;
	app.extensions.get::<LlmState>().unwrap().fake_queue().push(Canned::text("ok"));
	let spec = CallSpec { role: "extract".into(), request: ask("x"), ..Default::default() };
	let done = Llm::new(app.clone())
		.complete(&mintworks_core::Ctx::system("test"), &spec)
		.await
		.unwrap();
	assert_eq!(done.text, "ok");
}

#[tokio::test]
async fn ledger_records_each_attempt_and_budget_refuses() {
	let db = TmpDb::new("ledger");
	let app = app(&db).await;
	set(
		&app,
		&[
			("llm.kind.f", "fake"),
			("llm.profile.extract", "f:m"),
			("llm.price.f:m", "1000000,2000000"),
		],
	)
	.await;
	let store = mintworks_llm::ledger::store(&app).unwrap();
	let q = app.extensions.get::<LlmState>().unwrap().fake_queue().clone();
	let ctx = mintworks_core::Ctx::system("test");
	let spec = CallSpec {
		role: "extract".into(),
		request: ask("12345678"),
		subject: Some("project:p1".into()),
		step: "s".into(),
		..Default::default()
	};
	mintworks_llm::ledger::set_budget(&app, "project:p1", 1).await.unwrap();

	q.push(Canned::text("abcd"));
	Llm::new(app.clone()).complete(&ctx, &spec).await.unwrap();
	// Fake usage is chars/4 rounded up; 1 EUR in and 2 EUR out per 1M tokens.
	let spent = store.usage_cost_for_subject("project:p1").await.unwrap();
	assert!(spent >= 1, "the completed call was recorded: {spent}");

	q.push(Canned::text("never"));
	let err = Llm::new(app.clone()).complete(&ctx, &spec).await.unwrap_err();
	assert_eq!(err.parts().1, mintworks_llm::ledger::E_BUDGET);
	assert_eq!(q.len(), 1, "the refused call reached no provider");
}

#[tokio::test]
async fn dropped_stream_is_billed_by_estimate() {
	let db = TmpDb::new("dropped");
	let app = app(&db).await;
	let up = server(
		200,
		sse(&[
			r#"{"choices":[{"delta":{"content":"hello there"}}]}"#,
			r#"{"choices":[{"delta":{"content":" and more"}}]}"#,
		]),
	)
	.await;
	let uri = up.uri();
	set(
		&app,
		&[
			("llm.base_url.up", &uri),
			("llm.allow_internal.up", "1"),
			("llm.profile.writer", "up:m"),
			// Output only, so a nonzero cost means a nonzero `tokens_out`.
			("llm.price.up:m", "0,1000000"),
		],
	)
	.await;
	let spec = CallSpec {
		role: "writer".into(),
		request: ask("hello"),
		subject: Some("s".into()),
		..Default::default()
	};
	let mut stream = Llm::new(app.clone())
		.stream(&mintworks_core::Ctx::system("test"), &spec)
		.await
		.unwrap();
	assert!(matches!(stream.next().await.unwrap(), Some(ChatEvent::Text(_))));
	drop(stream);
	let store = mintworks_llm::ledger::store(&app).unwrap();
	let mut spent = 0;
	for _ in 0..100 {
		spent = store.usage_cost_for_subject("s").await.unwrap();
		if spent > 0 {
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}
	assert!(spent > 0, "a dropped stream was recorded free");
}

#[tokio::test]
async fn stream_without_usage_reports_estimated_used() {
	let db = TmpDb::new("nousage");
	let app = app(&db).await;
	let up = server(200, sse(&[r#"{"choices":[{"delta":{"content":"hello there"}}]}"#])).await;
	let uri = up.uri();
	set(
		&app,
		&[
			("llm.base_url.up", &uri),
			("llm.allow_internal.up", "1"),
			("llm.profile.writer", "up:m"),
		],
	)
	.await;
	let spec = CallSpec { role: "writer".into(), request: ask("hello"), ..Default::default() };
	let mut stream = Llm::new(app.clone())
		.stream(&mintworks_core::Ctx::system("test"), &spec)
		.await
		.unwrap();
	while stream.next().await.unwrap().is_some() {}
	let used = stream.used();
	assert!(used.input > 0 && used.output > 0, "{used:?}");
}

#[tokio::test]
async fn unconfigured_role_is_config_error() {
	let db = TmpDb::new("noprofile");
	let app = app(&db).await;
	let spec = CallSpec { role: "writer".into(), request: ask("x"), ..Default::default() };
	let err = Llm::new(app)
		.complete(&mintworks_core::Ctx::system("test"), &spec)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, mintworks_llm::service::E_CONFIG);
}

// vim: ts=4
