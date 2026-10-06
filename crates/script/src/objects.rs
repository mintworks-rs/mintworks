//! The org-scoped object store as Rune functions.
//!
//! The acting org comes from the `Ctx` and is **never** a parameter, which is what lets a future
//! org-level script profile inherit the scoping for free.
//!
//! [`mintworks_core::objects::ObjectStore`] checks no authorization and writes no audit row, and
//! says so: the layering rule puts both in the caller. This module is that caller — `org_id` comes
//! from `ctx.actor` alone, and every `create`, `put` and `delete` writes an audit row, because an
//! `invoice.ext` body stays writable after the invoice is `ISSUED` and would otherwise leave no
//! trace at all.

use mintworks_core::{
	App, Ctx,
	objects::{Object, ObjectType},
};
use mintworks_invoice::service_api::MAX_PAGE_LIMIT;
use rune::{ContextError, Module, Value, runtime::Ref};
use serde_json::{Value as Json, json};

use crate::{
	ScriptRuntime,
	ctx::ScriptCtx,
	error::{self, R, bad},
	value::{ScriptError, from_json, to_json},
};

/// One declared object type: what [`ObjectStore::object_index_reconcile`] takes, plus the uid
/// prefix [`create`] mints from.
///
/// The prefix is not part of [`ObjectType`] because the store has no opinion about keys — a key
/// is either a minted prefixed ULID or an external id the author chose.
///
/// [`ObjectStore::object_index_reconcile`]:
/// mintworks_core::objects::ObjectStore::object_index_reconcile
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectTypeDef {
	pub type_name: String,
	/// `Some("prj")` makes `objects::create` mint `prj_01J…`. `None` for an ext type, whose key
	/// is the framework entity's own uid and which therefore has no `create`.
	pub prefix: Option<String>,
	pub paths: Vec<String>,
}

impl ObjectTypeDef {
	#[must_use]
	pub fn new(type_name: impl Into<String>) -> Self {
		Self { type_name: type_name.into(), prefix: None, paths: Vec::new() }
	}

	/// The uid prefix `create` mints from, without the underscore: `"prj"` → `prj_01J…`.
	#[must_use]
	pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
		self.prefix = Some(prefix.into());
		self
	}

	/// A JSON path `query` may match on, in SQLite's spelling: `$.projectUid`.
	#[must_use]
	pub fn path(mut self, path: impl Into<String>) -> Self {
		self.paths.push(path.into());
		self
	}

	/// The declaration as the store reads it, for `object_index_reconcile`.
	#[must_use]
	pub fn declared(&self) -> ObjectType {
		ObjectType { type_name: self.type_name.clone(), paths: self.paths.clone() }
	}
}

// ----------------------------------------------------------------- plumbing

/// A Rune host function is a free `fn` that captures nothing, so everything this module needs
/// travels in the `App`'s type-map, put there by the application's builder.
fn parts(c: &Ref<ScriptCtx>) -> R<(App, Ctx, ScriptRuntime)> {
	let app = c.app()?.clone();
	let rt =
		app.extensions.get::<ScriptRuntime>().cloned().ok_or_else(|| {
			ScriptError(error::runtime("no ScriptRuntime extension is registered"))
		})?;
	Ok((app, c.ctx().clone(), rt))
}

/// An undeclared type is an error rather than an empty read: the declaration set is what drives
/// index reconciliation, so a typo would otherwise write an unindexed, unqueryable row.
fn def<'a>(rt: &'a ScriptRuntime, type_name: &str) -> R<&'a ObjectTypeDef> {
	rt.type_def(type_name)
		.ok_or_else(|| bad(format!("object type '{type_name}' was never declared")))
}

/// A uid is a known prefix plus a 26-character Crockford ULID, which an external id is not by
/// accident. A party `idOrCode` resolves uids only — `billing_parties.code` does not exist yet.
pub(crate) fn looks_like_uid(s: &str) -> bool {
	matches!(s.split_once('_'), Some((_, tail)) if tail.len() == 26
		&& tail.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()))
}

