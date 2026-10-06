// SPDX-License-Identifier: MPL-2.0
//! `mintworks-memory`'s store: the app-DB module `memory` and `impl MemoryStore for SqliteAppDb`.
//!
//! No triggers: `memory_fts` is rewritten by [`write_version`] in the same transaction
//! as the version insert, keyed by `rowid = memory_docs.id` and holding only the current body.

use async_trait::async_trait;
use mintworks_core::{ClResult, prelude::Timestamp};
use mintworks_memory::{Doc, MemoryStore, NewVersion, SearchHit, Space, Version, WriteMode};
use sqlx::{Connection, SqliteConnection};

use crate::{Conn, Fut, Module, SqliteAppDb, util::DbExt};

/// The `memory` app-DB module. Three-edit rule: change `SCHEMA`, add an `if from < N` step to
/// `apply`, bump `VERSION`.
pub const MEMORY: Module = Module { name: "memory", version: VERSION, apply };

const VERSION: i64 = 1;

const SCHEMA: &[&str] = &[
	"CREATE TABLE memory_spaces (
		id			INTEGER PRIMARY KEY,
		org			TEXT NOT NULL,
		key			TEXT NOT NULL,
		created_at	INTEGER NOT NULL,
		UNIQUE (org, key)
	)",
	"CREATE TABLE memory_docs (
		id			INTEGER PRIMARY KEY,
		space_id	INTEGER NOT NULL REFERENCES memory_spaces(id),
		path		TEXT NOT NULL,
		version		INTEGER NOT NULL,
		updated_at	INTEGER NOT NULL,
		UNIQUE (space_id, path)
	)",
	"CREATE TABLE memory_versions (
		id			INTEGER PRIMARY KEY,
		doc_id		INTEGER NOT NULL REFERENCES memory_docs(id),
		version		INTEGER NOT NULL,
		body		TEXT NOT NULL,
		author		TEXT NOT NULL,
		created_at	INTEGER NOT NULL,
		pdf_sha256	TEXT,
		UNIQUE (doc_id, version)
	)",
	"CREATE VIRTUAL TABLE memory_fts USING fts5(path, body)",
];

fn apply(conn: &mut SqliteConnection, from: i64) -> Fut<'_> {
	Box::pin(async move {
		if from == 0 {
			for &sql in SCHEMA {
				sqlx::query(sql).execute(&mut *conn).await.db()?;
			}
		}
		Ok(())
	})
}

/// `doc_id, version, body, author, created_at, pdf_sha256`.
type VersionRow = (i64, i64, String, String, i64, Option<String>);

fn version(r: VersionRow) -> Version {
	Version {
		doc_id: r.0,
		version: r.1,
		body: r.2,
		author: r.3,
		created_at: Timestamp(r.4),
		pdf_sha256: r.5,
	}
}

fn space(r: (i64, String, String, i64)) -> Space {
	Space { id: r.0, org: r.1, key: r.2, created_at: Timestamp(r.3) }
}

fn doc(r: (i64, i64, String, i64, i64)) -> Doc {
	Doc { id: r.0, space_id: r.1, path: r.2, version: r.3, updated_at: Timestamp(r.4) }
}

/// Every word as a quoted FTS5 string, implicitly ANDed, so user input never reaches the FTS5
/// query syntax (a stray `"` or `AND` there is a syntax error, not a miss).
fn fts_query(query: &str) -> String {
	let words: Vec<String> = query
		.split_whitespace()
		.map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
		.collect();
	words.join(" ")
}

/// Runs `$body` with `$c: &mut SqliteConnection` in a transaction of its own, or in a savepoint
/// of the open `db::tx` block: the writer pool has one connection, which that block holds.
macro_rules! write_tx {
	($db:expr, |$c:ident| $body:expr) => {{
		let mut conn = $db.conn(true).await?;
		if matches!(conn, Conn::Held(_)) {
			let $c = conn.get();
			sqlx::query("SAVEPOINT memory").execute(&mut *$c).await.db()?;
			let out = $body;
			if out.is_err() {
				sqlx::query("ROLLBACK TO memory").execute(&mut *$c).await.db()?;
			}
			sqlx::query("RELEASE memory").execute(&mut *$c).await.db()?;
			out
		} else {
			// sqlx's `Transaction`, not a raw `BEGIN`: dropped mid-flight it rolls back rather
			// than re-pool the only writer with a transaction open.
			let mut tx = conn.get().begin_with("BEGIN IMMEDIATE").await.db()?;
			let $c: &mut SqliteConnection = &mut tx;
			let out = $body;
			if out.is_ok() {
				tx.commit().await.db()?;
			}
			out
		}
	}};
}

