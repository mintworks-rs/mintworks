// SPDX-License-Identifier: MPL-2.0
//! OpenAI-compatible chat wire types. Every deserializer ignores unknown fields, and every
//! optional field also accepts `null`: providers add fields (`reasoning_content`) and send nulls
//! where the reference API omits the key.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One conversation turn, tagged by `role` on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
	System {
		content: String,
	},
	User {
		content: String,
	},
	Assistant {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		content: Option<String>,
		#[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "nullable")]
		tool_calls: Vec<ToolCall>,
	},
	Tool {
		tool_call_id: String,
		content: String,
	},
}

/// A complete tool call: `arguments` is the model's JSON text, unparsed — it may be invalid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "WireToolCall", from = "WireToolCall")]
pub struct ToolCall {
	pub id: String,
	pub name: String,
	pub arguments: String,
}

#[derive(Serialize, Deserialize)]
struct WireToolCall {
	id: String,
	#[serde(rename = "type", default = "function")]
	kind: String,
	function: WireFunction,
}

#[derive(Serialize, Deserialize)]
struct WireFunction {
	name: String,
	#[serde(default)]
	arguments: String,
}

fn function() -> String {
	"function".into()
}

impl From<ToolCall> for WireToolCall {
	fn from(c: ToolCall) -> Self {
		Self {
			id: c.id,
			kind: function(),
			function: WireFunction { name: c.name, arguments: c.arguments },
		}
	}
}

impl From<WireToolCall> for ToolCall {
	fn from(w: WireToolCall) -> Self {
		Self { id: w.id, name: w.function.name, arguments: w.function.arguments }
	}
}

/// A tool the model may call; `parameters` is its JSON schema.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(into = "WireTool")]
pub struct Tool {
	pub name: String,
	pub description: String,
	pub parameters: Value,
}

#[derive(Serialize)]
struct WireTool {
	#[serde(rename = "type")]
	kind: &'static str,
	function: WireToolDef,
}

#[derive(Serialize)]
struct WireToolDef {
	name: String,
	description: String,
	parameters: Value,
}

impl From<Tool> for WireTool {
	fn from(t: Tool) -> Self {
		Self {
			kind: "function",
			function: WireToolDef {
				name: t.name,
				description: t.description,
				parameters: t.parameters,
			},
		}
	}
}

/// One completion request, model aside: the provider call names the model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatRequest {
	pub messages: Vec<Message>,
	pub tools: Vec<Tool>,
	pub max_tokens: Option<u32>,
	/// Sampling temperature in thousandths (0–2000); a JSON number only in `body`.
	pub temperature_milli: Option<u16>,
}

impl ChatRequest {
	/// The streamed request body. `include_usage` makes the provider send a final usage chunk.
	pub(crate) fn body(&self, model: &str) -> Value {
		let mut body = serde_json::json!({
			"model": model,
			"messages": self.messages,
			"stream": true,
			"stream_options": { "include_usage": true },
		});
		if !self.tools.is_empty() {
			body["tools"] = serde_json::json!(self.tools);
		}
		if let Some(n) = self.max_tokens {
			body["max_tokens"] = n.into();
		}
		if let Some(t) = self.temperature_milli {
			// Parsed from a decimal string so no float arithmetic touches it.
			if let Ok(n) = format!("{}.{:03}", t / 1000, t % 1000).parse::<serde_json::Number>() {
				body["temperature"] = Value::Number(n);
			}
		}
		body
	}
}

/// Token counts as the provider reported them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "RawUsage")]
pub struct Usage {
	pub input: u64,
	pub output: u64,
	/// Input tokens served from the provider's prompt cache; a subset of `input`.
	pub cached: u64,
}

/// DeepSeek reports `prompt_cache_hit_tokens`, OpenAI `prompt_tokens_details.cached_tokens`.
#[derive(Deserialize)]
struct RawUsage {
	#[serde(default, deserialize_with = "nullable")]
	prompt_tokens: u64,
	#[serde(default, deserialize_with = "nullable")]
	completion_tokens: u64,
	#[serde(default, deserialize_with = "nullable")]
	total_tokens: u64,
	#[serde(default, deserialize_with = "nullable")]
	prompt_cache_hit_tokens: Option<u64>,
	#[serde(default, deserialize_with = "nullable")]
	prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize)]
struct PromptDetails {
	#[serde(default, deserialize_with = "nullable")]
	cached_tokens: Option<u64>,
}

