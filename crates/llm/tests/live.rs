//! A smoke test against a real provider, `#[ignore]`d. Run deliberately:
//! `LLM_LIVE_BASE_URL=… LLM_LIVE_API_KEY=… LLM_LIVE_MODEL=… cargo test -p mintworks-llm --test live -- --ignored`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_llm::{ChatEvent, ChatRequest, Message, OpenAi, Provider};

#[tokio::test]
#[ignore = "calls a real provider; set LLM_LIVE_BASE_URL/API_KEY/MODEL"]
async fn live_provider_streams_a_reply() {
	let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
	let provider = Provider::OpenAi(OpenAi::new(
		var("LLM_LIVE_BASE_URL"),
		std::env::var("LLM_LIVE_API_KEY").ok(),
	));
	let req = ChatRequest {
		messages: vec![Message::User { content: "Reply with the single word: pong".into() }],
		max_tokens: Some(16),
		..Default::default()
	};
	let mut stream = provider.stream(&var("LLM_LIVE_MODEL"), &req).await.unwrap();
	let (mut text, mut finished) = (String::new(), false);
	while let Some(ev) = stream.next().await.unwrap() {
		match ev {
			ChatEvent::Text(t) => text.push_str(&t),
			ChatEvent::Finish(_) => finished = true,
			ChatEvent::Usage(u) => println!("usage: {u:?}"),
			ChatEvent::ToolCall(c) => panic!("unexpected tool call {c:?}"),
		}
	}
	assert!(finished, "no finish_reason");
	assert!(!text.trim().is_empty(), "empty reply");
	println!("reply: {text}");
}

// vim: ts=4
