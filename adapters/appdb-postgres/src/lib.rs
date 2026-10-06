// SPDX-License-Identifier: MPL-2.0
//! The script's own PostgreSQL database — a **separate database** from the framework store's.
//!
//! `mintworks-script`'s `db::query` / `db::exec` / `db::tx` reach this and nothing else, so an
//! app-profile script cannot read or write any framework table. Isolation is the separate
//! database, not grants inside a shared one, so a superuser or server-file role behind
//! `APP_DB_URL` is refused on first connect.
//!
//! A leaf like `mintworks-appdb-sqlite`: it names `mintworks-script` (whose trait it implements)
//! and `mintworks-core`, never a store adapter.
#![forbid(unsafe_code)]

/// Runs `$body` with `$c: &mut PgConnection` in a transaction of its own, or in savepoint `$sp`
/// of the open `db::tx` block, so a content-module write inside a block commits with it.
#[cfg(feature = "ai")]
macro_rules! write_tx {
	($db:expr, $sp:literal, |$c:ident| $body:expr) => {{
		use sqlx::Connection as _;
		let mut conn = $db.conn(true).await?;
		if matches!(conn, $crate::Conn::Held(_)) {
			let $c = conn.get();
			$crate::util::DbExt::db(
				sqlx::query(concat!("SAVEPOINT ", $sp)).execute(&mut *$c).await,
			)?;
			let out = $body;
			if out.is_err() {
				$crate::util::DbExt::db(
					sqlx::query(concat!("ROLLBACK TO ", $sp)).execute(&mut *$c).await,
				)?;
			}
			$crate::util::DbExt::db(sqlx::query(concat!("RELEASE ", $sp)).execute(&mut *$c).await)?;
			out
		} else {
			// A `Transaction`, not a raw `BEGIN`: dropped mid-flight it rolls back.
			let mut tx = $crate::util::DbExt::db(conn.get().begin().await)?;
			let $c: &mut sqlx::PgConnection = &mut tx;
			let out = $body;
			if out.is_ok() {
				$crate::util::DbExt::db(tx.commit().await)?;
			}
			out
		}
	}};
}

#[cfg(feature = "ai")]
mod agent;
#[cfg(feature = "ai")]
mod memory;
mod migrate;
mod sql;
mod tx;
mod util;

