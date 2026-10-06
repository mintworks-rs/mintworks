//! The run loop (A3): build the thread's context, stream the model, run the tools it calls,
//! repeat until it answers without one, a cap is hit or the run is cancelled. Every step is
//! persisted as events through [`Live::emit`] and as thread messages at the step's end.

use std::{collections::HashSet, sync::Arc, time::Duration};

use mintworks_core::{App, ClResult, Ctx, Error, error::StatusCode};
use mintworks_llm::{
	CallSpec, ChatEvent, ChatRequest, Llm, Message as Wire, Prompts, ToolCall, service::E_CONFIG,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::{
	compact::compact,
	pool::{Admitted, Live, store, threads},
	skills::{SkillEntry, Skills},
	store::{EventKind, Message, NewMessage, RunStatus, Thread, ThreadStore},
	tool::{Access, SpaceGrant, ToolRun, Tools, public},
	tools::{SKILL_READ, skill::skill_stubs},
};

/// A run that hit `max_steps` or `max_tokens`.
pub const E_LIMIT: &str = "E-AGENT-LIMIT";

const DEFAULT_MAX_STEPS: u32 = 10;
/// Deltas are coalesced into one event per this window.
const DELTA_EVERY: Duration = Duration::from_millis(100);

/// What a run was started with, stored as `agent_runs.spec`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RunSpec {
	/// The `llm.profile.<role>` the model calls use.
	pub role: String,
	/// Prompt registry key rendered as the system message, with `vars` and `lang`.
	pub prompt: Option<String>,
	pub lang: Option<String>,
	pub vars: Value,
	/// The user turn appended to the thread when the run starts.
	pub input: Option<String>,
	/// Tool names offered to the model, from the process's [`Tools`].
	pub tools: Vec<String>,
	/// Memory spaces the run's tools may reach; `access: read` refuses writes, `about` is shown
	/// to the model in a section appended to the system prompt.
	pub spaces: Vec<SpaceGrant>,
	/// Skills listed in a system-prompt menu, loadable through the implicit `skill_read` tool.
	pub skills: Vec<String>,
	/// Model calls allowed; default 10.
	pub max_steps: Option<u32>,
	/// Tokens in + out allowed across the run's model calls.
	pub max_tokens: Option<u64>,
	/// Budget key; the thread's own `subject` when absent.
	pub subject: Option<String>,
	/// Overrides `agent.max_concurrent_per_org` for this run.
	pub org_limit: Option<usize>,
	/// A JSON schema for the final answer, handed to the model call.
	// Not validated in the streaming loop; re-ask like `Llm::complete` if needed.
	pub schema: Option<Value>,
}

enum End {
	Done,
	Cancelled,
	/// Swept `interrupted` before it started: the row and its final event are the sweep's.
	Swept,
}

/// The tool definitions `spec` offers: its `tools`, plus `skill_read` when it has a skill menu.
/// [`crate::Agent::start`] checks a spec with it before the run is queued, `drive` again after.
///
/// # Errors
/// `E-AGENT-CONFIG` when the app's own [`Tools`] holds a `skill_read`; a missing role, an unknown or repeated skill, an unknown tool, or `skill_read` named in
/// `tools`.
pub(crate) fn offered(
	app: &App,
	tools: &Tools,
	spec: &RunSpec,
) -> ClResult<Vec<mintworks_llm::Tool>> {
	let invalid = |m: String| Err(Error::validation(format!("agent run spec: {m}")));
	if app.extensions.get::<Tools>().is_some_and(|t| t.get(SKILL_READ).is_some()) {
		return Err(Error::coded(
			StatusCode::SERVICE_UNAVAILABLE,
			E_CONFIG,
			format!("{SKILL_READ} is a reserved tool name"),
		));
	}
	if spec.role.is_empty() {
		return invalid("role is required".to_owned());
	}
	if spec.tools.iter().any(|t| t == SKILL_READ) {
		return invalid(format!("{SKILL_READ} is offered through `skills`"));
	}
	let skills = app.extensions.get::<Arc<Skills>>();
	let mut seen = HashSet::new();
	for n in &spec.skills {
		if skills.and_then(|s| s.get(n)).is_none() {
			return invalid(format!("unknown skill {n}"));
		}
		if !seen.insert(n.as_str()) {
			return invalid(format!("duplicate skill {n}"));
		}
	}
	let mut names = spec.tools.clone();
	if !spec.skills.is_empty() {
		names.push(SKILL_READ.to_owned());
	}
	tools.defs(&names).or_else(|n| invalid(format!("unknown tool {n}")))
}

