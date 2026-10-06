//! [`TestDb`]: a scratch database on the `PG_TEST_URL` server, owned by a scratch role of its
//! own, for both of this crate's test files.
//!
//! Needs `PG_TEST_URL` (a `postgres://…/<db>` URL whose role has `CREATEDB` and `CREATEROLE`);
//! unset, every test prints a skip line and passes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]

use mintworks_appdb_postgres::PgAppDb;

#[path = "../../../store-postgres/tests/common/testdb.rs"]
mod testdb;

/// A scratch database owned by a `NOSUPERUSER` role rather than the admin: `PgAppDb` refuses a
/// superuser.
pub struct TestDb(testdb::TestDb);

impl TestDb {
	/// `None` when `PG_TEST_URL` is unset.
	pub async fn new() -> Option<Self> {
		testdb::TestDb::create("appdb", true).await.map(Self)
	}

	pub fn open(&self) -> PgAppDb {
		PgAppDb::new(self.0.url.clone())
	}
}

impl std::ops::Deref for TestDb {
	type Target = testdb::TestDb;

	fn deref(&self) -> &testdb::TestDb {
		&self.0
	}
}

// vim: ts=4
