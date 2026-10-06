// SPDX-License-Identifier: MPL-2.0
//! `CoreStore::job_claim`'s mutual exclusion and the job-row writes around it.
//!
//! The contention tests race two independent stores over one database (`Harness::reopen`): a
//! single store's writer would queue the tasks instead of racing them for the write lock.
//! The claim index's shape is backend-specific and is pinned in each adapter's own tests.

use std::sync::Arc;

use mintworks_core::{store::CoreStore, types::Timestamp};
use serde_json::{Value, json};

use crate::{Harness, fresh};

/// The existing coverage claims twice *sequentially* on one store, which proves the
/// `status = 'PENDING'` predicate and nothing about exclusion. An adapter implementing the
/// claim as the obvious `SELECT` then `UPDATE` hands one `NAV_REPORT` row to two workers:
/// both pass `mintworks_nav::job::may_send` — no `transactionId`, no verdict — and both POST
/// `manageInvoice`. That is a duplicated statutory filing, the one act here with no undo.
pub async fn a_job_is_claimed_by_exactly_one_worker<H: Harness>()
where
	H::Store: CoreStore,
{
	const N: usize = 20;

	let h = fresh!(H, "claim-exclusion");
	let store = h.store();

	let mut enqueued = Vec::new();
	for i in 0..N {
		let id = store
			.job_enqueue("RACE", "{}", Some(&format!("k{i}")), Timestamp(0))
			.await
			.unwrap()
			.unwrap();
		enqueued.push(id);
	}

	let a: Arc<dyn CoreStore> = Arc::new(store.clone());
	let b: Arc<dyn CoreStore> = Arc::new(h.reopen().await);

	// 2N claims for N rows, so N of them must come back empty.
	let mut tasks = Vec::new();
	for i in 0..N * 2 {
		let s = if i % 2 == 0 { Arc::clone(&a) } else { Arc::clone(&b) };
		tasks.push(tokio::spawn(async move { s.job_claim(Timestamp(1)).await.unwrap() }));
	}

	let mut claimed = Vec::new();
	let mut empty = 0;
	for t in tasks {
		match t.await.unwrap() {
			Some(job) => claimed.push(job.id),
			None => empty += 1,
		}
	}

	claimed.sort_unstable();
	let mut expected = enqueued;
	expected.sort_unstable();
	assert_eq!(claimed, expected, "every row exactly once, and no row twice");
	assert_eq!(empty, N, "the surplus claims must find nothing, not re-hand a running row");
}

/// `job_reclaim` had no age predicate, so a process starting during a rolling deploy handed
/// its live sibling's rows to itself and both ran the handler.
pub async fn a_reclaim_leaves_a_freshly_claimed_row_to_its_owner<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "claim-lease");
	let store = h.store();

	store.job_enqueue("LEASE", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	let job = store.job_claim(Timestamp(1_000)).await.unwrap().unwrap();

	assert_eq!(store.job_reclaim(Timestamp(900)).await.unwrap(), 0, "the lease is still live");
	assert_eq!(store.job_status(job.id).await.unwrap().as_deref(), Some("RUNNING"));

	assert_eq!(store.job_reclaim(Timestamp(1_100)).await.unwrap(), 1, "the lease has expired");
	assert_eq!(store.job_status(job.id).await.unwrap().as_deref(), Some("PENDING"));
}

