// SPDX-License-Identifier: MPL-2.0
//! Transaction binding: what `begin` promises about the handle it hands back, when a bound
//! handle goes stale, how a dropped transaction unwinds, that a savepoint stack stays a stack,
//! and the ambient `scope_writes` scope. A backend reproduces these through its own transaction
//! type, error wording included (`used after its transaction ended`,
//! `a nested write_tx is already open`).
//!
//! `ObjectStore` is only the vehicle: it is the shortest store method that writes, and the
//! `objects` tests cover it in its own right.

use mintworks_core::objects::ObjectStore;
use mintworks_core::prelude::*;
use mintworks_core::store::{AuditEntry, CoreStore};
use serde_json::json;

use crate::{Harness, eventually, fresh};

/// The fixture's org: `migrate` seeds the platform root at id 1, and `objects.org_id` needs
/// nothing more than a live `orgs` row.
const ORG: i64 = 1;

/// A fresh database plus the one account the audit rows reference.
async fn setup<H: Harness>(h: &H) {
	h.exec(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
		&[],
	)
	.await;
}

fn audit_entry(action: &str) -> AuditEntry {
	AuditEntry {
		at: Timestamp::now(),
		account_id: Some(1),
		org_id: Some(ORG),
		ip: None,
		entity: "invoice".into(),
		entity_id: None,
		action: action.into(),
		detail: None,
		request_id: None,
	}
}

/// `audit_logs` rows with the fixture's action, read outside any transaction so the assertion
/// never waits on whoever holds the writer connection.
async fn issued<H: Harness>(h: &H) -> i64 {
	h.scalar_i64("SELECT COUNT(*) FROM audit_logs WHERE action = 'ISSUE'", &[])
		.await
}

/// The handle rule from the caller's side: a write through the handle `begin()` bound joins the
/// transaction, and the outer transaction stays the unit of work.
pub async fn a_write_through_a_bound_handle_joins_the_transaction<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-nested");
	setup(&h).await;
	let store = h.store();

	// `object_put` opens its own `write_tx`. Given the handle `begin()` bound, that must be a
	// `SAVEPOINT` inside the outer transaction, not a second `BEGIN IMMEDIATE` queueing behind the
	// one writer connection until `acquire_timeout`.
	{
		let (tx, bound) = H::begin(store).await.unwrap();
		bound.object_put(ORG, "booking", "bk_1", &json!({"a": 1}), &[]).await.unwrap();
		H::commit(tx).await.unwrap();
	}
	assert!(store.object_get(ORG, "booking", "bk_1").await.unwrap().is_some());

	// A write through the bound handle in a transaction that rolls back goes with it, even though
	// the nested savepoint was released. Each binding is scoped: a bound handle keeps the writer
	// connection checked out until it drops, so the next `begin()` would queue behind it.
	{
		let (tx, bound) = H::begin(store).await.unwrap();
		bound.object_put(ORG, "booking", "bk_2", &json!({"a": 2}), &[]).await.unwrap();
		H::rollback(tx).await.unwrap();
	}
	assert!(store.object_get(ORG, "booking", "bk_2").await.unwrap().is_none());
}

/// `tokio::join!` polls both futures on one task: inferring re-entrancy from the task would join
/// the second write to the first's transaction, and a rollback of one would take the other.
pub async fn two_writes_joined_on_one_task_do_not_share_a_transaction<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-joined");
	setup(&h).await;
	let store = h.store();

	let holder = store.clone();
	let waiter = store.clone();
	let ((), other) = tokio::join!(
		async move {
			let (tx, bound) = H::begin(&holder).await.unwrap();
			// Yield so the joined future is polled while this transaction is open.
			tokio::task::yield_now().await;
			bound
				.object_put(ORG, "booking", "bk_held", &json!({"h": 1}), &[])
				.await
				.unwrap();
			H::rollback(tx).await.unwrap();
		},
		async move { waiter.object_put(ORG, "booking", "bk_other", &json!({"o": 2}), &[]).await }
	);
	other.unwrap();

	assert!(store.object_get(ORG, "booking", "bk_held").await.unwrap().is_none());
	assert!(
		store.object_get(ORG, "booking", "bk_other").await.unwrap().is_some(),
		"the joined write was rolled back with the other transaction"
	);
}

/// An audit row is evidence an attempt happened, so a rollback must not take it with the
/// attempt. `audit_detached` is the method for that: SQLite buffers it on the bound handle and
/// flushes after the transaction ends, PostgreSQL writes it at once on a second writer
/// connection — either way it survives the rollback.
///
/// The only test over a bound handle's detached audit — every call site in the tree passes the
/// pooled `app.store`.
pub async fn an_audit_row_raised_on_a_bound_handle_survives_a_rollback<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-audit-rollback");
	setup(&h).await;

	let (tx, bound) = H::begin(h.store()).await.unwrap();
	bound.audit_detached(&audit_entry("ISSUE")).await.unwrap();
	H::rollback(tx).await.unwrap();

	assert_eq!(issued(&h).await, 1);
}

