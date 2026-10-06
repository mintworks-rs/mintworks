//! `pdf::` — `mintworks-pdf` documents for script.
//!
//! Always compiled; without `app.feature("pdf")` no `DocumentStore` is registered and
//! `render`/`status` fail with a 500.

use mintworks_pdf::Documents;
use rune::{ContextError, Module, Value, runtime::Ref};
use serde_json::json;

use crate::{
	ctx::ScriptCtx,
	error::R,
	tx,
	value::{ScriptError, from_json, to_json},
};

/// `pdf::render(ctx, "templates/x.typ", data)` — queues a `RENDER_DOC`, returns the `doc_…`
/// uid. The path is relative to the app dir; its directory is the template set, and the
/// template reads `data` as `json(bytes(sys.inputs.data))`.
#[rune::function]
async fn render(c: Ref<ScriptCtx>, template: String, data: Value) -> R<String> {
	tx::outside_tx("pdf::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let data = to_json(&data)?;
	let docs = Documents::from_app(&app).map_err(ScriptError)?;
	let view = docs.create(&ctx, &template, data).await.map_err(ScriptError)?;
	Ok(view.uid.to_string())
}

/// `pdf::status(ctx, "doc_…")` — `#{status, sha256}`; `status` is `PENDING`, `READY` or `FAILED`.
#[rune::function]
async fn status(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let docs = Documents::from_app(&app).map_err(ScriptError)?;
	let view = docs.get(&ctx, &uid).await.map_err(ScriptError)?;
	from_json(&json!({ "status": view.status, "sha256": view.sha256 })).map_err(ScriptError)
}

/// `pdf::markdown(md)` — escaped typst markup; a template renders it with
/// `#eval(<field>, mode: "markup")`.
#[rune::function]
fn markdown(md: String) -> String {
	mintworks_pdf::to_typst(&md)
}

/// Registers `pdf::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["pdf"])?;
	m.function_meta(render)?;
	m.function_meta(status)?;
	m.function_meta(markdown)?;
	Ok(m)
}

// vim: ts=4
