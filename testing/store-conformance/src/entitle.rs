// SPDX-License-Identifier: MPL-2.0
//! `EntitleStore` conformance: expiry-ordered draining, idempotent debits, overdraft placement,
//! the no-overdraw guarantee under contention, `grants_cut` and `source_ref` deduplication,
//! and seat assignments.

use mintworks_core::error::Error;
use mintworks_core::ids::GrantId;
use mintworks_core::store::CoreStore;
use mintworks_core::types::Timestamp;
use mintworks_entitle::{Debit, EntitleStore, NewGrant, SeatOutcome, Source};
use serde_json::json;

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

async fn seat_org<H: Harness>(h: &H, parent: i64, name: &str) -> (i64, String) {
	let uid = format!("org_seat_{name}");
	h.exec(
		"INSERT INTO orgs (uid, parent_id, kind, name, created_at) VALUES (?, ?, 'SHARED', ?, 0)",
		&[json!(uid), json!(parent), json!(name)],
	)
	.await;
	(h.scalar_i64("SELECT id FROM orgs WHERE uid = ?", &[json!(uid)]).await, uid)
}

async fn seat_account<H: Harness>(h: &H, name: &str) -> i64 {
	let uid = format!("acc_seat_{name}");
	h.exec(
		"INSERT INTO accounts (uid, email, created_at) VALUES (?, ?, 0)",
		&[json!(uid), json!(format!("{name}@seat.test"))],
	)
	.await;
	h.scalar_i64("SELECT id FROM accounts WHERE uid = ?", &[json!(uid)]).await
}

/// An accepted `MEMBER` of `org`: `seat_assign` refuses anyone else.
async fn seat_join<H: Harness>(h: &H, org: i64, account: i64) {
	h.exec(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, ?, 'MEMBER', 0, 0)",
		&[json!(org), json!(account)],
	)
	.await;
}

/// A forever grant of `amount` seats on `key`, active from 0.
async fn seat_grant<S: EntitleStore>(store: &S, org_id: i64, key: &str, amount: i64) {
	let g = NewGrant {
		key: key.to_owned(),
		valid_from: Timestamp(0),
		..grant(org_id, amount, None, None)
	};
	store.grant_insert(&g).await.unwrap();
}

async fn stale<S: EntitleStore>(store: &S, account: i64, at: Timestamp) -> Vec<(String, String)> {
	let rows = store.seats_release_stale(account, at).await.unwrap();
	rows.into_iter().map(|(o, k)| (o.as_str().to_owned(), k)).collect()
}

async fn seated<S: EntitleStore>(store: &S, org: i64) -> Vec<String> {
	let seats = store.seats_of_org(org, "seats").await.unwrap();
	seats.into_iter().map(|(a, _)| a.as_str().to_owned()).collect()
}

pub async fn seats_fill_to_max_then_refuse_and_a_reassign_is_already_seated<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-seat-full");
	let store = h.store();
	let (org, _) = seat_org(&h, store.root_org_id().await.unwrap(), "school").await;
	let [a, b, c] = [
		seat_account(&h, "a").await,
		seat_account(&h, "b").await,
		seat_account(&h, "c").await,
	];
	for acc in [a, b, c] {
		seat_join(&h, org, acc).await;
	}
	seat_grant(store, org, "seats", 2).await;
	seat_grant(store, org, "other", 1).await;
	let at = Timestamp(100);
	assert_eq!(store.seat_assign(org, "seats", a, at).await.unwrap(), SeatOutcome::Inserted);
	assert_eq!(store.seat_assign(org, "seats", b, at).await.unwrap(), SeatOutcome::Inserted);
	assert_eq!(store.seat_assign(org, "seats", c, at).await.unwrap(), SeatOutcome::Full(2));
	assert_eq!(store.seat_assign(org, "seats", a, at).await.unwrap(), SeatOutcome::AlreadySeated);
	// Another key counts on its own.
	assert_eq!(store.seat_assign(org, "other", c, at).await.unwrap(), SeatOutcome::Inserted);
	assert_eq!(seated(store, org).await, ["acc_seat_a", "acc_seat_b"]);
}