impl From<RawUsage> for Usage {
	fn from(r: RawUsage) -> Self {
		let cached = r
			.prompt_cache_hit_tokens
			.or(r.prompt_tokens_details.and_then(|d| d.cached_tokens))
			.unwrap_or(0);
		Self {
			input: r.prompt_tokens,
			// Gemini's compat endpoint may leave thinking tokens out of `completion_tokens` but
			// not out of `total_tokens`; they are billed as output. Without a prompt count the
			// difference would book the whole prompt as output.
			output: if r.prompt_tokens > 0 {
				r.completion_tokens.max(r.total_tokens.saturating_sub(r.prompt_tokens))
			} else {
				r.completion_tokens
			},
			cached: cached.min(r.prompt_tokens),
		}
	}
}

/// What one streamed completion yields, in order: text deltas, then each tool call once
/// complete, then `Finish`; `Usage` usually arrives after `Finish` and may not arrive at all.
#[derive(Clone, Debug, PartialEq)]
pub enum ChatEvent {
	Text(String),
	ToolCall(ToolCall),
	Usage(Usage),
	/// The provider's `finish_reason` verbatim (`stop`, `tool_calls`, `length`, …).
	Finish(String),
}

/// One `data:` payload of the stream.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Chunk {
	#[serde(default, deserialize_with = "nullable")]
	pub choices: Vec<Choice>,
	#[serde(default)]
	pub usage: Option<Usage>,
	/// Some providers report a mid-stream failure as an `error` object inside the stream.
	#[serde(default)]
	pub error: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Choice {
	#[serde(default, deserialize_with = "nullable")]
	pub delta: Delta,
	#[serde(default)]
	pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Delta {
	#[serde(default)]
	pub content: Option<String>,
	#[serde(default, deserialize_with = "nullable")]
	pub tool_calls: Vec<ToolCallDelta>,
}

/// A fragment of tool call `index`: `id` and `name` come once, `arguments` in pieces.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ToolCallDelta {
	#[serde(default)]
	pub index: u32,
	#[serde(default)]
	pub id: Option<String>,
	#[serde(default, deserialize_with = "nullable")]
	pub function: FunctionDelta,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct FunctionDelta {
	#[serde(default)]
	pub name: Option<String>,
	#[serde(default)]
	pub arguments: Option<String>,
}

/// `null` reads as the type's default.
fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
	D: serde::Deserializer<'de>,
	T: Default + Deserialize<'de>,
{
	Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn assistant_tool_call_round_trips_in_wire_shape() {
		let m = Message::Assistant {
			content: None,
			tool_calls: vec![ToolCall {
				id: "c1".into(),
				name: "f".into(),
				arguments: "{}".into(),
			}],
		};
		let v = serde_json::to_value(&m).unwrap();
		assert_eq!(
			v,
			serde_json::json!({"role":"assistant","tool_calls":[
				{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]})
		);
		assert_eq!(serde_json::from_value::<Message>(v).unwrap(), m);
	}

	#[test]
	fn body_writes_temperature_milli_as_a_decimal() {
		let req = ChatRequest { temperature_milli: Some(700), ..ChatRequest::default() };
		assert_eq!(req.body("m")["temperature"].to_string(), "0.7");
		let none = ChatRequest::default().body("m");
		assert!(none.get("temperature").is_none());
	}

	#[test]
	fn chunk_tolerates_nulls_and_unknown_fields() {
		let c: Chunk = serde_json::from_str(
			r#"{"id":"x","choices":[{"index":0,"delta":{"content":null,"reasoning_content":"hm",
			"tool_calls":null},"finish_reason":null,"logprobs":null}],"usage":null}"#,
		)
		.unwrap();
		assert_eq!(c.choices.len(), 1);
		assert!(c.choices[0].delta.content.is_none());
	}

	#[test]
	fn usage_reads_cached_input_in_both_dialects() {
		let deepseek: Usage = serde_json::from_str(
			r#"{"prompt_tokens":100,"completion_tokens":5,"prompt_cache_hit_tokens":80,
			"prompt_cache_miss_tokens":20}"#,
		)
		.unwrap();
		assert_eq!(deepseek, Usage { input: 100, output: 5, cached: 80 });
		let openai: Usage = serde_json::from_str(
			r#"{"prompt_tokens":100,"completion_tokens":5,
			"prompt_tokens_details":{"cached_tokens":60}}"#,
		)
		.unwrap();
		assert_eq!(openai.cached, 60);
		let plain: Usage =
			serde_json::from_str(r#"{"prompt_tokens":1,"completion_tokens":null}"#).unwrap();
		assert_eq!(plain, Usage { input: 1, output: 0, cached: 0 });
		let gemini: Usage =
			serde_json::from_str(r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":40}"#)
				.unwrap();
		assert_eq!(gemini.output, 30, "thinking tokens counted from total_tokens");
		let no_prompt: Usage = serde_json::from_str(
			r#"{"prompt_tokens":null,"completion_tokens":5,"total_tokens":40}"#,
		)
		.unwrap();
		assert_eq!(no_prompt.output, 5);
	}
}

// vim: ts=4
