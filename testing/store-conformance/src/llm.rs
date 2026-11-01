// SPDX-License-Identifier: MPL-2.0
//! `LlmStore` conformance: the ledger's sums by subject and by time, the budget upsert and the
//! rolling windows.

use mintworks_core::prelude::Timestamp;
use mintworks_llm::store::{LlmStore, UsageDim, UsageKind, UsageQuery, UsageRow};

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

pub async fn usage_grouped_buckets_by_dimension_and_utc_day<H: Harness>()
where
	H::Store: LlmStore,
{
	let h = fresh!(H, "llm-grouped");
	let store = h.store();
	let q = |by: Vec<UsageDim>, until| UsageQuery {
		since: Timestamp(0),
		until,
		by,
		org_id: None,
		subject: None,
		kind: None,
		account_id: None,
	};
	assert!(store.usage_grouped(&q(vec![], None)).await.unwrap().is_empty());

	// 86_399 is 1970-01-01T23:59:59Z, 86_400 the next UTC day.
	store.usage_insert(&row(86_399, None, 5)).await.unwrap();
	store
		.usage_insert(&UsageRow { step: "chat".into(), ..row(86_400, None, 7) })
		.await
		.unwrap();
	store
		.usage_insert(&UsageRow { step: "chat".into(), ..row(90_000, None, 11) })
		.await
		.unwrap();

	let total = store.usage_grouped(&q(vec![], None)).await.unwrap();
	assert_eq!(total.len(), 1);
	assert_eq!(
		(total[0].calls, total[0].input, total[0].output, total[0].micro_eur),
		(3, 30, 60, 23)
	);
	assert_eq!(total[0].step, None);

	let by = store
		.usage_grouped(&q(vec![UsageDim::Step, UsageDim::Day], None))
		.await
		.unwrap();
	let keys: Vec<_> = by
		.iter()
		.map(|g| (g.step.as_deref(), g.day.as_deref(), g.calls, g.micro_eur))
		.collect();
	assert_eq!(
		keys,
		vec![
			(Some("chat"), Some("1970-01-02"), 2, 18),
			(Some("draft"), Some("1970-01-01"), 1, 5)
		]
	);
	assert_eq!(by[0].provider, None);

	let bounded = store
		.usage_grouped(&q(vec![UsageDim::Day], Some(Timestamp(86_400))))
		.await
		.unwrap();
	assert_eq!(bounded.len(), 1, "`until` is exclusive");
	assert_eq!(bounded[0].day.as_deref(), Some("1970-01-01"));

	store
		.usage_insert(&UsageRow { org_id: Some(1), ..row(100, None, 100) })
		.await
		.unwrap();
	store
		.usage_insert(&UsageRow { org_id: Some(2), ..row(100, None, 1000) })
		.await
		.unwrap();
	let org = store
		.usage_grouped(&UsageQuery { org_id: Some(1), ..q(vec![], None) })
		.await
		.unwrap();
	assert_eq!((org.len(), org[0].calls, org[0].micro_eur), (1, 1, 100), "one org's rows only");
}

pub async fn usage_grouped_filters_by_subject_kind_and_account<H: Harness>()
where
	H::Store: LlmStore,
{
	let h = fresh!(H, "llm-filters");
	let store = h.store();
	store.usage_insert(&row(100, Some("acc_a"), 5)).await.unwrap();
	store
		.usage_insert(&UsageRow { kind: UsageKind::Api, ..row(200, Some("acc_a"), 7) })
		.await
		.unwrap();
	store
		.usage_insert(&UsageRow { account_id: Some(8), ..row(300, Some("platform"), 11) })
		.await
		.unwrap();
	let q = |by: Vec<UsageDim>| UsageQuery {
		since: Timestamp(0),
		until: None,
		by,
		org_id: None,
		subject: None,
		kind: None,
		account_id: None,
	};

	let payer = store
		.usage_grouped(&UsageQuery {
			since: Timestamp(150),
			subject: Some("acc_a".into()),
			..q(vec![])
		})
		.await
		.unwrap();
	assert_eq!((payer.len(), payer[0].micro_eur), (1, 7), "subject and since together");
	let api = store
		.usage_grouped(&UsageQuery { kind: Some(UsageKind::Api), ..q(vec![]) })
		.await
		.unwrap();
	assert_eq!((api[0].calls, api[0].micro_eur), (1, 7));
	let acct = store
		.usage_grouped(&UsageQuery { account_id: Some(8), ..q(vec![]) })
		.await
		.unwrap();
	assert_eq!(acct[0].micro_eur, 11);

	let by: Vec<_> = store
		.usage_grouped(&q(vec![UsageDim::Subject, UsageDim::Account]))
		.await
		.unwrap()
		.into_iter()
		.map(|g| (g.subject, g.account_id, g.micro_eur, g.step))
		.collect();
	assert_eq!(
		by,
		vec![
			(Some("acc_a".into()), Some(7), 12, None),
			(Some("platform".into()), Some(8), 11, None)
		]
	);
}

