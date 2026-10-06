// SPDX-License-Identifier: MPL-2.0
//! `llm::` — `mintworks-llm` for script.
//!
//! Compiled into every `ai` build; without `app.feature("llm")` the `LlmState` extension is
//! absent and a call fails `E-LLM-CONFIG`, the same gate `invoices::` has without `invoice`.

use std::sync::Arc;

use mintworks_core::error::{Error, StatusCode};
use mintworks_llm::{CallSpec, ChatRequest, Llm, Message, Prompts, ledger, service::E_CONFIG};
use rune::{ContextError, Module, Value, runtime::Ref};
use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::{
	ctx::ScriptCtx,
	error::{R, bad},
	tx,
	value::{ScriptError, from_json, to_json},
};

/// The `#{…}` argument of `llm::complete`. Unknown keys are refused so a misspelt `schema`
/// does not silently return unvalidated text.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Args {
	role: String,
	/// `<pack>/<step>`, rendered from the app's `packs/*/prompts/` as the system message.
	prompt: Option<String>,
	lang: String,
	vars: Json,
	messages: Vec<Message>,
	schema: Option<Json>,
	subject: Option<String>,
	step: Option<String>,
	run: Option<String>,
	prefer: Option<String>,
}

impl Default for Args {
	fn default() -> Self {
		Self {
			role: String::new(),
			prompt: None,
			lang: "en".into(),
			vars: json!({}),
			messages: Vec::new(),
			schema: None,
			subject: None,
			step: None,
			run: None,
			prefer: None,
		}
	}
}

/// `llm::complete(ctx, #{role, prompt, lang, vars, messages, schema, subject, step, run, prefer})`
/// → `#{text, json, toolCalls, finish, usage, provider, model}`.
#[rune::function]
async fn complete(c: Ref<ScriptCtx>, args: Value) -> R<Value> {
	tx::outside_tx("llm::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let args: Args =
		serde_json::from_value(to_json(&args)?).map_err(|e| bad(format!("llm::complete: {e}")))?;
	if args.role.is_empty() {
		return Err(bad("llm::complete: role is required"));
	}

	let mut messages = args.messages;
	if let Some(prompt) = &args.prompt {
		let prompts = app.extensions.get::<Arc<Prompts>>().cloned().ok_or_else(|| {
			ScriptError(Error::coded(
				StatusCode::SERVICE_UNAVAILABLE,
				E_CONFIG,
				"no prompt registry: the app did not declare app.feature(\"llm\")",
			))
		})?;
		let content = prompts.render(prompt, &args.lang, &args.vars).map_err(ScriptError)?;
		messages.insert(0, Message::System { content });
	}
	if messages.is_empty() {
		return Err(bad("llm::complete: a prompt or messages is required"));
	}

	let spec = CallSpec {
		step: args.step.or_else(|| args.prompt.clone()).unwrap_or_else(|| args.role.clone()),
		role: args.role,
		request: ChatRequest { messages, ..ChatRequest::default() },
		schema: args.schema,
		subject: args.subject,
		run: args.run,
		prefer: args.prefer,
	};
	let done = Llm::new(app).complete(&ctx, &spec).await.map_err(ScriptError)?;

	let tool_calls: Vec<Json> = done
		.tool_calls
		.iter()
		.map(|t| json!({ "id": t.id, "name": t.name, "arguments": t.arguments }))
		.collect();
	let usage = done.usage.map(|u| json!({ "input": u.input, "output": u.output }));
	from_json(&json!({
		"text": done.text,
		"json": done.json,
		"toolCalls": tool_calls,
		"finish": done.finish,
		"usage": usage,
		"provider": done.target.provider,
		"model": done.target.model,
	}))
	.map_err(ScriptError)
}

/// `llm::set_budget(ctx, subject, micro_eur)` — the subject's lifetime budget; past it a call
/// fails `E-LLM-BUDGET`.
#[rune::function]
async fn set_budget(c: Ref<ScriptCtx>, subject: String, micro_eur: i64) -> R<()> {
	let app = c.app()?.clone();
	drop(c);
	ledger::set_budget(&app, &subject, micro_eur).await.map_err(ScriptError)
}

/// `llm::usage(ctx, subject)` → `#{spent, budget}` in micro-EUR; `budget` is `()` when uncapped.
#[rune::function]
async fn usage(c: Ref<ScriptCtx>, subject: String) -> R<Value> {
	let app = c.app()?.clone();
	drop(c);
	let (spent, budget) = ledger::usage(&app, &subject).await.map_err(ScriptError)?;
	from_json(&json!({ "spent": spent, "budget": budget })).map_err(ScriptError)
}

/// Registers `llm::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["llm"])?;
	m.function_meta(complete)?;
	m.function_meta(set_budget)?;
	m.function_meta(usage)?;
	Ok(m)
}

// vim: ts=4
