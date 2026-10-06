// SPDX-License-Identifier: MPL-2.0
//! What a script's statement is allowed to be, how its arguments bind, how a row crosses back,
//! and the startup reconcile of the declared tables.

use std::{collections::BTreeSet, str::FromStr};

use mintworks_core::error::{ClResult, Error};
use mintworks_script::{
	TableDef,
	db::{ColDef, ColType, Migration},
};
use serde_json::Value as Json;
use sqlx::{
	AssertSqlSafe, Column, Connection, Decode, Executor, PgConnection, PgPool, Postgres, Row,
	SqlSafeStr, Statement, Type, TypeInfo, ValueRef,
	postgres::{PgArguments, PgRow, PgStatement},
	query::Query,
	types::{
		BigDecimal, Uuid,
		time::{Date, OffsetDateTime, UtcOffset},
	},
};
use time::{
	format_description::{BorrowedFormatItem, well_known::Rfc3339},
	macros::format_description,
};

use crate::util::DbExt;

/// A `date` crosses as `'YYYY-MM-DD'` both ways.
const DATE: &[BorrowedFormatItem<'_>] = format_description!("[year]-[month]-[day]");

/// What `query` and `exec` each accept as a statement's first keyword.
///
/// DDL belongs to `app.table`, transaction control to `db::tx`; `SET`, `RESET`, `DISCARD`,
/// `LISTEN`, `COPY`, `DO` and `CALL` would change or escape the pooled session, so they are
/// refused by not being listed.
///
/// One statement only: a `;` anywhere but trailing is refused, including inside a literal —
/// values bind as `$n` arguments.
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

/// Prepares `sql` on `conn`, so [`bound`] can read the parameter types PostgreSQL inferred. The
/// statement is cached per connection, so running it afterwards parses nothing again.
pub(crate) async fn prepare(conn: &mut PgConnection, sql: &str) -> ClResult<PgStatement> {
	conn.prepare(AssertSqlSafe(sql.to_owned()).into_sql_str()).await.db()
}

/// `stmt` with each argument coerced to its inferred parameter type — so a script binds a uuid
/// string to a `uuid` or `'2026-01-01'` to a `date` without writing casts. `mintworks_script::db`
/// has already refused an amount and a non-finite float; this refuses the rest, because `AppDb` is
/// a public trait with other callers.
pub(crate) fn bound<'s>(
	stmt: &'s PgStatement,
	args: &[Json],
) -> ClResult<Query<'s, Postgres, PgArguments>> {
	let types = stmt.parameters().and_then(sqlx::Either::left).unwrap_or_default();
	if types.len() != args.len() {
		return Err(mintworks_script::error::db(format!(
			"the statement takes {} arguments, {} given",
			types.len(),
			args.len()
		)));
	}
	let mut q = stmt.query();
	for (i, (a, ty)) in args.iter().zip(types).enumerate() {
		q = coerce(q, i, a, ty.name())?;
	}
	Ok(q)
}

/// One argument, bound as the exact Rust type of parameter type `ty`: the statement is already
/// prepared, so the binary encoding has to match what PostgreSQL inferred.
fn coerce<'s>(
	q: Query<'s, Postgres, PgArguments>,
	i: usize,
	a: &Json,
	ty: &str,
) -> ClResult<Query<'s, Postgres, PgArguments>> {
	let bad =
		|| mintworks_script::error::db(format!("argument {i} cannot bind to a {ty} parameter"));
	if a.is_null() {
		// A NULL is sent without bytes, so its Rust type never reaches the server.
		return Ok(q.bind(None::<i64>));
	}
	let str_of = || a.as_str().ok_or_else(bad);
	Ok(match ty {
		"INT2" => q.bind(a.as_i64().and_then(|n| i16::try_from(n).ok()).ok_or_else(bad)?),
		"INT4" => q.bind(a.as_i64().and_then(|n| i32::try_from(n).ok()).ok_or_else(bad)?),
		"INT8" => q.bind(a.as_i64().ok_or_else(bad)?),
		// `float4` is lossy by declaration.
		#[allow(clippy::cast_possible_truncation)]
		"FLOAT4" => q.bind(finite(a).ok_or_else(bad)? as f32),
		"FLOAT8" => q.bind(finite(a).ok_or_else(bad)?),
		"NUMERIC" => q.bind(numeric(a).ok_or_else(bad)?),
		"BOOL" => q.bind(a.as_bool().ok_or_else(bad)?),
		"TEXT" | "VARCHAR" | "BPCHAR" | "NAME" => q.bind(str_of()?.to_owned()),
		"UUID" => q.bind(Uuid::parse_str(str_of()?).map_err(|_| bad())?),
		"TIMESTAMPTZ" => q.bind(OffsetDateTime::parse(str_of()?, &Rfc3339).map_err(|_| bad())?),
		"DATE" => q.bind(Date::parse(str_of()?, DATE).map_err(|_| bad())?),
		"JSON" | "JSONB" => q.bind(sqlx::types::Json(a.clone())),
		"TEXT[]" | "VARCHAR[]" => {
			q.bind(array(a, |v| v.as_str().map(str::to_owned)).ok_or_else(bad)?)
		}
		"INT4[]" => {
			q.bind(array(a, |v| v.as_i64().and_then(|n| i32::try_from(n).ok())).ok_or_else(bad)?)
		}
		"INT8[]" => q.bind(array(a, Json::as_i64).ok_or_else(bad)?),
		"UUID[]" => {
			q.bind(array(a, |v| v.as_str().and_then(|s| Uuid::parse_str(s).ok())).ok_or_else(bad)?)
		}
		_ => return Err(bad()),
	})
}

