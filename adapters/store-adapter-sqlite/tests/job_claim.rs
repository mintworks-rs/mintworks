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
			data_dir: String::new(),
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
	store.migrate(store_adapter_sqlite::STEPS).await.unwrap();

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
	store.migrate(store_adapter_sqlite::STEPS).await.unwrap();

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
	store.migrate(store_adapter_sqlite::STEPS).await.unwrap();

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
	store.migrate(store_adapter_sqlite::STEPS).await.unwrap();

	// `EXPLAIN QUERY PLAN` answers (id, parent, notused, detail); only the last is readable.
	// The subquery of `job_claim`, `INDEXED BY` and all: SQLite refuses to prepare the
	// statement at all if the index can no longer serve it, so this also pins the index shape.
	let rows = sqlx::query(
		"EXPLAIN QUERY PLAN \
		 SELECT id FROM jobs INDEXED BY idx_job_claim \
		 WHERE status = 'PENDING' AND run_at <= 0 ORDER BY run_at, id LIMIT 1",
	)
	.fetch_all(store.reader())
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

// vim: ts=4
