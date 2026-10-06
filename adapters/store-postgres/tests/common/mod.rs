// SPDX-License-Identifier: MPL-2.0
//! `PgHarness`: the conformance suite's [`Harness`] over a scratch database on the `PG_TEST_URL`
//! server, plus [`TestDb`], the bare scratch database the adapter-only tests open themselves.
//!
//! Needs `PG_TEST_URL` (a `postgres://…/<db>` URL whose role has `CREATEDB`); unset, every test
//! prints a skip line and passes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]

use mintworks_core::ClResult;
use mintworks_store_conformance::Harness;
use mintworks_store_postgres::{PgStore, WriteTx};
use serde_json::Value;
use sqlx::{
	AssertSqlSafe, Postgres, Row, TypeInfo, ValueRef,
	postgres::{PgArguments, PgRow},
	query::Query,
};

mod testdb;

/// The bare scratch database the adapter-only tests open themselves.
pub struct TestDb(testdb::TestDb);

impl TestDb {
	/// `None` when `PG_TEST_URL` is unset.
	pub async fn new() -> Option<Self> {
		testdb::TestDb::create("store", false).await.map(Self)
	}

	pub async fn open(&self) -> PgStore {
		PgStore::open(&self.0.url).await.unwrap()
	}
}

pub struct PgHarness {
	store: PgStore,
	db: TestDb,
}

/// `?` → `$1`, `$2`, …, with a `null` arg inlined as `NULL` instead: a bound NULL carries a type,
/// and an `INT8` NULL into a `TEXT` column is refused. The suite's SQL holds no `?` operator and
/// no `?` inside a literal.
fn bound<'a>(sql: &str, args: &'a [Value]) -> Query<'a, Postgres, PgArguments> {
	let mut out = String::with_capacity(sql.len() + 8);
	let (mut arg, mut n) = (args.iter(), 0);
	let mut binds = Vec::new();
	for c in sql.chars() {
		if c != '?' {
			out.push(c);
			continue;
		}
		match arg.next().expect("fewer args than placeholders") {
			Value::Null => out.push_str("NULL"),
			v => {
				n += 1;
				out.push('$');
				out.push_str(&n.to_string());
				binds.push(v);
			}
		}
	}
	let mut q = sqlx::query(AssertSqlSafe(out));
	for a in binds {
		q = match a {
			Value::Bool(b) => q.bind(i64::from(*b)),
			Value::Number(n) => q.bind(n.as_i64().expect("only integer args bind")),
			Value::String(s) => q.bind(s.as_str()),
			other => panic!("cannot bind {other}"),
		};
	}
	q
}

/// SQLite answers a boolean as `0`/`1`, so `BOOL` reads back as an integer here too.
fn row_values(row: &PgRow) -> Vec<Value> {
	(0..row.len())
		.map(|i| {
			let raw = row.try_get_raw(i).unwrap();
			let ty = raw.type_info();
			match if raw.is_null() { "NULL" } else { ty.name() } {
				"NULL" => Value::Null,
				"INT8" => row.get::<i64, _>(i).into(),
				"INT4" => row.get::<i32, _>(i).into(),
				"INT2" => row.get::<i16, _>(i).into(),
				"BOOL" => i64::from(row.get::<bool, _>(i)).into(),
				"TEXT" | "VARCHAR" | "NAME" | "BPCHAR" => row.get::<String, _>(i).into(),
				ty => panic!("column {i} is {ty}; the harness reads only integers and text"),
			}
		})
		.collect()
}

impl Harness for PgHarness {
	type Store = PgStore;
	type Tx = WriteTx;

	async fn fresh(_name: &str) -> Option<Self> {
		let db = TestDb::new().await?;
		let store = db.open().await;
		store.migrate(&[mintworks_store_postgres::FRAMEWORK]).await.unwrap();
		Some(Self { store, db })
	}

	fn store(&self) -> &PgStore {
		&self.store
	}

	async fn reopen(&self) -> PgStore {
		self.db.open().await
	}

	async fn try_exec(&self, sql: &str, args: &[Value]) -> Result<u64, String> {
		let done = bound(sql, args).execute(self.store.write_pool()).await;
		done.map(|r| r.rows_affected()).map_err(|e| e.to_string())
	}

	async fn rows(&self, sql: &str, args: &[Value]) -> Vec<Vec<Value>> {
		let rows = bound(sql, args).fetch_all(self.store.read_pool()).await.unwrap();
		rows.iter().map(row_values).collect()
	}

	async fn begin(store: &PgStore) -> ClResult<(WriteTx, PgStore)> {
		store.begin().await
	}

	async fn write_tx(store: &PgStore) -> ClResult<WriteTx> {
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
		PgStore::scope_writes(tx, fut).await
	}
}

// vim: ts=4
