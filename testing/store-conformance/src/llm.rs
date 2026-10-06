//! `LlmStore` conformance: the ledger's sums by subject and by time, and the budget upsert.

use saas_core::prelude::Timestamp;
use saas_llm::store::{LlmStore, UsageKind, UsageRow};

use crate::{Harness, fresh};

fn row(at: i64, subject: Option<&str>, cost: i64) -> UsageRow {
	UsageRow {
		at: Timestamp(at),
		run: None,
		account_id: Some(7),
		org_id: None,
		subject: subject.map(str::to_owned),
		step: "draft".into(),
		kind: UsageKind::Llm,
		provider: "p".into(),
		model: "m".into(),
		tokens_in: 10,
		tokens_out: 20,
		cost_micro_eur: cost,
		retry: false,
	}
}

pub async fn usage_sums_by_subject_and_since<H: Harness>()
where
	H::Store: LlmStore,
{
	let h = fresh!(H, "llm-sums");
	let store = h.store();
	assert_eq!(store.usage_cost_for_subject("s1").await.unwrap(), 0);
	assert_eq!(store.usage_cost_since(Timestamp(0)).await.unwrap(), 0);

	store.usage_insert(&row(100, Some("s1"), 5)).await.unwrap();
	store.usage_insert(&row(200, Some("s1"), 7)).await.unwrap();
	store.usage_insert(&row(300, Some("s2"), 11)).await.unwrap();
	store
		.usage_insert(&UsageRow { kind: UsageKind::Search, retry: true, ..row(400, None, 13) })
		.await
		.unwrap();

	assert_eq!(store.usage_cost_for_subject("s1").await.unwrap(), 12);
	assert_eq!(store.usage_cost_for_subject("s2").await.unwrap(), 11);
	assert_eq!(store.usage_cost_since(Timestamp(200)).await.unwrap(), 31, "`since` is inclusive");
}

pub async fn budget_is_absent_until_set_and_upserts<H: Harness>()
where
	H::Store: LlmStore,
{
	let h = fresh!(H, "llm-budget");
	let store = h.store();
	assert_eq!(store.budget_get("s1").await.unwrap(), None);
	store.budget_set("s1", 50_000).await.unwrap();
	store.budget_set("s1", 500_000).await.unwrap();
	assert_eq!(store.budget_get("s1").await.unwrap(), Some(500_000));
	assert_eq!(store.budget_get("s2").await.unwrap(), None);
}

// vim: ts=4
