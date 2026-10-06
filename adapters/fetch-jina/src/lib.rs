// SPDX-License-Identifier: MPL-2.0
//! Jina Reader: `GET {base}/{url}` with `Accept: application/json` answers
//! `{"code","status","data":{"title","url","content","usage":{"tokens"}}}`.
//! Jina Search: `GET {base}/?q=…` with `X-Respond-With: no-content` answers
//! `{"code","data":[{"title","url","description"}],"meta":{"usage":{"tokens"}}}`.
//! The API key is optional for both: Jina serves keyless requests at a lower rate limit.
#![forbid(unsafe_code)]

use std::time::Duration;

use async_trait::async_trait;
use mintworks_core::{ClResult, http, http::pct};
use mintworks_search::{
	Endpoint, Fetcher, Hits, Page, SearchHit, SearchProvider, bad_reply, status_error,
};
use serde::Deserialize;

const FETCH_DEADLINE: Duration = Duration::from_mins(1);
const SEARCH_DEADLINE: Duration = Duration::from_secs(30);

pub const PUBLIC_BASE_URL: &str = "https://r.jina.ai";
pub const SEARCH_BASE_URL: &str = "https://s.jina.ai";

#[derive(Clone, Copy, Debug, Default)]
pub struct Jina;

#[derive(Deserialize)]
struct Reply {
	data: Data,
}

#[derive(Deserialize)]
struct Data {
	#[serde(default)]
	title: String,
	#[serde(default)]
	url: String,
	#[serde(default)]
	content: String,
	#[serde(default)]
	usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
	tokens: u64,
}

#[async_trait]
impl Fetcher for Jina {
	fn id(&self) -> &'static str {
		"jina"
	}

	/// `Search::fetch` refuses a review site before this sees the URL.
	async fn fetch(&self, ep: &Endpoint, url: &str) -> ClResult<Page> {
		let base = ep.base_url.as_deref().unwrap_or(PUBLIC_BASE_URL);
		let uri = format!("{}/{url}", base.trim_end_matches('/'));
		let auth = ep.api_key.as_ref().map(|k| format!("Bearer {k}"));
		let mut headers = vec![("accept", "application/json")];
		if let Some(auth) = &auth {
			headers.push(("authorization", auth.as_str()));
		}
		let (status, _, bytes) =
			http::get_external(&uri, &headers, FETCH_DEADLINE, ep.allow_internal).await?;
		if !status.is_success() {
			return Err(status_error("jina", &uri, status));
		}
		let data = serde_json::from_slice::<Reply>(&bytes).map_err(|e| bad_reply("jina", &e))?.data;
		Ok(Page {
			url: if data.url.is_empty() { url.to_owned() } else { data.url },
			title: data.title,
			text: data.content,
			tokens: data.usage.map(|u| u.tokens),
		})
	}
}

/// Shares the id `jina` with [`Jina`]; each falls back to its own public host, so only an
/// operator setting `search.base_url.jina` has to pick one.
#[derive(Clone, Copy, Debug, Default)]
pub struct JinaSearch;

#[derive(Deserialize)]
struct SearchReply {
	#[serde(default)]
	data: Vec<SearchData>,
	/// The billed total for the whole search, not the per-hit `usage`.
	#[serde(default)]
	meta: Option<Meta>,
}

#[derive(Deserialize)]
struct Meta {
	#[serde(default)]
	usage: Option<Usage>,
}

#[derive(Deserialize)]
struct SearchData {
	#[serde(default)]
	title: String,
	url: String,
	#[serde(default)]
	description: String,
}

#[async_trait]
impl SearchProvider for JinaSearch {
	fn id(&self) -> &'static str {
		"jina"
	}

	async fn search(&self, ep: &Endpoint, query: &str, max_results: u32) -> ClResult<Hits> {
		let base = ep.base_url.as_deref().unwrap_or(SEARCH_BASE_URL);
		let uri = format!("{}/?q={}", base.trim_end_matches('/'), pct(query));
		let auth = ep.api_key.as_ref().map(|k| format!("Bearer {k}"));
		let mut headers = vec![("accept", "application/json"), ("x-respond-with", "no-content")];
		if let Some(auth) = &auth {
			headers.push(("authorization", auth.as_str()));
		}
		let (status, _, bytes) =
			http::get_external(&uri, &headers, SEARCH_DEADLINE, ep.allow_internal).await?;
		if !status.is_success() {
			return Err(status_error("jina", &uri, status));
		}
		let reply: SearchReply =
			serde_json::from_slice(&bytes).map_err(|e| bad_reply("jina", &e))?;
		let n = usize::try_from(max_results.max(1)).unwrap_or(usize::MAX);
		let hits = reply
			.data
			.into_iter()
			.take(n)
			.map(|h| SearchHit { title: h.title, url: h.url, snippet: h.description })
			.collect();
		Ok(Hits { hits, tokens: reply.meta.and_then(|m| m.usage).map(|u| u.tokens) })
	}
}

// vim: ts=4
