// SPDX-License-Identifier: MPL-2.0
//! SearXNG web search against the operator's own instance: `GET {base}/search?q=…&format=json`.
//! There is no public endpoint to fall back to, and the instance must list `json` under
//! `search.formats` — otherwise it answers 403.
#![forbid(unsafe_code)]

use std::time::Duration;

use async_trait::async_trait;
use mintworks_core::{ClResult, http, http::pct};
use mintworks_search::{
	Endpoint, Hits, SearchHit, SearchProvider, bad_reply, config_error, status_error,
};
use serde::Deserialize;

const DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default)]
pub struct Searxng;

#[derive(Deserialize)]
struct Reply {
	results: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
	#[serde(default)]
	title: String,
	url: String,
	#[serde(default)]
	content: String,
}

#[async_trait]
impl SearchProvider for Searxng {
	fn id(&self) -> &'static str {
		"searxng"
	}

	async fn search(&self, ep: &Endpoint, query: &str, max_results: u32) -> ClResult<Hits> {
		let Some(base) = ep.base_url.as_deref() else {
			return Err(config_error(
				"searxng has no public endpoint; set search.base_url.searxng",
			));
		};
		let uri = format!("{}/search?q={}&format=json", base.trim_end_matches('/'), pct(query));
		// Only for an instance behind an authenticating proxy; SearXNG itself takes no key.
		let auth = ep.api_key.as_ref().map(|k| format!("Bearer {k}"));
		let mut headers = vec![("accept", "application/json")];
		if let Some(auth) = &auth {
			headers.push(("authorization", auth.as_str()));
		}
		let (status, _, bytes) =
			http::get_external(&uri, &headers, DEADLINE, ep.allow_internal).await?;
		if !status.is_success() {
			return Err(status_error("searxng", &uri, status));
		}
		let reply: Reply = serde_json::from_slice(&bytes).map_err(|e| bad_reply("searxng", &e))?;
		// SearXNG has no result-count parameter.
		let n = usize::try_from(max_results.max(1)).unwrap_or(usize::MAX);
		let hits = reply
			.results
			.into_iter()
			.take(n)
			.map(|h| SearchHit { title: h.title, url: h.url, snippet: h.content })
			.collect();
		Ok(Hits { hits, tokens: None })
	}
}

// vim: ts=4