/// The run body [`crate::RunPool::submit`] spawns: owns every status change from `running` on
/// and always ends with a `done` or `error` event.
pub async fn execute(app: App, tools: Tools, adm: Admitted) {
	let live = &adm.live;
	let outcome = drive(&app, &tools, &adm).await;
	let (status, err, payload) = match &outcome {
		Ok(End::Swept) => return,
		Ok(End::Done) => (RunStatus::Done, None, json!({ "status": "done" })),
		Ok(End::Cancelled) => (RunStatus::Cancelled, None, json!({ "status": "cancelled" })),
		Err(e) => {
			let (code, msg) = public(e);
			(
				RunStatus::Error,
				Some(e.to_string()),
				json!({ "status": "error", "errCode": code, "errStr": msg }),
			)
		}
	};
	if let Err(e) = &outcome {
		tracing::warn!(run = adm.run.uid.as_str(), "agent run failed: {e}");
	}
	let kind = if status == RunStatus::Error { EventKind::Error } else { EventKind::Done };
	match store(&app) {
		Ok(s) => {
			if let Err(e) = s.run_set_status(live.run_id, status, err.as_deref()).await {
				tracing::error!(run = adm.run.uid.as_str(), "agent: final status: {e}");
			}
		}
		Err(e) => tracing::error!("agent: {e}"),
	}
	if let Err(e) = live.emit(kind, &payload.to_string()).await {
		tracing::error!(run = adm.run.uid.as_str(), "agent: final event: {e}");
	}
}

fn limit(msg: &str) -> Error {
	Error::coded(StatusCode::UNPROCESSABLE_ENTITY, E_LIMIT, msg)
}

/// Estimated tokens of a text: chars / 4, rounded up.
pub(crate) fn est(s: &str) -> i64 {
	i64::try_from(s.chars().count().div_ceil(4)).unwrap_or(i64::MAX)
}

pub(crate) fn est_msg(m: &Message) -> i64 {
	est(&m.content) + m.tool_calls.as_deref().map_or(0, est)
}

/// [`est_msg`] of what is sent: the stub in place of a stubbed message's content.
pub(crate) fn est_sent(m: &Message, stub: Option<&str>) -> i64 {
	stub.map_or_else(|| est_msg(m), est)
}

/// The run's actor: the account it was started by, confined to its org.
fn actor(adm: &Admitted) -> Ctx {
	let ctx = Ctx::system("agent");
	let ctx = match adm.run.account_id {
		Some(a) => ctx.as_user(a),
		None => ctx,
	};
	ctx.with_org(adm.run.org_id)
}

/// A stored message as the model sees it; `None` for a role the wire does not know.
fn wire(m: &Message) -> Option<Wire> {
	Some(match m.role.as_str() {
		"system" => Wire::System { content: m.content.clone() },
		"user" => Wire::User { content: m.content.clone() },
		"assistant" => Wire::Assistant {
			content: (!m.content.is_empty()).then(|| m.content.clone()),
			tool_calls: m
				.tool_calls
				.as_deref()
				.and_then(|t| serde_json::from_str(t).ok())
				.unwrap_or_default(),
		},
		"tool" => Wire::Tool {
			tool_call_id: m.tool_call_id.clone().unwrap_or_default(),
			content: m.content.clone(),
		},
		_ => return None,
	})
}

