// SPDX-License-Identifier: MPL-2.0
//! `llm::` — `mintworks-llm` for script.
//!
//! Compiled into every `ai` build; without `app.feature("llm")` the `LlmState` extension is
//! absent and a call fails `E-LLM-CONFIG`, the same gate `invoices::` has without `invoice`.

use std::sync::Arc;

use mintworks_core::error::{Error, StatusCode};
use mintworks_core::prelude::Timestamp;
use mintworks_llm::{
	CallSpec, ChatRequest, Llm, Message, Prompts, UsageDim, UsageQuery, ledger, service::E_CONFIG,
};
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
	#[serde(rename = "temperatureMilli")]
	temperature_milli: Option<i64>,
	#[serde(rename = "maxTokens")]
	max_tokens: Option<u32>,
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
			temperature_milli: None,
			max_tokens: None,
		}
	}
}

/// `llm::complete(ctx, #{role, prompt, lang, vars, messages, schema, subject, step, run, prefer,
/// temperatureMilli, maxTokens})`
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
	let temperature_milli = match args.temperature_milli {
		None => None,
		Some(t) => Some(
			u16::try_from(t)
				.ok()
				.filter(|t| *t <= 2000)
				.ok_or_else(|| bad("llm::complete: temperatureMilli must be 0..=2000"))?,
		),
	};

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
		request: ChatRequest {
			messages,
			max_tokens: args.max_tokens,
			temperature_milli,
			..ChatRequest::default()
		},
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

/// The `#{…}` argument of `llm::usage_report`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
	since: i64,
	#[serde(default)]
	until: Option<i64>,
	#[serde(default)]
	by: Vec<String>,
}

/// `llm::usage_report(ctx, #{since, until?, by})` with `by ⊆ ["provider", "model", "step", "day"]`
/// → `[#{provider?, model?, step?, day?, calls, input, output, microEur}]`, every ledger kind
/// summed. An operator or system ctx reads the deployment-wide ledger, an org Admin or Owner
/// their org's; anyone else is `E-AUTH-FORBIDDEN`.
#[rune::function]
async fn usage_report(c: Ref<ScriptCtx>, args: Value) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let org_id = match ctx.actor {
		mintworks_core::Actor::Operator { .. } | mintworks_core::Actor::System { .. } => None,
		_ => {
			let org = ctx.org().map_err(ScriptError)?;
			let role = match ctx.actor.account_id() {
				Some(me) => mintworks_auth::routes::store(&app)
					.map_err(ScriptError)?
					.accepted_membership_role(org, me)
					.await
					.map_err(ScriptError)?,
				None => None,
			};
			if !matches!(
				role,
				Some(mintworks_core::store::Role::Admin | mintworks_core::store::Role::Owner)
			) {
				return Err(ScriptError(Error::coded(
					StatusCode::FORBIDDEN,
					"E-AUTH-FORBIDDEN",
					"insufficient role",
				)));
			}
			Some(org)
		}
	};
	let args: ReportArgs = serde_json::from_value(to_json(&args)?)
		.map_err(|e| bad(format!("llm::usage_report: {e}")))?;
	let by = args
		.by
		.iter()
		.map(|d| match d.as_str() {
			"provider" => Ok(UsageDim::Provider),
			"model" => Ok(UsageDim::Model),
			"step" => Ok(UsageDim::Step),
			"day" => Ok(UsageDim::Day),
			other => Err(bad(format!("llm::usage_report: unknown `by` key {other}"))),
		})
		.collect::<Result<Vec<_>, _>>()?;
	let q =
		UsageQuery { since: Timestamp(args.since), until: args.until.map(Timestamp), by, org_id };
	let groups = ledger::store(&app)
		.map_err(ScriptError)?
		.usage_grouped(&q)
		.await
		.map_err(ScriptError)?;
	let rows: Vec<Json> = groups
		.into_iter()
		.map(|g| {
			let mut row = json!({
				"calls": g.calls,
				"input": g.input,
				"output": g.output,
				"microEur": g.micro_eur,
			});
			for (key, v) in
				[("provider", g.provider), ("model", g.model), ("step", g.step), ("day", g.day)]
			{
				if let Some(v) = v {
					row[key] = Json::String(v);
				}
			}
			row
		})
		.collect();
	from_json(&Json::Array(rows)).map_err(ScriptError)
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

/// Registers `llm::usage_report`, installed only under `IoProfile.sys`: the report spans the
/// caller's org (the whole deployment for an operator), not the script's own calls.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn sys_module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["llm"])?;
	m.function_meta(usage_report)?;
	Ok(m)
}

// vim: ts=4