/// An ext row must be keyed by the entity's real uid, or one invoice ends up with two ext rows
/// — one under its code and one under its uid.
///
/// Resolving an external id to its uid here would need a per-entity lookup; until that exists an
/// ext key is refused rather than guessed, which cannot create the second row.
#[allow(clippy::case_sensitive_file_extension_comparisons)] // a type name, not a filename
fn ext_key(type_name: &str, key: &str) -> R<()> {
	if type_name.ends_with(".ext") && !looks_like_uid(key) {
		return Err(bad(format!(
			"'{key}' is not a uid; a '{type_name}' row must be keyed by the entity's uid"
		)));
	}
	Ok(())
}

/// `ulid` is not a dependency of this crate; the tail of a framework id is the same ULID.
fn mint(prefix: &str) -> String {
	let id = mintworks_core::ids::ServiceId::generate().into_string();
	let tail = id.split_once('_').map_or(id.as_str(), |(_, tail)| tail);
	format!("{prefix}_{tail}")
}

/// `id` is the internal row id and never crosses: it exists only to become the next `before_id`.
fn object_json(o: &Object) -> Json {
	json!({
		"uid": o.uid,
		"type": o.type_name,
		"body": o.body,
		"createdAt": o.created_at,
		"updatedAt": o.updated_at,
	})
}

/// The cursor is the last row's **uid**, resolved back to a `before_id` here — the same shape
/// `api.rs`'s listings use, and the reason no internal id reaches a script.
async fn before(rt: &ScriptRuntime, org: i64, type_name: &str, cursor: &Value) -> R<Option<i64>> {
	if cursor.clone().into_unit().is_ok() {
		return Ok(None);
	}
	let uid: String = serde_json::from_value(to_json(cursor).map_err(ScriptError)?)
		.map_err(|e| bad(format!("cursor: {e}")))?;
	match rt.objects.object_get(org, type_name, &uid).await.map_err(ScriptError)? {
		Some(o) => Ok(Some(o.id)),
		None => Err(bad("the cursor does not name an object of this type")),
	}
}

fn page(rows: &[Object], limit: i64) -> R<Value> {
	// A short page is the last one, so there is nothing to ask for after it.
	let next = if i64::try_from(rows.len()).unwrap_or(limit) < limit {
		Json::Null
	} else {
		rows.last().map_or(Json::Null, |o| Json::String(o.uid.clone()))
	};
	let items = rows.iter().map(object_json).collect::<Vec<_>>();
	from_json(&json!({ "items": items, "nextCursor": next })).map_err(ScriptError)
}

/// One spelling for every paged binding. The cursor is decided on the clamped value too — on the
/// raw limit an over-max ask ended the list early and `limit <= 0` never ended it.
pub(crate) fn clamp(limit: i64) -> i64 {
	limit.clamp(1, MAX_PAGE_LIMIT)
}

async fn write(
	app: &App,
	ctx: &Ctx,
	rt: &ScriptRuntime,
	type_name: &str,
	uid: &str,
	body: &Json,
	action: &str,
) -> R<Value> {
	let org = ctx.org().map_err(ScriptError)?;
	let paths = def(rt, type_name)?.paths.clone();
	let row = rt
		.objects
		.object_put(org, type_name, uid, body, &paths)
		.await
		.map_err(ScriptError)?;
	mintworks_core::audit::log(
		&app.store,
		ctx,
		"object",
		Some(uid),
		action,
		Some(json!({ "type": type_name })),
	)
	.await;
	from_json(&object_json(&row)).map_err(ScriptError)
}

// ----------------------------------------------------------------- objects::

/// `objects::create(ctx, type, body)` — mints the key from the type's declared uid prefix.
#[rune::function]
pub async fn create(c: Ref<ScriptCtx>, type_name: String, body: Value) -> R<Value> {
	let (app, ctx, rt) = parts(&c)?;
	let body = to_json(&body).map_err(ScriptError)?;
	drop(c);
	let Some(prefix) = def(&rt, &type_name)?.prefix.clone() else {
		return Err(bad(format!(
			"object type '{type_name}' declares no uid prefix; use objects::put with a key"
		)));
	};
	write(&app, &ctx, &rt, &type_name, &mint(&prefix), &body, "OBJECT_CREATE").await
}

