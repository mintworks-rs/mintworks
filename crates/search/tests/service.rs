// SPDX-License-Identifier: MPL-2.0
//! `Search` over the `fake` provider and a real file database: source reuse across orgs, the
//! search cache, which calls reach the ledger, and the fetch refusals.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mintworks_core::{App, Ctx};
use mintworks_llm::LlmStore;
use mintworks_search::{
	E_BLOCKED, E_FAKE_EMPTY, Fixtures, Page, Search, SearchBackends, SearchHit, SearchStore,
};

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-search-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn app(db: &TmpDb, fixtures: &Fixtures) -> App {
	let config = mintworks_core::config::Config {
		master_key: [0; 32],
		db_path: db.0.join("test.db").to_string_lossy().into_owned(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	let store = mintworks_store_sqlite::SqliteStore::open(&config).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let ledger: Arc<dyn LlmStore> = Arc::new(store.clone());
	let sources: Arc<dyn SearchStore> = Arc::new(store.clone());
	let app = mintworks_core::AppBuilder::new()
		.config(config)
		.store(Arc::new(store) as Arc<dyn mintworks_core::store::CoreStore>)
		.settings(mintworks_llm::SETTINGS)
		.settings(mintworks_search::SETTINGS)
		.secrets(mintworks_search::SECRETS)
		.extension(ledger)
		.extension(sources)
		.extension(Arc::new(
			SearchBackends::new()
				.with_provider(Arc::new(fixtures.clone()))
				.with_fetcher(Arc::new(fixtures.clone())),
		))
		.build()
		.await
		.unwrap();
	set(
		&app,
		&[
			("search.provider", "fake"),
			("search.fetcher", "fake"),
			("search.price.fake", "7"),
		],
	)
	.await;
	app
}

async fn set(app: &App, pairs: &[(&str, &str)]) {
	for (k, v) in pairs {
		app.settings.set(k, v, None).await.unwrap();
	}
}

async fn spent(app: &App, subject: &str) -> i64 {
	let ledger = app.extensions.get::<Arc<dyn LlmStore>>().unwrap();
	ledger.usage_cost_for_subject(subject).await.unwrap()
}

fn hit(url: &str) -> SearchHit {
	SearchHit { title: format!("T {url}"), url: url.into(), snippet: format!("about {url}") }
}

fn page(url: &str) -> Page {
	Page { url: url.into(), title: "Page".into(), text: format!("text of {url}"), tokens: None }
}

#[tokio::test]
async fn fetch_reuses_a_fresh_source_across_orgs() {
	let db = TmpDb::new("reuse");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	let search = Search::new(app.clone());
	let url = "https://example.org/a";
	fixtures.put_page(page(url));

	let first = search
		.fetch(&Ctx::system("test").with_org(1), url, Some("s"), None)
		.await
		.unwrap();
	let second = search
		.fetch(&Ctx::system("test").with_org(2), url, Some("s"), None)
		.await
		.unwrap();
	assert_eq!(first.uid, second.uid);
	assert_eq!(first.text, "text of https://example.org/a");
	assert_eq!(spent(&app, "s").await, 7, "the reuse is not a provider call");
	assert_eq!(
		search
			.source(&Ctx::system("test").with_org(2), first.uid.as_str())
			.await
			.unwrap()
			.url,
		url
	);
	assert!(search.source(&Ctx::system("test").with_org(1), "src_nope").await.is_err());
}

#[tokio::test]
async fn token_billed_fetch_costs_per_mtok() {
	let db = TmpDb::new("per-mtok");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	set(&app, &[("search.price_per_mtok.fake", "44034")]).await;
	let url = "https://example.org/tokens";
	fixtures.put_page(Page { tokens: Some(20_000), ..page(url) });
	Search::new(app.clone())
		.fetch(&Ctx::system("test").with_org(1), url, Some("s"), None)
		.await
		.unwrap();
	// 20 000 × 44 034 / 1M = 880.68, rounded up, plus the per-call 7.
	assert_eq!(spent(&app, "s").await, 7 + 881);
}

#[tokio::test]
async fn cache_hit_writes_no_ledger_row() {
	let db = TmpDb::new("cache");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	let search = Search::new(app.clone());
	let ctx = Ctx::system("test").with_org(1);
	fixtures.push_search(vec![hit("https://example.org/a")]);

	let first = search.search(&ctx, "vat", None, None, Some("s"), None).await.unwrap();
	// The fixture queue is empty now: a second provider call would fail with E_FAKE_EMPTY.
	let second = search.search(&ctx, "vat", None, None, Some("s"), None).await.unwrap();
	assert_eq!(first[0].hit.url, second[0].hit.url);
	assert_eq!(spent(&app, "s").await, 7);

	let Err(e) = search.search(&ctx, "vat", Some("hu"), None, Some("s"), None).await else {
		panic!("another lang was answered from the cache")
	};
	assert_eq!(e.parts().1, E_FAKE_EMPTY);
}

#[tokio::test]
async fn review_site_is_never_fetched_but_its_snippet_is_citable() {
	let db = TmpDb::new("review");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	let search = Search::new(app.clone());
	let ctx = Ctx::system("test").with_org(1);
	let g2 = "https://www.g2.com/products/x/reviews";
	fixtures.put_page(page(g2));

	let Err(e) = search.fetch(&ctx, g2, Some("s"), None).await else {
		panic!("a review site was fetched")
	};
	assert_eq!(e.parts().1, E_BLOCKED);
	assert_eq!(spent(&app, "s").await, 0);

	fixtures.push_search(vec![hit(g2), hit("https://example.org/a")]);
	let hits = search.search(&ctx, "x reviews", None, None, None, None).await.unwrap();
	let src = hits[0].source.as_ref().expect("review hit carries a snippet source");
	assert_eq!(search.source(&ctx, src.as_str()).await.unwrap().text, format!("about {g2}"));
	assert!(hits[1].source.is_none(), "an ordinary hit is cited by fetching it");
}

#[tokio::test]
async fn direct_fetch_only_for_listed_hosts_and_never_internal() {
	let db = TmpDb::new("direct");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	let search = Search::new(app.clone());
	let ctx = Ctx::system("test").with_org(1);
	set(&app, &[("search.direct_domains", "127.0.0.1"), ("search.price.direct", "100")]).await;

	// Listed, so routed direct — where the NoInternal guard refuses a loopback target.
	let internal = "http://127.0.0.1:9/page";
	fixtures.put_page(page(internal));
	let Err(e) = search.fetch(&ctx, internal, Some("s"), None).await else {
		panic!("a direct fetch reached a loopback address")
	};
	assert_ne!(e.parts().1, E_FAKE_EMPTY, "a listed host went to the fetcher");
	assert_eq!(spent(&app, "s").await, 0);

	// Not listed, so the fetcher answers and bills at its own price.
	let url = "https://example.org/a";
	fixtures.put_page(page(url));
	search.fetch(&ctx, url, Some("s"), None).await.unwrap();
	assert_eq!(spent(&app, "s").await, 7);
}

#[tokio::test]
async fn source_needs_an_org() {
	let db = TmpDb::new("no-org");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	assert!(Search::new(app).source(&Ctx::system("test"), "src_nope").await.is_err());
}

#[tokio::test]
async fn fetch_refuses_internal_targets_even_with_a_reader_base_url() {
	let db = TmpDb::new("reader-internal");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	set(&app, &[("search.base_url.fake", "http://10.0.0.5:8080")]).await;
	let search = Search::new(app.clone());
	let ctx = Ctx::system("test").with_org(1);
	for url in ["http://127.0.0.1/", "http://169.254.169.254/"] {
		fixtures.put_page(page(url));
		assert!(search.fetch(&ctx, url, Some("s"), None).await.is_err(), "{url} was fetched");
	}
	assert_eq!(spent(&app, "s").await, 0);
}

#[tokio::test]
async fn redirect_to_a_review_site_is_refused() {
	let db = TmpDb::new("redirect-review");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	let url = "https://example.org/r";
	fixtures.put_page_at(url, page("https://www.capterra.com/p/x/reviews"));
	let Err(e) = Search::new(app)
		.fetch(&Ctx::system("test").with_org(1), url, Some("s"), None)
		.await
	else {
		panic!("a redirect to a review site was accepted")
	};
	assert_eq!(e.parts().1, E_BLOCKED);
}

#[tokio::test]
async fn redirected_fetch_is_a_cache_hit_the_second_time() {
	let db = TmpDb::new("redirect-cache");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	let search = Search::new(app.clone());
	let ctx = Ctx::system("test").with_org(1);
	let url = "https://example.org/old";
	fixtures.put_page_at(url, page("https://example.org/new"));
	let first = search.fetch(&ctx, url, Some("s"), None).await.unwrap();
	let second = search.fetch(&ctx, url, Some("s"), None).await.unwrap();
	assert_eq!(first.uid, second.uid);
	assert_eq!(spent(&app, "s").await, 7, "the second fetch was paid for");
}

#[tokio::test]
async fn unknown_provider_is_a_config_error() {
	let db = TmpDb::new("unknown");
	let fixtures = Fixtures::default();
	let app = app(&db, &fixtures).await;
	set(&app, &[("search.provider", "nope")]).await;
	let ctx = Ctx::system("test").with_org(1);
	let Err(e) = Search::new(app).search(&ctx, "q", None, None, None, None).await else {
		panic!("an unregistered provider answered")
	};
	assert_eq!(e.parts().1, mintworks_llm::service::E_CONFIG);
}

// vim: ts=4
