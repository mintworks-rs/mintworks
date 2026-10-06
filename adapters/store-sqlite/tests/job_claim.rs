// SPDX-License-Identifier: MPL-2.0
//! `CoreStore::job_claim`'s mutual exclusion, run from the shared conformance suite
//! (`mintworks_store_conformance::job_claim`), plus the claim index's SQLite plan.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::SqliteHarness;
use mintworks_store_conformance::Harness;

mintworks_store_conformance::job_claim_tests!(SqliteHarness);

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
	let h = SqliteHarness::fresh("claim-plan").await.unwrap();
	let store = h.store();

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

// vim: ts=4
