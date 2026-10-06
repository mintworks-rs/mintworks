//! What a run may call: built-in Rust tools and Rune-defined ones behind one trait, in one
//! registry shared by every run. A run's spec names the subset it offers the model.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use mintworks_core::{Ctx, Error, prelude::RunId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The run a tool call belongs to: its actor, its uid, the memory spaces and skills it was granted.
pub struct ToolRun<'a> {
	pub ctx: &'a Ctx,
	pub run: &'a RunId,
	pub spaces: &'a [SpaceGrant],
	/// The run's budget key, charged by the tools that call a paid provider.
	pub subject: Option<&'a str>,
	/// The run's skill menu, which `skill_read` is confined to.
	pub skills: &'a [String],
	/// The run's `spec.lang`, `en` when absent: the variant `skill_read` serves.
	pub lang: &'a str,
}

/// The errCode and a message safe to show the model (and through run events, the browser):
/// 5xx detail, upstream failures included, stays in the log.
#[must_use]
pub fn public(e: &Error) -> (&'static str, String) {
	let (status, code) = e.parts();
	let msg = if status.is_server_error() { "internal error".to_owned() } else { e.to_string() };
	(code, msg)
}

/// A failed tool call as the model sees it, `"{code}: {msg}"`; a 5xx is logged first.
#[must_use]
pub fn shown(e: &Error, tool: &str, run: &RunId) -> String {
	if e.parts().0.is_server_error() {
		tracing::warn!(tool, run = run.as_str(), "agent tool failed: {e}");
	}
	let (code, msg) = public(e);
	format!("{code}: {msg}")
}

/// A memory space a run may reach. A bare key string deserialises as a read-write grant, so
/// specs stored before grants carried access still load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "GrantRepr")]
pub struct SpaceGrant {
	pub key: String,
	pub access: Access,
	/// What the space is for, shown to the model.
	pub about: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
	Read,
	#[default]
	Write,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GrantRepr {
	Key(String),
	Full {
		key: String,
		#[serde(default)]
		access: Access,
		#[serde(default)]
		about: Option<String>,
	},
}

impl From<GrantRepr> for SpaceGrant {
	fn from(r: GrantRepr) -> Self {
		match r {
			GrantRepr::Key(key) => Self { key, access: Access::Write, about: None },
			GrantRepr::Full { key, access, about } => Self { key, access, about },
		}
	}
}

#[async_trait]
pub trait Tool: Send + Sync + 'static {
	fn name(&self) -> &str;
	fn description(&self) -> &str;
	/// The JSON schema of the arguments.
	fn schema(&self) -> Value;
	/// An `Err` goes back to the model as the tool result; it does not fail the run.
	async fn call(&self, run: &ToolRun<'_>, args: Value) -> Result<Value, String>;
}

/// Every tool this process offers, by name. One per process, in the `App` extensions.
#[derive(Clone, Default)]
pub struct Tools(BTreeMap<String, Arc<dyn Tool>>);

impl Tools {
	pub fn new() -> Self {
		Self::default()
	}

	/// A later tool of the same name replaces the earlier one.
	pub fn add(&mut self, tool: Arc<dyn Tool>) {
		self.0.insert(tool.name().to_owned(), tool);
	}

	pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
		self.0.get(name)
	}

	/// The model-facing definitions of `names`, or the first name not registered.
	///
	/// # Errors
	/// The unknown name.
	pub fn defs(&self, names: &[String]) -> Result<Vec<mintworks_llm::Tool>, String> {
		names
			.iter()
			.map(|n| {
				let t = self.get(n).ok_or_else(|| n.clone())?;
				Ok(mintworks_llm::Tool {
					name: t.name().to_owned(),
					description: t.description().to_owned(),
					parameters: t.schema(),
				})
			})
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_bare_key_is_a_write_grant() {
		let g: SpaceGrant = serde_json::from_str(r#""k:1""#).unwrap();
		assert_eq!(g, SpaceGrant { key: "k:1".into(), access: Access::Write, about: None });
	}

	#[test]
	fn a_full_grant_round_trips() {
		let v = serde_json::json!({ "key": "k:1", "access": "read", "about": "notes" });
		let g: SpaceGrant = serde_json::from_value(v.clone()).unwrap();
		assert_eq!(g.access, Access::Read);
		assert_eq!(serde_json::to_value(&g).unwrap(), v);
	}
}

// vim: ts=4
