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
//! `conn()`, `write_tx()` and `begin()` are the whole contract — everything else is your SQL.
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
//!             .fetch_optional(&mut *self.reader().await?)
//!             .await
//!             .map_err(|err| Error::internal(format!("database error: {err}")))
//!     }
//!
//!     async fn rename_project(&self, id: i64, name: &str) -> ClResult<()> {
//!         sqlx::query("UPDATE projects SET name = ? WHERE id = ?")
//!             .bind(name)
//!             .bind(id)
//!             .execute(&mut *self.conn().await?)
//!             .await
//!             .map_err(|err| Error::internal(format!("database error: {err}")))?;
//!         Ok(())
//!     }
//! }
//! ```
//!
//! Reads go through `reader()`, anything that writes through `conn()`, and a multi-statement
//! write through `write_tx()`. Both connection accessors answer with the bound transaction's own
//! connection when there is one, so a read inside a transaction sees that transaction's
//! uncommitted writes; `read_pool()` and `write_pool()` hand out the raw pools and bypass it.
//! A write that must join a transaction already open does so by handle, not by task:
//! [`SqliteStore::begin`] returns the transaction plus a clone of the store bound to it, and a
//! write through that clone joins it rather than queueing behind the one writer connection.
//! Your own tables ship as a [`Module`] of your own, versioned independently of the framework's:
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

#[cfg(feature = "ai")]
mod agent;
mod auth;
mod billing;
mod core;
mod invoice;
#[cfg(feature = "ai")]
mod llm;
pub mod migrate;
mod migrations;
mod nav;
mod objects;
mod pdf;
pub mod schema;
#[cfg(feature = "ai")]
mod search;
mod tx;
pub mod util;

pub use migrate::{Fut, Module};
#[doc(hidden)]
pub use nav::{BATCH_CANDIDATES, BY_DATE, BY_NUMBER, UNFILED};
pub use schema::FRAMEWORK;
/// `util::DbExt` is implemented over `sqlx::Error`, so a consumer's own store trait binds to
/// this crate's `sqlx`, not to whatever version its own Cargo.toml resolves.
pub use sqlx;
pub use tx::{ConnGuard, WriteTx};

use std::{path::Path, sync::Arc, time::Duration};

use saas_core::{config::Config, prelude::*};
use sqlx::sqlite::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::tx::ConnSource;
use crate::util::DbExt;

/// Concurrent readers. WAL lets them run while the single writer commits.
const READER_CONNECTIONS: u32 = 5;

