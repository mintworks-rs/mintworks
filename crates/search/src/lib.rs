// SPDX-License-Identifier: MPL-2.0
//! Web search and page fetching over engines registered in [`SearchBackends`] (or direct for an
//! allow-listed domain set), every fetched page kept as a citable `src_…` row shared across orgs.
#![forbid(unsafe_code)]

pub mod direct;
pub mod fake;
pub mod service;
pub mod store;

pub use direct::Direct;
pub use fake::Fixtures;
pub use service::{Hit, Search};
pub use store::{SearchKey, SearchStore, Source};

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use mintworks_core::{ClResult, Error, error::StatusCode, settings::SettingDef};
use serde::{Deserialize, Serialize};

/// A 429 or 5xx from a provider, or a transport failure: retrying later may succeed.
pub const E_UPSTREAM: &str = "E-SEARCH-UPSTREAM";
/// A 4xx other than 429, or a reply that does not parse.
pub const E_REJECTED: &str = "E-SEARCH-REJECTED";
/// A `fake` provider asked for a search or page no fixture supplies.
pub const E_FAKE_EMPTY: &str = "E-SEARCH-FAKE-EMPTY";
/// A review site: never fetched, only its search snippets may be quoted.
pub const E_BLOCKED: &str = "E-SEARCH-BLOCKED";
/// A direct fetch of a host outside `search.direct_domains`.
pub const E_NOT_DIRECT: &str = "E-SEARCH-NOT-DIRECT";

/// One search result. Serialized as-is into `search_cache.results`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
	pub title: String,
	pub url: String,
	pub snippet: String,
}

/// One search's results. Only `hits` is cached.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hits {
	pub hits: Vec<SearchHit>,
	/// The provider's billed tokens, when it reports them (Jina does).
	pub tokens: Option<u64>,
}

/// One fetched page, before it becomes a [`Source`] row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
	/// The URL the provider reports, after redirects.
	pub url: String,
	pub title: String,
	pub text: String,
	/// The provider's billed tokens, when it reports them (Jina does).
	pub tokens: Option<u64>,
}

/// What the service resolves per call from `search.base_url.<id>` and `search.api_key.<id>`.
// No `Debug`: it holds the API key.
#[derive(Clone, Default)]
pub struct Endpoint {
	/// `None` when the operator left `search.base_url.<id>` blank: the adapter's public
	/// endpoint, or a config error for an engine that has none.
	pub base_url: Option<String>,
	pub api_key: Option<String>,
	/// Let `base_url` be a loopback or private address.
	pub allow_internal: bool,
}

/// A web search engine, selected by `search.provider`.
#[async_trait]
pub trait SearchProvider: Send + Sync + 'static {
	/// The `search.provider` value, the cache key's provider and the ledger's `provider`.
	fn id(&self) -> &'static str;
	async fn search(&self, ep: &Endpoint, query: &str, max_results: u32) -> ClResult<Hits>;
}

/// A page fetcher, selected by `search.fetcher`. Direct fetch is not one: [`Direct`] is routed
/// by `search.direct_domains` ahead of it, and `Search::fetch` refuses a review site before
/// either sees the URL.
#[async_trait]
pub trait Fetcher: Send + Sync + 'static {
	fn id(&self) -> &'static str;
	async fn fetch(&self, ep: &Endpoint, url: &str) -> ClResult<Page>;
}

/// The engines an application links, registered with `AppBuilder::extension(Arc::new(..))`
/// like `mintworks_billing::PaymentProviders`.
#[derive(Default)]
pub struct SearchBackends {
	providers: HashMap<String, Arc<dyn SearchProvider>>,
	fetchers: HashMap<String, Arc<dyn Fetcher>>,
}

impl SearchBackends {
	pub fn new() -> Self {
		Self::default()
	}

	#[must_use]
	pub fn with_provider(mut self, p: Arc<dyn SearchProvider>) -> Self {
		self.providers.insert(p.id().to_owned(), p);
		self
	}

