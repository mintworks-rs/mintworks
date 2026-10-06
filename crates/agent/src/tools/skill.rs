//! `skill_read`: the body or a reference of a skill in the run's menu, in the run's language.
//! Implicit: a run with a non-empty `skills` menu is offered it, and `spec.tools` cannot name it.

use std::sync::Arc;

use async_trait::async_trait;
use mintworks_llm::ToolCall;
use serde_json::{Value, json};

use crate::{
	skills::{ReadError, Skills},
	store::Message,
	tool::{Tool, ToolRun},
};

pub const SKILL_READ: &str = "skill_read";

struct SkillRead(Arc<Skills>);

/// `skill_read`, for [`crate::Tools::add`].
pub fn skill_tool(skills: &Arc<Skills>) -> Arc<dyn Tool> {
	Arc::new(SkillRead(Arc::clone(skills)))
}

#[async_trait]
impl Tool for SkillRead {
	fn name(&self) -> &str {
		SKILL_READ
	}

	fn description(&self) -> &'static str {
		"Load a skill from the menu in the system prompt. Without `path` returns its instructions \
		 and the `references` it has; pass one of those as `path` to read it."
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"name": { "type": "string" },
				"path": { "type": "string", "description": "e.g. references/pricing.md" },
			},
			"required": ["name"],
		})
	}

	async fn call(&self, run: &ToolRun<'_>, args: Value) -> Result<Value, String> {
		let name =
			args.get("name").and_then(Value::as_str).ok_or("missing string argument name")?;
		let path = match args.get("path") {
			None | Some(Value::Null) => None,
			Some(Value::String(p)) => Some(p.as_str()),
			Some(_) => return Err("path must be a string".to_owned()),
		};
		let unavailable = || format!("skill {name} is not available in this run");
		if !run.skills.iter().any(|s| s == name) {
			return Err(unavailable());
		}
		let file = self.0.read(name, path, run.lang).map_err(|e| match e {
			ReadError::NoSkill(_) => unavailable(),
			e @ ReadError::NoFile { .. } => e.to_string(),
		})?;
		serde_json::to_value(file).map_err(|e| e.to_string())
	}
}

/// What each message is sent as when it is a successful `skill_read` result whose skill is
/// outside `menu` (a stub; the row keeps the content), else `None`. Matched by position — the
/// k-th `tool` message after an assistant message answers its k-th call — never by id:
/// providers reuse ids (`call_0`) or send empty ones.
/// Unparseable `tool_calls` fail closed: every following result that reads as a skill is stubbed.
pub(crate) fn skill_stubs(msgs: &[Message], menu: &[String]) -> Vec<Option<String>> {
	let mut names: Option<Vec<String>> = Some(Vec::new());
	let mut cursor = 0;
	msgs.iter()
		.map(|m| match m.role.as_str() {
			"assistant" => {
				names = m
					.tool_calls
					.as_deref()
					.map_or(Some(Vec::new()), |t| serde_json::from_str::<Vec<ToolCall>>(t).ok())
					.map(|calls| calls.into_iter().map(|c| c.name).collect());
				cursor = 0;
				None
			}
			"tool" => {
				let name = names.as_ref().map(|n| n.get(cursor));
				cursor += 1;
				match name {
					None => stub(m, menu),
					Some(n) if n.map(String::as_str) == Some(SKILL_READ) => stub(m, menu),
					Some(_) => None,
				}
			}
			_ => None,
		})
		.collect()
}

fn stub(m: &Message, menu: &[String]) -> Option<String> {
	let v: Value = serde_json::from_str(&m.content).ok()?;
	let name = v.get("name").and_then(Value::as_str)?;
	// A failed read is `{"error": …}`: short, and passed through.
	if v.get("content").is_none() || menu.iter().any(|s| s == name) {
		return None;
	}
	Some(
		json!({ "name": name, "path": v.get("path"), "note": "skill text not kept; read it again if it is still in the menu" })
			.to_string(),
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::store::msg;

	fn calls(names: &[(&str, &str)]) -> String {
		let calls: Vec<ToolCall> = names
			.iter()
			.map(|(id, n)| ToolCall { id: (*id).into(), name: (*n).into(), arguments: "{}".into() })
			.collect();
		serde_json::to_string(&calls).unwrap()
	}

	const OK: &str = r#"{"name":"a","path":"SKILL.md","lang":"en","bytes":1,"content":"x"}"#;

	#[test]
	fn stubs_only_successful_reads_outside_the_menu() {
		let calls = calls(&[("c1", SKILL_READ), ("c2", SKILL_READ), ("c3", "other")]);
		let msgs = [
			msg("assistant", "", Some(&calls), None),
			msg("tool", OK, None, Some("c1")),
			msg("tool", r#"{"error":"skill a is not available in this run"}"#, None, Some("c2")),
			msg("tool", OK, None, Some("c3")),
		];
		let out = skill_stubs(&msgs, &[]);
		let stub: Value = serde_json::from_str(out[1].as_deref().unwrap()).unwrap();
		assert_eq!(
			stub,
			json!({ "name": "a", "path": "SKILL.md", "note": "skill text not kept; read it again if it is still in the menu" })
		);
		assert_eq!(out[0], None);
		assert_eq!(out[2], None);
		assert_eq!(out[3], None);
		assert_eq!(skill_stubs(&msgs, &["a".into()])[1], None);
	}

	#[test]
	fn reused_and_empty_call_ids_are_matched_by_position() {
		let other = r#"{"name":"a","content":"x"}"#;
		for id in ["call_0", ""] {
			let msgs = [
				msg("user", "hi", None, None),
				msg("assistant", "", Some(&calls(&[(id, SKILL_READ)])), None),
				msg("tool", OK, None, Some(id)),
				msg("assistant", "", Some(&calls(&[(id, "other")])), None),
				msg("tool", other, None, Some(id)),
			];
			let out = skill_stubs(&msgs, &[]);
			assert!(out[2].is_some(), "{id:?}");
			assert_eq!(out[4], None, "{id:?}");
		}
	}

	#[test]
	fn unparseable_tool_calls_still_stub_skill_reads() {
		let msgs =
			[msg("assistant", "", Some("not json"), None), msg("tool", OK, None, Some("c1"))];
		assert!(skill_stubs(&msgs, &[])[1].is_some());
	}
}

// vim: ts=4