/// Messages of the current step, persisted together at its end or on cancel.
#[derive(Default)]
struct Pending(Vec<(&'static str, String, Option<String>, Option<String>)>);

impl Pending {
	async fn save(
		&mut self,
		threads: &Arc<dyn ThreadStore>,
		thread: &Thread,
		last_model: Option<&str>,
		estimate: i64,
	) -> ClResult<()> {
		if self.0.is_empty() {
			return Ok(());
		}
		let msgs: Vec<NewMessage<'_>> = self
			.0
			.iter()
			.map(|(role, content, calls, id)| NewMessage {
				role,
				content,
				tool_calls: calls.as_deref(),
				tool_call_id: id.as_deref(),
			})
			.collect();
		threads.messages_append(thread.id, &msgs, last_model, estimate).await?;
		self.0.clear();
		Ok(())
	}
}

/// Flushes coalesced text deltas as `delta` events.
struct Deltas<'a> {
	live: &'a Live,
	buf: String,
	last: Instant,
}

impl Deltas<'_> {
	async fn push(&mut self, text: &str) -> ClResult<()> {
		self.buf.push_str(text);
		// Flushes on the next token, not on a timer; a stalled stream holds its tail.
		if self.last.elapsed() >= DELTA_EVERY {
			self.flush().await?;
		}
		Ok(())
	}

	async fn flush(&mut self) -> ClResult<()> {
		if !self.buf.is_empty() {
			let payload = json!({ "text": std::mem::take(&mut self.buf) }).to_string();
			self.live.emit(EventKind::Delta, &payload).await?;
		}
		self.last = Instant::now();
		Ok(())
	}
}

