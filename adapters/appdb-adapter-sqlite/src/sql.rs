//! What a script's statement is allowed to be, how its arguments bind, how a row crosses back,
//! and the startup reconcile of the declared tables.

use std::collections::BTreeSet;

use saas_core::error::{ClResult, Error};
use saas_script::{ColDef, TableDef};
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
		return Err(saas_script::error::db("a db:: call takes one statement"));
	}
	let first = sql.split_whitespace().next().unwrap_or_default().to_uppercase();
	if kinds.contains(&first.as_str()) {
		return Ok(());
	}
	Err(saas_script::error::db(format!("a db:: statement must start with one of {kinds:?}")))
}

/// The statement with its arguments bound. `saas_script::db` has already refused a float and an
/// amount; this refuses the rest, because `AppDb` is a public trait with other callers.
pub(crate) fn bound<'a>(
	sql: &str,
	args: &'a [Json],
) -> ClResult<Query<'a, Sqlite, SqliteArguments>> {
	let mut q = sqlx::query(AssertSqlSafe(sql.to_owned()));
	for (i, a) in args.iter().enumerate() {
		let bad = || saas_script::error::db(format!("argument {i} cannot bind to a column"));
		q = match a {
			Json::Null => q.bind(None::<i64>),
			Json::Bool(b) => q.bind(i64::from(*b)),
			Json::Number(n) => q.bind(n.as_i64().ok_or_else(bad)?),
			Json::String(s) => q.bind(s.as_str()),
			_ => return Err(bad()),
		};
	}
	Ok(q)
}

/// One row as a JSON object, keyed by column name.
///
/// `REAL` and `BLOB` are refused rather than mapped, so a float cannot enter a script through an
/// `AVG()` either — the no-floats rule, held at the one door `db::query` opens.
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
			ty => {
				return Err(saas_script::error::db(format!(
					"column '{}' is {ty}; only NULL, INTEGER and TEXT cross into script",
					col.name()
				)));
			}
		};
		out.insert(col.name().to_owned(), value);
	}
	Ok(Json::Object(out))
}

/// One statement on the reconcile connection. The text is composed from tokens
/// `saas_script::db` has already validated, never from a declaration file verbatim.
async fn ddl(conn: &mut SqliteConnection, sql: String) -> ClResult<()> {
	sqlx::query(AssertSqlSafe(sql)).execute(conn).await.db()?;
	Ok(())
}

/// Brings the database up to the declarations, in one transaction. **Additive**: it never drops a
/// table and never drops a column, because a declaration file people edit casually must not be
/// able to lose data. A table that is no longer declared is left alone.
pub(crate) async fn reconcile(writer: &SqlitePool, tables: &[TableDef]) -> ClResult<()> {
	let mut conn = writer.acquire().await.db()?;
	sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await.db()?;
	// Either failure leaves the transaction open, so the connection is closed, not re-pooled.
	match apply(&mut conn, tables).await {
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

async fn apply(conn: &mut SqliteConnection, tables: &[TableDef]) -> ClResult<()> {
	for t in tables {
		let cols = t.cols.iter().map(ColDef::fragment).collect::<Vec<_>>().join(", ");
		ddl(conn, format!("CREATE TABLE IF NOT EXISTS {} ({cols})", t.name)).await?;

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
				Some((_, ty)) if !ty.eq_ignore_ascii_case(col.ty.keyword()) => {
					return Err(Error::internal(format!(
						"{}.{} is {ty} in the database but {} in the declaration",
						t.name,
						col.name,
						col.ty.keyword()
					)));
				}
				Some(_) => {}
				None => {
					ddl(conn, format!("ALTER TABLE {} ADD COLUMN {}", t.name, col.fragment()))
						.await?;
				}
			}
		}

		let mut wanted = BTreeSet::new();
		for idx in &t.indexes {
			// `:` and `,` cannot occur in an identifier, so unlike `idx_{t}_{cols}` two
			// declarations never map to one name (`a_b(c)` against `a(b_c)`).
			let name = format!("ix:{}:{}", t.name, idx.join(","));
			ddl(
				conn,
				format!("CREATE INDEX IF NOT EXISTS \"{name}\" ON {} ({})", t.name, idx.join(", ")),
			)
			.await?;
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
