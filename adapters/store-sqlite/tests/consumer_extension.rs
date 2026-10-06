//! The extensibility claim, executed: a consumer application declares its own store trait,
//! implements it for `SqliteStore` (legal under the orphan rule — the trait is local here),
//! ships its own migration [`Module`] beside the framework's, registers the store as an
//! extension and reads it back out of `app.extensions`.
//!
//! Everything below uses only the public surface a real consumer has. Note the one thing it
//! cannot use: `DbExt::db()` is `pub(crate)` in the adapter, so a driver error is collapsed
//! by hand — which is what the `# Extending the store` doc comment in `src/lib.rs` shows.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use async_trait::async_trait;
use mintworks_core::AppBuilder;
use mintworks_core::config::Config;
use mintworks_core::error::{ClResult, Error};
use mintworks_core::store::CoreStore;
use mintworks_store_sqlite::{FRAMEWORK, Fut, Module, SqliteStore};

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, so the store needs a file.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-consumer-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [7; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: self.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: String::new(),
			jobs_workers: None,
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

const M_TICKETS: Module = Module { name: "consumer", version: 1, apply: tickets };

fn tickets(conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		sqlx::raw_sql(
			"CREATE TABLE tickets (
				id     INTEGER PRIMARY KEY,
				title  TEXT NOT NULL,
				status TEXT NOT NULL DEFAULT 'OPEN'
			);",
		)
		.execute(conn)
		.await
		.map_err(|err| db_err(&err))?;
		Ok(())
	})
}

#[async_trait]
trait TicketStore: Send + Sync + 'static {
	async fn create_ticket(&self, title: &str) -> ClResult<i64>;
	async fn ticket(&self, id: i64) -> ClResult<Option<(String, String)>>;
}

fn db_err(err: &sqlx::Error) -> Error {
	Error::internal(format!("database error: {err}"))
}

#[async_trait]
impl TicketStore for SqliteStore {
	/// Insert and read back in one `BEGIN IMMEDIATE` transaction — the consumer's table is
	/// in the framework's database file and under the framework's write lock.
	async fn create_ticket(&self, title: &str) -> ClResult<i64> {
		let tx = self.write_tx().await?;
		let id: i64 = sqlx::query_scalar("INSERT INTO tickets (title) VALUES (?) RETURNING id")
			.bind(title)
			.fetch_one(&mut *tx.lock().await?)
			.await
			.map_err(|err| db_err(&err))?;
		tx.commit().await?;
		Ok(id)
	}

	async fn ticket(&self, id: i64) -> ClResult<Option<(String, String)>> {
		sqlx::query_as("SELECT title, status FROM tickets WHERE id = ?")
			.bind(id)
			.fetch_optional(self.read_pool())
			.await
			.map_err(|err| db_err(&err))
	}
}

#[tokio::test]
async fn consumer_extends_the_store() {
	let db = TmpDb::new("extension");
	let store = SqliteStore::open(&db.config()).await.unwrap();

	// The consumer's module goes beside the framework's: one runner, one file, one
	// transaction, two independently versioned rows in `schema_version`.
	store.migrate(&[FRAMEWORK, M_TICKETS]).await.unwrap();

	let applied: Vec<(String, i64)> =
		sqlx::query_as("SELECT module, version FROM schema_version ORDER BY module")
			.fetch_all(store.read_pool())
			.await
			.unwrap();
	assert_eq!(
		applied,
		[
			("consumer".into(), 1),
			(
				mintworks_store_sqlite::schema::MODULE_NAME.into(),
				mintworks_store_sqlite::schema::VERSION
			)
		]
	);

	// The same store handle goes in twice: once as the framework's `CoreStore`, once as the
	// consumer's own trait object through the extension type-map.
	let tickets: Arc<dyn TicketStore> = Arc::new(store.clone());
	let app = AppBuilder::new()
		.config(db.config())
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		.extension(tickets)
		.build()
		.await
		.unwrap();

	let tickets = app.extensions.get::<Arc<dyn TicketStore>>().expect("TicketStore extension");

	let id = tickets.create_ticket("printer on fire").await.unwrap();
	let got = tickets.ticket(id).await.unwrap().expect("ticket round-trips");
	assert_eq!(got, ("printer on fire".to_string(), "OPEN".to_string()));
	assert!(tickets.ticket(id + 1).await.unwrap().is_none());
}

// vim: ts=4