pub async fn a_released_seat_frees_its_place<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-seat-release");
	let store = h.store();
	let (org, _) = seat_org(&h, store.root_org_id().await.unwrap(), "school").await;
	let [a, b] = [seat_account(&h, "a").await, seat_account(&h, "b").await];
	seat_join(&h, org, a).await;
	seat_join(&h, org, b).await;
	seat_grant(store, org, "seats", 1).await;
	let t = Timestamp(100);
	assert_eq!(store.seat_assign(org, "seats", a, t).await.unwrap(), SeatOutcome::Inserted);
	assert!(store.seat_release(org, "seats", a, t).await.unwrap());
	assert!(!store.seat_release(org, "seats", a, t).await.unwrap(), "a no-op the second time");
	assert_eq!(store.seat_assign(org, "seats", b, t).await.unwrap(), SeatOutcome::Inserted);
	assert_eq!(store.seats_release_account(b, t).await.unwrap(), 1);
	assert!(seated(store, org).await.is_empty());
	let rows = h.scalar_i64("SELECT COUNT(*) FROM seat_assignments", &[]).await;
	assert_eq!(rows, 2, "released rows are kept");
}

pub async fn seats_of_account_lists_only_active_seats<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-seat-account");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let (one, _) = seat_org(&h, root, "one").await;
	let (two, two_uid) = seat_org(&h, root, "two").await;
	let a = seat_account(&h, "a").await;
	for org in [one, two] {
		seat_join(&h, org, a).await;
		seat_grant(store, org, "seats", 5).await;
	}
	let ok = SeatOutcome::Inserted;
	assert_eq!(store.seat_assign(one, "seats", a, Timestamp(100)).await.unwrap(), ok);
	assert_eq!(store.seat_assign(two, "seats", a, Timestamp(110)).await.unwrap(), ok);
	store.seat_release(one, "seats", a, Timestamp(120)).await.unwrap();
	let seats = store.seats_of_account(a).await.unwrap();
	let got: Vec<_> = seats.iter().map(|(o, k, at)| (o.as_str(), k.as_str(), *at)).collect();
	assert_eq!(got, [(two_uid.as_str(), "seats", Timestamp(110))]);
}

pub async fn deleting_an_org_drops_its_seats<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-seat-cascade");
	let store = h.store();
	let (org, _) = seat_org(&h, store.root_org_id().await.unwrap(), "school").await;
	let a = seat_account(&h, "a").await;
	seat_join(&h, org, a).await;
	seat_grant(store, org, "seats", 1).await;
	let got = store.seat_assign(org, "seats", a, Timestamp(100)).await.unwrap();
	assert_eq!(got, SeatOutcome::Inserted);
	h.exec("DELETE FROM orgs WHERE id = ?", &[json!(org)]).await;
	assert_eq!(h.scalar_i64("SELECT COUNT(*) FROM seat_assignments", &[]).await, 0);
	assert!(store.seats_of_account(a).await.unwrap().is_empty());
}

/// A seat whose holder left (its release event lost) neither shows nor holds capacity; the
/// limit is read inside the assign.
pub async fn a_seat_stops_counting_once_its_holder_leaves<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-seat-stale");
	let store = h.store();
	let (org, _) = seat_org(&h, store.root_org_id().await.unwrap(), "school").await;
	let [gone, next, outsider, late] = [
		seat_account(&h, "a").await,
		seat_account(&h, "b").await,
		seat_account(&h, "c").await,
		seat_account(&h, "d").await,
	];
	for acc in [gone, next, late] {
		seat_join(&h, org, acc).await;
	}
	seat_grant(store, org, "seats", 1).await;
	let at = Timestamp(100);
	assert_eq!(store.seat_assign(org, "seats", gone, at).await.unwrap(), SeatOutcome::Inserted);
	h.exec(
		"DELETE FROM memberships WHERE org_id = ? AND account_id = ?",
		&[json!(org), json!(gone)],
	)
	.await;
	assert!(seated(store, org).await.is_empty());
	assert_eq!(store.seat_assign(org, "seats", gone, at).await.unwrap(), SeatOutcome::NotMember);
	assert_eq!(store.seat_assign(org, "seats", next, at).await.unwrap(), SeatOutcome::Inserted);
	assert_eq!(
		store.seat_assign(org, "seats", outsider, at).await.unwrap(),
		SeatOutcome::NotMember
	);
	h.exec("UPDATE grants SET amount = 0 WHERE org_id = ?", &[json!(org)]).await;
	assert_eq!(store.seat_assign(org, "seats", late, at).await.unwrap(), SeatOutcome::Full(0));
}

