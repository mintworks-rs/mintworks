// SPDX-License-Identifier: MPL-2.0
//! The agent harness: **threads** of messages in the app DB, **runs** over a thread in the core
//! DB, each run driving the LLM through tool calls and recording what it streamed as events.
#![forbid(unsafe_code)]

pub mod compact;
pub mod hook;
pub mod pool;
pub mod routes;
pub mod run;
pub mod service;
pub mod skills;
pub mod store;
pub mod tool;
pub mod tools;

pub use hook::AgentHook;
pub use pool::{Admitted, E_BUSY, Live, RunPool};
pub use run::{E_LIMIT, RunSpec};
pub use service::{Agent, E_CONFIG};
pub use skills::Skills;
pub use store::{
	AgentRunStore, EventKind, Message, NewMessage, NewRun, Run, RunEvent, RunStatus, Thread,
	ThreadStore,
};
pub use tool::{Access, SpaceGrant, Tool, ToolRun, Tools};
pub use tools::memory_tools;

use mintworks_core::settings::SettingDef;

/// This crate's declared settings, registered with
/// `AppBuilder::settings(mintworks_agent::SETTINGS)`.
pub static SETTINGS: &[SettingDef] = &[
	SettingDef::int("agent.max_concurrent", "8", "Live agent runs allowed in this process.")
		.range(1, 1024),
	SettingDef::int(
		"agent.max_concurrent_per_org",
		"1",
		"Live agent runs allowed per org; a run spec may override it.",
	)
	.range(1, 1024)
	.org_scoped(),
	SettingDef::int(
		"agent.queue_max",
		"32",
		"Runs allowed to wait for a slot before E-AGENT-BUSY.",
	)
	.range(0, 10_000),
	SettingDef::int(
		"agent.compact_after_tokens",
		"24000",
		"A thread's estimated context size past which older turns are summarised.",
	)
	.range(1000, 10_000_000),
];

// vim: ts=4