use std::{pin::Pin, str::FromStr, time::Duration};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use mintworks_core::error::{ClResult, Error};
use mintworks_script::{AppDb, TableDef, TxBody, db::Migration};
use serde_json::Value as Json;
use sqlx::{
	Connection, PgConnection, PgPool, Postgres,
	pool::PoolConnection,
	postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::sync::{OnceCell, OwnedMutexGuard};

#[cfg(feature = "ai")]
pub use crate::agent::AGENT;
#[cfg(feature = "ai")]
pub use crate::memory::MEMORY;
pub use crate::migrate::{Fut, Module};
use crate::util::DbExt;

/// Writer pool size. Tuning beyond these goes through the URL's own parameters, not settings.
const WRITER_CONNECTIONS: u32 = 5;
/// Reader pool size; every reader session is `default_transaction_read_only`.
const READER_CONNECTIONS: u32 = 5;
/// How long a caller waits for a connection — sqlx's own default, spelled out.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
/// Server-side, because `script.tx_timeout_ms` only drops the client future: a `pg_sleep` would
/// keep the session busy and the follow-up `ROLLBACK` waiting on it.
// Fixed, not from `script.tx_timeout_ms`; make them settings if an app needs longer queries.
const STATEMENT_TIMEOUT: &str = "30s";
const IDLE_TX_TIMEOUT: &str = "60s";
/// Why a connection is refused; checked on every new session, so a later grant is caught too.
const ESCAPES: &str = "APP_DB_URL's role is a superuser or can reach server files; script SQL would escape the app DB";

/// The two pools, over one database.
struct Pools {
	reader: PgPool,
	writer: PgPool,
}

/// The script database, named at composition and connected on first use.
///
/// **Lazy on purpose**: an `AppDb` has to be registered before the bundle compiles, and an
/// application that never writes a `db::` call should not hold idle connections.
pub struct PgAppDb {
	url: String,
	pools: OnceCell<Pools>,
}

impl PgAppDb {
	/// Names the database by its `postgres://` URL. The pools connect on first use.
	#[must_use]
	pub fn new(url: String) -> Self {
		Self { url, pools: OnceCell::new() }
	}

	async fn pools(&self) -> ClResult<&Pools> {
		self.pools.get_or_try_init(|| open(&self.url)).await
	}

	/// Bring the framework's app-DB modules up to date — before the script's `reconcile`. Connects
	/// even for an empty list, so a privileged `APP_DB_URL` fails the boot.
	pub async fn migrate(&self, modules: &[Module]) -> ClResult<()> {
		let pools = self.pools().await?;
		if modules.is_empty() {
			return Ok(());
		}
		migrate::run(&pools.writer, modules).await
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
	Held(OwnedMutexGuard<PoolConnection<Postgres>>),
	Pooled(PoolConnection<Postgres>),
}

impl Conn {
	fn get(&mut self) -> &mut PgConnection {
		match self {
			Self::Held(guard) => guard,
			Self::Pooled(conn) => conn,
		}
	}
}

/// A writer pool and a reader pool whose sessions refuse writes server-side.
async fn open(url: &str) -> ClResult<Pools> {
	// The parse error, not the URL: it may carry a password.
	let opts = PgConnectOptions::from_str(url)
		.map_err(|e| Error::internal(format!("APP_DB_URL is not a PostgreSQL URL: {e}")))?
		.options([
			("statement_timeout", STATEMENT_TIMEOUT),
			("idle_in_transaction_session_timeout", IDLE_TX_TIMEOUT),
		]);
	// A direct session first: a pool's `after_connect` refusal is retried until the acquire
	// timeout and surfaces as a bare pool timeout.
	let mut probe = PgConnection::connect_with(&opts).await.db()?;
	let refused = escapes(&mut probe).await.db()?;
	let _ = probe.close().await;
	if refused {
		return Err(Error::internal(ESCAPES));
	}
	let pool = || {
		PgPoolOptions::new()
			.acquire_timeout(ACQUIRE_TIMEOUT)
			.after_release(reset)
			.after_connect(|conn, _| {
				Box::pin(async move {
					if escapes(conn).await? {
						return Err(sqlx::Error::Configuration(ESCAPES.into()));
					}
					Ok(())
				})
			})
	};
	let writer = pool().max_connections(WRITER_CONNECTIONS).connect_lazy_with(opts.clone());
	let reader = pool()
		.max_connections(READER_CONNECTIONS)
		.connect_lazy_with(opts.options([("default_transaction_read_only", "on")]));
	Ok(Pools { reader, writer })
}

/// `pg_read_server_files` and friends read any file the server can, the core DB's included.
async fn escapes(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
	sqlx::query_scalar(
		"SELECT rolsuper
			OR pg_has_role(current_user, 'pg_read_server_files', 'MEMBER')
			OR pg_has_role(current_user, 'pg_write_server_files', 'MEMBER')
			OR pg_has_role(current_user, 'pg_execute_server_program', 'MEMBER')
		FROM pg_roles WHERE rolname = current_user",
	)
	.fetch_one(conn)
	.await
}

/// Undoes what script SQL can leave on a session (`set_config`, a session advisory lock) before
/// the next lease. Not `DISCARD ALL`: it drops sqlx's cached prepared statements. `RESET ALL`
/// keeps the startup packet's settings: `default_transaction_read_only` and the timeouts.
fn reset(
	conn: &mut PgConnection,
	_: sqlx::pool::PoolConnectionMetadata,
) -> futures_util::future::BoxFuture<'_, Result<bool, sqlx::Error>> {
	Box::pin(async move {
		sqlx::raw_sql("RESET ALL; SELECT pg_advisory_unlock_all()")
			.execute(conn)
			.await
			.map(|_| true)
	})
}

#[async_trait]
impl AppDb for PgAppDb {
	async fn query(&self, sql: &str, args: &[Json], max_rows: usize) -> ClResult<Vec<Json>> {
		if self::sql::returning_dml(sql) {
			self::sql::allowed(sql, &["INSERT", "UPDATE", "DELETE"])?;
			// Only a `db::tx` block holds a writer; outside one `query` stays read-only.
			let Some(held) = tx::ambient() else {
				return Err(mintworks_script::error::db(
					"a RETURNING write through db::query needs db::tx",
				));
			};
			let mut held = held.lock_owned().await;
			return tx::statement(&mut held, async |c| fetch_capped(c, sql, args, max_rows).await)
				.await;
		}
		self::sql::allowed(sql, &["SELECT", "WITH"])?;
		let mut conn = self.conn(false).await?;
		// Inside `db::tx`: the block's connection, not read-only, so `FOR UPDATE` works.
		let Conn::Pooled(pooled) = &mut conn else {
			return tx::statement(conn.get(), async |c| fetch_capped(c, sql, args, max_rows).await)
				.await;
		};
		// `READ ONLY` makes PostgreSQL refuse a data-modifying CTE. A `Transaction` rather than a
		// bare `BEGIN`: a cancelled call then never re-pools the session mid-transaction.
		let mut tx = pooled.begin_with("BEGIN READ ONLY").await.db()?;
		let rows = fetch_capped(&mut tx, sql, args, max_rows).await?;
		tx.commit().await.db()?;
		Ok(rows)
	}

	async fn exec(&self, sql: &str, args: &[Json]) -> ClResult<u64> {
		self::sql::allowed(sql, &["INSERT", "UPDATE", "DELETE", "WITH"])?;
		let run = async |c: &mut PgConnection| {
			let stmt = self::sql::prepare(c, sql).await?;
			Ok(self::sql::bound(&stmt, args)?.execute(c).await.db()?.rows_affected())
		};
		match &mut self.conn(true).await? {
			Conn::Held(held) => tx::statement(held, run).await,
			Conn::Pooled(conn) => run(conn).await,
		}
	}

	async fn reconcile(&self, migrations: &[Migration], tables: &[TableDef]) -> ClResult<()> {
		// Before `pools()`, so an application that declares neither never connects.
		if migrations.is_empty() && tables.is_empty() {
			return Ok(());
		}
		self::sql::reconcile(&self.pools().await?.writer, migrations, tables).await
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
	conn: &mut PgConnection,
	sql: &str,
	args: &[Json],
	max_rows: usize,
) -> ClResult<Vec<Json>> {
	let stmt = self::sql::prepare(conn, sql).await?;
	let mut rows = self::sql::bound(&stmt, args)?.fetch(conn);
	let mut out = Vec::new();
	while let Some(row) = rows.try_next().await.db()? {
		if out.len() == max_rows {
			return Err(mintworks_script::error::db(format!(
				"query returned more than {max_rows} rows; add LIMIT"
			)));
		}
		out.push(self::sql::row_json(&row)?);
	}
	Ok(out)
}

/// Not derived: the URL may carry a password.
impl std::fmt::Debug for PgAppDb {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PgAppDb").finish_non_exhaustive()
	}
}

// vim: ts=4
