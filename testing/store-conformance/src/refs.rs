//! `RefStore` conformance: case-insensitive codes, one use per org, the `uses_left` guard under
//! contention, expiry and revocation, and the orphaned hold.

use mintworks_core::ids::RefId;
use mintworks_core::refs::{NewRef, RefStatus, RefStore};
use mintworks_core::store::CoreStore;
use mintworks_core::types::Timestamp;
use serde_json::json;

use crate::{Harness, fresh};

fn new_ref(org_id: i64, code: &str, uses_left: Option<i64>) -> NewRef {
	NewRef {
		uid: RefId::generate(),
		code: code.to_owned(),
		ref_type: "signup".to_owned(),
		org_id,
		created_by: None,
		target: None,
		email: None,
		params: json!({"admits": true}),
		uses_left,
		expires_at: None,
	}
}

pub async fn insert_and_read_back_case_insensitively<H: Harness>()
where
	H::Store: RefStore + CoreStore,
{
	let h = fresh!(H, "refs-read");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let r = store.ref_insert(&new_ref(root, "acme-launch", Some(3))).await.unwrap();
	assert_eq!(r.params, json!({"admits": true}));
	assert!(!r.org_name.is_empty());

	let by_code = store.ref_by_code("ACME-Launch").await.unwrap().unwrap();
	assert_eq!(by_code.uid, r.uid);
	assert_eq!(store.ref_by_uid(&r.uid).await.unwrap().unwrap().code, "acme-launch");

	let err = store.ref_insert(&new_ref(root, "Acme-Launch", None)).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-SLUG-TAKEN");

	assert_eq!(store.refs_of_org(root, Some("signup")).await.unwrap().len(), 1);
	assert!(store.refs_of_org(root, Some("coupon")).await.unwrap().is_empty());
	assert_eq!(store.refs_of_org(root, None).await.unwrap().len(), 1);
}

pub async fn redeem_decrements_once_and_repeat_is_idempotent<H: Harness>()
where
	H::Store: RefStore + CoreStore,
{
	let h = fresh!(H, "refs-redeem");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let r = store.ref_insert(&new_ref(root, "two-uses", Some(2))).await.unwrap();
	let now = Timestamp::now();

	let (first, fresh) = store.ref_redeem(r.id, 10, root, None, now).await.unwrap().unwrap();
	assert!(fresh);
	let (again, fresh) = store.ref_redeem(r.id, 10, root, None, now).await.unwrap().unwrap();
	assert_eq!(first, again);
	assert!(!fresh, "a repeat use is not fresh");
	assert_eq!(store.ref_by_uid(&r.uid).await.unwrap().unwrap().uses_left, Some(1));
	assert_eq!(store.ref_use_of(r.id, 10).await.unwrap(), Some(first.clone()));

	let (other, root_use) = store.ref_redeem(r.id, 11, root, None, now).await.unwrap().unwrap();
	assert_eq!((other, root_use), (first.clone(), false), "one use per org");
	assert!(store.ref_redeem(r.id, 11, root + 11, None, now).await.unwrap().unwrap().1);
	assert!(store.ref_redeem(r.id, 12, root + 12, None, now).await.unwrap().is_none(), "exhausted");
	assert_eq!(store.ref_use_of(r.id, 12).await.unwrap(), None);

	assert_eq!(store.ref_by_id(r.id).await.unwrap().unwrap().uid, r.uid);
	assert_eq!(store.ref_uses_of_org(root).await.unwrap(), vec![first]);
	assert!(store.ref_uses_of_org(root + 1_000).await.unwrap().is_empty());
}

