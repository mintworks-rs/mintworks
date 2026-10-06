//! `EntitleStore` conformance: expiry-ordered draining, idempotent debits, overdraft placement,
//! the no-overdraw guarantee under contention, `grants_cut` and `source_ref` deduplication.

use saas_core::error::Error;
use saas_core::ids::GrantId;
use saas_core::store::CoreStore;
use saas_core::types::Timestamp;
use saas_entitle::{Debit, EntitleStore, NewGrant, Source};

use crate::{Harness, fresh};

fn now() -> i64 {
	Timestamp::now().0
}

fn grant(org_id: i64, amount: i64, until: Option<i64>, source_ref: Option<&str>) -> NewGrant {
	NewGrant {
		uid: GrantId::generate(),
		org_id,
		key: "credits".to_owned(),
		amount,
		valid_from: Timestamp(now() - 10),
		valid_until: until.map(Timestamp),
		source: Source::Manual,
		source_ref: source_ref.map(str::to_owned),
	}
}

fn debit(org_id: i64, amount: i64, idem: &str, overdraw: bool) -> Debit {
	Debit {
		org_id,
		key: "credits".to_owned(),
		amount,
		idem_key: idem.to_owned(),
		account_id: None,
		at: Timestamp::now(),
		overdraw,
	}
}

async fn used<S: EntitleStore>(store: &S, org: i64) -> Vec<(String, i64)> {
	let mut gs = store.grants_active(org, Some("credits"), Timestamp::now()).await.unwrap();
	gs.sort_by_key(|g| g.id);
	gs.into_iter().map(|g| (g.source_ref.unwrap_or_default(), g.used)).collect()
}

pub async fn debit_drains_the_soonest_expiring_grant_first<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-drain");
	let store = h.store();
	let org = store.root_org_id().await.unwrap();
	store
		.grant_insert(&grant(org, 10, Some(now() + 100), Some("late")))
		.await
		.unwrap();
	store
		.grant_insert(&grant(org, 10, Some(now() + 50), Some("soon")))
		.await
		.unwrap();
	store.grant_insert(&grant(org, 10, None, Some("forever"))).await.unwrap();

	assert!(store.usage_debit(&debit(org, 15, "a", false)).await.unwrap());
	assert_eq!(
		used(store, org).await,
		[("late".into(), 5), ("soon".into(), 10), ("forever".into(), 0)]
	);
	assert!(!store.usage_debit(&debit(org, 16, "b", false)).await.unwrap(), "15 left");
}

pub async fn a_retried_idem_key_debits_once<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-idem");
	let store = h.store();
	let org = store.root_org_id().await.unwrap();
	store.grant_insert(&grant(org, 10, None, Some("g"))).await.unwrap();
	assert!(store.usage_debit(&debit(org, 4, "same", false)).await.unwrap());
	assert!(store.usage_debit(&debit(org, 4, "same", false)).await.unwrap());
	assert_eq!(used(store, org).await, [("g".into(), 4)]);
}

pub async fn a_reused_idem_key_with_another_debit_is_refused<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-idem-reuse");
	let store = h.store();
	let org = store.root_org_id().await.unwrap();
	store.grant_insert(&grant(org, 10, None, Some("g"))).await.unwrap();
	assert!(store.usage_debit(&debit(org, 4, "same", false)).await.unwrap());
	let other_key = Debit { key: "seats".to_owned(), ..debit(org, 4, "same", false) };
	for d in [debit(org, 5, "same", false), other_key] {
		let e = store.usage_debit(&d).await.unwrap_err();
		assert!(matches!(e, Error::Coded { code: "E-ENT-IDEM", .. }), "{e:?}");
	}
	assert_eq!(used(store, org).await, [("g".into(), 4)]);
}

pub async fn charge_overdraws_onto_the_last_grant_drained<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-overdraw");
	let store = h.store();
	let org = store.root_org_id().await.unwrap();
	store
		.grant_insert(&grant(org, 5, Some(now() + 50), Some("soon")))
		.await
		.unwrap();
	store.grant_insert(&grant(org, 5, None, Some("forever"))).await.unwrap();
	assert!(store.usage_debit(&debit(org, 13, "c", true)).await.unwrap());
	assert_eq!(used(store, org).await, [("soon".into(), 5), ("forever".into(), 8)]);
}

