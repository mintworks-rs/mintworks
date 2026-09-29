//! `SearchStore` over SQLite: `sources` (insert-only) and `search_cache`.

use async_trait::async_trait;
use saas_core::{ids::SourceId, prelude::*};
use saas_search::store::{SearchKey, SearchStore, Source};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::SqliteStore;
use crate::util::DbExt;

const SOURCE_COLS: &str = "uid, url, title, fetched_at, sha256, text";

fn source(row: &SqliteRow) -> ClResult<Source> {
	Ok(Source {
		uid: SourceId::from_trusted(row.try_get("uid").db()?),
		url: row.try_get("url").db()?,
		title: row.try_get("title").db()?,
		fetched_at: Timestamp(row.try_get("fetched_at").db()?),
		sha256: row.try_get("sha256").db()?,
		text: row.try_get("text").db()?,
	})
}

#[async_trait]
impl SearchStore for SqliteStore {
	async fn source_fresh(&self, url: &str, since: Timestamp) -> ClResult<Option<Source>> {
		let sql = format!(
			"SELECT {SOURCE_COLS} FROM sources WHERE url = ? AND fetched_at >= ?
			 ORDER BY fetched_at DESC, id DESC LIMIT 1"
		);
		let row = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(url)
			.bind(since.0)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()?;
		row.as_ref().map(source).transpose()
	}

	async fn source_insert(&self, s: &Source) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO sources (uid, url, title, fetched_at, sha256, text)
			 VALUES (?, ?, ?, ?, ?, ?)",
		)
		.bind(s.uid.as_str())
		.bind(&s.url)
		.bind(&s.title)
		.bind(s.fetched_at.0)
		.bind(&s.sha256)
		.bind(&s.text)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn source_get(&self, uid: &SourceId) -> ClResult<Option<Source>> {
		let sql = format!("SELECT {SOURCE_COLS} FROM sources WHERE uid = ?");
		let row = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.db()?;
		row.as_ref().map(source).transpose()
	}

	async fn search_cache_get(
		&self,
		key: &SearchKey<'_>,
		since: Timestamp,
	) -> ClResult<Option<String>> {
		sqlx::query_scalar(
			"SELECT results FROM search_cache
			 WHERE provider = ? AND query = ? AND lang = ? AND market = ? AND fetched_at >= ?",
		)
		.bind(key.provider)
		.bind(key.query)
		.bind(key.lang)
		.bind(key.market)
		.bind(since.0)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()
	}

	async fn search_cache_put(
		&self,
		key: &SearchKey<'_>,
		results: &str,
		at: Timestamp,
	) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO search_cache (provider, query, lang, market, results, fetched_at)
			 VALUES (?, ?, ?, ?, ?, ?)
			 ON CONFLICT (provider, query, lang, market)
			 DO UPDATE SET results = excluded.results, fetched_at = excluded.fetched_at",
		)
		.bind(key.provider)
		.bind(key.query)
		.bind(key.lang)
		.bind(key.market)
		.bind(results)
		.bind(at.0)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}
}

// vim: ts=4
