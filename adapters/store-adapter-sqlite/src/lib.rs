#![forbid(unsafe_code)]

//! SQLite store for the saas-framework.
//!
//! [`SqliteStore::open`] builds the two pools every framework crate expects: one writer
//! (a single connection — SQLite serialises writes regardless) and one reader pool of
//! five, all in WAL mode. Hand them to the application builder:
//!
//! ```ignore
//! let store = SqliteStore::open(&config).await?;
//! store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await?;
//! saas_core::AppBuilder::new()
//!     .config(config)
//!     .store(Arc::new(store) as Arc<dyn saas_core::store::CoreStore>)
//!     .run()
//!     .await
//! ```
//!
//! # Extending the store
//!
//! The framework's store traits are defined by the crates that need them, and this crate
//! implements none of them. Consumers extend it the same way: define the trait next to the
//! code that calls it, then implement it for `SqliteStore` in your own crate. `reader()`,
//! `writer()` and `write_tx()` are the whole contract — everything else is your SQL.
//!
//! `saas_core::Error` does not convert from the driver's error type — that was the last `sqlx`
//! type in the framework's API — so a driver failure is collapsed where it happens:
//!
//! ```ignore
//! use store_adapter_sqlite::SqliteStore;
//!
//! pub trait ProjectStore {
//!     async fn project_name(&self, id: i64) -> ClResult<Option<String>>;
//!     async fn rename_project(&self, id: i64, name: &str) -> ClResult<()>;
//! }
//!
//! impl ProjectStore for SqliteStore {
//!     async fn project_name(&self, id: i64) -> ClResult<Option<String>> {
//!         sqlx::query_scalar("SELECT name FROM projects WHERE id = ?")
//!             .bind(id)
//!             .fetch_optional(self.reader())
//!             .await
//!             .map_err(|err| Error::internal(format!("database error: {err}")))
//!     }
//!
//!     async fn rename_project(&self, id: i64, name: &str) -> ClResult<()> {
//!         sqlx::query("UPDATE projects SET name = ? WHERE id = ?")
//!             .bind(name)
//!             .bind(id)
//!             .execute(self.writer())
//!             .await
//!             .map_err(|err| Error::internal(format!("database error: {err}")))?;
//!         Ok(())
//!     }
//! }
//! ```
//!
//! Reads go through `reader()`, anything that writes through `writer()`, and a
//! multi-statement write through `write_tx()` (`BEGIN IMMEDIATE`). Your own tables ship as a
//! [`Module`] of your own, versioned independently of the framework's:
//!
//! ```ignore
//! pub const PROJECTS: Module = Module { name: "myapp", version: 1, apply };
//!
//! fn apply(conn: &mut sqlx::SqliteConnection, from: i64) -> Fut<'_> {
//!     Box::pin(async move {
//!         if from == 0 {
//!             sqlx::raw_sql("CREATE TABLE projects (…)").execute(conn).await.db()?;
//!         }
//!         Ok(())
//!     })
//! }
//!
//! store.migrate(&[FRAMEWORK, PROJECTS]).await?;
//! ```
//!
//! Both modules apply in one transaction against one file, so a consumer table may reference a
//! framework one. List the framework first where that is true: the runner keeps list order.
//!
//! `tests/consumer_extension.rs` is this section executed: a local trait, an `impl` for
//! `SqliteStore`, a consumer [`Module`], registration via `AppBuilder::extension` and a row
//! round-tripped back out of `app.extensions`.

mod auth;
mod billing;
mod core;
mod invoice;
pub mod migrate;
mod migrations;
mod nav;
pub mod schema;
pub mod util;

pub use migrate::{Fut, Module};
#[doc(hidden)]
pub use nav::{BATCH_CANDIDATES, BY_DATE, BY_NUMBER, UNFILED};
pub use schema::FRAMEWORK;
/// `util::DbExt` is implemented over `sqlx::Error`, so a consumer's own store trait binds to
/// this crate's `sqlx`, not to whatever version its own Cargo.toml resolves.
pub use sqlx;

use std::{path::Path, sync::Arc, time::Duration};

use saas_core::{config::Config, prelude::*};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, Transaction, sqlite::SqlitePool};

