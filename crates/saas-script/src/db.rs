//! `db::query` / `db::exec` / `db::tx` — raw SQL against the **script's own database**.
//!
//! The escape hatch from [`crate::objects`], for the join, the aggregate and the compound index
//! a JSON body plus indexed paths cannot do. It is **profile-gated the way `fs` and `http` are**:
//! [`crate::ScriptApp::context`] installs this module only when the application registered a
//! [`crate::AppDb`], so a future org-level profile leaves `db::` an unresolved item at compile
//! time rather than a permission check at runtime.
//!
//! A statement runs against a **separate database file** holding nothing but the tables
//! `app.table` declared: no framework table is reachable from here. That is also why `db::tx`
//! exists and is independent of `tx::with` — two databases, two transactions, neither rolling
//! the other back. The lock order is **script first**: `tx::with` may run inside `db::tx`, and a
//! `db::exec`/`db::tx` inside `tx::with` is refused — the reverse order deadlocks the two writers.

use std::time::Duration;

use rune::{ContextError, Module, Value, runtime::Function, runtime::Ref};
use saas_core::App;
use serde_json::Value as Json;

use crate::{
	ScriptRuntime,
	ctx::ScriptCtx,
	error::{self, R},
	tx,
	value::{ScriptError, from_json, to_json},
};

fn db_err(msg: impl Into<String>) -> ScriptError {
	ScriptError(error::db(msg))
}

/// `db::query` stays allowed: it takes no write lock — a reader, or the open `db::tx` block's
/// writer under `query_only`.
fn refuse_in_framework_block() -> R<()> {
	if tx::in_framework_block() {
		return Err(db_err("db:: writes are refused inside tx::with: open db::tx outside it"));
	}
	Ok(())
}

/// The adapter the application registered, or the error naming what is missing.
fn app_db(app: &App) -> R<std::sync::Arc<dyn crate::AppDb>> {
	let rt = app
		.extensions
		.get::<ScriptRuntime>()
		.ok_or_else(|| ScriptError(error::runtime("no ScriptRuntime extension is registered")))?;
	rt.db
		.clone()
		.ok_or_else(|| db_err("db:: needs an AppDb and the application registered none"))
}

/// `db::query(ctx, sql, args)` — an array of objects, one per row, keyed by column name.
///
/// `Ref<str>` rather than `String`: a `String` parameter is *taken* from the caller's value, so
/// a local used twice reads empty on its second use.
#[rune::function]
pub async fn query(c: Ref<ScriptCtx>, sql: Ref<str>, args: Value) -> R<Value> {
	let app = c.app()?.clone();
	let db = app_db(&app)?;
	let (stmt, args) = (sql.to_owned(), binds(&args)?);
	// Nothing borrowed from the VM may be alive across an await: `rune::Ref` is not `Send`.
	drop(sql);
	drop(c);
	let max = app.settings.int("script.db_max_rows").await.map_err(ScriptError)?;
	let max = usize::try_from(max).unwrap_or(usize::MAX);
	let rows = db.query(&stmt, &args, max).await.map_err(ScriptError)?;
	from_json(&Json::Array(rows)).map_err(ScriptError)
}