async fn write_version(c: &mut SqliteConnection, new: &NewVersion<'_>) -> ClResult<Version> {
	let now = Timestamp::now().0;
	sqlx::query(
		"INSERT INTO memory_spaces (org, key, created_at) VALUES (?, ?, ?) \
		 ON CONFLICT (org, key) DO NOTHING",
	)
	.bind(new.org)
	.bind(new.space_key)
	.bind(now)
	.execute(&mut *c)
	.await
	.db()?;
	let space_id: i64 =
		sqlx::query_scalar("SELECT id FROM memory_spaces WHERE org = ? AND key = ?")
			.bind(new.org)
			.bind(new.space_key)
			.fetch_one(&mut *c)
			.await
			.db()?;

	// Version 0 is a doc with no version yet; it lives only until the UPDATE below.
	let current: Option<(i64, i64)> =
		sqlx::query_as("SELECT id, version FROM memory_docs WHERE space_id = ? AND path = ?")
			.bind(space_id)
			.bind(new.path)
			.fetch_optional(&mut *c)
			.await
			.db()?;
	let (doc_id, prev) = if let Some(found) = current {
		found
	} else {
		let id: i64 = sqlx::query_scalar(
			"INSERT INTO memory_docs (space_id, path, version, updated_at) \
			 VALUES (?, ?, 0, ?) RETURNING id",
		)
		.bind(space_id)
		.bind(new.path)
		.bind(now)
		.fetch_one(&mut *c)
		.await
		.db()?;
		(id, 0)
	};

	let body = match new.mode {
		WriteMode::Append if prev > 0 => {
			let old: String = sqlx::query_scalar(
				"SELECT body FROM memory_versions WHERE doc_id = ? AND version = ?",
			)
			.bind(doc_id)
			.bind(prev)
			.fetch_one(&mut *c)
			.await
			.db()?;
			old + new.body
		}
		_ => new.body.to_owned(),
	};
	let version = prev + 1;
	sqlx::query(
		"INSERT INTO memory_versions (doc_id, version, body, author, created_at, pdf_sha256) \
		 VALUES (?, ?, ?, ?, ?, ?)",
	)
	.bind(doc_id)
	.bind(version)
	.bind(&body)
	.bind(new.author)
	.bind(now)
	.bind(new.pdf_sha256)
	.execute(&mut *c)
	.await
	.db()?;
	sqlx::query("UPDATE memory_docs SET version = ?, updated_at = ? WHERE id = ?")
		.bind(version)
		.bind(now)
		.bind(doc_id)
		.execute(&mut *c)
		.await
		.db()?;
	sqlx::query("DELETE FROM memory_fts WHERE rowid = ?")
		.bind(doc_id)
		.execute(&mut *c)
		.await
		.db()?;
	sqlx::query("INSERT INTO memory_fts (rowid, path, body) VALUES (?, ?, ?)")
		.bind(doc_id)
		.bind(new.path)
		.bind(&body)
		.execute(&mut *c)
		.await
		.db()?;

	Ok(Version {
		doc_id,
		version,
		body,
		author: new.author.to_owned(),
		created_at: Timestamp(now),
		pdf_sha256: new.pdf_sha256.map(str::to_owned),
	})
}

/// Children before parents; `memory_fts` has no foreign key, so it goes first while the docs
/// that name its rows still exist.
async fn erase_org(c: &mut SqliteConnection, org: &str) -> ClResult<u64> {
	for sql in [
		"DELETE FROM memory_fts WHERE rowid IN (SELECT d.id FROM memory_docs d \
		 JOIN memory_spaces s ON s.id = d.space_id WHERE s.org = ?)",
		"DELETE FROM memory_versions WHERE doc_id IN (SELECT d.id FROM memory_docs d \
		 JOIN memory_spaces s ON s.id = d.space_id WHERE s.org = ?)",
		"DELETE FROM memory_docs WHERE space_id IN (SELECT id FROM memory_spaces WHERE org = ?)",
	] {
		sqlx::query(sql).bind(org).execute(&mut *c).await.db()?;
	}
	let done = sqlx::query("DELETE FROM memory_spaces WHERE org = ?")
		.bind(org)
		.execute(&mut *c)
		.await
		.db()?;
	Ok(done.rows_affected())
}

