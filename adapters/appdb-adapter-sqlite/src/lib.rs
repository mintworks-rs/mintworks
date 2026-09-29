//! The script's own SQLite database — a **separate file** from the framework store's.
//!
//! `saas-script`'s `db::query` / `db::exec` / `db::tx` reach this and nothing else, so an
//! app-profile script cannot read or write `invoices`, `accounts`, `secrets` or any other
//! framework table: they are in another database, and `ATTACH` is refused. That is what makes
//! the raw-SQL escape hatch defensible.
//!
//! A leaf on the `payment-adapter-barion` model: it names `saas-script` (whose trait it
//! implements) and `saas-core`, nothing else. The ~45 lines of driver-error classification it
//! needs are copied into `util.rs` rather than imported from `store-adapter-sqlite`.
#![forbid(unsafe_code)]

#[cfg(feature = "ai")]
mod agent;
#[cfg(feature = "ai")]
mod memory;
mod migrate;
mod sql;
mod tx;
mod util;

use std::{
	path::{Path, PathBuf},
	pin::Pin,
	time::Duration,
};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use saas_core::error::{ClResult, Error};
use saas_script::{AppDb, TableDef, TxBody};
use serde_json::Value as Json;
use sqlx::{
	Sqlite, SqliteConnection, SqlitePool,
	pool::PoolConnection,
	sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use tokio::sync::{OnceCell, OwnedMutexGuard};

#[cfg(feature = "ai")]
pub use crate::agent::AGENT;
#[cfg(feature = "ai")]
pub use crate::memory::MEMORY;
pub use crate::migrate::{Fut, Module};
use crate::util::DbExt;

/// How many readers share the file. One writer, as SQLite serialises writes anyway.
const READER_CONNECTIONS: u32 = 5;
/// How long a connection waits for the write lock before returning `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a caller waits for a connection — sqlx's own default, spelled out.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// The two pools, over one file.
struct Pools {
	reader: SqlitePool,
	writer: SqlitePool,
}

/// The script database, named at composition and opened on first use.
///
/// **Lazy on purpose**: an `AppDb` has to be registered before the bundle compiles, because
/// that is what decides whether the `db` module is installed at all. An application that never
/// writes a `db::` call would otherwise get an empty `app.db` plus `-wal` and `-shm` and two
/// idle connections for nothing.
pub struct SqliteAppDb {
	path: PathBuf,
	pools: OnceCell<Pools>,
}

impl SqliteAppDb {
	/// Names the file. The parent directory is created, and the pools open, on first use.
	#[must_use]
	pub fn new(path: PathBuf) -> Self {
		Self { path, pools: OnceCell::new() }
	}

	async fn pools(&self) -> ClResult<&Pools> {
		self.pools.get_or_try_init(|| open(&self.path)).await
	}

	/// Bring the framework's app-DB modules up to date — before the script's `reconcile`, which
	/// may add columns to nothing but its own tables. An empty list opens nothing, so an app with
	/// no module and no `app.table` still gets no file.
	pub async fn migrate(&self, modules: &[Module]) -> ClResult<()> {
		if modules.is_empty() {
			return Ok(());
		}
		migrate::run(&self.pools().await?.writer, modules).await
	}

	/// The connection the statements about to run should use: the open `db::tx` block's, so a
	/// read inside a block sees its own uncommitted writes, else one leased from a pool.
	async fn conn(&self, write: bool) -> ClResult<Conn> {
		if let Some(held) = tx::ambient() {
			return Ok(Conn::Held(held.lock_owned().await));
		}
		let pools = self.pools().await?;
		let pool = if write { &pools.writer } else { &pools.reader };
		Ok(Conn::Pooled(pool.acquire().await.db()?))
	}
}

enum Conn {
	Held(OwnedMutexGuard<PoolConnection<Sqlite>>),
	Pooled(PoolConnection<Sqlite>),
}

impl Conn {
	fn get(&mut self) -> &mut SqliteConnection {
		match self {
			Self::Held(guard) => guard,
			Self::Pooled(conn) => conn,
		}
	}
}

/// WAL, one writer, five readers — the framework store's settings, for the same reasons.
async fn open(path: &Path) -> ClResult<Pools> {
	if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
		std::fs::create_dir_all(dir)
			.map_err(|e| Error::internal(format!("cannot create {}: {e}", dir.display())))?;
	}
	let opts = SqliteConnectOptions::new()
		.filename(path)
		.create_if_missing(true)
		.journal_mode(SqliteJournalMode::Wal)
		.synchronous(SqliteSynchronous::Normal)
		.foreign_keys(true)
		.busy_timeout(BUSY_TIMEOUT);

	// The writer opens first: it creates the file and the WAL. `min_connections(1)` on both
	// because sqlx idles to 0, and when the last connection closes SQLite unlinks `-wal`/`-shm`,
	// which the `read_only` reader below cannot recreate.
	let writer = SqlitePoolOptions::new()
		.max_connections(1)
		.min_connections(1)
		.acquire_timeout(ACQUIRE_TIMEOUT)
		.connect_with(opts.clone())
		.await
		.db()?;
	let reader = SqlitePoolOptions::new()
		.max_connections(READER_CONNECTIONS)
		.min_connections(1)
		.acquire_timeout(ACQUIRE_TIMEOUT)
		.connect_with(opts.create_if_missing(false).read_only(true))
		.await
		.db()?;
	Ok(Pools { reader, writer })
}