pub async fn charge_with_no_grant_goes_negative_and_a_later_grant_nets_it<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-overdraw-no-grant");
	let store = h.store();
	let org = store.root_org_id().await.unwrap();
	let balance = || async {
		let active = store.grants_active(org, Some("credits"), Timestamp::now()).await.unwrap();
		active.iter().map(|g| g.amount - g.used).sum::<i64>()
	};
	assert!(store.usage_debit(&debit(org, 5, "o", true)).await.unwrap());
	assert_eq!(balance().await, -5);
	store.grant_insert(&grant(org, 10, None, Some("g"))).await.unwrap();
	assert_eq!(balance().await, 5);
}

/// Kept as designed: the overdraft lives on its grant and never carries into the next one.
pub async fn an_overdraft_is_forgiven_when_its_grant_expires<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-overdraw-expiry");
	let store = h.store();
	let org = store.root_org_id().await.unwrap();
	let t = now() + 50;
	store.grant_insert(&grant(org, 5, Some(t), Some("p1"))).await.unwrap();
	assert!(store.usage_debit(&debit(org, 8, "o", true)).await.unwrap());
	let next = NewGrant { valid_from: Timestamp(t), ..grant(org, 5, None, Some("p2")) };
	store.grant_insert(&next).await.unwrap();
	let active = store.grants_active(org, Some("credits"), Timestamp(t + 1)).await.unwrap();
	assert_eq!(active.iter().map(|g| g.amount - g.used).sum::<i64>(), 5);
}

pub async fn concurrent_consumes_never_overdraw<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-race");
	let store = h.store();
	let org = store.root_org_id().await.unwrap();
	store.grant_insert(&grant(org, 5, None, Some("g"))).await.unwrap();
	let tasks: Vec<_> = (0..8)
		.map(|i| {
			let store = store.clone();
			tokio::spawn(async move {
				store.usage_debit(&debit(org, 1, &format!("k{i}"), false)).await.unwrap()
			})
		})
		.collect();
	let mut won = 0;
	for t in tasks {
		won += usize::from(t.await.unwrap());
	}
	assert_eq!(won, 5);
	assert_eq!(used(store, org).await, [("g".into(), 5)]);
}

pub async fn cut_ends_the_grant_and_keeps_its_usage<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-cut");
	let store = h.store();
	let org = store.root_org_id().await.unwrap();
	store.grant_insert(&grant(org, 10, None, Some("sub"))).await.unwrap();
	assert!(store.usage_debit(&debit(org, 3, "d", false)).await.unwrap());
	let at = Timestamp::now();
	assert_eq!(store.grants_cut(org, Source::Manual, "sub", at).await.unwrap(), 1);
	assert!(used(store, org).await.is_empty());
	let all = store.grants_of_org(org).await.unwrap();
	assert_eq!((all[0].used, all[0].valid_until), (3, Some(at)));
	// A later cut never lengthens it.
	assert_eq!(
		store
			.grants_cut(org, Source::Manual, "sub", Timestamp(at.0 + 99))
			.await
			.unwrap(),
		0
	);
}

pub async fn grant_insert_is_idempotent_on_its_source_ref<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-grant");
	let store = h.store();
	let org = store.root_org_id().await.unwrap();
	let a = store.grant_insert(&grant(org, 10, None, Some("r"))).await.unwrap();
	let b = store.grant_insert(&grant(org, 99, None, Some("r"))).await.unwrap();
	assert_eq!((a.uid.as_str(), b.amount), (b.uid.as_str(), 10));
	// A NULL ref is never deduplicated.
	let null_a = store.grant_insert(&grant(org, 1, None, None)).await.unwrap();
	let null_b = store.grant_insert(&grant(org, 1, None, None)).await.unwrap();
	assert_ne!(null_a.uid.as_str(), null_b.uid.as_str());
	assert_eq!(store.grants_of_org(org).await.unwrap().len(), 3);
}

// vim: ts=4
