//! A smoke test against the real Linkup, `#[ignore]`d. Run deliberately:
//! `LINKUP_LIVE_API_KEY=… cargo test -p mintworks-search-linkup --test live -- --ignored`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_search::{Endpoint, SearchProvider};
use mintworks_search_linkup::Linkup;

#[tokio::test]
#[ignore = "calls the real Linkup API; set LINKUP_LIVE_API_KEY"]
async fn live_linkup_search() {
	let key = std::env::var("LINKUP_LIVE_API_KEY").expect("LINKUP_LIVE_API_KEY is not set");
	let ep = Endpoint { api_key: Some(key), ..Endpoint::default() };
	let hits = Linkup.search(&ep, "NAV Online Számla API", 3).await.unwrap().hits;
	println!("{hits:#?}");
	assert!(!hits.is_empty());
	assert!(hits.iter().all(|h| h.url.starts_with("http")));
}

// vim: ts=4
