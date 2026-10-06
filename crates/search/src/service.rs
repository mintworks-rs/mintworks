// SPDX-License-Identifier: MPL-2.0
//! `Search`: the service handle. Every provider call is gated by the LLM ledger's caps and
//! recorded there; a cache hit or a fresh `sources` row is neither.

use std::sync::Arc;

use mintworks_core::{App, ClResult, Ctx, Error, http, ids::SourceId, prelude::Timestamp};
use mintworks_llm::{UsageKind, UsageRow, ledger};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
	Direct, E_BLOCKED, Endpoint, Fetcher, Page, SearchBackends, SearchHit, SearchKey,
	SearchProvider, SearchStore, Source, config_error as config, direct,
};

const MAX_RESULTS: u32 = 10;

/// One search result as a caller sees it. `source` is set only for a review site's hit: such a
/// page is never fetched, so its snippet is the one citable thing about it.
#[derive(Clone, Debug, Serialize)]
pub struct Hit {
	#[serde(flatten)]
	pub hit: SearchHit,
	pub source: Option<SourceId>,
}

/// Holds the `App`: providers, prices and the TTL are resolved per call, so an operator's
/// setting change applies without a restart.
#[derive(Clone)]
pub struct Search {
	app: App,
}

impl Search {
	pub fn new(app: App) -> Self {
		Self { app }
	}

	/// Cached per (provider, query, lang, market) for `search.source_ttl_days`, across orgs.
	///
	/// # Errors
	/// `E-LLM-CONFIG` without a store, the ledger's caps, or the provider's `E-SEARCH-*`.
	pub async fn search(
		&self,
		ctx: &Ctx,
		query: &str,
		lang: Option<&str>,
		market: Option<&str>,
		subject: Option<&str>,
		run: Option<&str>,
	) -> ClResult<Vec<Hit>> {
		if query.trim().is_empty() {
			return Err(Error::Validation("search: query is empty".into()));
		}
		let store = self.store()?;
		let (provider, ep) = self.provider().await?;
		let key = SearchKey {
			provider: provider.id(),
			query,
			lang: lang.unwrap_or_default(),
			market: market.unwrap_or_default(),
		};
		let hits = if let Some(json) = store.search_cache_get(&key, self.cutoff().await?).await? {
			serde_json::from_str(&json)
				.map_err(|e| Error::internal(format!("search_cache row: {e}")))?
		} else {
			ledger::check(&self.app, subject).await?;
			let reply = provider.search(&ep, query, MAX_RESULTS).await?;
			let json = serde_json::to_string(&reply.hits)
				.map_err(|e| Error::internal(format!("search results: {e}")))?;
			self.record(ctx, UsageKind::Search, provider.id(), reply.tokens, subject, run)
				.await?;
			store.search_cache_put(&key, &json, Timestamp::now()).await?;
			reply.hits
		};
		let mut out = Vec::with_capacity(hits.len());
		for hit in hits {
			let source = if is_review_site(&hit.url) {
				Some(self.snippet_source(store.as_ref(), &hit).await?)
			} else {
				None
			};
			out.push(Hit { hit, source });
		}
		Ok(out)
	}

	/// A `sources` row younger than `search.source_ttl_days` is reused, whoever fetched it.
	/// Hosts in `search.direct_domains` are fetched directly, everything else by the fetcher.
	///
	/// # Errors
	/// `E-SEARCH-BLOCKED` for a review site, a bad URL, the ledger's caps, or the provider's
	/// `E-SEARCH-*`.
	pub async fn fetch(
		&self,
		ctx: &Ctx,
		url: &str,
		subject: Option<&str>,
		run: Option<&str>,
	) -> ClResult<Source> {
		direct::check_fetchable(url)?;
		let store = self.store()?;
		if let Some(src) = store.source_fresh(url, self.cutoff().await?).await? {
			return Ok(src);
		}
		let settings = &self.app.settings;
		let direct = Direct::from_csv(&settings.text("search.direct_domains").await?);
		ledger::check(&self.app, subject).await?;
		let (name, page) = if direct.covers(url) {
			("direct", direct.fetch(url).await?)
		} else {
			let (fetcher, ep) = self.fetcher().await?;
			// A self-hosted reader sits on the private net, so it would fetch an internal target.
			// The reader re-resolves, so DNS rebinding is not covered; pin the IP if a reader ever takes one.
			if ep.allow_internal {
				http::refuse_internal_target(url).await?;
			}
			let page = fetcher.fetch(&ep, url).await?;
			if page.url != url {
				direct::check_fetchable(&page.url)?;
				if ep.allow_internal {
					http::refuse_internal_target(&page.url).await?;
				}
			}
			(fetcher.id(), page)
		};
		self.record(ctx, UsageKind::Fetch, name, page.tokens, subject, run).await?;
		// Keyed by the requested URL, which `source_fresh` looks up, not the redirect target.
		let src = source(Page { url: url.to_owned(), ..page });
		store.source_insert(&src).await?;
		Ok(src)
	}

