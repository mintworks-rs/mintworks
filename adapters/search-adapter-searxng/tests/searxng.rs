//! SearXNG against a `wiremock` stand-in.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_search::{E_REJECTED, Endpoint, SearchHit, SearchProvider};
use search_adapter_searxng::Searxng;
use serde_json::json;
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{method, path, query_param},
};

fn ep(server: &MockServer) -> Endpoint {
	Endpoint { base_url: Some(server.uri()), api_key: None, allow_internal: true }
}

#[tokio::test]
async fn searxng_maps_results_to_hits() {
	let server = MockServer::start().await;
	Mock::given(method("GET"))
		.and(path("/search"))
		.and(query_param("q", "áfa kulcs & 2026"))
		.and(query_param("format", "json"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"query": "áfa kulcs & 2026",
			"number_of_results": 0,
			"results": [
				{"title": "NAV", "url": "https://nav.gov.hu/afa", "content": "27%", "engine": "brave"},
				{"title": "Other", "url": "https://example.com/", "engine": "bing"},
				{"title": "Third", "url": "https://example.org/", "content": "x"},
			],
		})))
		.expect(1)
		.mount(&server)
		.await;
	let hits = Searxng.search(&ep(&server), "áfa kulcs & 2026", 2).await.unwrap().hits;
	assert_eq!(
		hits,
		[
			SearchHit {
				title: "NAV".into(),
				url: "https://nav.gov.hu/afa".into(),
				snippet: "27%".into()
			},
			SearchHit {
				title: "Other".into(),
				url: "https://example.com/".into(),
				snippet: String::new()
			},
		]
	);
}

#[tokio::test]
async fn searxng_json_disabled_is_rejected() {
	let server = MockServer::start().await;
	Mock::given(method("GET"))
		.respond_with(ResponseTemplate::new(403))
		.mount(&server)
		.await;
	let e = Searxng.search(&ep(&server), "q", 3).await.unwrap_err();
	assert_eq!(e.parts().1, E_REJECTED);
}

#[tokio::test]
async fn searxng_without_base_url_is_a_config_error() {
	let e = Searxng.search(&Endpoint::default(), "q", 3).await.unwrap_err();
	assert_eq!(e.parts().1, "E-LLM-CONFIG");
}

// vim: ts=4