pub async fn expired_and_revoked_refuse<H: Harness>()
where
	H::Store: RefStore + CoreStore,
{
	let h = fresh!(H, "refs-refuse");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let now = Timestamp::now();

	let mut expiring = new_ref(root, "expiring", None);
	expiring.expires_at = Some(Timestamp(now.0 + 10));
	let expiring = store.ref_insert(&expiring).await.unwrap();
	assert!(
		store
			.ref_redeem(expiring.id, 1, root, None, Timestamp(now.0 + 10))
			.await
			.unwrap()
			.is_none()
	);
	assert!(store.ref_redeem(expiring.id, 1, root, None, now).await.unwrap().is_some());

	let revoked = store.ref_insert(&new_ref(root, "revoked", None)).await.unwrap();
	let set = |org, status| store.ref_set_status(org, &revoked.uid, status);
	assert!(!set(root + 1_000, RefStatus::Revoked).await.unwrap(), "another org's");
	assert!(set(root, RefStatus::Revoked).await.unwrap());
	assert!(store.ref_redeem(revoked.id, 1, root, None, now).await.unwrap().is_none());

	assert!(!set(root + 1_000, RefStatus::Active).await.unwrap(), "another org's");
	assert!(set(root, RefStatus::Active).await.unwrap());
	assert!(store.ref_redeem(revoked.id, 1, root, None, now).await.unwrap().is_some());
}

pub async fn concurrent_redeem_of_a_single_use_succeeds_once<H: Harness>()
where
	H::Store: RefStore + CoreStore,
{
	let h = fresh!(H, "refs-race");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let r = store.ref_insert(&new_ref(root, "only-once", Some(1))).await.unwrap();
	let now = Timestamp::now();

	// One org per account, so the `uses_left` guard decides, not the one-use-per-org check.
	let tasks: Vec<_> = (0..8)
		.map(|account| {
			let store = store.clone();
			let org = root + 100 + account;
			tokio::spawn(
				async move { store.ref_redeem(r.id, account, org, None, now).await.unwrap() },
			)
		})
		.collect();
	let mut wins = 0;
	for t in tasks {
		wins += usize::from(t.await.unwrap().is_some());
	}
	assert_eq!(wins, 1);
	assert_eq!(store.ref_by_uid(&r.uid).await.unwrap().unwrap().uses_left, Some(0));
}

/// The per-org check ran before the transaction, so two members of one org redeeming at once
/// both got a fresh use.
pub async fn concurrent_redeem_by_one_org_succeeds_once<H: Harness>()
where
	H::Store: RefStore + CoreStore,
{
	let h = fresh!(H, "refs-org-race");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let r = store.ref_insert(&new_ref(root, "org-once", None)).await.unwrap();
	let now = Timestamp::now();

	let tasks: Vec<_> = (0..8)
		.map(|account| {
			let store = store.clone();
			tokio::spawn(
				async move { store.ref_redeem(r.id, account, root, None, now).await.unwrap() },
			)
		})
		.collect();
	let mut fresh = 0;
	for t in tasks {
		fresh += usize::from(t.await.unwrap().unwrap().1);
	}
	assert_eq!(fresh, 1);
	assert_eq!(store.ref_uses_of_org(root).await.unwrap().len(), 1);
}

/// No invoice has this id: the hold's draft is gone.
const GONE: i64 = 999_999;

pub async fn a_hold_whose_draft_is_gone_is_reclaimed<H: Harness>()
where
	H::Store: RefStore + CoreStore,
{
	let h = fresh!(H, "refs-orphan");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let r = store.ref_insert(&new_ref(root, "held-once", Some(1))).await.unwrap();
	let now = Timestamp::now();
	assert!(store.ref_redeem(r.id, 1, root + 1, Some(GONE), now).await.unwrap().unwrap().1);
	assert_eq!(store.ref_use_of(r.id, 1).await.unwrap(), None, "an orphan reads as unused");
	assert!(store.ref_uses_of_org(root + 1).await.unwrap().is_empty());

	assert!(store.ref_redeem(r.id, 2, root + 2, None, now).await.unwrap().unwrap().1);
	assert_eq!(store.ref_by_uid(&r.uid).await.unwrap().unwrap().uses_left, Some(0));
}

// vim: ts=4
