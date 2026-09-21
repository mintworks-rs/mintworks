//! Transaction binding — adapter-internal invariants, not conformance: what
//! `SqliteStore::begin` promises about the handle it hands back, when a bound handle goes
//! stale, how a dropped `WriteTx` unwinds, and that a savepoint stack stays a stack.
//!
//! A second store adapter is not expected to reproduce any of this, so it is a sibling of
//! `job_claim.rs` and `migrate.rs` rather than of `auth.rs`/`invoice.rs`/`objects.rs`.
//!
//! `ObjectStore` is only the vehicle: it is the shortest store method that writes, and the
//! `objects` tests next door cover it in its own right. Every test opens a real file database —
//! `sqlite::memory:` gives each *connection* its own database, so two stores over one in-memory
//! URL would never contend for the write lock.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_core::objects::ObjectStore;
use saas_core::store::{AuditEntry, CoreStore};
use saas_core::{config::Config, prelude::*};
use serde_json::json;
use store_adapter_sqlite::SqliteStore;

/// The fixture's org: `migrate` seeds the platform root at id 1, and `objects.org_id` needs
/// nothing more than a live `orgs` row.
const ORG: i64 = 1;

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("saas-tx-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn path(&self) -> String {
		self.0.join("test.db").to_string_lossy().into_owned()
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn open(db: &TmpDb) -> SqliteStore {
	SqliteStore::open(&Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	})
	.await
	.unwrap()
}