/// A seat survives while its holder has an accepted membership in the org or any org below
/// it; not one above it.
pub async fn seats_release_stale_walks_down_the_org_tree<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-seat-member");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let (school, school_uid) = seat_org(&h, root, "school").await;
	let (class, _) = seat_org(&h, school, "class").await;
	let [learner, invited, staff] = [
		seat_account(&h, "l").await,
		seat_account(&h, "i").await,
		seat_account(&h, "s").await,
	];
	for (org, acc, accepted) in [
		(class, learner, json!(0)),
		(class, invited, json!(null)),
		(root, staff, json!(0)),
	] {
		h.exec(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES (?, ?, 'MEMBER', ?, 0)",
			&[json!(org), json!(acc), accepted],
		)
		.await;
		h.exec(
			"INSERT INTO seat_assignments (org_id, key, account_id, assigned_at)
			 VALUES (?, 'seats', ?, 0)",
			&[json!(school), json!(acc)],
		)
		.await;
	}
	let released = [(school_uid, "seats".to_owned())];
	assert!(stale(store, learner, Timestamp(1)).await.is_empty(), "a class member qualifies");
	assert_eq!(stale(store, invited, Timestamp(1)).await, released, "not accepted");
	assert_eq!(stale(store, staff, Timestamp(1)).await, released, "a member above the org");
}

/// Qualification spans active descendants only: suspending the class frees its member's seat.
pub async fn seat_qualification_spans_active_descendants_only<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-seat-active");
	let store = h.store();
	let (school, _) = seat_org(&h, store.root_org_id().await.unwrap(), "school").await;
	let (class, _) = seat_org(&h, school, "class").await;
	let kid = seat_account(&h, "k").await;
	let direct = seat_account(&h, "d").await;
	seat_join(&h, class, kid).await;
	seat_join(&h, school, direct).await;
	seat_grant(store, school, "seats", 1).await;
	let at = Timestamp(100);
	assert_eq!(store.seat_assign(school, "seats", kid, at).await.unwrap(), SeatOutcome::Inserted);
	assert_eq!(seated(store, school).await, ["acc_seat_k"]);
	h.exec("UPDATE orgs SET status = 'SUSPENDED' WHERE id = ?", &[json!(class)])
		.await;
	assert!(seated(store, school).await.is_empty());
	let got = store.seat_assign(school, "seats", direct, at).await.unwrap();
	assert_eq!(got, SeatOutcome::Inserted, "the suspended class's seat holds no capacity");
	assert!(store.seats_of_account(kid).await.unwrap().is_empty(), "the assign released it");
	assert!(stale(store, kid, Timestamp(110)).await.is_empty());
}

pub async fn seats_release_stale_spans_orgs<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-seat-stale-orgs");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let (one, one_uid) = seat_org(&h, root, "one").await;
	let (two, _) = seat_org(&h, root, "two").await;
	let a = seat_account(&h, "a").await;
	for org in [one, two] {
		seat_join(&h, org, a).await;
		seat_grant(store, org, "seats", 1).await;
		let got = store.seat_assign(org, "seats", a, Timestamp(100)).await.unwrap();
		assert_eq!(got, SeatOutcome::Inserted);
	}
	h.exec("DELETE FROM memberships WHERE org_id = ? AND account_id = ?", &[json!(one), json!(a)])
		.await;
	assert_eq!(stale(store, a, Timestamp(110)).await, [(one_uid, "seats".to_owned())]);
	assert_eq!(store.seats_of_account(a).await.unwrap().len(), 1);
}

/// A stale row (release event lost) does not count again once its holder rejoins.
pub async fn stale_seat_does_not_revive_on_rejoin<H: Harness>()
where
	H::Store: EntitleStore + CoreStore,
{
	let h = fresh!(H, "entitle-seat-revive");
	let store = h.store();
	let (org, _) = seat_org(&h, store.root_org_id().await.unwrap(), "school").await;
	let a = seat_account(&h, "a").await;
	let b = seat_account(&h, "b").await;
	seat_join(&h, org, a).await;
	seat_join(&h, org, b).await;
	seat_grant(store, org, "seats", 1).await;
	let at = Timestamp(100);
	assert_eq!(store.seat_assign(org, "seats", a, at).await.unwrap(), SeatOutcome::Inserted);
	h.exec("DELETE FROM memberships WHERE org_id = ? AND account_id = ?", &[json!(org), json!(a)])
		.await;
	assert_eq!(store.seat_assign(org, "seats", b, at).await.unwrap(), SeatOutcome::Inserted);
	seat_join(&h, org, a).await;
	assert_eq!(seated(store, org).await, ["acc_seat_b"]);
}

// vim: ts=4