async fn drive(app: &App, tools: &Tools, adm: &Admitted) -> ClResult<End> {
	let (run, live) = (&adm.run, &adm.live);
	let cancel = &live.cancel;
	if !store(app)?.run_set_status(live.run_id, RunStatus::Running, None).await? {
		return Ok(End::Swept);
	}
	let spec: RunSpec = serde_json::from_str(&run.spec)
		.map_err(|e| Error::validation(format!("agent run spec: {e}")))?;
	let defs = offered(app, tools, &spec)?;
	let threads = threads(app)?;
	let thread = threads.thread_get(&run.thread).await?.ok_or(Error::NotFound)?;
	let subject = spec.subject.clone().or_else(|| thread.subject.clone());
	let ctx = actor(adm);
	let llm = Llm::new(app.clone());
	let max_steps = spec.max_steps.unwrap_or(DEFAULT_MAX_STEPS);

	// Context: rendered system prompt, the compaction summary, then the uncompacted messages.
	let mut messages = Vec::new();
	let lang = spec.lang.as_deref().unwrap_or("en");
	if let Some(key) = &spec.prompt {
		let prompts = app.extensions.get::<Arc<Prompts>>().cloned().ok_or_else(|| {
			Error::coded(StatusCode::SERVICE_UNAVAILABLE, E_CONFIG, "no prompt registry")
		})?;
		messages.push(Wire::System { content: prompts.render(key, lang, &spec.vars)? });
	}
	if !spec.spaces.is_empty() && spec.tools.iter().any(|t| t.starts_with("memory_")) {
		append_system(&mut messages, spaces_section(&spec.spaces));
	}
	if let Some(skills) = app.extensions.get::<Arc<Skills>>()
		&& !spec.skills.is_empty()
	{
		let menu: Vec<SkillEntry> = spec.skills.iter().filter_map(|n| skills.get(n)).collect();
		append_system(&mut messages, skills_section(&menu));
	}
	let mut summary = thread.summary.clone();
	let mut live_msgs = threads.messages_live(thread.id).await?;
	let compact_after = app.settings.int("agent.compact_after_tokens").await?;
	// A `skill_read` result from a run with another menu goes out as a stub; the row keeps it.
	let mut stubs = skill_stubs(&live_msgs, &spec.skills);
	let before = summary.as_deref().map_or(0, est)
		+ live_msgs
			.iter()
			.zip(&stubs)
			.map(|(m, s)| est_sent(m, s.as_deref()))
			.sum::<i64>();
	if before > compact_after {
		let folded = tokio::select! {
			() = cancel.cancelled() => return Ok(End::Cancelled),
			r = compact(&llm, &ctx, &threads, &thread, &run.uid, subject.clone(), &live_msgs, &stubs, compact_after) => r,
		};
		// A failed compaction must not fail the run: it goes out uncompacted and retries next time.
		match folded {
			Ok(Some(c)) => {
				summary = Some(c.summary);
				// The cut is at a `user` turn, so the tail's positions still line up.
				live_msgs.drain(..c.kept_from);
				stubs.drain(..c.kept_from);
			}
			Ok(None) => {}
			Err(e) => tracing::warn!(run = run.uid.as_str(), "agent compaction failed: {e}"),
		}
	}
	let mut estimate = 0;
	if let Some(summary) = &summary {
		estimate += est(summary);
		messages.push(Wire::System {
			content: format!("Summary of the earlier conversation:\n{summary}"),
		});
	}
	for (m, stub) in live_msgs.iter().zip(stubs) {
		estimate += est_sent(m, stub.as_deref());
		match stub {
			Some(content) => messages.push(Wire::Tool {
				tool_call_id: m.tool_call_id.clone().unwrap_or_default(),
				content,
			}),
			None => messages.extend(wire(m)),
		}
	}
	let mut pending = Pending::default();
	if let Some(input) = &spec.input {
		estimate += est(input);
		messages.push(Wire::User { content: input.clone() });
		// Saved with the first step's reply: a model call refused up front (budget, config)
		// writes nothing, so a retry does not duplicate the turn.
		pending.0.push(("user", input.clone(), None, None));
	}

	let mut last_model = thread.last_model.clone();
	let mut used: u64 = 0;
	for step in 1..=max_steps {
		if cancel.is_cancelled() {
			return Ok(End::Cancelled);
		}
		let remaining = spec.max_tokens.map(|m| m.saturating_sub(used));
		if remaining == Some(0) {
			return Err(limit("the run used its max_tokens"));
		}
		let call = CallSpec {
			role: spec.role.clone(),
			request: ChatRequest {
				messages: messages.clone(),
				tools: defs.clone(),
				max_tokens: remaining.map(|r| u32::try_from(r).unwrap_or(u32::MAX)),
			},
			schema: spec.schema.clone(),
			subject: subject.clone(),
			step: format!("agent.{step}"),
			run: Some(run.uid.as_str().to_owned()),
			prefer: last_model.clone(),
		};
		let mut stream = tokio::select! {
			() = cancel.cancelled() => return Ok(End::Cancelled),
			s = llm.stream(&ctx, &call) => s?,
		};
		let model = format!("{}:{}", stream.target.provider, stream.target.model);
		last_model = Some(model.clone());

		let mut text = String::new();
		let mut calls: Vec<ToolCall> = Vec::new();
		let mut deltas = Deltas { live, buf: String::new(), last: Instant::now() };
		let mut cancelled = false;
		let mut failed = None;
		loop {
			let ev = tokio::select! {
				() = cancel.cancelled() => { cancelled = true; break; }
				ev = stream.next() => match ev {
					Ok(ev) => ev,
					Err(e) => { failed = Some(e); break; }
				},
			};
			match ev {
				Some(ChatEvent::Text(t)) => {
					text.push_str(&t);
					if let Err(e) = deltas.push(&t).await {
						failed = Some(e);
						break;
					}
				}
				Some(ChatEvent::ToolCall(c)) => calls.push(c),
				Some(ChatEvent::Usage(_) | ChatEvent::Finish(_)) => {}
				None => break,
			}
		}
		// `used()`, not the `Usage` event: a provider that never sends one must still hit the cap.
		let u = stream.used();
		used = used.saturating_add(u.input.saturating_add(u.output));
		drop(stream);
		if let Err(e) = deltas.flush().await {
			failed.get_or_insert(e);
		}

		// On cancel or a stream error the partial text is kept; tool calls never ran, so they are not.
		if cancelled || failed.is_some() {
			calls.clear();
		}
		let calls_json = if calls.is_empty() {
			None
		} else {
			Some(serde_json::to_string(&calls).map_err(|e| Error::internal(e.to_string()))?)
		};
		estimate += est(&text) + calls_json.as_deref().map_or(0, est);
		messages.push(Wire::Assistant {
			content: (!text.is_empty()).then(|| text.clone()),
			tool_calls: calls.clone(),
		});
		let mut shown = json!({ "role": "assistant", "content": text, "model": model });
		if !calls.is_empty() {
			shown["toolCalls"] = json!(calls);
		}
		if failed.is_none() || !text.is_empty() {
			pending.0.push(("assistant", text, calls_json, None));
			if let Err(e) = live.emit(EventKind::Message, &shown.to_string()).await {
				// The calls will not run now, and a saved call without a result breaks the thread.
				if pending.0.last().is_some_and(|m| m.1.is_empty()) {
					pending.0.pop();
				} else if let Some(m) = pending.0.last_mut() {
					m.2 = None;
				}
				failed.get_or_insert(e);
			}
		}
		if let Some(e) = failed {
			pending.save(&threads, &thread, last_model.as_deref(), estimate).await?;
			return Err(e);
		}
		if cancelled {
			pending.save(&threads, &thread, last_model.as_deref(), estimate).await?;
			return Ok(End::Cancelled);
		}
		if calls.is_empty() {
			pending.save(&threads, &thread, last_model.as_deref(), estimate).await?;
			return Ok(End::Done);
		}

		// Every call gets a result, even after a cancel: the next model call rejects a thread
		// holding a tool call without one.
		let tool_run = ToolRun {
			ctx: &ctx,
			run: &run.uid,
			spaces: &spec.spaces,
			subject: subject.as_deref(),
			skills: &spec.skills,
			lang,
		};
		for c in &calls {
			live.emit(
				EventKind::ToolCall,
				&json!({ "id": c.id, "name": c.name, "arguments": c.arguments }).to_string(),
			)
			.await?;
			let result = if cancel.is_cancelled() {
				Err("cancelled".to_owned())
			} else if !defs.iter().any(|d| d.name == c.name) {
				Err(format!("unknown tool {}", c.name))
			} else if let Some(tool) = tools.get(&c.name) {
				match serde_json::from_str::<Value>(&c.arguments) {
					Err(e) => Err(format!("invalid JSON arguments: {e}")),
					Ok(args) => tokio::select! {
						() = cancel.cancelled() => Err("cancelled".to_owned()),
						r = tool.call(&tool_run, args) => r,
					},
				}
			} else {
				Err(format!("unknown tool {}", c.name))
			};
			match &result {
				Ok(_) => tracing::debug!(run = run.uid.as_str(), tool = %c.name,
					arg = gist(&c.arguments), "tool call"),
				Err(e) => tracing::info!(run = run.uid.as_str(), tool = %c.name,
					arg = gist(&c.arguments), error = %e, "tool call failed"),
			}
			let (content, event) = match result {
				Ok(v) => {
					// Skill text is operator instructions; the run stream reaches the end user.
					let mut shown = v.clone();
					if c.name == SKILL_READ
						&& let Some(o) = shown.as_object_mut()
					{
						o.remove("content");
					}
					(
						v.to_string(),
						json!({ "id": c.id, "name": c.name, "ok": true, "result": shown }),
					)
				}
				Err(e) => (
					json!({ "error": e }).to_string(),
					json!({ "id": c.id, "name": c.name, "ok": false, "error": e }),
				),
			};
			live.emit(EventKind::ToolResult, &event.to_string()).await?;
			estimate += est(&content);
			messages.push(Wire::Tool { tool_call_id: c.id.clone(), content: content.clone() });
			pending.0.push(("tool", content, None, Some(c.id.clone())));
		}
		pending.save(&threads, &thread, last_model.as_deref(), estimate).await?;
		if cancel.is_cancelled() {
			return Ok(End::Cancelled);
		}
		if spec.max_tokens.is_some_and(|m| used >= m) {
			return Err(limit("the run used its max_tokens"));
		}
	}
	Err(limit("the run used its max_steps"))
}

