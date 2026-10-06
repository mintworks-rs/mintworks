// SPDX-License-Identifier: MPL-2.0
//! Linkup against a `wiremock` stand-in.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_core::{Error, Retry};
use mintworks_search::{E_REJECTED, E_UPSTREAM, Endpoint, SearchHit, SearchProvider};
use mintworks_search_linkup::Linkup;
use serde_json::json;
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{body_partial_json, header, method, path},
};

fn code(e: &Error) -> &'static str {
	e.parts().1
}

fn ep(server: &MockServer) -> Endpoint {
	Endpoint { base_url: Some(server.uri()), api_key: Some("lk-test".into()), allow_internal: true }
}

#[tokio::test]
async fn linkup_maps_search_results_to_hits() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v1/search"))
		.and(header("authorization", "Bearer lk-test"))
		.and(body_partial_json(json!({
			"q": "áfa kulcs 2026",
			"depth": "standard",
			"outputType": "searchResults",
			"maxResults": 5,
		})))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
			{"type": "text", "name": "NAV", "url": "https://nav.gov.hu/afa", "content": "27%", "favicon": ""},
			{"type": "text", "name": "Other", "url": "https://example.com/", "content": "…"},
		]})))
		.expect(1)
		.mount(&server)
		.await;
	let hits = Linkup.search(&ep(&server), "áfa kulcs 2026", 5).await.unwrap();
	assert_eq!(hits.tokens, None, "Linkup reports no usage");
	let hits = hits.hits;
	assert_eq!(
		hits[0],
		SearchHit {
			title: "NAV".into(),
			url: "https://nav.gov.hu/afa".into(),
			snippet: "27%".into()
		}
	);
	assert_eq!(hits.len(), 2);
}

#[tokio::test]
async fn provider_status_is_classified() {
	for (status, want, retry) in [
		(429, E_UPSTREAM, Retry::Backoff),
		(503, E_UPSTREAM, Retry::Backoff),
		(401, E_REJECTED, Retry::Never),
	] {
		let server = MockServer::builder().start().await;
		Mock::given(method("POST"))
			.respond_with(ResponseTemplate::new(status))
			.mount(&server)
			.await;
		let e = Linkup.search(&ep(&server), "q", 3).await.unwrap_err();
		assert_eq!((code(&e), e.retry()), (want, retry), "{status}");
	}
}

#[tokio::test]
async fn linkup_unparseable_reply_is_rejected() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
		.mount(&server)
		.await;
	let e = Linkup.search(&ep(&server), "q", 3).await.unwrap_err();
	assert_eq!(code(&e), E_REJECTED);
}

// vim: ts=4
