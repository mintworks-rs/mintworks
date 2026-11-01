// SPDX-License-Identifier: MPL-2.0
//! `llm::` — `mintworks-llm` for script.
//!
//! Compiled into every `ai` build; without `app.feature("llm")` the `LlmState` extension is
//! absent and a call fails `E-LLM-CONFIG`, the same gate `invoices::` has without `invoice`.

use std::sync::Arc;

use mintworks_core::error::{Error, StatusCode};
use mintworks_core::ids::AccountId;
use mintworks_core::prelude::Timestamp;
use mintworks_core::{App, Ctx};
use mintworks_llm::{
	CallSpec, ChatRequest, Llm, Message, Prompts, Tool, UsageDim, UsageKind, UsageQuery, UsageRow,
	ledger, service::E_CONFIG,
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
	/// Offered to the model; the script runs its own tool loop over the returned `toolCalls`.
	tools: Vec<ToolArg>,
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
			tools: Vec::new(),
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

/// One `tools` entry of `llm::complete`; `schema` is the wire `parameters`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolArg {
	name: String,
	#[serde(default)]
	description: String,
	schema: Json,
}

/// `llm::complete(ctx, #{role, prompt, lang, vars, messages, tools, schema, subject, step, run,
/// prefer, temperatureMilli, maxTokens})`
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
			tools: args
				.tools
				.into_iter()
				.map(|t| Tool { name: t.name, description: t.description, parameters: t.schema })
				.collect(),
			max_tokens: args.max_tokens,
			temperature_milli,
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

/// The `#{…}` argument of `llm::record`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RecordArgs {
	provider: String,
	model: String,
	step: String,
	#[serde(default)]
	subject: Option<String>,
	#[serde(default)]
	units: Option<i64>,
	cost_micro_eur: i64,
}

/// `llm::record(ctx, #{provider, model, step, subject?, units?, costMicroEur})` — books a
/// non-LLM paid call as a `kind = 'api'` ledger row (`units` in `tokens_in`), attributed to the
/// ctx's account and org.
#[rune::function]
async fn record(c: Ref<ScriptCtx>, args: Value) -> R<()> {
	tx::outside_tx("llm::")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let a: RecordArgs =
		serde_json::from_value(to_json(&args)?).map_err(|e| bad(format!("llm::record: {e}")))?;
	if a.cost_micro_eur < 0 || a.units.is_some_and(|u| u < 0) {
		return Err(bad("llm::record: costMicroEur and units cannot be negative"));
	}
	let row = UsageRow {
		at: Timestamp::now(),
		run: None,
		account_id: ctx.actor.account_id(),
		org_id: ctx.org_id,
		subject: a.subject,
		step: a.step,
		kind: UsageKind::Api,
		provider: a.provider,
		model: a.model,
		tokens_in: a.units.unwrap_or(0),
		tokens_out: 0,
		cost_micro_eur: a.cost_micro_eur,
		retry: false,
	};
	ledger::record(&app, &row).await.map_err(ScriptError)
}

/// The ledger an actor may read: an operator or system ctx the deployment-wide one (`None`), an
/// org Admin or Owner their org's; anyone else is `E-AUTH-FORBIDDEN`.
async fn usage_scope(app: &App, ctx: &Ctx) -> R<Option<i64>> {
	Ok(match ctx.actor {
		mintworks_core::Actor::Operator { .. } | mintworks_core::Actor::System { .. } => None,
		_ => {
			let org = ctx.org().map_err(ScriptError)?;
			let role = match ctx.actor.account_id() {
				Some(me) => mintworks_auth::routes::store(app)
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
	})
}

/// The `#{…}` argument of `llm::spent`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpentArgs {
	subject: String,
	since: i64,
}

/// `llm::spent(ctx, #{subject, since})` → the subject's micro-EUR since `since`, every kind summed,
/// within the ledger [`usage_scope`] grants.
#[rune::function]
async fn spent(c: Ref<ScriptCtx>, args: Value) -> R<i64> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let org_id = usage_scope(&app, &ctx).await?;
	let a: SpentArgs =
		serde_json::from_value(to_json(&args)?).map_err(|e| bad(format!("llm::spent: {e}")))?;
	let q = UsageQuery {
		since: Timestamp(a.since),
		until: None,
		by: Vec::new(),
		org_id,
		subject: Some(a.subject),
		kind: None,
		account_id: None,
	};
	let groups = ledger::store(&app)
		.map_err(ScriptError)?
		.usage_grouped(&q)
		.await
		.map_err(ScriptError)?;
	Ok(groups.first().map_or(0, |g| g.micro_eur))
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
	#[serde(default)]
	subject: Option<String>,
	#[serde(default)]
	kind: Option<String>,
	#[serde(default)]
	account: Option<String>,
}

/// `llm::usage_report(ctx, #{since, until?, by, subject?, kind?, account?})` with
/// `by ⊆ ["provider", "model", "step", "day", "subject", "account"]`
/// → `[#{provider?, model?, step?, day?, subject?, account?, calls, input, output, microEur}]`,
/// every ledger kind summed unless `kind` (`llm|search|fetch|api`) narrows it; `account` is an
/// `acc_…` uid in and out. An operator or system ctx reads the deployment-wide ledger, an org Admin or Owner
/// their org's; anyone else is `E-AUTH-FORBIDDEN`.
#[rune::function]
async fn usage_report(c: Ref<ScriptCtx>, args: Value) -> R<Value> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let org_id = usage_scope(&app, &ctx).await?;
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
			"subject" => Ok(UsageDim::Subject),
			"account" => Ok(UsageDim::Account),
			other => Err(bad(format!("llm::usage_report: unknown `by` key {other}"))),
		})
		.collect::<Result<Vec<_>, _>>()?;
	let kind = (args.kind.as_deref())
		.map(|k| {
			UsageKind::parse(k).ok_or_else(|| bad(format!("llm::usage_report: unknown kind {k}")))
		})
		.transpose()?;
	let auth = mintworks_auth::routes::store(&app).map_err(ScriptError)?;
	let account_id = match &args.account {
		None => None,
		Some(uid) => {
			let uid = AccountId::parse(uid).map_err(ScriptError)?;
			// An unknown uid matches no row, rather than telling the caller it does not exist.
			match auth.account_by_uid(&uid).await.map_err(ScriptError)? {
				Some(a) => Some(a.id),
				None => return from_json(&Json::Array(Vec::new())).map_err(ScriptError),
			}
		}
	};
	let q = UsageQuery {
		since: Timestamp(args.since),
		until: args.until.map(Timestamp),
		by,
		org_id,
		subject: args.subject,
		kind,
		account_id,
	};
	let groups = ledger::store(&app)
		.map_err(ScriptError)?
		.usage_grouped(&q)
		.await
		.map_err(ScriptError)?;
	let mut rows = Vec::with_capacity(groups.len());
	for g in groups {
		let mut row = json!({
			"calls": g.calls,
			"input": g.input,
			"output": g.output,
			"microEur": g.micro_eur,
		});
		for (key, v) in [
			("provider", g.provider),
			("model", g.model),
			("step", g.step),
			("day", g.day),
			("subject", g.subject),
		] {
			if let Some(v) = v {
				row[key] = Json::String(v);
			}
		}
		// One lookup per account group; batch through `accounts_by_id` if reports grow.
		if let Some(id) = g.account_id
			&& let Some(a) = auth.account_by_id(id).await.map_err(ScriptError)?
		{
			row["account"] = Json::String(a.uid.as_str().to_owned());
		}
		rows.push(row);
	}
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
	m.function_meta(record)?;
	m.function_meta(spent)?;
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
