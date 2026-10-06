//! Smoke tests against the real Jina Reader and Search, `#[ignore]`d. Run deliberately:
//! `[JINA_LIVE_API_KEY=…] cargo test -p mintworks-fetch-jina --test live -- --ignored`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_fetch_jina::{Jina, JinaSearch};
use mintworks_search::{Endpoint, Fetcher, SearchProvider};

#[tokio::test]
#[ignore = "calls the real Jina Reader; JINA_LIVE_API_KEY is optional"]
async fn live_jina_fetch() {
	let ep = Endpoint { api_key: std::env::var("JINA_LIVE_API_KEY").ok(), ..Endpoint::default() };
	let page = Jina.fetch(&ep, "https://example.com/").await.unwrap();
	println!("{} ({:?} tokens)\n{}", page.title, page.tokens, page.text);
	assert!(page.text.contains("documentation"));
}

#[tokio::test]
#[ignore = "calls the real Jina Search; JINA_LIVE_API_KEY may be required"]
async fn live_jina_search() {
	let ep = Endpoint { api_key: std::env::var("JINA_LIVE_API_KEY").ok(), ..Endpoint::default() };
	let reply = JinaSearch.search(&ep, "rust programming language", 3).await.unwrap();
	println!("{:#?}\n({:?} tokens)", reply.hits, reply.tokens);
	let hits = reply.hits;
	assert!(!hits.is_empty() && hits.len() <= 3);
}

// vim: ts=4