/// The other half of the split: a row recording a mutation that succeeded is not evidence of an
/// attempt, so the rollback that undoes the mutation must take the row with it.
pub async fn an_audit_row_written_on_a_bound_handle_rolls_back_with_it<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-audit-rolled-back");
	setup(&h).await;

	let (tx, bound) = H::begin(h.store()).await.unwrap();
	bound.audit_log(&audit_entry("ISSUE")).await.unwrap();
	H::rollback(tx).await.unwrap();

	assert_eq!(issued(&h).await, 0, "a rolled-back operation left a row saying it happened");
}

/// A bound handle outlives the transaction it is bound to, and the connection the transaction
/// just gave up is still the one it points at: without the closed flag the write lands in
/// autocommit, in no transaction at all, with nothing to notice.
pub async fn a_write_through_a_bound_handle_after_commit_is_an_error<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-bound-after-commit");
	setup(&h).await;
	let store = h.store();

	let (tx, bound) = H::begin(store).await.unwrap();
	H::commit(tx).await.unwrap();

	let err = bound
		.object_put(ORG, "booking", "bk_late", &json!({ "late": 1 }), &[])
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-INTERNAL", "{err:?}");
	assert!(format!("{err:?}").contains("used after its transaction ended"), "{err:?}");
	assert!(store.object_get(ORG, "booking", "bk_late").await.unwrap().is_none());
}

/// The same rule on the drop path, where the rollback is spawned: the scope closes in `Drop`
/// itself, so a write through the bound clone cannot slip into the doomed transaction — or,
/// once the rollback has landed, into autocommit.
pub async fn a_write_through_a_bound_handle_after_drop_is_an_error<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-bound-after-drop");
	setup(&h).await;
	let store = h.store();

	let (tx, bound) = H::begin(store).await.unwrap();
	drop(tx);

	let err = bound
		.object_put(ORG, "booking", "bk_dropped", &json!({ "a": 1 }), &[])
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-INTERNAL", "{err:?}");
	assert!(format!("{err:?}").contains("used after its transaction ended"), "{err:?}");
	assert!(store.object_get(ORG, "booking", "bk_dropped").await.unwrap().is_none());
}

/// A savepoint is a stack, so two scopes open at once under the same parent have no meaning:
/// either one's `ROLLBACK TO` would undo the other's statements. The second is rejected, and
/// the first is left a working transaction.
pub async fn a_second_nested_write_tx_is_rejected_not_stacked<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-sibling");
	setup(&h).await;
	let store = h.store();

	let (tx, bound) = H::begin(store).await.unwrap();
	let (first, second) = tokio::join!(H::write_tx(&bound), H::write_tx(&bound));
	let live = match (first, second) {
		(Ok(live), Err(_)) | (Err(_), Ok(live)) => live,
		(Ok(_), Ok(_)) => panic!("two savepoints open at once under one scope"),
		(Err(a), Err(b)) => panic!("neither scope opened: {a:?} / {b:?}"),
	};

	H::tx_exec(
		&live,
		"INSERT INTO objects (org_id, type, uid, body, created_at, updated_at)
		 VALUES (?, 'booking', 'bk_sib', '{}', 0, 0)",
		&[json!(ORG)],
	)
	.await;
	H::commit(live).await.unwrap();
	H::commit(tx).await.unwrap();

	assert!(store.object_get(ORG, "booking", "bk_sib").await.unwrap().is_some());
}

/// A read through a bound handle must see the transaction's own uncommitted writes; a pooled
/// reader is a different connection and cannot. Every read-modify-write inside `begin()` is one.
pub async fn bound_handle_reads_its_own_writes<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-bound-reads");
	setup(&h).await;
	let store = h.store();

	let (tx, bound) = H::begin(store).await.unwrap();
	bound
		.object_put(ORG, "booking", "bk_own", &json!({ "a": 1 }), &[])
		.await
		.unwrap();

	let seen = bound.object_get(ORG, "booking", "bk_own").await.unwrap();
	assert_eq!(seen.map(|o| o.body), Some(json!({ "a": 1 })), "before the commit");
	assert!(store.object_get(ORG, "booking", "bk_own").await.unwrap().is_none());

	H::commit(tx).await.unwrap();
}

/// A bound handle parked in a struct must not keep the writer connection checked out past its
/// transaction: every later write in the process would block until `acquire_timeout`.
pub async fn commit_returns_the_writer_connection<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-conn-returned");
	setup(&h).await;
	let store = h.store();

	let (tx, bound) = H::begin(store).await.unwrap();
	H::commit(tx).await.unwrap();

	let write = tokio::time::timeout(
		std::time::Duration::from_secs(5),
		store.object_put(ORG, "booking", "bk_next", &json!({ "a": 1 }), &[]),
	)
	.await
	.expect("the writer connection never came back");
	write.unwrap();

	// Kept alive across the write on purpose: that is the shape that wedged the pool.
	drop(bound);
}

