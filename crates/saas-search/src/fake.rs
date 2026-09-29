//! The `fake` search provider and fetcher, fed by `test::search_fixture(...)` under `saas-run test`.

use std::{
	collections::{HashMap, VecDeque},
	sync::Arc,
};

use async_trait::async_trait;
use parking_lot::Mutex;
use saas_core::{ClResult, Error, error::StatusCode};

use crate::{E_FAKE_EMPTY, Endpoint, Fetcher, Hits, Page, SearchHit, SearchProvider};

#[derive(Default)]
struct Inner {
	searches: VecDeque<Vec<SearchHit>>,
	pages: HashMap<String, Page>,
}

/// Shared fixtures: clones read and write the same set. Searches are answered in FIFO order;
/// pages by URL, and stay until [`Fixtures::clear`], since an agent may fetch one twice.
#[derive(Clone, Default)]
pub struct Fixtures(Arc<Mutex<Inner>>);

impl Fixtures {
	pub fn push_search(&self, hits: Vec<SearchHit>) {
		self.0.lock().searches.push_back(hits);
	}

	pub fn put_page(&self, page: Page) {
		self.0.lock().pages.insert(page.url.clone(), page);
	}

	/// Answers `url` with `page`, whose own `url` is where a redirect landed.
	pub fn put_page_at(&self, url: &str, page: Page) {
		self.0.lock().pages.insert(url.to_owned(), page);
	}

	pub fn clear(&self) {
		let mut inner = self.0.lock();
		inner.searches.clear();
		inner.pages.clear();
	}
}

#[async_trait]
impl SearchProvider for Fixtures {
	fn id(&self) -> &'static str {
		"fake"
	}

	async fn search(&self, _: &Endpoint, query: &str, _: u32) -> ClResult<Hits> {
		let hits = self.0.lock().searches.pop_front().ok_or_else(|| {
			empty(format!("the fake search provider has no fixture left for {query:?}"))
		})?;
		Ok(Hits { hits, tokens: None })
	}
}

#[async_trait]
impl Fetcher for Fixtures {
	fn id(&self) -> &'static str {
		"fake"
	}

	async fn fetch(&self, _: &Endpoint, url: &str) -> ClResult<Page> {
		self.0
			.lock()
			.pages
			.get(url)
			.cloned()
			.ok_or_else(|| empty(format!("the fake fetcher has no page for {url}")))
	}
}

fn empty(msg: String) -> Error {
	Error::coded(StatusCode::INTERNAL_SERVER_ERROR, E_FAKE_EMPTY, msg)
}

// vim: ts=4
