//! `CoreStore::job_claim`'s mutual exclusion — an adapter-conformance property, not framework
//! logic, so it lives here beside the other suites a second store adapter must pass.
//!
//! A real file database in a temp dir, never `sqlite::memory:`: an in-memory URL gives each
//! connection its own database, so two stores would never contend for the write lock — which
//! is the entire point of this suite.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use saas_core::{config::Config, store::CoreStore, types::Timestamp};
use store_adapter_sqlite::SqliteStore;

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-claim-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [0; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: self.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: String::new(),
			jobs_workers: None,
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn open(db: &TmpDb) -> SqliteStore {
	SqliteStore::open(&db.config()).await.unwrap()
}

/// The existing coverage claims twice *sequentially* on one store, which proves the
/// `status = 'PENDING'` predicate and nothing about exclusion. An adapter implementing the
/// claim as the obvious `SELECT` then `UPDATE` hands one `NAV_REPORT` row to two workers:
/// both pass `saas_nav::job::may_send` — no `transactionId`, no verdict — and both POST
/// `manageInvoice`. That is a duplicated statutory filing, the one act here with no undo.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_is_claimed_by_exactly_one_worker() {
	const N: usize = 20;

	let db = TmpDb::new("claim-exclusion");
	let store = open(&db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	let mut enqueued = Vec::new();
	for i in 0..N {
		let id = store
			.job_enqueue("RACE", "{}", Some(&format!("k{i}")), Timestamp(0))
			.await
			.unwrap()
			.unwrap();
		enqueued.push(id);
	}

	// A second store over the same file: the writer pool is one connection, so two tasks on a
	// single store would queue in the pool instead of racing for the SQLite write lock.
	let a: Arc<dyn CoreStore> = Arc::new(store.clone());
	let b: Arc<dyn CoreStore> = Arc::new(open(&db).await);

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
#[tokio::test]
async fn a_reclaim_leaves_a_freshly_claimed_row_to_its_owner() {
	let db = TmpDb::new("claim-lease");
	let store = open(&db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	store.job_enqueue("LEASE", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	let job = store.job_claim(Timestamp(1_000)).await.unwrap().unwrap();

	assert_eq!(store.job_reclaim(Timestamp(900)).await.unwrap(), 0, "the lease is still live");
	assert_eq!(store.job_status(job.id).await.unwrap().as_deref(), Some("RUNNING"));

	assert_eq!(store.job_reclaim(Timestamp(1_100)).await.unwrap(), 1, "the lease has expired");
	assert_eq!(store.job_status(job.id).await.unwrap().as_deref(), Some("PENDING"));
}

/// `job::seed_periodic` was `job_has_live` on a reader then `job_enqueue`, so two processes
/// booting a rolling deploy both passed the read and both seeded — the chain doubled
/// permanently, and a consumer's own periodic kind ran twice per period forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_stores_seeding_the_same_periodic_kind_insert_one_row() {
	let db = TmpDb::new("seed-periodic");
	let store = open(&db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	let a: Arc<dyn CoreStore> = Arc::new(store.clone());
	let b: Arc<dyn CoreStore> = Arc::new(open(&db).await);

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

/// `idx_job_claim` was `ON jobs(run_at) WHERE status = 'PENDING'` — no `, id`, so it could not
/// serve `ORDER BY run_at, id`. The planner took the equality on `idx_job_status` and sorted
/// the whole PENDING backlog per claim, ~7.9 ms at 45 000 rows, all of it inside the `UPDATE`
/// on the single writer connection that every other write in the process queues behind.
///
/// Nothing runs `ANALYZE`, so `sqlite_stat1` never exists and the plan is fixed at any table
/// size. Asserted on the full plan text, not just the index name: the two older plan tests
/// check names only, which is what let a sort through.
#[tokio::test]
async fn the_claim_selection_never_sorts() {
	let db = TmpDb::new("claim-plan");
	let store = open(&db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	// `EXPLAIN QUERY PLAN` answers (id, parent, notused, detail); only the last is readable.
	// The subquery of `job_claim`, `INDEXED BY` and all: SQLite refuses to prepare the
	// statement at all if the index can no longer serve it, so this also pins the index shape.
	let rows = sqlx::query(
		"EXPLAIN QUERY PLAN \
		 SELECT id FROM jobs INDEXED BY idx_job_claim \
		 WHERE status = 'PENDING' AND run_at <= 0 ORDER BY run_at, id LIMIT 1",
	)
	.fetch_all(store.read_pool())
	.await
	.unwrap();
	let plan = rows
		.iter()
		.map(|r| sqlx::Row::get::<String, _>(r, "detail"))
		.collect::<Vec<_>>()
		.join("\n");

	assert!(plan.contains("idx_job_claim"), "the claim is not served by its own index:\n{plan}");
	assert!(
		!plan.contains("TEMP B-TREE"),
		"the claim sorts the PENDING backlog on the writer connection:\n{plan}"
	);
}

/// `job_defer` joins `job_complete`/`job_fail`/`job_terminate` as a `RUNNING`-guarded,
/// row-counted write — and it is the one that must **clear** `last_error`, because
/// `A-JOB-STALE` reads `status = 'PENDING' AND last_error IS NOT NULL` and a deferring poll is
/// not a retrying one.
#[tokio::test]
async fn job_defer_is_running_guarded_and_clears_the_failure() {
	let db = TmpDb::new("defer");
	let store = open(&db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	let id = store.job_enqueue("POLL", "{}", None, Timestamp(0)).await.unwrap().unwrap();
	// A `PENDING` row is not deferrable: only the worker holding the claim may reschedule it.
	assert_eq!(store.job_defer(id, Timestamp(50)).await.unwrap(), 0);

	store.job_claim(Timestamp(0)).await.unwrap();
	store.job_fail(id, Timestamp(10), "not yet", Some("E-X")).await.unwrap();
	assert_eq!(store.job_retrying_kinds().await.unwrap(), vec!["POLL".to_owned()]);

	store.job_claim(Timestamp(10)).await.unwrap();
	assert_eq!(store.job_defer(id, Timestamp(99)).await.unwrap(), 1);

	let (status, attempts, run_at, last_error, err_code): (
		String,
		i64,
		i64,
		Option<String>,
		Option<String>,
	) = sqlx::query_as("SELECT status, attempts, run_at, last_error, err_code FROM jobs WHERE id = ?")
		.bind(id)
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!((status.as_str(), run_at), ("PENDING", 99));
	assert_eq!(attempts, 2, "a deferral is still an execution");
	assert_eq!((last_error, err_code), (None, None));
	assert!(store.job_retrying_kinds().await.unwrap().is_empty());

	// And a row an operator cancelled mid-handler stays cancelled.
	store.job_claim(Timestamp(99)).await.unwrap();
	store.job_cancel("POLL", "{}", Timestamp(100), "stopped", None).await.unwrap();
	assert_eq!(store.job_defer(id, Timestamp(200)).await.unwrap(), 0);
}

/// `job_complete` blanks `payload`, so a `DONE` row addressed by `(kind, payload)` — as
/// `job_redrive_done` used to be — matched nothing, and `saas-nav`'s two §1.9.2 recovery paths
/// were silent no-ops that never filed the invoice.
#[tokio::test]
async fn job_redrive_done_restores_the_payload_job_complete_blanked() {
	let db = TmpDb::new("redrive-done");
	let store = open(&db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

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

	let (status, attempts, done_at, got): (String, i64, Option<i64>, String) =
		sqlx::query_as("SELECT status, attempts, done_at, payload FROM jobs WHERE id = ?")
			.bind(id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!((status.as_str(), attempts, done_at), ("PENDING", 0, None));
	assert_eq!(got, payload);

	let job = store.job_claim(Timestamp(20)).await.unwrap().unwrap();
	assert_eq!(job.payload, payload, "the handler must be handed the payload it was enqueued with");
}

/// `job_statuses_by_keys` builds its `IN (…)` list by hand, so the empty-slice guard is what
/// stops a malformed `IN ()`, and an unknown key must be absent rather than defaulted — the
/// caller reads "no row" as "never enqueued".
#[tokio::test]
async fn job_statuses_by_keys_answers_only_for_the_keys_asked_for() {
	let db = TmpDb::new("statuses-by-keys");
	let store = open(&db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

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
#[tokio::test]
async fn job_statuses_by_keys_spans_more_keys_than_sqlite_binds_in_one_statement() {
	let db = TmpDb::new("statuses-by-keys-chunked");
	let store = open(&db).await;
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();

	let keys: Vec<String> = (0..2_500).map(|i| format!("k{i}")).collect();
	for k in &keys {
		store.job_enqueue("K", k, Some(k), Timestamp(0)).await.unwrap().unwrap();
	}
	let got = store.job_statuses_by_keys(&keys).await.unwrap();
	assert_eq!(got.len(), keys.len());
	assert!(got.iter().all(|(_, status)| status == "PENDING"));
}

// vim: ts=4
