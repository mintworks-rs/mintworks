//! `memory::` — `mintworks-memory` for script.
//!
//! Compiled into every `ai` build; without `app.feature("memory")` the `Memory` extension is
//! absent and a call fails `E-MEMORY-CONFIG`, the same gate `llm::` has.

use std::sync::Arc;

use mintworks_core::{
	App,
	error::{Error, StatusCode},
};
use mintworks_memory::{
	Memory,
	store::{Doc, SearchHit, Version},
};
use rune::{ContextError, Module, Value, runtime::Ref};
use serde_json::{Value as Json, json};

use crate::{
	ctx::ScriptCtx,
	error::R,
	value::{ScriptError, from_json},
};

const E_CONFIG: &str = "E-MEMORY-CONFIG";

fn memory(app: &App) -> R<Arc<Memory>> {
	app.extensions.get::<Arc<Memory>>().cloned().ok_or_else(|| {
		ScriptError(Error::coded(
			StatusCode::SERVICE_UNAVAILABLE,
			E_CONFIG,
			"no memory store: the app did not declare app.feature(\"memory\")",
		))
	})
}

fn doc(d: &Doc) -> Json {
	json!({ "path": d.path, "version": d.version, "updatedAt": d.updated_at })
}

fn version(v: &Version) -> Json {
	json!({
		"version": v.version,
		"body": v.body,
		"author": v.author,
		"createdAt": v.created_at,
		"pdfSha256": v.pdf_sha256,
	})
}

fn hit(h: &SearchHit) -> Json {
	json!({ "space": h.space_key, "path": h.path, "version": h.version, "snippet": h.snippet })
}

fn out(j: &Json) -> R<Value> {
	from_json(j).map_err(ScriptError)
}

/// `memory::list(ctx, space)` → `[#{path, version, updatedAt}]`.
#[rune::function]
async fn list(c: Ref<ScriptCtx>, space: String) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let docs = memory(&app)?.list(&ctx, &space).await.map_err(ScriptError)?;
	out(&Json::Array(docs.iter().map(doc).collect()))
}

/// `memory::read(ctx, space, path, version)` → `#{version, body, author, createdAt, pdfSha256}`;
/// `version` is `None` for the current one.
#[rune::function]
async fn read(c: Ref<ScriptCtx>, space: String, path: String, ver: Option<i64>) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let v = memory(&app)?.read(&ctx, &space, &path, ver).await.map_err(ScriptError)?;
	out(&version(&v))
}

/// `memory::write(ctx, space, path, body)` → the new version, authored by the ctx's account.
#[rune::function]
async fn write(c: Ref<ScriptCtx>, space: String, path: String, body: String) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let v = memory(&app)?
		.write(&ctx, &space, &path, &body, None)
		.await
		.map_err(ScriptError)?;
	out(&version(&v))
}

/// `memory::append(ctx, space, path, body)` → the new version, `body` concatenated as is.
#[rune::function]
async fn append(c: Ref<ScriptCtx>, space: String, path: String, body: String) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let v = memory(&app)?
		.append(&ctx, &space, &path, &body, None)
		.await
		.map_err(ScriptError)?;
	out(&version(&v))
}

/// `memory::search(ctx, space, query, limit)` → `[#{space, path, version, snippet}]`; `space`
/// is `None` for the whole org.
#[rune::function]
async fn search(c: Ref<ScriptCtx>, space: Option<String>, query: String, limit: u32) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let hits = memory(&app)?
		.search(&ctx, space.as_deref(), &query, limit)
		.await
		.map_err(ScriptError)?;
	out(&Json::Array(hits.iter().map(hit).collect()))
}

/// `memory::history(ctx, space, path)` → every version, oldest first.
#[rune::function]
async fn history(c: Ref<ScriptCtx>, space: String, path: String) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let vs = memory(&app)?.history(&ctx, &space, &path).await.map_err(ScriptError)?;
	out(&Json::Array(vs.iter().map(version).collect()))
}

/// Registers `memory::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["memory"])?;
	m.function_meta(list)?;
	m.function_meta(read)?;
	m.function_meta(write)?;
	m.function_meta(append)?;
	m.function_meta(search)?;
	m.function_meta(history)?;
	Ok(m)
}

// vim: ts=4
