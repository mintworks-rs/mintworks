// SPDX-License-Identifier: MPL-2.0
//! `InvoiceStore` and `NavStore` guarantees, run from the shared conformance suite
//! (`mintworks_store_conformance::invoice`), plus what only SQLite can state: the index plans the sweeps,
//! the batch leader and the audit export are selected by, and a source check on `src/invoice.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::SqliteHarness;
use mintworks_store_conformance::{Harness, invoice};

mintworks_store_conformance::invoice_tests!(SqliteHarness);

/// The sweep used to `LEFT JOIN (… GROUP BY invoice_id)` with no predicate, so every
/// hourly tick re-aggregated `nav_submissions` whole, and the outer `invoices.seller_id`
/// filter had no index either. Both grew linearly with invoice volume forever.
///
/// No migration was needed: `idx_invoice_number` (`invoices(seller_id, number) WHERE number IS
/// NOT NULL`) is exactly the outer predicate, and `idx_nav_submission_live`
/// (`nav_submissions(invoice_id, op)`) serves the `NOT EXISTS`.
///
/// `UNFILED` is the statement `unfiled_invoices` actually runs, exported for this test so
/// the plan is checked against the query rather than a copy of it that can drift.
#[tokio::test]
async fn the_sweep_selection_is_served_by_indexes() {
	let h = SqliteHarness::fresh("sweep-plan").await.unwrap();
	invoice::setup(&h).await;
	let store = h.store();
	let inv = invoice::issued(store).await;
	invoice::fail_submission(store, inv.id).await;

	// `EXPLAIN QUERY PLAN` answers (id, parent, notused, detail); only the last is readable.
	// `sqlx::query` takes `&'static str`; the statement is a compile-time constant with the
	// `EXPLAIN` prefix glued on, so leaking it in a test costs one allocation.
	let sql: &'static str = Box::leak(
		format!("EXPLAIN QUERY PLAN {}", mintworks_store_sqlite::UNFILED).into_boxed_str(),
	);
	let rows = sqlx::query(sql).fetch_all(store.read_pool()).await.unwrap();
	let plan = rows
		.iter()
		.map(|r| sqlx::Row::get::<String, _>(r, "detail"))
		.collect::<Vec<_>>()
		.join("\n");

	assert!(plan.contains("idx_invoice_number"), "the seller filter is a full scan:\n{plan}");
	assert!(
		plan.contains("idx_nav_submission_live"),
		"the filing-record lookup is a full scan:\n{plan}"
	);
	assert!(
		!plan.contains("GROUP BY"),
		"the unpredicated aggregate is back; it re-reads every submission row ever \
		 written on every tick:\n{plan}"
	);
}

/// The batch leader's candidate selection runs once per `NAV_REPORT` job, so it is on the hot
/// path the sweep is not. It is `UNFILED`'s shape plus `kind = 'NORMAL'`, `id <> ?` and an
/// optional `invoice_documents` existence test, all of which are row filters over an index
/// seek — none of them may turn the outer selection into a scan.
#[tokio::test]
async fn the_batch_candidate_selection_is_served_by_indexes() {
	let h = SqliteHarness::fresh("batch-candidates-plan").await.unwrap();
	invoice::setup(&h).await;
	let store = h.store();
	let inv = invoice::issued(store).await;
	invoice::fail_submission(store, inv.id).await;

	let sql: &'static str = Box::leak(
		format!("EXPLAIN QUERY PLAN {}", mintworks_store_sqlite::BATCH_CANDIDATES).into_boxed_str(),
	);
	let rows = sqlx::query(sql).fetch_all(store.read_pool()).await.unwrap();
	let plan = rows
		.iter()
		.map(|r| sqlx::Row::get::<String, _>(r, "detail"))
		.collect::<Vec<_>>()
		.join("\n");

	assert!(plan.contains("idx_invoice_number"), "the seller filter is a full scan:\n{plan}");
	assert!(
		plan.contains("idx_nav_submission_live"),
		"the filing-record lookup is a full scan:\n{plan}"
	);
	assert!(
		!plan.contains("SCAN invoice_documents"),
		"the document test scans; `invoice_documents` is keyed on (invoice_id, kind):\n{plan}"
	);
}