#[async_trait]
impl MemoryStore for SqliteAppDb {
	async fn spaces_list(&self, org: &str) -> ClResult<Vec<Space>> {
		let mut conn = self.conn(false).await?;
		let rows = sqlx::query_as(
			"SELECT id, org, key, created_at FROM memory_spaces WHERE org = ? ORDER BY key",
		)
		.bind(org)
		.fetch_all(conn.get())
		.await
		.db()?;
		Ok(rows.into_iter().map(space).collect())
	}

	async fn space_get(&self, org: &str, key: &str) -> ClResult<Option<Space>> {
		let mut conn = self.conn(false).await?;
		let row = sqlx::query_as(
			"SELECT id, org, key, created_at FROM memory_spaces WHERE org = ? AND key = ?",
		)
		.bind(org)
		.bind(key)
		.fetch_optional(conn.get())
		.await
		.db()?;
		Ok(row.map(space))
	}

	async fn docs_list(&self, space_id: i64) -> ClResult<Vec<Doc>> {
		let mut conn = self.conn(false).await?;
		let rows = sqlx::query_as(
			"SELECT id, space_id, path, version, updated_at FROM memory_docs \
			 WHERE space_id = ? ORDER BY path",
		)
		.bind(space_id)
		.fetch_all(conn.get())
		.await
		.db()?;
		Ok(rows.into_iter().map(doc).collect())
	}

	async fn doc_get(&self, space_id: i64, path: &str) -> ClResult<Option<Doc>> {
		let mut conn = self.conn(false).await?;
		let row = sqlx::query_as(
			"SELECT id, space_id, path, version, updated_at FROM memory_docs \
			 WHERE space_id = ? AND path = ?",
		)
		.bind(space_id)
		.bind(path)
		.fetch_optional(conn.get())
		.await
		.db()?;
		Ok(row.map(doc))
	}

	async fn version_get(&self, doc_id: i64, version: Option<i64>) -> ClResult<Option<Version>> {
		let mut conn = self.conn(false).await?;
		let row: Option<VersionRow> = sqlx::query_as(
			"SELECT doc_id, version, body, author, created_at, pdf_sha256 FROM memory_versions \
			 WHERE doc_id = ?1 \
			 AND version = coalesce(?2, (SELECT version FROM memory_docs WHERE id = ?1))",
		)
		.bind(doc_id)
		.bind(version)
		.fetch_optional(conn.get())
		.await
		.db()?;
		Ok(row.map(self::version))
	}

	async fn versions_list(&self, doc_id: i64) -> ClResult<Vec<Version>> {
		let mut conn = self.conn(false).await?;
		let rows: Vec<VersionRow> = sqlx::query_as(
			"SELECT doc_id, version, body, author, created_at, pdf_sha256 FROM memory_versions \
			 WHERE doc_id = ? ORDER BY version",
		)
		.bind(doc_id)
		.fetch_all(conn.get())
		.await
		.db()?;
		Ok(rows.into_iter().map(version).collect())
	}

	async fn version_write(&self, new: &NewVersion<'_>) -> ClResult<Version> {
		write_tx!(self, |c| write_version(c, new).await)
	}

	async fn search(
		&self,
		org: &str,
		space_id: Option<i64>,
		query: &str,
		limit: u32,
	) -> ClResult<Vec<SearchHit>> {
		let query = fts_query(query);
		if query.is_empty() {
			return Ok(Vec::new());
		}
		let mut conn = self.conn(false).await?;
		let rows: Vec<(String, String, i64, String)> = sqlx::query_as(
			"SELECT s.key, d.path, d.version, snippet(memory_fts, 1, '**', '**', '…', 16) \
			 FROM memory_fts f \
			 JOIN memory_docs d ON d.id = f.rowid \
			 JOIN memory_spaces s ON s.id = d.space_id \
			 WHERE memory_fts MATCH ?1 AND s.org = ?2 AND (?3 IS NULL OR s.id = ?3) \
			 ORDER BY f.rank LIMIT ?4",
		)
		.bind(query)
		.bind(org)
		.bind(space_id)
		.bind(i64::from(limit))
		.fetch_all(conn.get())
		.await
		.db()?;
		Ok(rows
			.into_iter()
			.map(|(space_key, path, version, snippet)| SearchHit {
				space_key,
				path,
				version,
				snippet,
			})
			.collect())
	}

	async fn org_erase(&self, org: &str) -> ClResult<u64> {
		write_tx!(self, |c| erase_org(c, org).await)
	}
}

// vim: ts=4
