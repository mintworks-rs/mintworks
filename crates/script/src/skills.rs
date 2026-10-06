// SPDX-License-Identifier: MPL-2.0
//! `skills::` — the app's own read access to the `skills/` registry, outside any run's menu.

use std::sync::Arc;

use mintworks_agent::{E_CONFIG, Skills};
use mintworks_core::{
	App,
	auth_mw::require_operator,
	error::{Error, StatusCode},
};
use rune::{ContextError, Module, Value, runtime::Ref};

use crate::{
	ScriptCtx,
	error::R,
	value::{ScriptError, from_json},
};

fn skills(app: &App) -> R<Arc<Skills>> {
	app.extensions.get::<Arc<Skills>>().cloned().ok_or_else(|| {
		ScriptError(Error::coded(
			StatusCode::SERVICE_UNAVAILABLE,
			E_CONFIG,
			"skills need the agent feature",
		))
	})
}

/// `skills::list(ctx)` → `[#{name, description, references}]`, sorted by name. Operator or
/// system only.
#[rune::function]
async fn list(c: Ref<ScriptCtx>) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	require_operator(&app, &ctx).await.map_err(ScriptError)?;
	let list = skills(&app)?.list();
	from_json(&serde_json::json!(list)).map_err(ScriptError)
}

/// `skills::read(ctx, name, path, lang)` → `#{name, path, lang, bytes, references?, content}`,
/// what `skill_read` returns; `path` `()` is the body, `lang` `()` is `en`. Operator or system
/// only — skill text is operator instructions; no menu check.
#[rune::function]
async fn read(
	c: Ref<ScriptCtx>,
	name: String,
	path: Option<String>,
	lang: Option<String>,
) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	require_operator(&app, &ctx).await.map_err(ScriptError)?;
	let file = skills(&app)?
		.read(&name, path.as_deref(), lang.as_deref().unwrap_or("en"))
		.map_err(|_| ScriptError(Error::NotFound))?;
	from_json(&serde_json::json!(file)).map_err(ScriptError)
}

/// Registers `skills::`. Compiled into every `ai` build; without `app.feature("agent")` a call
/// fails `E-AGENT-CONFIG`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["skills"])?;
	m.function_meta(list)?;
	m.function_meta(read)?;
	Ok(m)
}

// vim: ts=4