/// Every scope closes, not only the outermost: a handle bound by a *nested* `begin()` stays open
/// after its savepoint is released, and its writes would join the outer transaction.
pub async fn nested_bound_handle_errors_after_release<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-nested-closed");
	setup(&h).await;
	let store = h.store();

	let (tx, bound) = H::begin(store).await.unwrap();
	let (inner, inner_bound) = H::begin(&bound).await.unwrap();
	H::commit(inner).await.unwrap();

	let err = inner_bound
		.object_put(ORG, "booking", "bk_nested", &json!({ "a": 1 }), &[])
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-INTERNAL", "{err:?}");

	// The outer transaction is untouched by its inner scope ending.
	bound
		.object_put(ORG, "booking", "bk_outer", &json!({ "a": 2 }), &[])
		.await
		.unwrap();
	H::commit(tx).await.unwrap();
	assert!(store.object_get(ORG, "booking", "bk_outer").await.unwrap().is_some());
	assert!(store.object_get(ORG, "booking", "bk_nested").await.unwrap().is_none());
}

/// `RELEASE` frees every savepoint nested inside it, so a dropped nested transaction must not
/// leave a name behind for a later `ROLLBACK TO` to abort on.
pub async fn dropped_nested_tx_does_not_poison_a_later_statement<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-stale-savepoint");
	setup(&h).await;
	let store = h.store();

	let (tx, bound) = H::begin(store).await.unwrap();
	let (mid, mid_bound) = H::begin(&bound).await.unwrap();
	let inner = H::write_tx(&mid_bound).await.unwrap();
	H::commit(mid).await.unwrap();
	drop(inner);

	bound
		.object_put(ORG, "booking", "bk_after", &json!({ "a": 1 }), &[])
		.await
		.unwrap();
	H::commit(tx).await.unwrap();
	assert!(store.object_get(ORG, "booking", "bk_after").await.unwrap().is_some());
}

/// Sibling scopes are legal one after another, and only one at a time: the rejection is about a
/// scope still being *live*, not about one having been opened before.
pub async fn nested_scopes_reopen_in_sequence<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-sequential-siblings");
	setup(&h).await;
	let store = h.store();

	let (tx, bound) = H::begin(store).await.unwrap();
	for uid in ["bk_a", "bk_b"] {
		let (nested, nested_bound) = H::begin(&bound).await.unwrap();
		nested_bound
			.object_put(ORG, "booking", uid, &json!({ "a": 1 }), &[])
			.await
			.unwrap();
		H::commit(nested).await.unwrap();
	}

	// And the sibling window is shut while one is live, not merely after the first ever opened.
	let live = H::write_tx(&bound).await.unwrap();
	let Err(err) = H::write_tx(&bound).await else { panic!("two savepoints open at once") };
	assert!(format!("{err:?}").contains("a nested write_tx is already open"), "{err:?}");
	H::commit(live).await.unwrap();

	H::commit(tx).await.unwrap();
	assert!(store.object_get(ORG, "booking", "bk_a").await.unwrap().is_some());
	assert!(store.object_get(ORG, "booking", "bk_b").await.unwrap().is_some());
}

/// Committing the outer transaction ends every scope inside it — its `COMMIT` released their
/// savepoints — so a handle bound by a nested `begin()` must not go on writing in autocommit.
pub async fn an_ancestor_commit_ends_the_scopes_inside_it<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-ancestor-commit");
	setup(&h).await;
	let store = h.store();

	let (tx, bound) = H::begin(store).await.unwrap();
	let (nested, nested_bound) = H::begin(&bound).await.unwrap();
	H::commit(tx).await.unwrap();

	let err = nested_bound
		.object_put(ORG, "booking", "bk_orphan", &json!({ "a": 1 }), &[])
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-INTERNAL", "{err:?}");
	assert!(format!("{err:?}").contains("used after its transaction ended"), "{err:?}");
	drop(nested);
	assert!(store.object_get(ORG, "booking", "bk_orphan").await.unwrap().is_none());
}

/// The commit path flushes what it buffered, not only the drop path: `end` closes the scope
/// before the flush awaits, so the two orders must both leave the row written.
pub async fn commit_flushes_buffered_audit<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-audit-committed");
	setup(&h).await;

	let (tx, bound) = H::begin(h.store()).await.unwrap();
	bound.audit_detached(&audit_entry("ISSUE")).await.unwrap();
	H::commit(tx).await.unwrap();

	assert_eq!(issued(&h).await, 1);
}

