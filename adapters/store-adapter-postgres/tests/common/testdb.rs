//! [`TestDb`]: a scratch database on the `PG_TEST_URL` server, optionally owned by a scratch
//! `NOSUPERUSER` role of the same name. Shared by both PostgreSQL adapters' tests — the app-DB
//! adapter includes this file by `#[path]`, since neither adapter may depend on the other.

use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, PgConnection};

/// A scratch database, dropped with `WITH (FORCE)` so the pools a test leaves open do not keep
/// it alive.
pub struct TestDb {
	pub admin: String,
	name: String,
	owned: bool,
	pub url: String,
}

/// Numbers the databases rather than naming them after the test: a test name can pass the
/// 63-byte identifier limit, and PostgreSQL truncates rather than refuses.
static NEXT: AtomicUsize = AtomicUsize::new(0);

impl TestDb {
	/// `None` when `PG_TEST_URL` is unset. `owned` adds the owner role (`CREATEROLE` needed).
	pub async fn create(prefix: &str, owned: bool) -> Option<Self> {
		let Ok(admin) = std::env::var("PG_TEST_URL") else {
			eprintln!("skipped: PG_TEST_URL is not set");
			return None;
		};
		let name =
			format!("{prefix}_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
		let mut opts = PgConnectOptions::from_str(&admin).unwrap();
		assert!(opts.get_database().is_some(), "PG_TEST_URL must name a database");
		let mut conn = PgConnection::connect(&admin).await.unwrap();
		let mut sqls = vec![format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")];
		if owned {
			let password: String = sqlx::query_scalar("SELECT gen_random_uuid()::text")
				.fetch_one(&mut conn)
				.await
				.unwrap();
			sqls.extend([
				format!("DROP ROLE IF EXISTS {name}"),
				format!("CREATE ROLE {name} NOSUPERUSER LOGIN PASSWORD '{password}'"),
				format!("CREATE DATABASE {name} OWNER {name}"),
			]);
			opts = opts.username(&name).password(&password);
		} else {
			sqls.push(format!("CREATE DATABASE {name}"));
		}
		for sql in sqls {
			sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await.unwrap();
		}
		let url = opts.database(&name).to_url_lossy().to_string();
		Some(Self { admin, name, owned, url })
	}
}

impl Drop for TestDb {
	fn drop(&mut self) {
		let (admin, name, owned) = (self.admin.clone(), self.name.clone(), self.owned);
		// Its own runtime: `Drop` cannot await, and the test's runtime may be the one unwinding.
		let _ = std::thread::spawn(move || {
			let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
			rt.block_on(async {
				let mut conn = PgConnection::connect(&admin).await.unwrap();
				let mut sqls = vec![format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")];
				if owned {
					sqls.push(format!("DROP ROLE IF EXISTS {name}"));
				}
				for sql in sqls {
					sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await.unwrap();
				}
			});
		})
		.join();
	}
}

// vim: ts=4