fn finite(a: &Json) -> Option<f64> {
	a.as_f64().filter(|f| f.is_finite())
}

/// A string is the exact form; a number goes through its JSON text, which `serde_json` prints
/// without exponent loss for an integer and shortest-round-trip for a float.
fn numeric(a: &Json) -> Option<BigDecimal> {
	match a {
		Json::String(s) => BigDecimal::from_str(s).ok(),
		Json::Number(n) => BigDecimal::from_str(&n.to_string()).ok(),
		_ => None,
	}
}

/// A JSON array as a PostgreSQL array; a `null` element stays NULL.
fn array<T>(a: &Json, each: impl Fn(&Json) -> Option<T>) -> Option<Vec<Option<T>>> {
	a.as_array()?
		.iter()
		.map(|v| if v.is_null() { Some(None) } else { each(v).map(Some) })
		.collect()
}

/// One row as a JSON object, keyed by column name.
///
/// `numeric` crosses as a **string** — a float would lose the exact value a script stored.
/// `bytea` is refused rather than mapped: a script has no byte type to receive it.
pub(crate) fn row_json(row: &PgRow) -> ClResult<Json> {
	let mut out = serde_json::Map::new();
	for (i, col) in row.columns().iter().enumerate() {
		let raw = row.try_get_raw(i).map_err(|e| crate::util::map_db(&e))?;
		let value = if raw.is_null() {
			Json::Null
		} else {
			cell(row, i, col.name(), col.type_info().name())?
		};
		out.insert(col.name().to_owned(), value);
	}
	Ok(Json::Object(out))
}

fn cell(row: &PgRow, i: usize, name: &str, ty: &str) -> ClResult<Json> {
	let refused =
		|| mintworks_script::error::db(format!("column '{name}' is {ty}; it has no script value"));
	let fmt = |e: time::error::Format| Error::internal(format!("column '{name}': {e}"));
	Ok(match ty {
		"INT2" => get::<i16>(row, i)?.into(),
		"INT4" => get::<i32>(row, i)?.into(),
		"INT8" => get::<i64>(row, i)?.into(),
		"FLOAT4" => serde_json::Number::from_f64(get::<f32>(row, i)?.into())
			.ok_or_else(refused)?
			.into(),
		"FLOAT8" => serde_json::Number::from_f64(get::<f64>(row, i)?).ok_or_else(refused)?.into(),
		"NUMERIC" => numeric_cell(row, i)?.to_plain_string().into(),
		"BOOL" => get::<bool>(row, i)?.into(),
		"TEXT" | "VARCHAR" | "BPCHAR" | "NAME" => get::<String>(row, i)?.into(),
		"UUID" => get::<Uuid>(row, i)?.to_string().into(),
		"TIMESTAMPTZ" => get::<OffsetDateTime>(row, i)?
			.to_offset(UtcOffset::UTC)
			.format(&Rfc3339)
			.map_err(fmt)?
			.into(),
		"DATE" => get::<Date>(row, i)?.format(DATE).map_err(fmt)?.into(),
		"JSON" | "JSONB" => get::<sqlx::types::Json<Json>>(row, i)?.0,
		"TEXT[]" | "VARCHAR[]" => get::<Vec<Option<String>>>(row, i)?.into(),
		"INT4[]" => get::<Vec<Option<i32>>>(row, i)?.into(),
		"INT8[]" => get::<Vec<Option<i64>>>(row, i)?.into(),
		"UUID[]" => get::<Vec<Option<Uuid>>>(row, i)?
			.into_iter()
			.map(|u| u.map(|u| u.to_string()))
			.collect::<Vec<_>>()
			.into(),
		_ => return Err(refused()),
	})
}

/// sqlx 0.9 decodes `numeric` with a scale rounded up to its base-10000 digit groups
/// (`12.50` → `12.5000`); the wire header's `dscale` (bytes 6..8) is the scale stored.
fn numeric_cell(row: &PgRow, i: usize) -> ClResult<BigDecimal> {
	let raw = row.try_get_raw(i).map_err(|e| crate::util::map_db(&e))?;
	let dscale = raw
		.as_bytes()
		.ok()
		.and_then(|b| b.get(6..8))
		.map(|b| i64::from(u16::from_be_bytes([b[0], b[1]])));
	let n = get::<BigDecimal>(row, i)?;
	Ok(match dscale {
		Some(scale) => n.with_scale(scale),
		None => n,
	})
}

fn get<'r, T: Decode<'r, Postgres> + Type<Postgres>>(row: &'r PgRow, i: usize) -> ClResult<T> {
	row.try_get(i).map_err(|e| crate::util::map_db(&e))
}

