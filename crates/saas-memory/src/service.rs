//! `Memory`: the service handle. Every method takes `&Ctx` first and is confined to `ctx.org`,
//! so another org's space is simply absent — `E-CORE-NOTFOUND`, never 403.

use std::sync::Arc;

use axum::http::StatusCode;
use saas_auth::store::AuthStore;
use saas_core::{ClResult, Ctx, Error};

use crate::store::{Doc, MemoryStore, NewVersion, SearchHit, Version, WriteMode};

const MAX_PATH: usize = 512;
const MAX_KEY: usize = 200;
const MAX_SEARCH: u32 = 100;

pub struct Memory {
	pub(crate) store: Arc<dyn MemoryStore>,
	/// The core DB, for the uids the app DB stores in place of foreign keys.
	pub(crate) orgs: Arc<dyn AuthStore>,
}

impl Memory {
	pub fn new(store: Arc<dyn MemoryStore>, orgs: Arc<dyn AuthStore>) -> Self {
		Self { store, orgs }
	}

	/// The space's docs, by path. A space that was never written to is empty, not an error.
	pub async fn list(&self, ctx: &Ctx, space: &str) -> ClResult<Vec<Doc>> {
		check_key(space)?;
		let org = self.org_uid(ctx).await?;
		match self.store.space_get(&org, space).await? {
			Some(s) => self.store.docs_list(s.id).await,
			None => Ok(Vec::new()),
		}
	}

	/// The current version, or `version` when given.
	pub async fn read(
		&self,
		ctx: &Ctx,
		space: &str,
		path: &str,
		version: Option<i64>,
	) -> ClResult<Version> {
		let doc = self.doc(ctx, space, path).await?;
		self.store.version_get(doc.id, version).await?.ok_or(Error::NotFound)
	}

	/// A new version whose body is `body`. `author` overrides the ctx's account — an agent run
	/// records its `run_…` uid.
	pub async fn write(
		&self,
		ctx: &Ctx,
		space: &str,
		path: &str,
		body: &str,
		author: Option<&str>,
	) -> ClResult<Version> {
		self.put(ctx, space, path, body, author, WriteMode::Replace).await
	}

	/// A new version whose body is the current one with `body` appended as is.
	pub async fn append(
		&self,
		ctx: &Ctx,
		space: &str,
		path: &str,
		body: &str,
		author: Option<&str>,
	) -> ClResult<Version> {
		self.put(ctx, space, path, body, author, WriteMode::Append).await
	}

	/// Current bodies containing every word of `query`, across the org or within `space`.
	pub async fn search(
		&self,
		ctx: &Ctx,
		space: Option<&str>,
		query: &str,
		limit: u32,
	) -> ClResult<Vec<SearchHit>> {
		let org = self.org_uid(ctx).await?;
		let space_id = match space {
			Some(key) => {
				check_key(key)?;
				match self.store.space_get(&org, key).await? {
					Some(s) => Some(s.id),
					None => return Ok(Vec::new()),
				}
			}
			None => None,
		};
		if query.trim().is_empty() {
			return Ok(Vec::new());
		}
		self.store.search(&org, space_id, query, limit.clamp(1, MAX_SEARCH)).await
	}

	/// Every version of the doc, oldest first.
	pub async fn history(&self, ctx: &Ctx, space: &str, path: &str) -> ClResult<Vec<Version>> {
		let doc = self.doc(ctx, space, path).await?;
		self.store.versions_list(doc.id).await
	}

	async fn put(
		&self,
		ctx: &Ctx,
		space: &str,
		path: &str,
		body: &str,
		author: Option<&str>,
		mode: WriteMode,
	) -> ClResult<Version> {
		check_key(space)?;
		check_path(path)?;
		let org = self.org_uid(ctx).await?;
		let author = match author {
			Some(a) if a.starts_with("acc_") || a.starts_with("run_") => a.to_owned(),
			Some(_) => {
				return Err(invalid("E-MEMORY-AUTHOR", "author must be an acc_ or run_ uid"));
			}
			None => self.account_uid(ctx).await?,
		};
		let new = NewVersion {
			org: &org,
			space_key: space,
			path,
			body,
			author: &author,
			pdf_sha256: None,
			mode,
		};
		self.store.version_write(&new).await
	}

	async fn doc(&self, ctx: &Ctx, space: &str, path: &str) -> ClResult<Doc> {
		check_key(space)?;
		check_path(path)?;
		let org = self.org_uid(ctx).await?;
		let s = self.store.space_get(&org, space).await?.ok_or(Error::NotFound)?;
		self.store.doc_get(s.id, path).await?.ok_or(Error::NotFound)
	}

	async fn org_uid(&self, ctx: &Ctx) -> ClResult<String> {
		let org = self.orgs.org_by_id(ctx.org()?).await?.ok_or(Error::NotFound)?;
		Ok(org.uid.to_string())
	}

	async fn account_uid(&self, ctx: &Ctx) -> ClResult<String> {
		let id = ctx.actor.account_id().ok_or_else(|| {
			invalid("E-MEMORY-AUTHOR", "an author is required when no account is acting")
		})?;
		let acc = self.orgs.account_by_id(id).await?.ok_or(Error::NotFound)?;
		Ok(acc.uid.to_string())
	}
}

fn invalid(code: &'static str, msg: &str) -> Error {
	Error::coded(StatusCode::BAD_REQUEST, code, msg)
}

/// `<kind>:<id>`: a lowercase kind, a non-empty id, no whitespace or control characters.
pub(crate) fn check_key(key: &str) -> ClResult<()> {
	let ok = key.len() <= MAX_KEY
		&& key.split_once(':').is_some_and(|(kind, id)| {
			!kind.is_empty()
				&& kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
				&& !id.is_empty()
		}) && !key.chars().any(|c| c.is_whitespace() || c.is_control());
	if ok { Ok(()) } else { Err(invalid("E-MEMORY-KEY", "space key must be <kind>:<id>")) }
}

/// Relative and `/`-separated: no leading `/`, no empty, `.` or `..` segment, no `\`.
pub(crate) fn check_path(path: &str) -> ClResult<()> {
	let ok = !path.is_empty()
		&& path.len() <= MAX_PATH
		&& path.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
		&& !path.chars().any(|c| c == '\\' || c.is_control());
	if ok { Ok(()) } else { Err(invalid("E-MEMORY-PATH", "path must be relative, / separated")) }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn paths() {
		for ok in ["summary.md", "artifacts/one-pager.md", "a/b/c"] {
			assert!(check_path(ok).is_ok(), "{ok}");
		}
		for bad in ["", "/abs.md", "a//b", "a/", "../x", "a/./b", "a\\b", "a\nb"] {
			assert!(check_path(bad).is_err(), "{bad:?}");
		}
	}

	#[test]
	fn keys() {
		for ok in ["account:acc_01", "project:prj_x", "run_log:2026"] {
			assert!(check_key(ok).is_ok(), "{ok}");
		}
		for bad in ["", "account", ":x", "account:", "Account:x", "a b:x", "account:a b"] {
			assert!(check_key(bad).is_err(), "{bad:?}");
		}
	}
}

// vim: ts=4