/// A `NULL` group sorts first on every engine.
pub async fn usage_grouped_sorts_a_null_group_first<H: Harness>()
where
	H::Store: LlmStore,
{
	let h = fresh!(H, "llm-null-first");
	let store = h.store();
	store.usage_insert(&row(100, None, 5)).await.unwrap();
	store
		.usage_insert(&UsageRow { account_id: None, ..row(200, None, 7) })
		.await
		.unwrap();
	let q = UsageQuery {
		since: Timestamp(0),
		until: None,
		by: vec![UsageDim::Account],
		org_id: None,
		subject: None,
		kind: None,
		account_id: None,
	};
	let by: Vec<_> = store
		.usage_grouped(&q)
		.await
		.unwrap()
		.into_iter()
		.map(|g| g.account_id)
		.collect();
	assert_eq!(by, [None, Some(7)]);
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

/// `(window_secs, cap)` of each of the subject's windows.
async fn caps<S: LlmStore>(store: &S, subject: &str) -> Vec<(i64, i64)> {
	let rows = store.budget_windows(subject, Timestamp(0)).await.unwrap();
	rows.into_iter().map(|(w, cap, _, _)| (w, cap)).collect()
}

pub async fn budget_windows_replace_as_a_set_and_clear<H: Harness>()
where
	H::Store: LlmStore,
{
	let h = fresh!(H, "llm-windows");
	let store = h.store();
	assert!(caps(store, "s1").await.is_empty());
	store.budget_windows_set("s1", &[(86_400, 900), (3_600, 100)]).await.unwrap();
	store.budget_windows_set("s2", &[(60, 5)]).await.unwrap();
	assert_eq!(caps(store, "s1").await, [(3_600, 100), (86_400, 900)]);
	store.budget_windows_set("s1", &[(18_000, 0)]).await.unwrap();
	assert_eq!(caps(store, "s1").await, [(18_000, 0)], "replaced, not merged");
	store.budget_windows_set("s1", &[]).await.unwrap();
	assert!(caps(store, "s1").await.is_empty());
	assert_eq!(caps(store, "s2").await, [(60, 5)]);
}

pub async fn budget_windows_sum_one_subject_since_and_report_the_oldest<H: Harness>()
where
	H::Store: LlmStore,
{
	let h = fresh!(H, "llm-usage-window");
	let store = h.store();
	store.budget_windows_set("s1", &[(1_000, 9), (100, 8), (300, 7)]).await.unwrap();
	let empty = [(100, 8, 0, None), (300, 7, 0, None), (1_000, 9, 0, None)];
	assert_eq!(store.budget_windows("s1", Timestamp(300)).await.unwrap(), empty);

	store.usage_insert(&row(100, Some("s1"), 5)).await.unwrap();
	store.usage_insert(&row(200, Some("s1"), 7)).await.unwrap();
	store.usage_insert(&row(300, Some("s1"), 11)).await.unwrap();
	store.usage_insert(&row(250, Some("s2"), 13)).await.unwrap();
	store.usage_insert(&row(260, None, 17)).await.unwrap();

	assert_eq!(
		store.budget_windows("s1", Timestamp(300)).await.unwrap(),
		[
			(100, 8, 18, Some(Timestamp(200))),
			(300, 7, 23, Some(Timestamp(100))),
			(1_000, 9, 23, Some(Timestamp(100))),
		],
		"`now - window_secs` is inclusive"
	);
	let late = store.budget_windows("s1", Timestamp(1_301)).await.unwrap();
	assert_eq!(late, [(100, 8, 0, None), (300, 7, 0, None), (1_000, 9, 0, None)]);
}

pub async fn subject_forget_drops_budgets_and_keeps_the_spend<H: Harness>()
where
	H::Store: LlmStore,
{
	let h = fresh!(H, "llm-forget");
	let store = h.store();
	store.budget_set("s1", 50).await.unwrap();
	store.budget_set("s2", 60).await.unwrap();
	store.budget_windows_set("s1", &[(3_600, 100)]).await.unwrap();
	store.usage_insert(&row(100, Some("s1"), 5)).await.unwrap();
	store.usage_insert(&row(200, Some("s2"), 7)).await.unwrap();

	store.subject_forget("s1").await.unwrap();
	assert_eq!(store.budget_get("s1").await.unwrap(), None);
	assert!(caps(store, "s1").await.is_empty());
	assert_eq!(store.usage_cost_for_subject("s1").await.unwrap(), 0);
	assert_eq!(store.usage_cost_since(Timestamp(0)).await.unwrap(), 12, "the spend stays");
	assert_eq!(store.budget_get("s2").await.unwrap(), Some(60), "another subject is untouched");
	assert_eq!(store.usage_cost_for_subject("s2").await.unwrap(), 7);
}

// vim: ts=4
