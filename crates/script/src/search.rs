// SPDX-License-Identifier: MPL-2.0
//! `search::` — `mintworks-search` for script.
//!
//! Compiled into every `ai` build; without `app.feature("search")` no `SearchStore` is
//! registered and a call fails `E-LLM-CONFIG`.

use mintworks_search::Search;
use rune::{ContextError, Module, Value, runtime::Ref};
use serde::Deserialize;
use serde_json::json;

use crate::{
	ctx::ScriptCtx,
	error::{R, bad},
	tx,
	value::{ScriptError, from_json, to_json},
};

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct SearchArgs {
	query: String,
	lang: Option<String>,
	market: Option<String>,
	subject: Option<String>,
	run: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct FetchArgs {
	url: String,
	subject: Option<String>,
	run: Option<String>,
}

/// `search::web(ctx, #{query, lang?, market?, subject?, run?})` — `[#{title, url, snippet, source}]`.
#[rune::function]
async fn web(c: Ref<ScriptCtx>, args: Value) -> R<Value> {
	tx::outside_tx("search::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let a: SearchArgs =
		serde_json::from_value(to_json(&args)?).map_err(|e| bad(format!("search::web: {e}")))?;
	let hits = Search::new(app)
		.search(
			&ctx,
			&a.query,
			a.lang.as_deref(),
			a.market.as_deref(),
			a.subject.as_deref(),
			a.run.as_deref(),
		)
		.await
		.map_err(ScriptError)?;
	from_json(&json!(hits)).map_err(ScriptError)
}

/// `search::fetch(ctx, #{url, subject?, run?})` — `#{source, url, title, text, fetchedAt}`.
#[rune::function]
async fn fetch(c: Ref<ScriptCtx>, args: Value) -> R<Value> {
	tx::outside_tx("search::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let a: FetchArgs =
		serde_json::from_value(to_json(&args)?).map_err(|e| bad(format!("search::fetch: {e}")))?;
	let src = Search::new(app)
		.fetch(&ctx, &a.url, a.subject.as_deref(), a.run.as_deref())
		.await
		.map_err(ScriptError)?;
	source_json(&src)
}

/// `search::source(ctx, "src_…")` — what the model was shown, for resolving a citation.
#[rune::function]
async fn source(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let src = Search::new(app).source(&ctx, &uid).await.map_err(ScriptError)?;
	source_json(&src)
}

fn source_json(src: &mintworks_search::Source) -> R<Value> {
	from_json(&json!({
		"source": src.uid.as_str(),
		"url": src.url,
		"title": src.title,
		"text": src.text,
		"fetchedAt": src.fetched_at.to_rfc3339(),
	}))
	.map_err(ScriptError)
}

/// Registers `search::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["search"])?;
	m.function_meta(web)?;
	m.function_meta(fetch)?;
	m.function_meta(source)?;
	Ok(m)
}

// vim: ts=4