/// Appends `section` to the system message, or makes it the system message when there is none.
fn append_system(messages: &mut Vec<Wire>, section: String) {
	match messages.first_mut() {
		Some(Wire::System { content }) => {
			content.push_str("\n\n");
			content.push_str(&section);
		}
		_ => messages.push(Wire::System { content: section }),
	}
}

/// The system-prompt menu of the run's skills, in spec order.
fn skills_section(menu: &[SkillEntry]) -> String {
	let mut s = format!("Skills you can load with `{SKILL_READ}` when a task needs them:");
	for e in menu {
		s.push_str("\n- `");
		s.push_str(&e.name);
		s.push_str("`: ");
		s.push_str(&e.description);
	}
	s
}

/// The system-prompt section telling the model which spaces it may use and what each is for.
fn spaces_section(grants: &[SpaceGrant]) -> String {
	let mut s = "Memory spaces you may use; pass the key as `space`:".to_owned();
	for g in grants {
		s.push_str("\n- `");
		s.push_str(&g.key);
		s.push('`');
		if g.access == Access::Read {
			s.push_str(" (read-only)");
		}
		if let Some(about) = &g.about {
			s.push_str(": ");
			s.push_str(about);
		}
	}
	s
}

/// The identifying arguments of a tool call (`name`, `url` cut to scheme and host, `path`), for
/// the log. Never free text: a `memory_write` carries the user's document, a `web_search` query
/// the user's words.
fn gist(args: &str) -> String {
	let v: Value = serde_json::from_str(args).unwrap_or_default();
	let parts: Vec<&str> = ["name", "url", "path"]
		.iter()
		.filter_map(|k| {
			let s = v.get(k)?.as_str()?;
			if *k != "url" {
				return Some(s);
			}
			// The path, query or fragment may carry a token or the user's search.
			let host = s.find("://").map_or(0, |i| i + 3);
			Some(&s[..s[host..].find(['/', '?', '#']).map_or(s.len(), |j| host + j)])
		})
		.collect();
	parts.join(" ").chars().filter(|c| !c.is_control()).take(120).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn gist_keeps_identifiers_and_drops_content() {
		assert_eq!(
			gist(r#"{"name":"demo","path":"references/checklist.md"}"#),
			"demo references/checklist.md"
		);
		assert_eq!(gist(r#"{"path":"notes.md","content":"secret"}"#), "notes.md");
		assert_eq!(gist(r#"{"query":"secret"}"#), "");
		assert_eq!(gist("not json"), "");
		assert_eq!(gist(r#"{"url":"https://x.io/a?token=s#f"}"#), "https://x.io");
		assert_eq!(gist(r#"{"name":"a\nb"}"#), "ab");
	}

	#[test]
	fn spaces_section_marks_read_only_and_carries_about() {
		let grants = [
			SpaceGrant {
				key: "project:prj_1".into(),
				access: Access::Write,
				about: Some("Tax.".into()),
			},
			SpaceGrant { key: "notebook:main".into(), access: Access::Read, about: None },
		];
		assert_eq!(
			spaces_section(&grants),
			"Memory spaces you may use; pass the key as `space`:\n- `project:prj_1`: Tax.\n- `notebook:main` (read-only)"
		);
	}

	#[test]
	fn skills_section_lists_the_menu_in_spec_order() {
		let entry = |name: &str, description: &str| SkillEntry {
			name: name.into(),
			description: description.into(),
			references: Vec::new(),
		};
		assert_eq!(
			skills_section(&[entry("zeta", "Use last."), entry("alpha", "Use first.")]),
			"Skills you can load with `skill_read` when a task needs them:\n- `zeta`: Use last.\n- `alpha`: Use first."
		);
	}
}

// vim: ts=4