use crate::util::DbExt;

/// Concurrent readers. WAL lets them run while the single writer commits.
const READER_CONNECTIONS: u32 = 5;

/// How long a connection waits for the write lock before returning `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The two pools, opened over one database file.
#[derive(Clone, Debug)]
pub struct SqliteStore {
	reader: SqlitePool,
	writer: SqlitePool,
	/// Cached because `auth_mw::require_operator` reads it on every gated call and the row
	/// never moves: the migration seeds it and `delete_org` refuses `kind = 'ROOT'`.
	///
	/// The `OnceLock` is sound only while that holds, so the root's id must not change after the
	/// first `CoreStore::root_org_id` — a stale id would read as a scoping bug.
	root_org: Arc<std::sync::OnceLock<i64>>,
}

impl SqliteStore {
	/// Opens `config.db_path`, creating the file and its parent directory if missing.
	pub async fn open(config: &Config) -> ClResult<Self> {
		let db_path = &config.db_path;
		if let Some(dir) = Path::new(db_path).parent().filter(|d| !d.as_os_str().is_empty()) {
			std::fs::create_dir_all(dir).map_err(|err| {
				Error::Internal(format!("cannot create {}: {err}", dir.display()))
			})?;
		}

		let opts = SqliteConnectOptions::new()
			.filename(db_path)
			.create_if_missing(true)
			.journal_mode(SqliteJournalMode::Wal)
			.synchronous(SqliteSynchronous::Normal)
			// Runtime only: `migrate::run` turns this off on its own connection for the
			// duration of the migration transaction and back on afterwards.
			.foreign_keys(true)
			.busy_timeout(BUSY_TIMEOUT);

		// The writer opens first: it creates the file and the WAL, and a single connection makes
		// SQLite's own write serialisation explicit. `min_connections(1)` on both because sqlx
		// idles to 0, and when the last connection closes SQLite unlinks `-wal`/`-shm`, which
		// the `read_only` reader below cannot recreate.
		let writer = SqlitePoolOptions::new()
			.max_connections(1)
			.min_connections(1)
			.connect_with(opts.clone())
			.await
			.db()?;
		// `read_only`, so passing the reader where the writer belongs is a loud runtime error
		// rather than a silent write bypassing the single-writer serialization. Safe on ordering
		// because the writer above already created the file and the WAL.
		let reader = SqlitePoolOptions::new()
			.max_connections(READER_CONNECTIONS)
			.min_connections(1)
			.connect_with(opts.create_if_missing(false).read_only(true))
			.await
			.db()?;

		Ok(Self { reader, writer, root_org: Arc::new(std::sync::OnceLock::new()) })
	}

	/// Brings every module up to the version this build knows — normally `&[FRAMEWORK]`, or
	/// `&[FRAMEWORK, MY_MODULE]` when the consumer has tables of its own. Call it after
	/// [`SqliteStore::open`] and before handing the store to `AppBuilder::store`.
	///
	/// # Errors
	/// `Error::Internal` when a module is listed twice, when the database records a *higher*
	/// version than this build knows, and when a module's DDL fails or leaves a dangling
	/// foreign key.
	pub async fn migrate(&self, modules: &[Module]) -> ClResult<()> {
		migrate::run(&self.writer, modules).await
	}

	/// Pool for reads. Five connections, concurrent with the writer under WAL.
	pub fn reader(&self) -> &SqlitePool {
		&self.reader
	}

	/// Pool for anything that writes. One connection.
	pub fn writer(&self) -> &SqlitePool {
		&self.writer
	}

	/// A write transaction, opened with `BEGIN IMMEDIATE` so the write lock is taken up
	/// front and `busy_timeout` covers the wait. A deferred `BEGIN` that reads first and
	/// writes later cannot upgrade its lock and fails with `SQLITE_BUSY` on the spot,
	/// however long the timeout — which is what a second process on the same file sees.
	///
	/// # Errors
	/// `Error::Internal` when the write lock is still held after `busy_timeout`.
	pub async fn write_tx(&self) -> ClResult<Transaction<'static, Sqlite>> {
		self.writer.begin_with("BEGIN IMMEDIATE").await.db()
	}
}

// vim: ts=4
