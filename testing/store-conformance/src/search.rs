// SPDX-License-Identifier: MPL-2.0
//! `SearchStore` conformance: the newest fresh source wins, the TTL cutoff, and the search cache
//! keyed on all four of its columns.

use mintworks_core::ids::SourceId;
use mintworks_core::prelude::Timestamp;
use mintworks_search::store::{SearchKey, SearchStore, Source};

use crate::{Harness, fresh};

fn source(url: &str, at: i64, text: &str) -> Source {
	Source {
		uid: SourceId::generate(),
		url: url.into(),
		title: "T".into(),
		fetched_at: Timestamp(at),
		sha256: "00".into(),
		text: text.into(),
	}
}

fn key<'a>(lang: &'a str, market: &'a str) -> SearchKey<'a> {
	SearchKey { provider: "fake", query: "vat", lang, market }
}

pub async fn source_fresh_returns_the_newest_row_inside_the_cutoff<H: Harness>()
where
	H::Store: SearchStore,
{
	let h = fresh!(H, "search-fresh");
	let store = h.store();
	let url = "https://example.org/a";
	let old = source(url, 1_000, "old");
	let new = source(url, 2_000, "new");
	store.source_insert(&old).await.unwrap();
	store.source_insert(&new).await.unwrap();
	store
		.source_insert(&source("https://example.org/b", 3_000, "other"))
		.await
		.unwrap();

	let got = store.source_fresh(url, Timestamp(500)).await.unwrap().unwrap();
	assert_eq!(got.uid, new.uid);
	assert_eq!(got.text, "new");
	assert!(store.source_fresh(url, Timestamp(2_001)).await.unwrap().is_none());

	let back = store.source_get(&old.uid).await.unwrap().unwrap();
	assert_eq!((back.url.as_str(), back.fetched_at.0, back.text.as_str()), (url, 1_000, "old"));
	assert!(store.source_get(&SourceId::generate()).await.unwrap().is_none());
}

pub async fn search_cache_upserts_per_full_key<H: Harness>()
where
	H::Store: SearchStore,
{
	let h = fresh!(H, "search-cache");
	let store = h.store();
	store.search_cache_put(&key("", ""), "[1]", Timestamp(1_000)).await.unwrap();
	store.search_cache_put(&key("hu", ""), "[2]", Timestamp(1_000)).await.unwrap();
	store.search_cache_put(&key("", "HU"), "[3]", Timestamp(1_000)).await.unwrap();

	let get = |k: SearchKey<'static>, since: i64| {
		let store = store.clone();
		async move { store.search_cache_get(&k, Timestamp(since)).await.unwrap() }
	};
	assert_eq!(get(key("", ""), 0).await.as_deref(), Some("[1]"));
	assert_eq!(get(key("hu", ""), 0).await.as_deref(), Some("[2]"));
	assert_eq!(get(key("", "HU"), 0).await.as_deref(), Some("[3]"));
	assert!(get(key("", ""), 1_001).await.is_none(), "a stale row is a miss");

	store.search_cache_put(&key("", ""), "[4]", Timestamp(2_000)).await.unwrap();
	assert_eq!(get(key("", ""), 1_500).await.as_deref(), Some("[4]"));
}

// vim: ts=4
