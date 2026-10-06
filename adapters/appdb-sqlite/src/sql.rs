//! What a script's statement is allowed to be, how its arguments bind, how a row crosses back,
//! and the startup reconcile of the declared tables.

use std::collections::BTreeSet;

use mintworks_core::error::{ClResult, Error};
use mintworks_script::{ColDef, ColType, TableDef, db::Migration};
use serde_json::Value as Json;
use sqlx::{
	AssertSqlSafe, Column, Row, Sqlite, SqliteConnection, SqlitePool, TypeInfo, ValueRef,
	query::Query, sqlite::SqliteArguments,
};

use crate::util::DbExt;

/// What `query` and `exec` each accept as a statement's first keyword.
///
/// DDL belongs to `app.table` and transaction control to `db::tx`. `ATTACH` is the load-bearing
/// one: it would reach *another* database file, which now includes the framework's.
///
/// One statement only: sqlx runs every `;`-separated statement, so `SELECT 1; ATTACH …` would
/// pass a first-keyword check. A `;` inside a literal is refused too — values bind as arguments.
pub(crate) fn allowed(sql: &str, kinds: &[&str]) -> ClResult<()> {
	if sql.trim_end_matches(|c: char| c.is_whitespace() || c == ';').contains(';') {
		return Err(mintworks_script::error::db("a db:: call takes one statement"));
	}
	let first = sql.split_whitespace().next().unwrap_or_default().to_uppercase();
	if kinds.contains(&first.as_str()) {
		return Ok(());
	}
	Err(mintworks_script::error::db(format!("a db:: statement must start with one of {kinds:?}")))
}

/// `INSERT`/`UPDATE`/`DELETE … RETURNING`: the one write `query` runs, and only inside `db::tx`.
/// A plain token check — a `RETURNING` inside a string literal passes it, and the statement is
/// then nothing worse than a write the tx was allowed anyway.
pub(crate) fn returning_dml(sql: &str) -> bool {
	let mut words = sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
	let first = words.find(|w| !w.is_empty()).unwrap_or_default();
	["INSERT", "UPDATE", "DELETE"].iter().any(|k| first.eq_ignore_ascii_case(k))
		&& words.any(|w| w.eq_ignore_ascii_case("RETURNING"))
}

/// The statement with its arguments bound. `mintworks_script::db` has already refused an amount and
/// a non-finite float; this refuses the rest, because `AppDb` is a public trait with other callers.
pub(crate) fn bound<'a>(
	sql: &str,
	args: &'a [Json],
) -> ClResult<Query<'a, Sqlite, SqliteArguments>> {
	let mut q = sqlx::query(AssertSqlSafe(sql.to_owned()));
	for (i, a) in args.iter().enumerate() {
		let bad = || mintworks_script::error::db(format!("argument {i} cannot bind to a column"));
		q = match a {
			Json::Null => q.bind(None::<i64>),
			Json::Bool(b) => q.bind(i64::from(*b)),
			Json::Number(n) => match n.as_i64() {
				Some(i) => q.bind(i),
				None => q.bind(n.as_f64().filter(|f| f.is_finite()).ok_or_else(bad)?),
			},
			Json::String(s) => q.bind(s.as_str()),
			_ => return Err(bad()),
		};
	}
	Ok(q)
}

/// One row as a JSON object, keyed by column name.
///
/// `BLOB` is refused rather than mapped: a script has no byte type to receive it.
pub(crate) fn row_json(row: &sqlx::sqlite::SqliteRow) -> ClResult<Json> {
	let mut out = serde_json::Map::new();
	for (i, col) in row.columns().iter().enumerate() {
		let raw = row.try_get_raw(i).map_err(|e| crate::util::map_db(&e))?;
		// A NULL in a typed column reports the column's declared type, not "NULL".
		let ty = raw.type_info();
		let value = match if raw.is_null() { "NULL" } else { ty.name() } {
			"NULL" => Json::Null,
			"INTEGER" => row.try_get::<i64, _>(i).map_err(|e| crate::util::map_db(&e))?.into(),
			"TEXT" => row.try_get::<String, _>(i).map_err(|e| crate::util::map_db(&e))?.into(),
			"REAL" => row.try_get::<f64, _>(i).map_err(|e| crate::util::map_db(&e))?.into(),
			ty => {
				return Err(mintworks_script::error::db(format!(
					"column '{}' is {ty}; only NULL, INTEGER, REAL and TEXT cross into script",
					col.name()
				)));
			}
		};
		out.insert(col.name().to_owned(), value);
	}
	Ok(Json::Object(out))
}

/// One statement on the reconcile connection. The text is composed from tokens
/// `mintworks_script::db` has already validated, never from a declaration file verbatim
/// (`app.migration`'s raw DDL goes through `migrate::app_chain` instead).
async fn ddl(conn: &mut SqliteConnection, sql: String) -> ClResult<()> {
	sqlx::query(AssertSqlSafe(sql)).execute(conn).await.db()?;
	Ok(())
}