/// `db::exec(ctx, sql, args)` — rows affected.
#[rune::function]
pub async fn exec(c: Ref<ScriptCtx>, sql: Ref<str>, args: Value) -> R<i64> {
	refuse_in_framework_block()?;
	let db = app_db(c.app()?)?;
	let (stmt, args) = (sql.to_owned(), binds(&args)?);
	drop(sql);
	drop(c);
	let n = db.exec(&stmt, &args).await.map_err(ScriptError)?;
	Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

/// `db::tx(ctx, closure)` — one transaction on the **script** database.
///
/// Independent of `tx::with`: each block covers its own database, so a framework rollback does
/// not undo what this one wrote, and this one rolling back does not undo a framework write.
/// Script transaction outside: `tx::with` (or a service call that writes) may run in here, while
/// `db::tx` inside `tx::with` is refused — the reverse order is ABBA on two writers.
#[rune::function]
pub async fn tx(c: Ref<ScriptCtx>, body: Function) -> R<Value> {
	refuse_in_framework_block()?;
	let app = c.app()?.clone();
	let inner = (*c).clone();
	drop(c);
	let db = app_db(&app)?;

	// The same knob as `tx::with` and for the same reason: the block holds the script database's
	// only writer connection, so every other write to it waits out this number.
	let ms = app.settings.int("script.tx_timeout_ms").await.map_err(ScriptError)?;
	let deadline = Duration::from_millis(u64::try_from(ms).unwrap_or(tx::DEFAULT_TX_TIMEOUT_MS));

	// The deadline goes on the body, never around `transaction`: a timeout outside it would
	// cancel the commit. Elapsed, the body resolves to an error and the adapter rolls back.
	let called = body.async_send_call::<_, tx::Bridged>((inner,));
	let fut: crate::TxBody<'_> = Box::pin(async move {
		let out = match tokio::time::timeout(deadline, tx::scope_open(called)).await {
			Err(_) => {
				Err(tx::coded(tx::E_TX_TIMEOUT, "the db::tx block outran script.tx_timeout_ms").0)
			}
			Ok(vm) => match vm.into_result() {
				Err(e) => Err(error::runtime(e.to_string())),
				Ok(tx::Bridged(v)) => v,
			},
		};
		tx::committing(out.is_ok());
		out
	});

	let json = db.transaction(fut).await;
	tx::committing(false);
	let json = json.map_err(ScriptError)?;
	from_json(&json).map_err(ScriptError)
}

/// `db::org(ctx)` — the internal org key, for the scoping column.
///
/// The script database is **one file for the whole deployment**, not one per org, so putting this
/// in a `WHERE` is the script's own job — nothing below does it.
///
/// Here and not on `ctx` deliberately: it is a row key, never a public id, and putting it on the
/// handle would make it one keystroke from a response body (`ctx.rs` says the same of `org()`).
#[rune::function]
pub fn org(c: Ref<ScriptCtx>) -> R<i64> {
	c.ctx().org().map_err(ScriptError)
}

/// The bind list, checked element by element.
///
/// A JSON float can never reach a column (the no-floats rule), and a `Money` arrives as its wire
/// object, which is a mistake with a fix rather than a type error.
fn binds(args: &Value) -> R<Vec<Json>> {
	let Json::Array(items) = to_json(args).map_err(ScriptError)? else {
		return Err(db_err("the db:: argument list must be an array"));
	};
	for (i, v) in items.iter().enumerate() {
		match v {
			Json::Null | Json::Bool(_) | Json::String(_) => {}
			Json::Number(n) if n.is_i64() => {}
			Json::Object(o) if o.contains_key("amount") && o.contains_key("currency") => {
				return Err(db_err(format!("argument {i} is an amount; bind `m.minor()` instead")));
			}
			_ => {
				return Err(db_err(format!(
					"argument {i} is not null, a bool, an integer or a string"
				)));
			}
		}
	}
	Ok(items)
}

// ----------------------------------------------------------------- declarations

/// A column's storage class. No `Real`: the no-floats rule, made structural.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColType {
	Int,
	Text,
	Blob,
}

impl ColType {
	#[must_use]
	pub fn keyword(self) -> &'static str {
		match self {
			Self::Int => "INTEGER",
			Self::Text => "TEXT",
			Self::Blob => "BLOB",
		}
	}
}

/// One declared column of a script table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColDef {
	pub name: String,
	pub ty: ColType,
	pub not_null: bool,
	pub pk: bool,
	pub default: Option<String>,
}

impl ColDef {
	/// The column's DDL fragment, composed from tokens [`parse_col`] has already validated —
	/// which is what keeps a declaration file's text out of a statement unchecked.
	#[must_use]
	pub fn fragment(&self) -> String {
		let mut s = format!("{} {}", self.name, self.ty.keyword());
		if self.pk {
			s.push_str(" PRIMARY KEY");
		}
		if self.not_null {
			s.push_str(" NOT NULL");
		}
		if let Some(d) = &self.default {
			s.push_str(" DEFAULT ");
			s.push_str(d);
		}
		s
	}
}