/// `objects::put(ctx, type, key, body)` — upsert at a key the caller chose.
#[rune::function]
pub async fn put(c: Ref<ScriptCtx>, type_name: String, key: String, body: Value) -> R<Value> {
	let (app, ctx, rt) = parts(&c)?;
	let body = to_json(&body).map_err(ScriptError)?;
	drop(c);
	ext_key(&type_name, &key)?;
	write(&app, &ctx, &rt, &type_name, &key, &body, "OBJECT_PUT").await
}

/// `objects::get(ctx, type, key)` — the object, or `()`. `Value::from(())`, never
/// `Value::empty()`: rune's `Inline::Empty` matches no `()` arm and its serializer refuses it,
/// so a missing key 500s instead of reaching the script's `None` branch.
#[rune::function]
pub async fn get(c: Ref<ScriptCtx>, type_name: String, key: String) -> R<Value> {
	let (_, ctx, rt) = parts(&c)?;
	drop(c);
	def(&rt, &type_name)?;
	let org = ctx.org().map_err(ScriptError)?;
	match rt.objects.object_get(org, &type_name, &key).await.map_err(ScriptError)? {
		None => Ok(Value::from(())),
		Some(o) => from_json(&object_json(&o)).map_err(ScriptError),
	}
}

/// `objects::delete(ctx, type, key)` — `false` when nothing matched, so a repeat is a no-op.
#[rune::function]
pub async fn delete(c: Ref<ScriptCtx>, type_name: String, key: String) -> R<bool> {
	let (app, ctx, rt) = parts(&c)?;
	drop(c);
	def(&rt, &type_name)?;
	let org = ctx.org().map_err(ScriptError)?;
	let gone = rt.objects.object_delete(org, &type_name, &key).await.map_err(ScriptError)?;
	if gone {
		mintworks_core::audit::log(
			&app.store,
			&ctx,
			"object",
			Some(&key),
			"OBJECT_DELETE",
			Some(json!({ "type": type_name })),
		)
		.await;
	}
	Ok(gone)
}

/// `objects::list(ctx, type, cursor, limit)` -> `#{items, nextCursor}`, newest first.
#[rune::function]
pub async fn list(c: Ref<ScriptCtx>, type_name: String, cursor: Value, limit: i64) -> R<Value> {
	let (_, ctx, rt) = parts(&c)?;
	drop(c);
	def(&rt, &type_name)?;
	let org = ctx.org().map_err(ScriptError)?;
	let before = before(&rt, org, &type_name, &cursor).await?;
	let limit = clamp(limit);
	let rows = rt
		.objects
		.object_list(org, &type_name, before, limit)
		.await
		.map_err(ScriptError)?;
	page(&rows, limit)
}

/// `objects::query(ctx, type, path, value, #{cursor, limit})` — exact match on one declared path.
///
/// The paging pair is one `opts` object because rune binds host functions of at most five
/// arguments (`rune-0.14.2/src/internal_macros.rs:14-20`); six would not compile at all.
#[rune::function]
pub async fn query(
	c: Ref<ScriptCtx>,
	type_name: String,
	path: String,
	value: Value,
	opts: Value,
) -> R<Value> {
	let (cursor, limit) = query_opts(&opts)?;
	let (_, ctx, rt) = parts(&c)?;
	drop(c);
	// An undeclared path has no index rows, so the store would answer "nothing matched" and the
	// author would debug the data instead of the declaration.
	if !def(&rt, &type_name)?.paths.iter().any(|p| p == &path) {
		return Err(bad(format!("'{path}' is not a declared indexed path of '{type_name}'")));
	}
	let needle = scalar(&value)?;
	let org = ctx.org().map_err(ScriptError)?;
	let before = before(&rt, org, &type_name, &cursor).await?;
	let rows = rt
		.objects
		.object_query(org, &type_name, &path, &needle, before, limit)
		.await
		.map_err(ScriptError)?;
	page(&rows, limit)
}

