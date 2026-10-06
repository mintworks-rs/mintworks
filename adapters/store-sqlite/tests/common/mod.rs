//! `SqliteHarness`: the conformance suite's [`Harness`] over a real file database in a temp dir.
//! Never `sqlite::memory:` — an in-memory URL gives each connection its own database, so two
//! stores would never contend for the write lock, which is what the contention tests exercise.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_core::{ClResult, config::Config};
use mintworks_store_conformance::Harness;
use mintworks_store_sqlite::{SqliteStore, WriteTx};
use serde_json::Value;
use sqlx::{
	AssertSqlSafe, Row, Sqlite, TypeInfo, ValueRef, query::Query, sqlite::SqliteArguments,
	sqlite::SqliteRow,
};

pub struct SqliteHarness {
	dir: std::path::PathBuf,
	store: SqliteStore,
}

impl SqliteHarness {
	fn config(&self) -> Config {
		config(&self.dir)
	}
}

fn config(dir: &std::path::Path) -> Config {
	Config {
		master_key: [0; 32],
		db_path: dir.join("test.db").to_string_lossy().into_owned(),
		data_dir: dir.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	}
}

impl Drop for SqliteHarness {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

fn bound<'a>(sql: &str, args: &'a [Value]) -> Query<'a, Sqlite, SqliteArguments> {
	let mut q = sqlx::query(AssertSqlSafe(sql.to_owned()));
	for a in args {
		q = match a {
			Value::Null => q.bind(None::<i64>),
			Value::Bool(b) => q.bind(i64::from(*b)),
			Value::Number(n) => q.bind(n.as_i64().expect("only integer args bind")),
			Value::String(s) => q.bind(s.as_str()),
			other => panic!("cannot bind {other}"),
		};
	}
	q
}

fn row_values(row: &SqliteRow) -> Vec<Value> {
	(0..row.len())
		.map(|i| {
			let raw = row.try_get_raw(i).unwrap();
			let ty = raw.type_info();
			match if raw.is_null() { "NULL" } else { ty.name() } {
				"NULL" => Value::Null,
				"INTEGER" => row.get::<i64, _>(i).into(),
				"TEXT" => row.get::<String, _>(i).into(),
				ty => panic!("column {i} is {ty}; the harness reads only INTEGER and TEXT"),
			}
		})
		.collect()
}

impl Harness for SqliteHarness {
	type Store = SqliteStore;
	type Tx = WriteTx;

	async fn fresh(name: &str) -> Option<Self> {
		let dir =
			std::env::temp_dir().join(format!("mintworks-conf-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let store = SqliteStore::open(&config(&dir)).await.unwrap();
		store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
		Some(Self { dir, store })
	}

	fn store(&self) -> &SqliteStore {
		&self.store
	}

	async fn reopen(&self) -> SqliteStore {
		SqliteStore::open(&self.config()).await.unwrap()
	}

	async fn try_exec(&self, sql: &str, args: &[Value]) -> Result<u64, String> {
		let done = bound(sql, args).execute(self.store.write_pool()).await;
		done.map(|r| r.rows_affected()).map_err(|e| e.to_string())
	}

	async fn rows(&self, sql: &str, args: &[Value]) -> Vec<Vec<Value>> {
		let rows = bound(sql, args).fetch_all(self.store.read_pool()).await.unwrap();
		rows.iter().map(row_values).collect()
	}

	async fn begin(store: &SqliteStore) -> ClResult<(WriteTx, SqliteStore)> {
		store.begin().await
	}

	async fn write_tx(store: &SqliteStore) -> ClResult<WriteTx> {
		store.write_tx().await
	}

	async fn commit(tx: WriteTx) -> ClResult<()> {
		tx.commit().await
	}

	async fn rollback(tx: WriteTx) -> ClResult<()> {
		tx.rollback().await
	}

	async fn tx_exec(tx: &WriteTx, sql: &str, args: &[Value]) -> u64 {
		let mut conn = tx.lock().await.unwrap();
		bound(sql, args).execute(&mut *conn).await.unwrap().rows_affected()
	}

	async fn scope_writes<T>(tx: &WriteTx, fut: impl Future<Output = T>) -> T {
		SqliteStore::scope_writes(tx, fut).await
	}
}

// vim: ts=4
