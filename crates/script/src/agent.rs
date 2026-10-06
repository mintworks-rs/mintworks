//! Rune-defined agent tools: `app.tool(name, description, schema, fn)`. Each call is one
//! ordinary script invocation — `fn(ctx, args)` under `script.budget`/`script.timeout_ms` —
//! with the run's actor `Ctx`, never a `System` one.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mintworks_agent::{Agent, RunSpec, Tool, ToolRun};
use mintworks_core::{App, error::ClResult};
use rune::{
	ContextError, Hash, Module, Value,
	runtime::{FromValue, Function, Ref, RuntimeError},
};
use serde::Deserialize;
use serde_json::json;

use crate::{
	ScriptCtx,
	error::{R, bad},
	routes::{Arg, Decl},
	tx,
	value::{ScriptError, from_json, to_json},
	vm::Script,
};

pub struct ToolDecl {
	pub name: String,
	pub description: String,
	pub schema: serde_json::Value,
	pub entry: Hash,
}

/// The name rule providers enforce on a function name; a bad one fails at boot, not mid-run.
fn valid_name(name: &str) -> bool {
	(1..=64).contains(&name.len())
		&& name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `app.tool("lookup_order", "…", #{ type: "object", properties: #{…} }, |ctx, args| …)`.
#[rune::function(instance)]
pub(crate) fn tool(
	this: &Decl,
	name: String,
	description: String,
	schema: Value,
	handler: Function,
) {
	if !valid_name(&name) {
		return this.fail(format!("tool '{name}': a name is 1-64 of [A-Za-z0-9_-]"));
	}
	if name == mintworks_agent::tools::SKILL_READ {
		return this.fail(format!("tool '{name}': the name is reserved for skills"));
	}
	let schema = match to_json(&schema) {
		Ok(s) if s.is_object() => s,
		Ok(_) => return this.fail(format!("tool '{name}': the schema is not an object")),
		Err(e) => return this.fail(format!("tool '{name}': {e}")),
	};
	this.push_tool(ToolDecl { name, description, schema, entry: handler.type_hash() });
}

/// The script's tools, in the `App` extensions for the agent's `Tools` registry to take in.
#[derive(Clone, Default)]
pub struct RuneTools(pub Vec<Arc<dyn Tool>>);

impl RuneTools {
	/// `app` is set by `ScriptApp`'s `on_init`: the tools are built before an `App` exists.
	pub(crate) fn new(script: &Arc<Script>, decls: &[ToolDecl], app: &Arc<OnceLock<App>>) -> Self {
		Self(
			decls
				.iter()
				.map(|d| {
					Arc::new(RuneTool {
						name: d.name.clone(),
						description: d.description.clone(),
						schema: d.schema.clone(),
						entry: d.entry,
						script: Arc::clone(script),
						app: Arc::clone(app),
					}) as Arc<dyn Tool>
				})
				.collect(),
		)
	}
}

struct RuneTool {
	name: String,
	description: String,
	schema: serde_json::Value,
	entry: Hash,
	script: Arc<Script>,
	app: Arc<OnceLock<App>>,
}

/// The return value as JSON, converted inside `FromValue`: `rune::Value` is not `Send`.
struct Json(ClResult<serde_json::Value>);

impl FromValue for Json {
	fn from_value(value: Value) -> Result<Self, RuntimeError> {
		Ok(Self(to_json(&value)))
	}
}

#[async_trait]
impl Tool for RuneTool {
	fn name(&self) -> &str {
		&self.name
	}

	fn description(&self) -> &str {
		&self.description
	}

	fn schema(&self) -> serde_json::Value {
		self.schema.clone()
	}

	async fn call(
		&self,
		run: &ToolRun<'_>,
		args: serde_json::Value,
	) -> Result<serde_json::Value, String> {
		let Some(app) = self.app.get() else {
			return Err("tool called before the app started".to_owned());
		};
		let ctx = ScriptCtx::new(app.clone(), run.ctx.clone());
		let out = self.script.invoke::<Json>(self.entry, (ctx, Arg(args))).await;
		out.and_then(|j| j.0)
			.map_err(|e| mintworks_agent::tool::shown(&e, &self.name, run.run))
	}
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThreadArgs {
	subject: Option<String>,
	title: Option<String>,
}

/// `agent::create_thread(ctx, #{subject, title})` → `"thr_…"`, in `ctx`'s org.
#[rune::function]
async fn create_thread(c: Ref<ScriptCtx>, args: Value) -> R<String> {
	tx::outside_tx("agent::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let args: ThreadArgs = serde_json::from_value(to_json(&args)?)
		.map_err(|e| bad(format!("agent::create_thread: {e}")))?;
	let thread = Agent::new(app)
		.create_thread(&ctx, args.subject.as_deref(), args.title.as_deref())
		.await
		.map_err(ScriptError)?;
	Ok(thread.uid.into_string())
}

/// `agent::threads(ctx)` → `[#{uid, title, createdAt, updatedAt}]`, newest first.
#[rune::function]
async fn threads(c: Ref<ScriptCtx>) -> R<Value> {
	tx::outside_tx("agent::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let list = Agent::new(app).threads(&ctx).await.map_err(ScriptError)?;
	let list: Vec<_> = list
		.iter()
		.map(|t| {
			json!({
				"uid": t.uid.as_str(),
				"title": t.title,
				"createdAt": t.created_at,
				"updatedAt": t.updated_at,
			})
		})
		.collect();
	from_json(&json!(list)).map_err(ScriptError)
}

/// `agent::messages(ctx, thread)` → `[#{role, content, toolCalls, toolCallId, createdAt}]`,
/// oldest first, compacted ones included; `toolCalls` is the parsed array or `()`.
#[rune::function]
async fn messages(c: Ref<ScriptCtx>, thread: String) -> R<Value> {
	tx::outside_tx("agent::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let list = Agent::new(app).messages(&ctx, &thread).await.map_err(ScriptError)?;
	let list: Vec<_> = list
		.iter()
		.map(|m| {
			let calls = m
				.tool_calls
				.as_deref()
				.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
			json!({
				"role": m.role,
				"content": m.content,
				"toolCalls": calls,
				"toolCallId": m.tool_call_id,
				"createdAt": m.created_at,
			})
		})
		.collect();
	from_json(&json!(list)).map_err(ScriptError)
}

/// `agent::start(ctx, thread, #{role, prompt, vars, input, tools, spaces, maxSteps, …})` →
/// `"run_…"` at once; the run proceeds in the pool as `ctx`'s actor.
#[rune::function]
async fn start(c: Ref<ScriptCtx>, thread: String, spec: Value) -> R<String> {
	tx::outside_tx("agent::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let spec: RunSpec =
		serde_json::from_value(to_json(&spec)?).map_err(|e| bad(format!("agent::start: {e}")))?;
	let run = Agent::new(app).start(&ctx, &thread, &spec).await.map_err(ScriptError)?;
	Ok(run.into_string())
}

/// `agent::cancel(ctx, run)` — a run already over is a no-op.
#[rune::function]
async fn cancel(c: Ref<ScriptCtx>, run: String) -> R<()> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	Agent::new(app).cancel(&ctx, &run).await.map_err(ScriptError)
}

/// Registers `agent::`. Compiled into every `ai` build; without `app.feature("agent")` a call
/// fails `E-AGENT-CONFIG`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["agent"])?;
	m.function_meta(create_thread)?;
	m.function_meta(threads)?;
	m.function_meta(messages)?;
	m.function_meta(start)?;
	m.function_meta(cancel)?;
	Ok(m)
}

// vim: ts=4