/// Migrations plus the one account the audit rows reference.
async fn setup(db: &TmpDb) -> SqliteStore {
	let store = open(db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	sqlx::query(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	store
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

/// The handle rule from the caller's side: a write through the handle `begin()` bound joins the
/// transaction, and the outer transaction stays the unit of work.
#[tokio::test]
async fn a_write_through_a_bound_handle_joins_the_transaction() {
	let db = TmpDb::new("nested");
	let store = setup(&db).await;

	// `object_put` opens its own `write_tx`. Given the handle `begin()` bound, that must be a
	// `SAVEPOINT` inside the outer transaction, not a second `BEGIN IMMEDIATE` queueing behind the
	// one writer connection until `acquire_timeout`.
	{
		let (tx, bound) = store.begin().await.unwrap();
		bound.object_put(ORG, "booking", "bk_1", &json!({"a": 1}), &[]).await.unwrap();
		tx.commit().await.unwrap();
	}
	assert!(store.object_get(ORG, "booking", "bk_1").await.unwrap().is_some());

	// A write through the bound handle in a transaction that rolls back goes with it, even though
	// the nested savepoint was released. Each binding is scoped: a bound handle keeps the writer
	// connection checked out until it drops, so the next `begin()` would queue behind it.
	{
		let (tx, bound) = store.begin().await.unwrap();
		bound.object_put(ORG, "booking", "bk_2", &json!({"a": 2}), &[]).await.unwrap();
		tx.rollback().await.unwrap();
	}
	assert!(store.object_get(ORG, "booking", "bk_2").await.unwrap().is_none());
}

/// `tokio::join!` polls both futures on one task: inferring re-entrancy from the task would join
/// the second write to the first's transaction, and a rollback of one would take the other.
#[tokio::test]
async fn two_writes_joined_on_one_task_do_not_share_a_transaction() {
	let db = TmpDb::new("joined");
	let store = std::sync::Arc::new(setup(&db).await);

	let holder = std::sync::Arc::clone(&store);
	let waiter = std::sync::Arc::clone(&store);
	let ((), other) = tokio::join!(
		async move {
			let (tx, bound) = holder.begin().await.unwrap();
			// Yield so the joined future is polled while this transaction is open.
			tokio::task::yield_now().await;
			bound
				.object_put(ORG, "booking", "bk_held", &json!({"h": 1}), &[])
				.await
				.unwrap();
			tx.rollback().await.unwrap();
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
/// attempt. `audit_detached` is the method for that: it cannot commit before the transaction
/// ends, so it is buffered on the bound handle and flushed after.
///
/// The only test over the buffering branch at all — every call site in the tree passes the
/// pooled `app.store`, which takes the immediate one.
#[tokio::test]
async fn an_audit_row_raised_on_a_bound_handle_survives_a_rollback() {
	let db = TmpDb::new("audit-rollback");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	bound.audit_detached(&audit_entry("ISSUE")).await.unwrap();
	assert_eq!(issued(&store).await, 0, "buffered, not written, while the transaction is open");
	tx.rollback().await.unwrap();

	let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs WHERE action = 'ISSUE'")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(count, 1);
}

/// The other half of the split: a row recording a mutation that succeeded is not evidence of an
/// attempt, so the rollback that undoes the mutation must take the row with it.
#[tokio::test]
async fn an_audit_row_written_on_a_bound_handle_rolls_back_with_it() {
	let db = TmpDb::new("audit-rolled-back");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	bound.audit_log(&audit_entry("ISSUE")).await.unwrap();
	tx.rollback().await.unwrap();

	let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs WHERE action = 'ISSUE'")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(count, 0, "a rolled-back operation left a row saying it happened");
}

/// A bound handle outlives the transaction it is bound to, and the connection the transaction
/// just gave up is still the one it points at: without the closed flag the write lands in
/// autocommit, in no transaction at all, with nothing to notice.
#[tokio::test]
async fn a_write_through_a_bound_handle_after_commit_is_an_error() {
	let db = TmpDb::new("bound-after-commit");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	tx.commit().await.unwrap();

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
#[tokio::test]
async fn a_write_through_a_bound_handle_after_drop_is_an_error() {
	let db = TmpDb::new("bound-after-drop");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
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
#[tokio::test]
async fn a_second_nested_write_tx_is_rejected_not_stacked() {
	let db = TmpDb::new("sibling");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	let (first, second) = tokio::join!(bound.write_tx(), bound.write_tx());
	let live = match (first, second) {
		(Ok(live), Err(_)) | (Err(_), Ok(live)) => live,
		(Ok(_), Ok(_)) => panic!("two savepoints open at once under one scope"),
		(Err(a), Err(b)) => panic!("neither scope opened: {a:?} / {b:?}"),
	};

	sqlx::query(
		"INSERT INTO objects (org_id, type, uid, body, created_at, updated_at)
		 VALUES (?, 'booking', 'bk_sib', '{}', 0, 0)",
	)
	.bind(ORG)
	.execute(&mut *live.lock().await.unwrap())
	.await
	.unwrap();
	live.commit().await.unwrap();
	tx.commit().await.unwrap();

	assert!(store.object_get(ORG, "booking", "bk_sib").await.unwrap().is_some());
}

/// `audit_logs` rows with the fixture's action, read off the reader pool so the assertion never
/// waits on whoever holds the writer connection.
async fn issued(store: &SqliteStore) -> i64 {
	sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs WHERE action = 'ISSUE'")
		.fetch_one(store.read_pool())
		.await
		.unwrap()
}

/// Waits for the work a `Drop` handed to the runtime. Nothing signals it, so the choice is a
/// poll or a fixed sleep long enough to be slower than the test.
async fn eventually(mut done: impl AsyncFnMut() -> bool) {
	for _ in 0..200 {
		if done().await {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}
	panic!("the spawned work never finished");
}

/// A read through a bound handle must see the transaction's own uncommitted writes; the reader
/// pool is a different connection and cannot. Every read-modify-write inside `begin()` is one.
#[tokio::test]
async fn bound_handle_reads_its_own_writes() {
	let db = TmpDb::new("bound-reads");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	bound
		.object_put(ORG, "booking", "bk_own", &json!({ "a": 1 }), &[])
		.await
		.unwrap();

	let seen = bound.object_get(ORG, "booking", "bk_own").await.unwrap();
	assert_eq!(seen.map(|o| o.body), Some(json!({ "a": 1 })), "before the commit");
	assert!(store.object_get(ORG, "booking", "bk_own").await.unwrap().is_none());

	tx.commit().await.unwrap();
}

/// A bound handle parked in a struct must not keep the writer connection checked out past its
/// transaction: every later write in the process would block until `acquire_timeout`.
#[tokio::test]
async fn commit_returns_the_writer_connection() {
	let db = TmpDb::new("conn-returned");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	tx.commit().await.unwrap();

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
#[tokio::test]
async fn nested_bound_handle_errors_after_release() {
	let db = TmpDb::new("nested-closed");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	let (inner, inner_bound) = bound.begin().await.unwrap();
	inner.commit().await.unwrap();

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
	tx.commit().await.unwrap();
	assert!(store.object_get(ORG, "booking", "bk_outer").await.unwrap().is_some());
	assert!(store.object_get(ORG, "booking", "bk_nested").await.unwrap().is_none());
}

/// `RELEASE` frees every savepoint nested inside it, so a dropped nested transaction must not
/// leave a name behind for a later `ROLLBACK TO` to abort on.
#[tokio::test]
async fn dropped_nested_tx_does_not_poison_a_later_statement() {
	let db = TmpDb::new("stale-savepoint");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	let (mid, mid_bound) = bound.begin().await.unwrap();
	let inner = mid_bound.write_tx().await.unwrap();
	mid.commit().await.unwrap();
	drop(inner);

	bound
		.object_put(ORG, "booking", "bk_after", &json!({ "a": 1 }), &[])
		.await
		.unwrap();
	tx.commit().await.unwrap();
	assert!(store.object_get(ORG, "booking", "bk_after").await.unwrap().is_some());
}

/// Sibling scopes are legal one after another, and only one at a time: the rejection is about a
/// scope still being *live*, not about one having been opened before.
#[tokio::test]
async fn nested_scopes_reopen_in_sequence() {
	let db = TmpDb::new("sequential-siblings");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	for uid in ["bk_a", "bk_b"] {
		let (nested, nested_bound) = bound.begin().await.unwrap();
		nested_bound
			.object_put(ORG, "booking", uid, &json!({ "a": 1 }), &[])
			.await
			.unwrap();
		nested.commit().await.unwrap();
	}

	// And the sibling window is shut while one is live, not merely after the first ever opened.
	let live = bound.write_tx().await.unwrap();
	let Err(err) = bound.write_tx().await else { panic!("two savepoints open at once") };
	assert!(format!("{err:?}").contains("a nested write_tx is already open"), "{err:?}");
	live.commit().await.unwrap();

	tx.commit().await.unwrap();
	assert!(store.object_get(ORG, "booking", "bk_a").await.unwrap().is_some());
	assert!(store.object_get(ORG, "booking", "bk_b").await.unwrap().is_some());
}

/// Committing the outer transaction ends every scope inside it — its `COMMIT` released their
/// savepoints — so a handle bound by a nested `begin()` must not go on writing in autocommit.
#[tokio::test]
async fn an_ancestor_commit_ends_the_scopes_inside_it() {
	let db = TmpDb::new("ancestor-commit");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	let (nested, nested_bound) = bound.begin().await.unwrap();
	tx.commit().await.unwrap();

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
#[tokio::test]
async fn commit_flushes_buffered_audit() {
	let db = TmpDb::new("audit-committed");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	bound.audit_detached(&audit_entry("ISSUE")).await.unwrap();
	tx.commit().await.unwrap();

	assert_eq!(issued(&store).await, 1);
}

/// A transaction that ends by being dropped still flushes the detached rows it is holding.
#[tokio::test]
async fn dropped_tx_flushes_buffered_audit() {
	let db = TmpDb::new("audit-dropped");
	let store = setup(&db).await;

	let (tx, bound) = store.begin().await.unwrap();
	bound.audit_detached(&audit_entry("ISSUE")).await.unwrap();
	drop(tx);

	eventually(async || issued(&store).await == 1).await;
}

// vim: ts=4
