// SPDX-License-Identifier: MPL-2.0
#![forbid(unsafe_code)]

//! PostgreSQL store for Mintworks — the same public shape as `mintworks-store-sqlite`.
//!
//! [`PgStore::open`] builds a writer pool and a reader pool over one database; every reader
//! session is `default_transaction_read_only`, so a write sent to the wrong pool fails loudly.
//!
//! ```ignore
//! let store = PgStore::open(&url).await?;
//! store.migrate(&[mintworks_store_postgres::FRAMEWORK]).await?;
//! mintworks_core::AppBuilder::new()
//!     .config(config)
//!     .store(Arc::new(store) as Arc<dyn mintworks_core::store::CoreStore>)
//!     .run()
//!     .await
//! ```
//!
//! # Extending the store
//!
//! As with `SqliteStore`: define the trait next to the code that calls it and implement it for
//! `PgStore` in your own crate. `reader()`, `conn()`, `write_tx()` and `begin()` are the whole
//! contract; collapse driver errors through [`util::DbExt`].
//!
//! ```ignore
//! impl ProjectStore for PgStore {
//!     async fn rename_project(&self, id: i64, name: &str) -> ClResult<()> {
//!         sqlx::query("UPDATE projects SET name = $1 WHERE id = $2")
//!             .bind(name)
//!             .bind(id)
//!             .execute(&mut *self.conn().await?)
//!             .await
//!             .db()?;
//!         Ok(())
//!     }
//! }
//!
//! pub const PROJECTS: Module = Module { name: "myapp", version: 1, apply };
//!
//! fn apply(conn: &mut sqlx::PgConnection, from: i64) -> Fut<'_> {
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
//! Both modules apply in one transaction, so a consumer table may reference a framework one;
//! list the framework first where that is true.

#[cfg(feature = "ai")]
mod agent;
mod auth;
mod billing;
mod core;
mod entitle;
mod invoice;
mod invoice_doc;
#[cfg(feature = "ai")]
mod llm;
pub mod migrate;
mod migrations;
mod nav;
mod objects;
mod pdf;
mod plans;
mod refs;
pub mod schema;
#[cfg(feature = "ai")]
mod search;
mod tx;
pub mod util;

pub use migrate::{Fut, Module};
pub use nav::{BATCH_CANDIDATES, BY_DATE, BY_NUMBER, UNFILED};
pub use schema::FRAMEWORK;
/// `util::DbExt` is implemented over `sqlx::Error`, so a consumer's own store trait binds to
/// this crate's `sqlx`, not to whatever version its own Cargo.toml resolves.
pub use sqlx;
pub use tx::{ConnGuard, WriteTx};

use std::{str::FromStr, sync::Arc, time::Duration};

use mintworks_core::prelude::*;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use crate::tx::ConnSource;
use crate::util::DbExt;

/// Writer connections. Write transactions serialize on one advisory lock, so the rest serve
/// autocommit writes and `audit_detached` while one is open.
const WRITER_CONNECTIONS: u32 = 10;

const READER_CONNECTIONS: u32 = 5;

/// How long a caller waits for a connection: sqlx's own default, spelled out because `tx.rs`
/// bounds the wait for a bound transaction's connection by the same figure.
pub(crate) const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// The two pools, opened over one database.
#[derive(Clone, Debug)]
pub struct PgStore {
	reader: PgPool,
	writer: PgPool,
	/// Cached because `auth_mw::require_operator` reads it on every gated call and the row
	/// never moves: the migration seeds it and `delete_org` refuses `kind = 'ROOT'`.
	root_org: Arc<std::sync::OnceLock<i64>>,
	/// One permit: this process's outermost `write_tx` callers queue here, not on writer slots.
	write_gate: Arc<tokio::sync::Semaphore>,
	/// Where this handle's writes go: the pool, or a transaction [`begin`](Self::begin) bound it
	/// to. Which one is decided by the handle a caller passes, never by inspecting the task.
	conn: ConnSource,
}

impl PgStore {
	/// Connects both pools to the `postgres://` URL.
	///
	/// # Errors
	/// `Error::Internal` when the URL does not parse; `Error::Unavailable` when the server cannot
	/// be reached.
	pub async fn open(url: &str) -> ClResult<Self> {
		// The parse error, not the URL: it may carry a password.
		let opts = PgConnectOptions::from_str(url)
			.map_err(|e| Error::internal(format!("not a PostgreSQL URL: {e}")))?;
		let writer = PgPoolOptions::new()
			.max_connections(WRITER_CONNECTIONS)
			.acquire_timeout(ACQUIRE_TIMEOUT)
			.connect_with(opts.clone())
			.await
			.db()?;
		let reader = PgPoolOptions::new()
			.max_connections(READER_CONNECTIONS)
			.acquire_timeout(ACQUIRE_TIMEOUT)
			.connect_with(opts.options([("default_transaction_read_only", "on")]))
			.await
			.db()?;
		Ok(Self {
			reader,
			writer,
			root_org: Arc::new(std::sync::OnceLock::new()),
			write_gate: Arc::new(tokio::sync::Semaphore::new(1)),
			conn: ConnSource::Pool,
		})
	}