/// One statement on the reconcile connection. The text is composed from tokens
/// `mintworks_script::db` has already validated, never from a declaration file verbatim
/// (`app.migration`'s raw DDL goes through `migrate::app_chain` instead).
async fn ddl(conn: &mut PgConnection, sql: String) -> ClResult<()> {
	sqlx::query(AssertSqlSafe(sql)).execute(conn).await.db()?;
	Ok(())
}

/// Brings the database up to the declarations, in one transaction: the `app.migration` chain,
/// then the tables. **Additive**: it never drops a table and never drops a column, because a
/// declaration file people edit casually must not be able to lose data. A table that is no
/// longer declared is left alone.
pub(crate) async fn reconcile(
	writer: &PgPool,
	migrations: &[Migration],
	tables: &[TableDef],
) -> ClResult<()> {
	let mut conn = writer.acquire().await.db()?;
	// Dropped uncommitted on an error, which rolls it back.
	let mut tx = conn.begin().await.db()?;
	crate::migrate::app_chain(&mut tx, migrations).await?;
	apply(&mut tx, tables).await?;
	tx.commit().await.db()
}

/// PostgreSQL's DDL spelling of a column type, and the `udt_name` `information_schema` reports
/// for it — what the reconcile's type check compares.
fn pg_type(ty: ColType) -> (&'static str, &'static str) {
	match ty {
		ColType::Int => ("BIGINT", "int8"),
		ColType::Text => ("TEXT", "text"),
		ColType::Blob => ("BYTEA", "bytea"),
		ColType::Real => ("DOUBLE PRECISION", "float8"),
		ColType::Bool => ("BOOLEAN", "bool"),
		ColType::Numeric => ("NUMERIC", "numeric"),
		ColType::Uuid => ("UUID", "uuid"),
		ColType::Timestamptz => ("TIMESTAMPTZ", "timestamptz"),
		ColType::Date => ("DATE", "date"),
		ColType::Jsonb => ("JSONB", "jsonb"),
		ColType::TextArray => ("TEXT[]", "_text"),
		ColType::IntArray => ("BIGINT[]", "_int8"),
		ColType::UuidArray => ("UUID[]", "_uuid"),
	}
}

/// An `INTEGER PRIMARY KEY` numbers itself, as SQLite's rowid alias does.
fn pg_col(col: &ColDef) -> String {
	let (ty, _) = pg_type(col.ty);
	if col.pk && col.ty == ColType::Int {
		return col.fragment(&format!("{ty} GENERATED BY DEFAULT AS IDENTITY"));
	}
	col.fragment(ty)
}

async fn apply(conn: &mut PgConnection, tables: &[TableDef]) -> ClResult<()> {
	for t in tables {
		let cols = t.cols.iter().map(pg_col).collect::<Vec<_>>().join(", ");
		ddl(conn, format!("CREATE TABLE IF NOT EXISTS \"{}\" ({cols})", t.name)).await?;

		let have: Vec<(String, String)> = sqlx::query_as(
			"SELECT column_name::text, udt_name::text FROM information_schema.columns
			  WHERE table_schema = current_schema() AND table_name = $1",
		)
		.bind(&t.name)
		.fetch_all(&mut *conn)
		.await
		.db()?;
		for col in &t.cols {
			let (_, udt) = pg_type(col.ty);
			match have.iter().find(|(name, _)| *name == col.name) {
				// A silent skip would leave a live database in a shape no fresh install reaches —
				// the same reasoning as the framework's schema-version stamp.
				Some((_, ty)) if ty != udt => {
					return Err(Error::internal(format!(
						"{}.{} is {ty} in the database but {} in the declaration",
						t.name,
						col.name,
						col.ty.keyword()
					)));
				}
				Some(_) => {}
				None => {
					ddl(conn, format!("ALTER TABLE \"{}\" ADD COLUMN {}", t.name, pg_col(col)))
						.await?;
				}
			}
		}

		let mut wanted = BTreeSet::new();
		for idx in &t.indexes {
			// `:` and `,` cannot occur in an identifier, so two declarations never map to one name.
			let name = format!("ix:{}:{}", t.name, idx.join(","));
			// PostgreSQL truncates a longer name silently, and the withdrawal pass below would
			// then drop and recreate the index on every boot.
			if name.len() > 63 {
				return Err(Error::internal(format!(
					"index {name} is longer than PostgreSQL's 63-byte identifier limit"
				)));
			}
			let cols = idx.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
			let sql = format!("CREATE INDEX IF NOT EXISTS \"{name}\" ON \"{}\" ({cols})", t.name);
			ddl(conn, sql).await?;
			wanted.insert(name);
		}
		// The withdrawal half: an `ix:` index the declaration no longer lists goes. The prefix
		// leaves the primary key's and any migration-made index alone.
		let existing: Vec<(String,)> = sqlx::query_as(
			"SELECT indexname::text FROM pg_indexes
			  WHERE schemaname = current_schema() AND tablename = $1 AND indexname LIKE 'ix:%'",
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
