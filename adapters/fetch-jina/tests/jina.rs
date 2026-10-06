//! Jina Reader against a `wiremock` stand-in.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_fetch_jina::{Jina, JinaSearch};
use mintworks_search::{Endpoint, Fetcher, Page, SearchProvider};
use serde_json::json;
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{header, method, path, query_param},
};

#[tokio::test]
async fn jina_reads_the_json_reply() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/https://example.com/page"))
		.and(header("accept", "application/json"))
		.and(header("authorization", "Bearer jina-test"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"code": 200,
			"status": 20000,
			"data": {
				"title": "Example",
				"description": "",
				"url": "https://example.com/page",
				"content": "Body text.",
				"usage": {"tokens": 41},
			},
		})))
		.expect(1)
		.mount(&server)
		.await;
	let ep = Endpoint {
		base_url: Some(server.uri()),
		api_key: Some("jina-test".into()),
		allow_internal: true,
	};
	let page = Jina.fetch(&ep, "https://example.com/page").await.unwrap();
	assert_eq!(
		page,
		Page {
			url: "https://example.com/page".into(),
			title: "Example".into(),
			text: "Body text.".into(),
			tokens: Some(41),
		}
	);
}

#[tokio::test]
async fn jina_search_reports_the_billed_total() {
	let server = MockServer::builder().start().await;
	let hit = json!({"title": "A", "url": "https://a.example/", "description": "about a",
		"usage": {"tokens": 1000}});
	Mock::given(method("GET"))
		.and(query_param("q", "with meta"))
		.and(header("x-respond-with", "no-content"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"code": 200, "data": [hit], "meta": {"usage": {"tokens": 10000}},
		})))
		.mount(&server)
		.await;
	Mock::given(method("GET"))
		.and(query_param("q", "no meta"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 200, "data": [hit]})))
		.mount(&server)
		.await;
	let ep = Endpoint { base_url: Some(server.uri()), allow_internal: true, ..Endpoint::default() };
	let reply = JinaSearch.search(&ep, "with meta", 5).await.unwrap();
	assert_eq!(reply.hits[0].snippet, "about a");
	assert_eq!(reply.tokens, Some(10_000), "meta.usage, not the per-hit usage");
	assert_eq!(JinaSearch.search(&ep, "no meta", 5).await.unwrap().tokens, None);
}

// vim: ts=4