/// A transaction that ends by being dropped still flushes the detached rows it is holding.
pub async fn dropped_tx_flushes_buffered_audit<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-audit-dropped");
	setup(&h).await;

	let (tx, bound) = H::begin(h.store()).await.unwrap();
	bound.audit_detached(&audit_entry("ISSUE")).await.unwrap();
	drop(tx);

	eventually(async || issued(&h).await == 1).await;
}

/// The ambient half of the mechanism: `scope_writes` joins every *pooled* handle on the task to
/// one transaction, for the caller — a script's `tx::with` — that cannot hand a bound handle to
/// what runs inside it.
pub async fn a_pooled_handle_inside_scope_writes_joins_the_transaction<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-ambient-joins");
	setup(&h).await;
	let store = h.store();

	let tx = H::write_tx(store).await.unwrap();
	// A separate handle, never told about `tx`: it is the task that carries the scope.
	let pooled = store.clone();
	H::scope_writes(&tx, async {
		pooled
			.object_put(ORG, "booking", "bk_amb", &json!({ "a": 1 }), &[])
			.await
			.unwrap();
	})
	.await;
	H::rollback(tx).await.unwrap();

	assert!(store.object_get(ORG, "booking", "bk_amb").await.unwrap().is_none());
}

/// A bound handle names its transaction; the ambient scope is only the fallback for a handle
/// that could not be rebound. So inside a scope over the outer transaction, a handle bound to a
/// savepoint follows the **savepoint**, and a pooled one follows the scope.
pub async fn a_bound_handle_wins_over_the_ambient_scope<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-ambient-bound");
	setup(&h).await;
	let store = h.store();

	let (outer, bound_outer) = H::begin(store).await.unwrap();
	let (inner, bound_inner) = H::begin(&bound_outer).await.unwrap();
	let pooled = store.clone();
	H::scope_writes(&outer, async move {
		bound_inner
			.object_put(ORG, "booking", "bk_bound", &json!({ "a": 1 }), &[])
			.await
			.unwrap();
		// Ended inside the scope: a savepoint is a stack, so the pooled write below could not
		// open its own under `outer` while `inner` is still live.
		H::rollback(inner).await.unwrap();
		pooled
			.object_put(ORG, "booking", "bk_ambient", &json!({ "a": 2 }), &[])
			.await
			.unwrap();
	})
	.await;
	H::commit(outer).await.unwrap();

	assert!(store.object_get(ORG, "booking", "bk_bound").await.unwrap().is_none());
	assert!(store.object_get(ORG, "booking", "bk_ambient").await.unwrap().is_some());
}

/// The scope is task-local, so a `tokio::spawn` inside it is a different task and gets a pooled
/// connection in autocommit. The sharpest edge of the mechanism, and the one a second adapter
/// most needs stated: its write outlives the scope's rollback.
pub async fn a_spawned_task_does_not_inherit_the_ambient_scope<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-ambient-spawn");
	setup(&h).await;
	let store = h.store();

	let tx = H::write_tx(store).await.unwrap();
	let spawned = store.clone();
	// The handle leaves through a binding rather than the block's tail: an `async` block whose
	// value is itself a future is `clippy::async_yields_async`.
	let mut handle = None;
	H::scope_writes(&tx, async {
		handle = Some(tokio::spawn(async move {
			spawned.object_put(ORG, "booking", "bk_spawn", &json!({ "a": 1 }), &[]).await
		}));
	})
	.await;
	let handle = handle.unwrap();
	// Rolled back first: on a single-writer backend the spawned write blocks on the connection
	// `tx` holds until it ends, so awaiting the join handle before this deadlocks.
	H::rollback(tx).await.unwrap();
	handle.await.unwrap().unwrap();

	assert!(store.object_get(ORG, "booking", "bk_spawn").await.unwrap().is_some());
}

/// The scope ends with the future, not with the transaction: a pooled handle is back in
/// autocommit afterwards rather than pointing at a transaction that has since gone stale.
pub async fn the_ambient_scope_ends_with_the_future<H: Harness>()
where
	H::Store: ObjectStore + CoreStore,
{
	let h = fresh!(H, "tx-ambient-ends");
	setup(&h).await;
	let store = h.store();

	let tx = H::write_tx(store).await.unwrap();
	H::scope_writes(&tx, async {
		store
			.object_put(ORG, "booking", "bk_in", &json!({ "a": 1 }), &[])
			.await
			.unwrap();
	})
	.await;
	H::rollback(tx).await.unwrap();

	store
		.object_put(ORG, "booking", "bk_out", &json!({ "a": 2 }), &[])
		.await
		.unwrap();
	assert!(store.object_get(ORG, "booking", "bk_in").await.unwrap().is_none());
	assert!(store.object_get(ORG, "booking", "bk_out").await.unwrap().is_some());
}

// vim: ts=4
