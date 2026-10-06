// SPDX-License-Identifier: MPL-2.0
//! `SearchStore`: the shared `sources` pages and the `search_cache`, in the core DB.

use async_trait::async_trait;
use mintworks_core::{ClResult, ids::SourceId, prelude::Timestamp};

/// One fetched page as the model saw it. Public web content only, never user data, which is
/// what lets one row serve every org.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
	pub uid: SourceId,
	pub url: String,
	pub title: String,
	pub fetched_at: Timestamp,
	/// Hex sha256 of `text`.
	pub sha256: String,
	/// The extracted text.
	pub text: String,
}

/// A cached search's identity. An absent `lang`/`market` is `""`, so it takes part in the key.
#[derive(Clone, Copy, Debug)]
pub struct SearchKey<'a> {
	pub provider: &'a str,
	pub query: &'a str,
	pub lang: &'a str,
	pub market: &'a str,
}

#[async_trait]
pub trait SearchStore: Send + Sync + 'static {
	/// The newest row for `url` fetched at or after `since`. A refetch inserts a new row rather
	/// than overwriting, so an older `src_…` keeps citing what was seen then.
	async fn source_fresh(&self, url: &str, since: Timestamp) -> ClResult<Option<Source>>;
	async fn source_insert(&self, source: &Source) -> ClResult<()>;
	async fn source_get(&self, uid: &SourceId) -> ClResult<Option<Source>>;
	/// The cached results (JSON) when stored at or after `since`.
	async fn search_cache_get(
		&self,
		key: &SearchKey<'_>,
		since: Timestamp,
	) -> ClResult<Option<String>>;
	/// Insert or replace the key's results (JSON), stamped `at`.
	async fn search_cache_put(
		&self,
		key: &SearchKey<'_>,
		results: &str,
		at: Timestamp,
	) -> ClResult<()>;
}

// vim: ts=4