#[async_trait]
impl AppDb for SqliteAppDb {
	async fn query(&self, sql: &str, args: &[Json], max_rows: usize) -> ClResult<Vec<Json>> {
		self::sql::allowed(sql, &["SELECT", "WITH"])?;
		let mut conn = self.conn(false).await?;
		// Inside `db::tx` this is the writer: `query_only` stops a `WITH … DELETE` writing through
		// it. Outside, the reader pool is opened `read_only` already.
		let Conn::Held(held) = &mut conn else {
			return fetch_capped(conn.get(), sql, args, max_rows).await;
		};
		sqlx::query("PRAGMA query_only = ON").execute(&mut ***held).await.db()?;
		let out = fetch_capped(held, sql, args, max_rows).await;
		if let Err(e) = sqlx::query("PRAGMA query_only = OFF").execute(&mut ***held).await {
			// Never re-pool a writer stuck read-only.
			held.close_on_drop();
			return Err(util::map_db(&e));
		}
		out
	}

	async fn exec(&self, sql: &str, args: &[Json]) -> ClResult<u64> {
		self::sql::allowed(sql, &["INSERT", "UPDATE", "DELETE", "WITH"])?;
		let mut conn = self.conn(true).await?;
		let done = self::sql::bound(sql, args)?.execute(conn.get()).await.db()?;
		Ok(done.rows_affected())
	}

	async fn reconcile(&self, tables: &[TableDef]) -> ClResult<()> {
		// Before `pools()`, so an application that declares no table never creates the file.
		if tables.is_empty() {
			return Ok(());
		}
		self::sql::reconcile(&self.pools().await?.writer, tables).await
	}

	fn transaction<'a>(
		&'a self,
		body: TxBody<'a>,
	) -> Pin<Box<dyn Future<Output = ClResult<Json>> + 'a>> {
		Box::pin(async move { tx::run(&self.pools().await?.writer, body).await })
	}
}

/// Every row of `sql`, or `E-SCRIPT-DB` at row `max_rows + 1` — never a silent truncation.
async fn fetch_capped(
	conn: &mut SqliteConnection,
	sql: &str,
	args: &[Json],
	max_rows: usize,
) -> ClResult<Vec<Json>> {
	let mut rows = self::sql::bound(sql, args)?.fetch(conn);
	let mut out = Vec::new();
	while let Some(row) = rows.try_next().await.db()? {
		if out.len() == max_rows {
			return Err(saas_script::error::db(format!(
				"query returned more than {max_rows} rows; add LIMIT"
			)));
		}
		out.push(self::sql::row_json(&row)?);
	}
	Ok(out)
}

/// Not derived: `Pools` holds no `Debug` worth printing and the path is the only useful field.
impl std::fmt::Debug for SqliteAppDb {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("SqliteAppDb").field("path", &self.path).finish_non_exhaustive()
	}
}

// vim: ts=4