/// How long a connection waits for the write lock before returning `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a caller waits for a connection: sqlx's own default, spelled out because `tx.rs`
/// bounds the wait for a bound transaction's connection by the same figure.
pub(crate) const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

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
	/// Where this handle's writes go: the pool, or a transaction [`begin`](Self::begin) bound it
	/// to. Which one is decided by the handle a caller passes, never by inspecting the task.
	conn: ConnSource,
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
			.acquire_timeout(ACQUIRE_TIMEOUT)
			.connect_with(opts.clone())
			.await
			.db()?;
		// `read_only`, so passing the reader where the writer belongs is a loud runtime error
		// rather than a silent write bypassing the single-writer serialization. Safe on ordering
		// because the writer above already created the file and the WAL.
		let reader = SqlitePoolOptions::new()
			.max_connections(READER_CONNECTIONS)
			.min_connections(1)
			.acquire_timeout(ACQUIRE_TIMEOUT)
			.connect_with(opts.create_if_missing(false).read_only(true))
			.await
			.db()?;

		Ok(Self {
			reader,
			writer,
			root_org: Arc::new(std::sync::OnceLock::new()),
			conn: ConnSource::Pool,
		})
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

	/// A read connection for the statements about to run: this handle's own when it is bound to
	/// a transaction, else one leased from the reader pool.
	///
	/// A read reached from inside a transaction comes through here, so it sees that
	/// transaction's own uncommitted writes; the reader pool is a different connection and would
	/// not, which turns a read-modify-write on a bound handle into two connections disagreeing.
	///
	/// # Errors
	/// `Error::Internal` when the transaction's connection cannot be taken, or when the reader
	/// pool cannot lease a connection within `acquire_timeout`.
	pub async fn reader(&self) -> ClResult<ConnGuard> {
		match self.conn.scope()? {
			None => Ok(ConnGuard::Pooled(self.reader.acquire().await.db()?)),
			Some(held) => Ok(ConnGuard::Held(held.lock_conn().await?)),
		}
	}

	/// The raw reader pool, five connections, concurrent with the writer under WAL. It bypasses
	/// any transaction this handle is bound to; [`reader`](Self::reader) is what a statement
	/// wants. This is for the caller that needs a pool rather than a connection — opening a
	/// read-only snapshot transaction of its own, say.
	pub fn read_pool(&self) -> &SqlitePool {
		&self.reader
	}

	/// The raw writer pool, one connection. Bypasses any transaction this handle is bound to and
	/// will block until [`ACQUIRE_TIMEOUT`] if one holds the connection, so a statement wants
	/// [`conn`](Self::conn) or [`write_tx`](Self::write_tx) instead.
	pub fn write_pool(&self) -> &SqlitePool {
		&self.writer
	}

	/// A write connection for the statements about to run: this handle's own when it is bound to
	/// a transaction, else one leased from the writer pool.
	///
	/// A write reached from inside a transaction comes through here rather than through
	/// [`write_pool`](Self::write_pool), which would ask the pool for the connection the
	/// transaction is already holding and block on it until [`ACQUIRE_TIMEOUT`].
	///
	/// # Errors
	/// `Error::Internal` when the transaction's connection cannot be taken, or when the pool cannot
	/// hand out its one connection within `acquire_timeout`.
	pub async fn conn(&self) -> ClResult<ConnGuard> {
		match self.conn.scope()? {
			None => Ok(ConnGuard::Pooled(self.writer.acquire().await.db()?)),
			Some(held) => Ok(ConnGuard::Held(held.lock_conn().await?)),
		}
	}

	/// A write transaction, opened with `BEGIN IMMEDIATE` so the write lock is taken up
	/// front and `busy_timeout` covers the wait. A deferred `BEGIN` that reads first and
	/// writes later cannot upgrade its lock and fails with `SQLITE_BUSY` on the spot,
	/// however long the timeout — which is what a second process on the same file sees.
	///
	/// On a handle bound by [`begin`](Self::begin) this marks a `SAVEPOINT` in the transaction
	/// that handle is bound to, so the write joins it. A second `BEGIN IMMEDIATE` would queue
	/// behind the connection the bound handle already holds, until `acquire_timeout`.
	///
	/// # Errors
	/// `Error::Internal` when the write lock is still held after `busy_timeout`, and when a bound
	/// transaction's connection cannot be taken.
	pub async fn write_tx(&self) -> ClResult<WriteTx> {
		tx::begin(&self.conn, &self.writer).await
	}

	/// A write transaction plus a clone of this handle **bound to it**. Hand the bound handle to
	/// the code that must run inside the transaction; code still holding `self` writes in
	/// autocommit. This is what lets a script run a service method in its own transaction with no
	/// method taking a transaction parameter.
	///
	/// The binding is weak: the returned [`WriteTx`] is what holds the writer connection, so the
	/// transaction ends when it is committed, rolled back or dropped, whether or not the bound
	/// clone is still alive. A write through a clone that outlives it is
	/// `E-CORE-INTERNAL`, not a write in autocommit.
	///
	/// # Errors
	/// As [`write_tx`](Self::write_tx).
	pub async fn begin(&self) -> ClResult<(WriteTx, SqliteStore)> {
		let tx = self.write_tx().await?;
		let bound = Self { conn: ConnSource::Held(tx.held()), ..self.clone() };
		Ok((tx, bound))
	}

	/// Runs `fut` with this process's *pooled* handles joined to `tx`.
	///
	/// [`begin`](Self::begin) binds a handle; this binds a task, for the caller that cannot hand
	/// the bound handle to what runs inside it — a script's `tx::with` block reaches its service
	/// methods through an `App` it cannot rebuild. A handle bound by `begin` still wins.
	///
	/// A `tokio::spawn` inside `fut` does **not** inherit the scope: its writes take their own
	/// `BEGIN IMMEDIATE` and then block on the connection `tx` holds until [`ACQUIRE_TIMEOUT`].
	///
	/// Associated rather than a method: which handle the caller holds is what stops mattering.
	pub async fn scope_writes<T>(tx: &WriteTx, fut: impl Future<Output = T>) -> T {
		crate::tx::scoped(tx, fut).await
	}
}

// vim: ts=4