/// One `app.table(…)` declaration, reconciled at startup by [`crate::AppDb::reconcile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableDef {
	pub name: String,
	/// Sorted by name: a Rune object has no reliable key order, and the DDL has to be the same
	/// on every boot.
	pub cols: Vec<ColDef>,
	pub indexes: Vec<Vec<String>>,
}

impl TableDef {
	/// `#{ cols: #{ name: "<spec>", … }, indexes: [["a", "b"]] }`, as `app.table` hands it over.
	///
	/// # Errors
	/// The declaration error, ready to go on `Decls::errors`.
	pub fn parse(name: &str, decl: &Json) -> Result<Self, String> {
		if !valid_table_name(name) {
			return Err(format!(
				"table '{name}': a script table is named with a lowercase identifier, at most 41 \
				 characters, not starting with sqlite_"
			));
		}
		let cols = decl
			.get("cols")
			.and_then(Json::as_object)
			.ok_or_else(|| format!("table '{name}': `cols` must be an object"))?;
		if cols.is_empty() {
			return Err(format!("table '{name}': `cols` is empty"));
		}
		let mut parsed = Vec::new();
		for (col, spec) in cols {
			if !valid_ident(col) {
				return Err(format!(
					"table '{name}': column '{col}' is not a lowercase identifier"
				));
			}
			let spec = spec
				.as_str()
				.ok_or_else(|| format!("table '{name}': column '{col}' is not a string"))?;
			let (ty, not_null, pk, default) =
				parse_col(spec).map_err(|e| format!("table '{name}', column '{col}': {e}"))?;
			parsed.push(ColDef { name: col.clone(), ty, not_null, pk, default });
		}
		parsed.sort_by(|a, b| a.name.cmp(&b.name));

		let mut indexes = Vec::new();
		for idx in decl.get("indexes").and_then(Json::as_array).into_iter().flatten() {
			let cols = idx
				.as_array()
				.ok_or_else(|| format!("table '{name}': an index is not an array of columns"))?;
			if cols.is_empty() {
				return Err(format!("table '{name}': an index names no column"));
			}
			let mut names = Vec::new();
			for c in cols {
				let c = c
					.as_str()
					.ok_or_else(|| format!("table '{name}': an index column is not a string"))?;
				if !parsed.iter().any(|p| p.name == c) {
					return Err(format!("table '{name}': index column '{c}' is not declared"));
				}
				names.push(c.to_owned());
			}
			indexes.push(names);
		}
		Ok(Self { name: name.to_owned(), cols: parsed, indexes })
	}
}

/// `^[a-z][a-z0-9_]{0,40}$`, minus SQLite's own `sqlite_` namespace and the framework's: its
/// content modules (`agent_*`, `memory_*`, the runner's `schema_version`) share the app DB, and
/// reconcile would `ALTER` those tables and drop their indexes.
#[must_use]
pub fn valid_table_name(name: &str) -> bool {
	valid_ident(name)
		&& !["sqlite_", "agent_", "memory_"].iter().any(|p| name.starts_with(p))
		&& name != "schema_version"
}