/// `#{cursor, limit}`, both optional — `()` means the first page at the maximum size.
fn query_opts(v: &Value) -> R<(Value, i64)> {
	if v.clone().into_unit().is_ok() {
		return Ok((Value::from(()), clamp(i64::MAX)));
	}
	let Json::Object(m) = to_json(v).map_err(ScriptError)? else {
		return Err(bad("query opts must be an object"));
	};
	let cursor = match m.get("cursor") {
		None | Some(Json::Null) => Value::from(()),
		Some(j) => from_json(j).map_err(ScriptError)?,
	};
	let limit = match m.get("limit") {
		None | Some(Json::Null) => clamp(i64::MAX),
		Some(j) => clamp(j.as_i64().ok_or_else(|| bad("query opts.limit must be an integer"))?),
	};
	Ok((cursor, limit))
}

/// The store matches the scalar's JSON text as SQLite extracted it: a string matches bare,
/// everything else matches its JSON rendering.
fn scalar(v: &Value) -> R<String> {
	match to_json(v).map_err(ScriptError)? {
		Json::String(s) => Ok(s),
		Json::Array(_) | Json::Object(_) | Json::Null => {
			Err(bad("a query value must be a string, a number or a boolean"))
		}
		other => Ok(other.to_string()),
	}
}

/// Registers `objects::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["objects"])?;
	m.function_meta(create)?;
	m.function_meta(put)?;
	m.function_meta(get)?;
	m.function_meta(delete)?;
	m.function_meta(list)?;
	m.function_meta(query)?;
	Ok(m)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn an_ext_row_is_refused_a_non_uid_key() {
		assert!(ext_key("invoice.ext", "ACME-2026").is_err());
		assert!(ext_key("invoice.ext", "inv_01JQ8F3K9M2NP4RSTVWXYZ0AB2").is_ok());
		assert!(ext_key("project", "ACME-2026").is_ok());
	}

	#[test]
	fn a_minted_key_carries_the_declared_prefix() {
		let uid = mint("prj");
		assert!(uid.starts_with("prj_"), "{uid}");
		assert!(looks_like_uid(&uid), "{uid}");
	}

	/// The cursor is decided on the **clamped** limit, which is what the query ran with.
	#[test]
	fn the_cursor_follows_the_clamped_limit() {
		let row = |uid: &str| Object {
			id: 1,
			uid: uid.to_string(),
			type_name: "note".to_string(),
			body: json!({}),
			created_at: mintworks_core::types::Timestamp(0),
			updated_at: mintworks_core::types::Timestamp(0),
		};
		assert_eq!(clamp(500), MAX_PAGE_LIMIT);
		assert_eq!(clamp(0), 1);
		assert_eq!(clamp(-7), 1);

		let full: Vec<Object> = (0..2).map(|i| row(&format!("nte_{i}"))).collect();
		let cursor = |rows: &[Object], limit: i64| {
			to_json(&page(rows, clamp(limit)).unwrap()).unwrap()["nextCursor"].clone()
		};
		assert_eq!(cursor(&full, 2), Json::String("nte_1".to_string()));
		assert_eq!(cursor(&full, 3), Json::Null);
		// Asked for 500, answered 2: the page is short against the clamp too, so it is the last.
		assert_eq!(cursor(&full, 500), Json::Null);
		assert_eq!(cursor(&[], 0), Json::Null);
	}

	#[test]
	fn a_declaration_drops_its_prefix_for_the_store() {
		let def = ObjectTypeDef::new("project").prefix("prj").path("$.projectUid");
		assert_eq!(
			def.declared(),
			ObjectType {
				type_name: "project".to_string(),
				paths: vec!["$.projectUid".to_string()]
			}
		);
	}
}

// vim: ts=4