	/// Brings every module up to the version this build knows — normally `&[FRAMEWORK]`, or
	/// `&[FRAMEWORK, MY_MODULE]`. Call it after [`PgStore::open`] and before `AppBuilder::store`.
	///
	/// # Errors
	/// `Error::Internal` when a module is listed twice, when the database records a *higher*
	/// version than this build knows, and when a module's DDL fails or leaves a dangling
	/// foreign key.
	pub async fn migrate(&self, modules: &[Module]) -> ClResult<()> {
		migrate::run(&self.writer, modules).await
	}

	/// A read connection: this handle's transaction's own when it is bound to one (so a read sees
	/// that transaction's uncommitted writes), else one leased from the reader pool.
	///
	/// # Errors
	/// `Error::Internal` when the handle's transaction has ended; `Error::Unavailable` when no
	/// connection is free within `acquire_timeout`.
	pub async fn reader(&self) -> ClResult<ConnGuard> {
		match self.conn.scope()? {
			None => Ok(ConnGuard::Pooled(self.reader.acquire().await.db()?)),
			Some(held) => Ok(ConnGuard::Held(held.lock_conn().await?)),
		}
	}

	/// The raw reader pool. Bypasses any transaction this handle is bound to.
	pub fn read_pool(&self) -> &PgPool {
		&self.reader
	}

	/// The raw writer pool. Bypasses any transaction this handle is bound to: a statement wants
	/// [`conn`](Self::conn) or [`write_tx`](Self::write_tx) instead.
	pub fn write_pool(&self) -> &PgPool {
		&self.writer
	}

	/// A write connection: this handle's transaction's own when it is bound to one, else one
	/// leased from the writer pool, in autocommit and outside the write lock.
	///
	/// # Errors
	/// As [`reader`](Self::reader).
	pub async fn conn(&self) -> ClResult<ConnGuard> {
		match self.conn.scope()? {
			None => Ok(ConnGuard::Pooled(self.writer.acquire().await.db()?)),
			Some(held) => Ok(ConnGuard::Held(held.lock_conn().await?)),
		}
	}

	/// Runs one write whose failure the caller may recover from (a unique violation answered as
	/// `Conflict`). In a held transaction it runs under a savepoint: PostgreSQL aborts the whole
	/// transaction on any error (25P02), so the caller's next statement would fail.
	pub(crate) async fn recoverable<T>(
		&self,
		run: impl AsyncFnOnce(&mut sqlx::PgConnection) -> ClResult<T>,
	) -> ClResult<T> {
		if self.conn.scope()?.is_none() {
			return run(&mut *self.conn().await?).await;
		}
		let tx = self.write_tx().await?;
		let out = run(&mut *tx.lock().await?).await;
		match out {
			Ok(v) => tx.commit().await.map(|()| v),
			Err(e) => tx.rollback().await.and(Err(e)),
		}
	}

	/// A write transaction holding the database-wide write lock (`pg_advisory_xact_lock`), the
	/// counterpart of SQLite's `BEGIN IMMEDIATE`. On a handle bound by [`begin`](Self::begin), or
	/// inside [`scope_writes`](Self::scope_writes), it marks a `SAVEPOINT` instead.
	///
	/// # Errors
	/// `Error::Unavailable` when the write lock is still held after `lock_timeout`;
	/// `Error::Internal` when a bound transaction has ended.
	pub async fn write_tx(&self) -> ClResult<WriteTx> {
		tx::begin(&self.conn, &self.writer, &self.write_gate).await
	}

	/// A write transaction plus a clone of this handle **bound to it**. The binding is weak: the
	/// returned [`WriteTx`] owns the connection, and a write through a clone that outlives it is
	/// `E-CORE-INTERNAL`, not a write in autocommit.
	///
	/// # Errors
	/// As [`write_tx`](Self::write_tx).
	pub async fn begin(&self) -> ClResult<(WriteTx, PgStore)> {
		let tx = self.write_tx().await?;
		let bound = Self { conn: ConnSource::Held(tx.held()), ..self.clone() };
		Ok((tx, bound))
	}

	/// Runs `fut` with this process's *pooled* handles joined to `tx`. A handle bound by
	/// [`begin`](Self::begin) still wins; a `tokio::spawn` inside `fut` does not inherit the
	/// scope, and its `write_tx` waits on the write lock `tx` holds.
	pub async fn scope_writes<T>(tx: &WriteTx, fut: impl Future<Output = T>) -> T {
		crate::tx::scoped(tx, fut).await
	}
}

// vim: ts=4
