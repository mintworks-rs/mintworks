//! Linkup web search: `POST {base}/v1/search`, `depth=standard`, `outputType=searchResults`.
//! Linkup takes no language or market parameter; the query's own wording carries them.
#![forbid(unsafe_code)]

use std::time::Duration;

use async_trait::async_trait;
use mintworks_core::{ClResult, Error, http};
use mintworks_search::{Endpoint, Hits, SearchHit, SearchProvider, bad_reply, status_error};
use serde::Deserialize;
use serde_json::json;

const DEADLINE: Duration = Duration::from_secs(30);

/// Without `/v1`.
pub const PUBLIC_BASE_URL: &str = "https://api.linkup.so";

#[derive(Clone, Copy, Debug, Default)]
pub struct Linkup;

#[derive(Deserialize)]
struct Reply {
	results: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
	#[serde(default)]
	name: String,
	url: String,
	#[serde(default)]
	content: String,
}

#[async_trait]
impl SearchProvider for Linkup {
	fn id(&self) -> &'static str {
		"linkup"
	}

	async fn search(&self, ep: &Endpoint, query: &str, max_results: u32) -> ClResult<Hits> {
		let base = ep.base_url.as_deref().unwrap_or(PUBLIC_BASE_URL);
		let uri = format!("{}/v1/search", base.trim_end_matches('/'));
		let auth = ep.api_key.as_ref().map(|k| format!("Bearer {k}"));
		let mut headers = vec![("content-type", "application/json")];
		if let Some(auth) = &auth {
			headers.push(("authorization", auth.as_str()));
		}
		let body = serde_json::to_vec(&json!({
			"q": query,
			"depth": "standard",
			"outputType": "searchResults",
			"maxResults": max_results.max(1),
		}))
		.map_err(|e| Error::internal(format!("linkup request body: {e}")))?;
		let (status, _, bytes) =
			http::post_external(&uri, &headers, body, DEADLINE, ep.allow_internal).await?;
		if !status.is_success() {
			return Err(status_error("linkup", &uri, status));
		}
		let reply: Reply = serde_json::from_slice(&bytes).map_err(|e| bad_reply("linkup", &e))?;
		let hits = reply
			.results
			.into_iter()
			.map(|h| SearchHit { title: h.name, url: h.url, snippet: h.content })
			.collect();
		Ok(Hits { hits, tokens: None })
	}
}

// vim: ts=4