/// `idx_invoice_issued` was `ON invoices(issued_at) WHERE status <> 'DRAFT'`, and the
/// audit export it exists for filters `number IS NOT NULL` — which does not *imply*
/// `status <> 'DRAFT'`, so SQLite refused the partial index and fell back to
/// `idx_invoice_number(seller_id, number)`, filtering `issued_at` row by row, which made a
/// one-month export a scan of every invoice the seller ever issued. `idx_invoice_issued` is shaped `(seller_id, issued_at)
/// WHERE number IS NOT NULL`, which is the query's own predicate.
///
/// The two selections are exported so the plan is checked against the statements the store
/// runs rather than a copy that can drift — the same reason `UNFILED` is.
#[tokio::test]
async fn the_audit_export_selections_are_served_by_indexes() {
	let h = SqliteHarness::fresh("export-plan").await.unwrap();
	invoice::setup(&h).await;
	let store = h.store();
	let _ = invoice::issued(store).await;

	let plan_of = |sql: &str| {
		let leaked: &'static str = Box::leak(format!("EXPLAIN QUERY PLAN {sql}").into_boxed_str());
		let store = store.clone();
		async move {
			sqlx::query(leaked)
				.fetch_all(store.read_pool())
				.await
				.unwrap()
				.iter()
				.map(|r| sqlx::Row::get::<String, _>(r, "detail"))
				.collect::<Vec<_>>()
				.join("\n")
		}
	};

	// What is asserted is the `sel` CTE — the selection that grows with the seller's history
	// that `idx_invoice_issued` exists to bound. `close_over_storno_pairs!` used to add a
	// `SCAN invoices` on top of it; re-applying `seller_id`/`number IS NOT NULL` there — which
	// it needs for correctness anyway — put the closure on `idx_invoice_number` instead.
	let plan = plan_of(mintworks_store_sqlite::BY_DATE).await;
	assert!(
		plan.contains("idx_invoice_issued (seller_id=? AND issued_at>? AND issued_at<?)"),
		"the date range is not served by the index that exists for it:\n{plan}"
	);
	assert_eq!(plan.matches("SCAN invoices").count(), 0, "{plan}");

	// The number export is served by `idx_invoice_number`, asserted here so a later index
	// change cannot quietly cost it.
	let plan = plan_of(mintworks_store_sqlite::BY_NUMBER).await;
	assert!(plan.contains("idx_invoice_number"), "the number export is a scan:\n{plan}");
	assert_eq!(plan.matches("SCAN invoices").count(), 0, "{plan}");
}

/// The module doc claims every `UPDATE invoices` carries `AND status = …`. Nothing enforced
/// it: a new method with a missing predicate compiled, linted and passed the whole suite.
#[test]
fn every_invoice_write_carries_a_status_predicate() {
	const SRC: &str = include_str!("../src/invoice.rs");
	// `update_notes` is the one documented exception — `notes` is the single column an issued
	// invoice may still change.
	const ALLOWED_WITHOUT: &[&str] = &["SET notes = ?"];

	for (i, _) in SRC
		.match_indices("UPDATE invoices")
		.chain(SRC.match_indices("DELETE FROM invoices"))
	{
		let stmt: String = SRC[i..].chars().take_while(|c| *c != '"').collect();
		// After the `WHERE`, not anywhere: `SET status = 'X' WHERE id = ?` carries the word
		// and no predicate at all, and used to pass.
		let predicate = stmt.split_once("WHERE").map(|(_, w)| w).unwrap_or_default();
		assert!(
			predicate.contains("status") || ALLOWED_WITHOUT.iter().any(|a| stmt.contains(a)),
			"an invoice write with no status predicate:\n{stmt}"
		);
	}
}

// vim: ts=4
