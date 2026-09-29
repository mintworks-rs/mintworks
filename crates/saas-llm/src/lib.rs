//! An OpenAI-compatible chat client behind role profiles, provider fallback and a cost ledger.
#![forbid(unsafe_code)]

pub mod config;
pub mod fake;
pub mod ledger;
pub mod prompts;
pub mod provider;
pub mod service;
pub mod sse;
pub mod store;
pub mod structured;
pub mod wire;

pub use config::Target;
pub use fake::{Canned, FakeQueue};
pub use prompts::Prompts;
pub use provider::{ChatStream, OpenAi, Provider};
pub use service::{CallSpec, Completion, Llm, LlmState, LlmStream};
pub use store::{LlmStore, UsageKind, UsageRow};
pub use wire::{ChatEvent, ChatRequest, Message, Tool, ToolCall, Usage};

use saas_core::settings::SettingDef;

/// This crate's declared settings, registered with `AppBuilder::settings(saas_llm::SETTINGS)`.
/// Per-provider keys are families suffixed by provider name: families match a prefix only.
pub static SETTINGS: &[SettingDef] = &[
	SettingDef::text("llm.base_url.", "", "A provider's OpenAI-compatible base URL, without /v1.")
		.family(),
	SettingDef::choice("llm.kind.", &["openai", "fake"], "openai", "A provider's wire protocol.")
		.family(),
	SettingDef::int("llm.max_concurrent.", "4", "Concurrent calls allowed to one provider.")
		.family()
		.range(1, 256),
	SettingDef::flag(
		"llm.allow_internal.",
		"0",
		"Let a provider's base URL be a loopback or private address (a self-hosted model).",
	)
	.family(),
	SettingDef::int(
		"llm.slot_wait_secs",
		"10",
		"How long a call waits for a provider slot before the next profile entry; 0 = no wait.",
	)
	.range(0, 600),
	SettingDef::text(
		"llm.profile.",
		"",
		"A role's ordered provider:model fallback list, comma-separated.",
	)
	.family()
	.check(config::check_profile),
	// The key is `llm.price.<provider>:<model>` in the model's own spelling.
	SettingDef::text(
		"llm.price.",
		"",
		"A model's input,output[,cached-input] price in integer micro-EUR per 1M tokens.",
	)
	.family()
	.check(config::check_price),
	SettingDef::int(
		"llm.daily_ceiling_micro_eur",
		"5000000",
		"Deployment-wide LLM spend per UTC day in micro-EUR; 0 disables.",
	)
	.range(0, i64::MAX),
	SettingDef::int(
		"llm.alert_loaded_cost_micro_eur",
		"800000",
		"Daily LLM spend in micro-EUR above which A-LLM-LOADED-COST is raised.",
	)
	.range(0, i64::MAX),
];

/// `llm.api_key.<provider>`: a family, per the trailing `.`.
pub static SECRETS: &[&str] = &["llm.api_key."];

// vim: ts=4