	/// Sources are shared across orgs, but an org-less caller is refused.
	///
	/// # Errors
	/// `E-CORE-NOTFOUND` for an unknown or malformed uid.
	pub async fn source(&self, ctx: &Ctx, uid: &str) -> ClResult<Source> {
		ctx.org()?;
		let uid = SourceId::parse(uid).map_err(|_| Error::NotFound)?;
		self.store()?.source_get(&uid).await?.ok_or(Error::NotFound)
	}

	async fn snippet_source(&self, store: &dyn SearchStore, hit: &SearchHit) -> ClResult<SourceId> {
		if let Some(src) = store.source_fresh(&hit.url, self.cutoff().await?).await? {
			return Ok(src.uid);
		}
		let src = source(Page {
			url: hit.url.clone(),
			title: hit.title.clone(),
			text: hit.snippet.clone(),
			tokens: None,
		});
		store.source_insert(&src).await?;
		Ok(src.uid)
	}

	fn store(&self) -> ClResult<Arc<dyn SearchStore>> {
		self.app
			.extensions
			.get::<Arc<dyn SearchStore>>()
			.cloned()
			.ok_or_else(|| config("mintworks-search: no SearchStore was registered on the app"))
	}

	async fn cutoff(&self) -> ClResult<Timestamp> {
		let days = self.app.settings.int("search.source_ttl_days").await?;
		Ok(Timestamp(Timestamp::now().0 - days * 86_400))
	}

	async fn provider(&self) -> ClResult<(Arc<dyn SearchProvider>, Endpoint)> {
		let id = self.app.settings.text("search.provider").await?;
		let backends = self.backends()?;
		let p = backends.provider(&id).ok_or_else(|| {
			config(format!(
				"search.provider {id:?} is not registered (registered: {})",
				backends.provider_ids().join(", ")
			))
		})?;
		Ok((p, self.endpoint(&id).await?))
	}

	async fn fetcher(&self) -> ClResult<(Arc<dyn Fetcher>, Endpoint)> {
		let id = self.app.settings.text("search.fetcher").await?;
		let backends = self.backends()?;
		let f = backends.fetcher(&id).ok_or_else(|| {
			config(format!(
				"search.fetcher {id:?} is not registered (registered: {})",
				backends.fetcher_ids().join(", ")
			))
		})?;
		Ok((f, self.endpoint(&id).await?))
	}

	fn backends(&self) -> ClResult<Arc<SearchBackends>> {
		self.app
			.extensions
			.get::<Arc<SearchBackends>>()
			.cloned()
			.ok_or_else(|| config("mintworks-search: no SearchBackends was registered on the app"))
	}

	/// `search.base_url.<id>` (blank = `None`) and `search.api_key.<id>`.
	async fn endpoint(&self, id: &str) -> ClResult<Endpoint> {
		let base = self.app.settings.text(&format!("search.base_url.{id}")).await?;
		let base_url = (!base.trim().is_empty()).then_some(base);
		let api_key = match self.app.secrets.get(&format!("search.api_key.{id}")).await? {
			Some(k) => Some(
				String::from_utf8(k)
					.map_err(|_| config(format!("search.api_key.{id} is not UTF-8")))?,
			),
			None => None,
		};
		// An operator-set base URL is trusted config, never model input, and a self-hosted
		// engine usually sits on loopback or a private net.
		Ok(Endpoint { allow_internal: base_url.is_some(), base_url, api_key })
	}

	async fn record(
		&self,
		ctx: &Ctx,
		kind: UsageKind,
		provider: &str,
		tokens: Option<u64>,
		subject: Option<&str>,
		run: Option<&str>,
	) -> ClResult<()> {
		let settings = &self.app.settings;
		let tokens = tokens.unwrap_or(0);
		let per_call = settings.int(&format!("search.price.{provider}")).await?;
		let per_mtok = settings.int(&format!("search.price_per_mtok.{provider}")).await?;
		let cost = per_call.saturating_add(ledger::per_mtok(tokens, per_mtok));
		let row = UsageRow {
			at: Timestamp::now(),
			run: run.map(str::to_owned),
			account_id: ctx.actor.account_id(),
			org_id: ctx.org_id,
			subject: subject.map(str::to_owned),
			step: if matches!(kind, UsageKind::Search) { "search" } else { "fetch" }.to_owned(),
			kind,
			provider: provider.to_owned(),
			model: String::new(),
			tokens_in: i64::try_from(tokens).unwrap_or(i64::MAX),
			tokens_out: 0,
			cost_micro_eur: cost,
			retry: false,
		};
		ledger::record(&self.app, &row).await
	}
}

fn is_review_site(url: &str) -> bool {
	matches!(direct::check_fetchable(url), Err(e) if e.parts().1 == E_BLOCKED)
}

fn source(page: Page) -> Source {
	Source {
		uid: SourceId::generate(),
		sha256: hex::encode(Sha256::digest(page.text.as_bytes())),
		url: page.url,
		title: page.title,
		fetched_at: Timestamp::now(),
		text: page.text,
	}
}

// vim: ts=4