/// Brings the database up to the declarations, in one transaction: the `app.migration` chain,
/// then the tables. **Additive**: it never drops a table and never drops a column, because a
/// declaration file people edit casually must not be able to lose data. A table that is no
/// longer declared is left alone.
pub(crate) async fn reconcile(
	writer: &SqlitePool,
	migrations: &[Migration],
	tables: &[TableDef],
) -> ClResult<()> {
	let mut conn = writer.acquire().await.db()?;
	sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await.db()?;
	// Either failure leaves the transaction open, so the connection is closed, not re-pooled.
	let done = match crate::migrate::app_chain(&mut conn, migrations).await {
		Ok(()) => apply(&mut conn, tables).await,
		Err(e) => Err(e),
	};
	match done {
		Ok(()) => {
			let done = sqlx::query("COMMIT").execute(&mut *conn).await;
			if done.is_err() {
				conn.close_on_drop();
			}
			done.db()?;
			Ok(())
		}
		Err(e) => {
			if let Err(fault) = sqlx::query("ROLLBACK").execute(&mut *conn).await {
				tracing::error!(error = %fault, "rolling back the script table reconcile failed");
				conn.close_on_drop();
			}
			Err(e)
		}
	}
}

/// SQLite's spelling of a column type; the rest exist on PostgreSQL only.
fn sqlite_type(ty: ColType) -> ClResult<&'static str> {
	match ty {
		ColType::Int | ColType::Text | ColType::Blob | ColType::Real => Ok(ty.keyword()),
		_ => Err(Error::internal(format!(
			"column type {} is PostgreSQL only; SQLite has INTEGER, TEXT, BLOB and REAL",
			ty.keyword()
		))),
	}
}

/// A column's DDL on SQLite. `now()` and `gen_random_uuid()` defaults are PostgreSQL only too.
fn sqlite_col(col: &ColDef) -> ClResult<String> {
	if let Some(d) = col.default.as_deref().filter(|d| d.ends_with("()")) {
		return Err(Error::internal(format!("{}: DEFAULT {d} is PostgreSQL only", col.name)));
	}
	Ok(col.fragment(sqlite_type(col.ty)?))
}

async fn apply(conn: &mut SqliteConnection, tables: &[TableDef]) -> ClResult<()> {
	for t in tables {
		let cols = t.cols.iter().map(sqlite_col).collect::<ClResult<Vec<_>>>()?.join(", ");
		ddl(conn, format!("CREATE TABLE IF NOT EXISTS \"{}\" ({cols})", t.name)).await?;

		let have: Vec<(String, String)> =
			sqlx::query_as("SELECT name, type FROM pragma_table_info(?)")
				.bind(&t.name)
				.fetch_all(&mut *conn)
				.await
				.db()?;
		for col in &t.cols {
			match have.iter().find(|(name, _)| *name == col.name) {
				// A silent skip would leave a live database in a shape no fresh install reaches —
				// the same reasoning as the framework's schema-version stamp.
				Some((_, ty)) if !ty.eq_ignore_ascii_case(sqlite_type(col.ty)?) => {
					return Err(Error::internal(format!(
						"{}.{} is {ty} in the database but {} in the declaration",
						t.name,
						col.name,
						col.ty.keyword()
					)));
				}
				Some(_) => {}
				None => {
					let col = sqlite_col(col)?;
					ddl(conn, format!("ALTER TABLE \"{}\" ADD COLUMN {col}", t.name)).await?;
				}
			}
		}

		let mut wanted = BTreeSet::new();
		for idx in &t.indexes {
			// `:` and `,` cannot occur in an identifier, so unlike `idx_{t}_{cols}` two
			// declarations never map to one name (`a_b(c)` against `a(b_c)`).
			let name = format!("ix:{}:{}", t.name, idx.join(","));
			let cols = idx.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
			let sql = format!("CREATE INDEX IF NOT EXISTS \"{name}\" ON \"{}\" ({cols})", t.name);
			ddl(conn, sql).await?;
			wanted.insert(name);
		}
		// The withdrawal half: an index the declaration no longer lists goes. Every explicit index
		// on a script table is reconcile's own: `valid_table_name` keeps script names off the
		// framework modules' tables.
		let existing: Vec<(String,)> = sqlx::query_as(
			"SELECT name FROM sqlite_master
			  WHERE type = 'index' AND tbl_name = ? AND name NOT LIKE 'sqlite_autoindex_%'",
		)
		.bind(&t.name)
		.fetch_all(&mut *conn)
		.await
		.db()?;
		for (name,) in existing.iter().filter(|(n,)| !wanted.contains(n)) {
			ddl(conn, format!("DROP INDEX \"{name}\"")).await?;
		}
	}
	Ok(())
}

// vim: ts=4
