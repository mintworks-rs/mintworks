// SPDX-License-Identifier: MPL-2.0
//! Structured output: the reply is parsed as JSON and checked against a schema; `Llm::complete`
//! retries a failure at most [`MAX_RETRIES`] times. Provider `json_schema` support is never assumed.
//!
//! The checker is a deliberate subset: `type`, `enum`, `required`, `properties`,
//! `additionalProperties: false` and `items`. Any other keyword is ignored.

use serde_json::Value;

/// The reply still failed its schema after every retry.
pub const E_SCHEMA: &str = "E-LLM-SCHEMA";
/// Re-asks after the first attempt; each is its own ledger row.
pub const MAX_RETRIES: usize = 2;

/// The JSON value in a reply, tolerating a surrounding Markdown code fence.
///
/// # Errors
/// The text is not JSON.
pub fn parse(text: &str) -> Result<Value, String> {
	let t = text.trim();
	let t = match t.strip_prefix("```") {
		Some(rest) => {
			let rest = rest.split_once('\n').map_or("", |(_, body)| body);
			rest.trim_end().strip_suffix("```").unwrap_or(rest)
		}
		None => t,
	};
	serde_json::from_str(t).map_err(|e| format!("not valid JSON: {e}"))
}

/// Check `v` against `schema`.
///
/// # Errors
/// The first violation, with its `$.path`.
pub fn validate(schema: &Value, v: &Value) -> Result<(), String> {
	check(schema, v, "$")
}

fn check(schema: &Value, v: &Value, path: &str) -> Result<(), String> {
	match schema.get("type") {
		Some(Value::String(t)) if !is_type(t, v) => return Err(format!("{path}: expected {t}")),
		Some(Value::Array(ts)) if !ts.iter().any(|t| t.as_str().is_some_and(|t| is_type(t, v))) => {
			return Err(format!("{path}: expected one of {}", Value::Array(ts.clone())));
		}
		_ => {}
	}
	if let Some(Value::Array(allowed)) = schema.get("enum")
		&& !allowed.contains(v)
	{
		return Err(format!("{path}: must be one of {}", Value::Array(allowed.clone())));
	}
	if let Value::Object(obj) = v {
		if let Some(Value::Array(req)) = schema.get("required") {
			for k in req.iter().filter_map(Value::as_str) {
				if !obj.contains_key(k) {
					return Err(format!("{path}: missing required property {k:?}"));
				}
			}
		}
		let props = schema.get("properties").and_then(Value::as_object);
		for (k, val) in obj {
			match props.and_then(|p| p.get(k)) {
				Some(sub) => check(sub, val, &format!("{path}.{k}"))?,
				None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
					return Err(format!("{path}: unexpected property {k:?}"));
				}
				None => {}
			}
		}
	}
	if let (Value::Array(items), Some(sub)) = (v, schema.get("items")) {
		for (i, item) in items.iter().enumerate() {
			check(sub, item, &format!("{path}[{i}]"))?;
		}
	}
	Ok(())
}

fn is_type(t: &str, v: &Value) -> bool {
	match t {
		"object" => v.is_object(),
		"array" => v.is_array(),
		"string" => v.is_string(),
		"boolean" => v.is_boolean(),
		"null" => v.is_null(),
		"number" => v.is_number(),
		"integer" => v.is_i64() || v.is_u64(),
		_ => true,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn subset_checks() {
		let s = json!({
			"type": "object",
			"required": ["name", "tags"],
			"additionalProperties": false,
			"properties": {
				"name": {"type": "string"},
				"level": {"enum": ["low", "high"]},
				"tags": {"type": "array", "items": {"type": "integer"}}
			}
		});
		assert_eq!(validate(&s, &json!({"name": "a", "tags": [1, 2], "level": "low"})), Ok(()));
		assert!(validate(&s, &json!({"name": "a"})).unwrap_err().contains("\"tags\""));
		assert!(
			validate(&s, &json!({"name": "a", "tags": [1.5]}))
				.unwrap_err()
				.starts_with("$.tags[0]")
		);
		assert!(
			validate(&s, &json!({"name": "a", "tags": [], "x": 1}))
				.unwrap_err()
				.contains("\"x\"")
		);
		assert!(validate(&s, &json!({"name": "a", "tags": [], "level": "mid"})).is_err());
		assert!(validate(&s, &json!([])).is_err());
	}

	#[test]
	fn parse_strips_fence() {
		assert_eq!(parse("```json\n{\"a\": 1}\n```"), Ok(json!({"a": 1})));
		assert_eq!(parse(" [1] "), Ok(json!([1])));
		assert!(parse("sure! {\"a\": 1}").is_err());
	}
}

// vim: ts=4
