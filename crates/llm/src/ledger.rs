// SPDX-License-Identifier: MPL-2.0
//! Cost, the ledger record, the pre-call cap check and the ledger's alerts.

use std::sync::Arc;

use mintworks_core::{
	App, ClResult, Error,
	alert::{Alert, Severity},
	error::StatusCode,
	prelude::Timestamp,
};

use crate::{
	config::{Price, Target, parse_price},
	store::{LlmStore, UsageRow},
	wire::Usage,
};

/// The subject's lifetime spend reached its `llm::set_budget` value.
pub const E_BUDGET: &str = "E-LLM-BUDGET";
/// Today's deployment-wide spend reached `llm.daily_ceiling_micro_eur`.
pub const E_CEILING: &str = "E-LLM-CEILING";

/// The registered `Arc<dyn LlmStore>`; without one no call may run, since it could not be recorded.
///
/// # Errors
/// `E-LLM-CONFIG` when no store was registered.
pub fn store(app: &App) -> ClResult<Arc<dyn LlmStore>> {
	app.extensions.get::<Arc<dyn LlmStore>>().cloned().ok_or_else(|| {
		crate::service::config("mintworks-llm: no LlmStore was registered on the app")
	})
}

/// Integer micro-EUR, rounded up: a call is never recorded as cheaper than it was.
pub fn cost_micro_eur(price: Price, usage: Usage) -> i64 {
	let cached = usage.cached.min(usage.input);
	let total = i128::from(usage.input - cached) * i128::from(price.input)
		+ i128::from(cached) * i128::from(price.cached.unwrap_or(price.input))
		+ i128::from(usage.output) * i128::from(price.output);
	round_up_mtok(total)
}

/// `tokens` at `micro_eur_per_mtok` per 1M, rounded up like [`cost_micro_eur`].
pub fn per_mtok(tokens: u64, micro_eur_per_mtok: i64) -> i64 {
	round_up_mtok(i128::from(tokens) * i128::from(micro_eur_per_mtok))
}

fn round_up_mtok(total: i128) -> i64 {
	i64::try_from((total + 999_999) / 1_000_000).unwrap_or(i64::MAX)
}

/// `llm.price.<provider>:<model>`, or `None` when unset.
///
/// # Errors
/// A settings read failure or a malformed price.
pub async fn price(app: &App, target: &Target) -> ClResult<Option<Price>> {
	let raw = app
		.settings
		.text(&format!("llm.price.{}:{}", target.provider, target.model))
		.await?;
	if raw.trim().is_empty() {
		return Ok(None);
	}
	parse_price(&raw).map(Some)
}

/// Append one ledger row. Public so `mintworks-search` records its searches and fetches here too.
///
/// # Errors
/// `E-LLM-CONFIG` without a store, or the store's error.
pub async fn record(app: &App, row: &UsageRow) -> ClResult<()> {
	store(app)?.usage_insert(row).await
}

/// Refuse the next call when the global daily ceiling or `subject`'s budget is spent. Run before
/// every provider call, so a call already in flight may overshoot by its own cost.
///
/// # Errors
/// `E-LLM-CEILING`, `E-LLM-BUDGET`, or a store/settings failure.
pub async fn check(app: &App, subject: Option<&str>) -> ClResult<()> {
	let store = store(app)?;
	let ceiling = app.settings.int("llm.daily_ceiling_micro_eur").await?;
	if ceiling > 0 && store.usage_cost_since(day_start()).await? >= ceiling {
		return Err(Error::coded(
			StatusCode::SERVICE_UNAVAILABLE,
			E_CEILING,
			"the daily LLM spending ceiling is reached",
		));
	}
	if let Some(subject) = subject
		&& let Some(budget) = store.budget_get(subject).await?
		&& store.usage_cost_for_subject(subject).await? >= budget
	{
		return Err(Error::coded(
			StatusCode::PAYMENT_REQUIRED,
			E_BUDGET,
			format!("the LLM budget of {subject} is spent"),
		));
	}
	Ok(())
}

/// Set `subject`'s lifetime budget in micro-EUR.
///
/// # Errors
/// `E-CORE-VALIDATION` for a negative budget, or the store's error.
pub async fn set_budget(app: &App, subject: &str, micro_eur: i64) -> ClResult<()> {
	if micro_eur < 0 {
		return Err(Error::validation("an LLM budget cannot be negative"));
	}
	store(app)?.budget_set(subject, micro_eur).await
}

/// `subject`'s lifetime spend and its budget (`None` when uncapped), both in micro-EUR.
///
/// # Errors
/// A store failure.
pub async fn usage(app: &App, subject: &str) -> ClResult<(i64, Option<i64>)> {
	let store = store(app)?;
	Ok((store.usage_cost_for_subject(subject).await?, store.budget_get(subject).await?))
}

/// `A-LLM-CEILING` and `A-LLM-LOADED-COST`, from today's ledger total. Register with
/// `AppBuilder::alerts(mintworks_llm::ledger::alerts)`.
///
/// # Errors
/// A store or settings failure.
pub async fn alerts(app: App) -> ClResult<Vec<Alert>> {
	let spent = store(&app)?.usage_cost_since(day_start()).await?;
	let ceiling = app.settings.int("llm.daily_ceiling_micro_eur").await?;
	let loaded = app.settings.int("llm.alert_loaded_cost_micro_eur").await?;
	let mut out = Vec::new();
	if ceiling > 0 && spent >= ceiling {
		out.push(alert(
			"A-LLM-CEILING",
			Severity::Error,
			spent,
			ceiling,
			"ceiling reached; calls are refused",
		));
	}
	if loaded > 0 && spent >= loaded {
		out.push(alert(
			"A-LLM-LOADED-COST",
			Severity::Warn,
			spent,
			loaded,
			"alert threshold passed",
		));
	}
	Ok(out)
}

fn alert(code: &'static str, severity: Severity, spent: i64, limit: i64, what: &str) -> Alert {
	Alert {
		code,
		severity,
		count: spent,
		message: format!("LLM spend today is {spent} micro-EUR, limit {limit}: {what}"),
		since: Some(day_start()),
		link: None,
	}
}

/// 00:00 UTC today — the "day" of the ceiling and the alerts.
fn day_start() -> Timestamp {
	let now = Timestamp::now().0;
	Timestamp(now - now.rem_euclid(86_400))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cost_rounds_up() {
		let price = Price { input: 150_000, output: 600_000, cached: None };
		let u = |input, output, cached| Usage { input, output, cached };
		assert_eq!(cost_micro_eur(price, u(0, 0, 0)), 0);
		assert_eq!(cost_micro_eur(price, u(1, 0, 0)), 1);
		assert_eq!(cost_micro_eur(price, u(1_000_000, 1_000_000, 0)), 750_000);
		assert_eq!(cost_micro_eur(price, u(10, 10, 0)), 8);
		// No cached price: a cache hit costs the full input rate.
		assert_eq!(cost_micro_eur(price, u(1_000_000, 0, 900_000)), 150_000);
		let deepseek = Price { input: 300_000, output: 1_200_000, cached: Some(6_000) };
		assert_eq!(cost_micro_eur(deepseek, u(1_000_000, 0, 900_000)), 30_000 + 5_400);
		assert_eq!(per_mtok(20_000, 44_034), 881);
		assert_eq!(per_mtok(0, 44_034), 0);
	}
}

// vim: ts=4