	#[must_use]
	pub fn with_fetcher(mut self, f: Arc<dyn Fetcher>) -> Self {
		self.fetchers.insert(f.id().to_owned(), f);
		self
	}

	pub fn provider(&self, id: &str) -> Option<Arc<dyn SearchProvider>> {
		self.providers.get(id).cloned()
	}

	pub fn fetcher(&self, id: &str) -> Option<Arc<dyn Fetcher>> {
		self.fetchers.get(id).cloned()
	}

	/// Sorted, for an error message.
	pub fn provider_ids(&self) -> Vec<&str> {
		sorted(self.providers.keys())
	}

	pub fn fetcher_ids(&self) -> Vec<&str> {
		sorted(self.fetchers.keys())
	}
}

fn sorted<'a>(keys: impl Iterator<Item = &'a String>) -> Vec<&'a str> {
	let mut v: Vec<&str> = keys.map(String::as_str).collect();
	v.sort_unstable();
	v
}

/// A provider's non-2xx `status`, classified: 429/5xx retry, other 4xx do not.
pub fn status_error(provider: &str, uri: &str, status: StatusCode) -> Error {
	tracing::warn!(%uri, %status, provider, "search provider refused");
	let msg = format!("{provider} answered {status}");
	if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
		Error::coded_retry(StatusCode::BAD_GATEWAY, E_UPSTREAM, msg)
	} else {
		Error::coded(StatusCode::BAD_GATEWAY, E_REJECTED, msg)
	}
}

pub fn bad_reply(provider: &str, e: &serde_json::Error) -> Error {
	Error::coded(StatusCode::BAD_GATEWAY, E_REJECTED, format!("{provider} reply: {e}"))
}

/// `E-LLM-CONFIG`: the engine is misconfigured, not the request. Adapters raise it without
/// depending on `mintworks-llm`.
pub fn config_error(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::SERVICE_UNAVAILABLE, mintworks_llm::service::E_CONFIG, msg)
}

/// This crate's declared settings, registered with
/// `AppBuilder::settings(mintworks_search::SETTINGS)`. The provider is its kind: search and fetch
/// backends are linked code, not operator-named endpoints as in `mintworks-llm`, so only the base
/// URL and the key are per-provider families.
pub static SETTINGS: &[SettingDef] = &[
	// Text, not a choice: the accepted values are whatever the application registered.
	SettingDef::text(
		"search.provider",
		"",
		"The web search backend: an id registered in `SearchBackends`.",
	),
	SettingDef::text(
		"search.fetcher",
		"",
		"The page fetch backend: an id registered in `SearchBackends`.",
	),
	SettingDef::text(
		"search.base_url.",
		"",
		"A search or fetch provider's base URL; blank means its public endpoint, if it has one.",
	)
	.family(),
	SettingDef::text(
		"search.direct_domains",
		"",
		"Comma-separated hosts fetched directly instead of through the fetcher, e.g. nav.gov.hu.",
	),
	SettingDef::int(
		"search.source_ttl_days",
		"7",
		"Days a fetched page or a search result is reused, across orgs, before it is refetched.",
	)
	.range(0, 3650),
	SettingDef::int(
		"search.price.",
		"0",
		"A search or fetch provider's (or `direct`'s) price per call, in integer micro-EUR.",
	)
	.family()
	.range(0, 1_000_000_000),
	// Keyed by provider id: Jina's search and fetch share id `jina` and one token balance.
	SettingDef::int(
		"search.price_per_mtok.",
		"0",
		"A token-billed search or fetch provider's price in integer micro-EUR per 1M billed tokens.",
	)
	.family()
	.range(0, 1_000_000_000),
];

/// `search.api_key.<provider>`: a family, per the trailing `.`.
pub static SECRETS: &[&str] = &["search.api_key."];

// vim: ts=4
