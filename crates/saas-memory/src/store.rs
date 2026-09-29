//! `MemoryStore`: spaces, docs, versions and the full-text index, in the app DB.
//!
//! Org and author are uid strings, never foreign keys: the app DB is a separate file from the
//! core DB. Nothing here checks who may see a space — the `Memory` handle confines every call to
//! `ctx.org` before it reaches the store.

use async_trait::async_trait;
use saas_core::{ClResult, prelude::Timestamp};

/// A named container of docs, unique per org. `key` is `<kind>:<id>`, e.g. `account:acc_…`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Space {
	pub id: i64,
	pub org: String,
	pub key: String,
	pub created_at: Timestamp,
}

/// A path within a space, unique per space. `version` is the current one, starting at 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Doc {
	pub id: i64,
	pub space_id: i64,
	pub path: String,
	pub version: i64,
	pub updated_at: Timestamp,
}

/// One immutable snapshot of a doc's body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
	pub doc_id: i64,
	pub version: i64,
	pub body: String,
	/// The `acc_…` or `run_…` uid that wrote it.
	pub author: String,
	pub created_at: Timestamp,
	/// Set when the version was rendered to a stored PDF.
	pub pdf_sha256: Option<String>,
}

/// A doc whose current body matched a search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
	pub space_key: String,
	pub path: String,
	pub version: i64,
	/// An excerpt of the matching body, the matched terms wrapped in `**`.
	pub snippet: String,
}

/// How a new version's body relates to the current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
	/// The body is the new version as given.
	Replace,
	/// The body is concatenated, as is, onto the current version's; the store adds no separator.
	Append,
}

/// A version to write. The space and the doc are created when missing.
#[derive(Clone, Copy, Debug)]
pub struct NewVersion<'a> {
	pub org: &'a str,
	pub space_key: &'a str,
	pub path: &'a str,
	pub body: &'a str,
	pub author: &'a str,
	pub pdf_sha256: Option<&'a str>,
	pub mode: WriteMode,
}

#[async_trait]
pub trait MemoryStore: Send + Sync + 'static {
	/// The org's spaces, by key.
	async fn spaces_list(&self, org: &str) -> ClResult<Vec<Space>>;
	async fn space_get(&self, org: &str, key: &str) -> ClResult<Option<Space>>;
	/// The space's docs, by path.
	async fn docs_list(&self, space_id: i64) -> ClResult<Vec<Doc>>;
	async fn doc_get(&self, space_id: i64, path: &str) -> ClResult<Option<Doc>>;
	/// `version: None` is the current one.
	async fn version_get(&self, doc_id: i64, version: Option<i64>) -> ClResult<Option<Version>>;
	/// Every version of the doc, oldest first.
	async fn versions_list(&self, doc_id: i64) -> ClResult<Vec<Version>>;
	/// Insert the next version, make it current and re-index the doc — atomically. Inside an open
	/// `db::tx` block it joins that block. Existing versions are never modified.
	async fn version_write(&self, new: &NewVersion<'_>) -> ClResult<Version>;
	/// Current bodies containing every word of `query`, best match first; `query` is plain words,
	/// not an index query language. `space_id: None` searches all of the org's spaces.
	async fn search(
		&self,
		org: &str,
		space_id: Option<i64>,
		query: &str,
		limit: u32,
	) -> ClResult<Vec<SearchHit>>;
	/// Hard-delete the org's spaces, docs, versions and index rows (GDPR erase). Returns the
	/// number of spaces removed. The only path that deletes a version.
	async fn org_erase(&self, org: &str) -> ClResult<u64>;
}

// vim: ts=4