pub async fn job_wake_moves_only_pending_rows_forward<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "wake");
	let store = h.store();

	let later = store
		.job_enqueue("W", "{}", Some("later"), Timestamp(5_000))
		.await
		.unwrap()
		.unwrap();
	let due = store.job_enqueue("W", "{}", Some("due"), Timestamp(10)).await.unwrap().unwrap();
	let other = store
		.job_enqueue("W", "{}", Some("other"), Timestamp(5_000))
		.await
		.unwrap()
		.unwrap();
	let running = store
		.job_enqueue("W", "{}", Some("running"), Timestamp(20))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(store.job_claim(Timestamp(30)).await.unwrap().unwrap().id, due);
	assert_eq!(store.job_claim(Timestamp(30)).await.unwrap().unwrap().id, running);

	let keys = ["later", "due", "running", "missing"].map(String::from);
	assert_eq!(store.job_wake(&keys, Timestamp(100)).await.unwrap(), 1);
	let at =
		async |id: i64| h.scalar_i64("SELECT run_at FROM jobs WHERE id = ?", &[json!(id)]).await;
	assert_eq!(at(later).await, 100);
	assert_eq!(at(other).await, 5_000, "a key not named is not woken");
	assert_eq!(store.job_status(running).await.unwrap().as_deref(), Some("RUNNING"));
	assert_eq!(store.job_wake(&[], Timestamp(100)).await.unwrap(), 0);
}

/// `job::seed_periodic` was `job_has_live` on a reader then `job_enqueue`, so two processes
/// booting a rolling deploy both passed the read and both seeded — the chain doubled
/// permanently, and a consumer's own periodic kind ran twice per period forever.
pub async fn two_stores_seeding_the_same_periodic_kind_insert_one_row<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "seed-periodic");
	let store = h.store();

	let a: Arc<dyn CoreStore> = Arc::new(store.clone());
	let b: Arc<dyn CoreStore> = Arc::new(h.reopen().await);

	let mut tasks = Vec::new();
	for i in 0..8 {
		let s = if i % 2 == 0 { Arc::clone(&a) } else { Arc::clone(&b) };
		tasks.push(tokio::spawn(async move { s.job_seed_periodic("SEED", Timestamp(0)).await }));
	}
	let mut inserted = 0;
	for t in tasks {
		if t.await.unwrap().unwrap().is_some() {
			inserted += 1;
		}
	}
	assert_eq!(inserted, 1, "exactly one caller may hold the chain");

	// And a stopped chain is still revivable — the reason a fixed `dedup_key` was refused.
	let id = store.job_claim(Timestamp(1)).await.unwrap().unwrap().id;
	store.job_complete(id, Timestamp(2)).await.unwrap();
	assert!(store.job_seed_periodic("SEED", Timestamp(3)).await.unwrap().is_some());
}

/// `job_defer` joins `job_complete`/`job_fail`/`job_terminate` as a `RUNNING`-guarded,
/// row-counted write — and it is the one that must **clear** `last_error`, because
/// `A-JOB-STALE` reads `status = 'PENDING' AND last_error IS NOT NULL` and a deferring poll is
/// not a retrying one.
pub async fn job_defer_is_running_guarded_and_clears_the_failure<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "defer");
	let store = h.store();

	let id = store.job_enqueue("POLL", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	// A `PENDING` row is not deferrable: only the worker holding the claim may reschedule it.
	assert_eq!(store.job_defer(id, Timestamp(50)).await.unwrap(), 0);

	store.job_claim(Timestamp(0)).await.unwrap();
	store.job_fail(id, Timestamp(10), "not yet", Some("E-X")).await.unwrap();
	assert_eq!(store.job_retrying_kinds().await.unwrap(), vec!["POLL".to_owned()]);

	store.job_claim(Timestamp(10)).await.unwrap();
	assert_eq!(store.job_defer(id, Timestamp(99)).await.unwrap(), 1);

	let row = h
		.rows(
			"SELECT status, attempts, run_at, last_error, err_code FROM jobs WHERE id = ?",
			&[json!(id)],
		)
		.await;
	assert_eq!(
		row[0][..3],
		[json!("PENDING"), json!(2), json!(99)],
		"a deferral is still an execution"
	);
	assert_eq!(row[0][3..], [Value::Null, Value::Null]);
	assert!(store.job_retrying_kinds().await.unwrap().is_empty());

	// And a row an operator cancelled mid-handler stays cancelled.
	store.job_claim(Timestamp(99)).await.unwrap();
	store.job_cancel("POLL", "{}", Timestamp(100), "stopped", None).await.unwrap();
	assert_eq!(store.job_defer(id, Timestamp(200)).await.unwrap(), 0);
}