fn valid_ident(s: &str) -> bool {
	!s.is_empty()
		&& s.len() <= 41
		&& s.starts_with(|c: char| c.is_ascii_lowercase())
		&& s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A column spec: `INTEGER | TEXT | BLOB`, then any of `NOT NULL`, `PRIMARY KEY`,
/// `DEFAULT <integer | 'quoted' | NULL>`.
///
/// # Errors
/// The reason the spec was refused, without the table or column name — the caller adds those.
pub fn parse_col(spec: &str) -> Result<(ColType, bool, bool, Option<String>), String> {
	let mut toks = spec.split_whitespace();
	let ty = match toks.next() {
		Some("INTEGER") => ColType::Int,
		Some("TEXT") => ColType::Text,
		Some("BLOB") => ColType::Blob,
		other => {
			return Err(format!(
				"the type must be INTEGER, TEXT or BLOB, not '{}'",
				other.unwrap_or_default()
			));
		}
	};
	let (mut not_null, mut pk, mut default) = (false, false, None);
	while let Some(tok) = toks.next() {
		match (tok, toks.next()) {
			("NOT", Some("NULL")) => not_null = true,
			("PRIMARY", Some("KEY")) => pk = true,
			("DEFAULT", Some(v)) => default = Some(literal(v)?),
			(a, b) => {
				return Err(format!("unexpected '{a} {}'", b.unwrap_or_default()));
			}
		}
	}
	Ok((ty, not_null, pk, default))
}

/// A `DEFAULT` value: an integer, `NULL`, or a single-quoted literal with no quote inside.
fn literal(tok: &str) -> Result<String, String> {
	let quoted = tok
		.strip_prefix('\'')
		.and_then(|t| t.strip_suffix('\''))
		.is_some_and(|v| !v.contains('\''));
	if tok == "NULL" || tok.parse::<i64>().is_ok() || quoted {
		return Ok(tok.to_owned());
	}
	Err(format!("DEFAULT '{tok}' must be an integer, a 'quoted' literal or NULL"))
}

/// Registers `db::`. Installed **only** when the application registered a [`crate::AppDb`].
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["db"])?;
	m.function_meta(query)?;
	m.function_meta(exec)?;
	m.function_meta(org)?;
	m.function_meta(tx)?;
	Ok(m)
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn a_column_spec_composes_from_known_tokens() {
		assert_eq!(parse_col("INTEGER NOT NULL").unwrap(), (ColType::Int, true, false, None));
		assert_eq!(parse_col("TEXT PRIMARY KEY").unwrap(), (ColType::Text, false, true, None));
		assert_eq!(
			parse_col("INTEGER NOT NULL DEFAULT 0").unwrap(),
			(ColType::Int, true, false, Some("0".to_string()))
		);
		assert_eq!(parse_col("BLOB").unwrap().0, ColType::Blob);
	}

	#[test]
	fn a_column_spec_refuses_anything_else() {
		// REAL is unreachable by construction, not by a check somewhere downstream.
		assert!(parse_col("REAL").is_err());
		assert!(parse_col("TEXT; DROP TABLE ledger").is_err());
		assert!(parse_col("VARCHAR(20)").is_err());
		assert!(parse_col("TEXT DEFAULT 1.5").is_err());
		assert!(parse_col("TEXT UNIQUE").is_err());
		assert!(parse_col("").is_err());
	}

	#[test]
	fn a_table_name_is_a_plain_lowercase_identifier() {
		assert!(valid_table_name("ledger"));
		assert!(!valid_table_name("sqlite_x"));
		assert!(!valid_table_name("agent_threads"));
		assert!(!valid_table_name("memory_fts_data"));
		assert!(!valid_table_name("schema_version"));
		assert!(valid_table_name("agents"));
		assert!(!valid_table_name("Ledger"));
		assert!(!valid_table_name("x; --"));
		assert!(!valid_table_name(""));
	}

	#[test]
	fn a_declaration_sorts_its_columns_and_checks_its_indexes() {
		let decl = json!({
			"cols": { "uid": "TEXT PRIMARY KEY", "org_id": "INTEGER NOT NULL" },
			"indexes": [["org_id"]],
		});
		let def = TableDef::parse("ledger", &decl).unwrap();
		assert_eq!(def.cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["org_id", "uid"]);
		assert_eq!(def.cols[1].fragment(), "uid TEXT PRIMARY KEY");

		let bad = json!({ "cols": { "uid": "TEXT" }, "indexes": [["day"]] });
		assert!(TableDef::parse("ledger", &bad).is_err());
		assert!(TableDef::parse("ledger", &json!({})).is_err());
	}
}

// vim: ts=4