/// `job_complete` blanks `payload`, so a `DONE` row addressed by `(kind, payload)` — as
/// `job_redrive_done` used to be — matched nothing, and `mintworks-nav`'s two §1.9.2 recovery paths
/// were silent no-ops that never filed the invoice.
pub async fn job_redrive_done_restores_the_payload_job_complete_blanked<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "redrive-done");
	let store = h.store();

	let payload = r#"{"invoiceId":7}"#;
	let key = "nav:invoice:7";
	let id = store
		.job_enqueue("NAV_REPORT", payload, Some(key), Timestamp(0))
		.await
		.unwrap()
		.unwrap();
	store.job_claim(Timestamp(0)).await.unwrap();
	store.job_complete(id, Timestamp(10)).await.unwrap();

	// The bug, pinned directly: a `DONE` row is not addressable by `(kind, payload)` any more,
	// and `payload = ''` is the only key that would have matched — reviving every `DONE` row.
	assert_eq!(store.job_redrive("NAV_REPORT", payload, Timestamp(20)).await.unwrap(), 0);

	assert_eq!(store.job_redrive_done(key, payload, Timestamp(20)).await.unwrap(), 1);

	let row = h
		.rows("SELECT status, attempts, done_at, payload FROM jobs WHERE id = ?", &[json!(id)])
		.await;
	assert_eq!(row[0], [json!("PENDING"), json!(0), Value::Null, json!(payload)]);

	let job = store.job_claim(Timestamp(20)).await.unwrap().unwrap();
	assert_eq!(job.payload, payload, "the handler must be handed the payload it was enqueued with");
}

/// `job_statuses_by_keys` builds its `IN (…)` list by hand, so the empty-slice guard is what
/// stops a malformed `IN ()`, and an unknown key must be absent rather than defaulted — the
/// caller reads "no row" as "never enqueued".
pub async fn job_statuses_by_keys_answers_only_for_the_keys_asked_for<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "statuses-by-keys");
	let store = h.store();

	assert!(store.job_statuses_by_keys(&[]).await.unwrap().is_empty());

	for k in ["a", "b", "c"] {
		store.job_enqueue("K", k, Some(k), Timestamp(0)).await.unwrap().unwrap();
	}
	// Claims are FIFO by (run_at, id): `a` goes DONE, `b` stays RUNNING, `c` PENDING.
	let first = store.job_claim(Timestamp(0)).await.unwrap().unwrap();
	store.job_complete(first.id, Timestamp(1)).await.unwrap();
	store.job_claim(Timestamp(0)).await.unwrap().unwrap();

	let mut got = store
		.job_statuses_by_keys(&["a".to_owned(), "b".to_owned(), "nope".to_owned()])
		.await
		.unwrap();
	got.sort();
	assert_eq!(
		got,
		vec![("a".to_owned(), "DONE".to_owned()), ("b".to_owned(), "RUNNING".to_owned())],
		"an unknown key is absent, not defaulted"
	);
}

/// The `IN (…)` list is chunked inside the store, not bounded by a doc comment the next caller
/// reads: one bind per key hits SQLite's 32 766 variable cap, and `SQLITE_TOOMANY` on a status
/// lookup fails the leader's whole batch.
pub async fn job_statuses_by_keys_spans_more_keys_than_sqlite_binds_in_one_statement<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "statuses-by-keys-chunked");
	let store = h.store();

	let keys: Vec<String> = (0..2_500).map(|i| format!("k{i}")).collect();
	for k in &keys {
		store.job_enqueue("K", k, Some(k), Timestamp(0)).await.unwrap().unwrap();
	}
	let got = store.job_statuses_by_keys(&keys).await.unwrap();
	assert_eq!(got.len(), keys.len());
	assert!(got.iter().all(|(_, status)| status == "PENDING"));
}

// vim: ts=4
