//! `AuthStore` guarantees, run from the shared conformance suite
//! (`mintworks_store_conformance::auth`), plus the per-request auth queries' SQLite plans, which
//! only SQLite can state.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::SqliteHarness;
use mintworks_store_conformance::Harness;

mintworks_store_conformance::auth_tests!(SqliteHarness);
mintworks_store_conformance::auth_ext_tests!(SqliteHarness);

/// Every authenticated request runs these two statements, where it used to run two point
/// lookups. `memberships` is `PRIMARY KEY (org_id, account_id) WITHOUT ROWID` with
/// `idx_membership_account`, `orgs.uid` is UNIQUE and `idx_org_root` covers the kind probe —
/// drop one and the hottest path in the framework quietly becomes a scan, with nothing else
/// failing.
///
/// The two statements are copies of `core.rs`'s `account_for_token` and `org_membership_role`
/// (`ancestors`/`BEST_ROLE` are `pub(crate)`), so they move together.
#[tokio::test]
async fn the_per_request_auth_queries_do_not_scan() {
	let h = SqliteHarness::fresh("auth-plans").await.unwrap();
	let store = h.store();

	let account_for_token = "SELECT a.id, a.token_epoch, \
	        EXISTS (SELECT 1 FROM memberships m \
	                 WHERE m.account_id = a.id \
	                   AND m.org_id = (SELECT id FROM orgs WHERE kind = 'ROOT') \
	                   AND m.role IN ('ADMIN', 'OWNER') \
	                   AND m.accepted_at IS NOT NULL), \
	        a.status \
	   FROM accounts a WHERE a.uid = ?";
	let org_membership_role = "WITH RECURSIVE anc(id, parent_id, depth) AS ( \
	         SELECT id, parent_id, 0 FROM orgs WHERE uid = ? AND status = 'ACTIVE' \
	   UNION ALL \
	         SELECT o.id, o.parent_id, anc.depth + 1 FROM orgs o JOIN anc ON o.id = anc.parent_id \
	          WHERE o.status = 'ACTIVE' \
	   LIMIT 16 \
	 ) SELECT o.id, (SELECT MAX(CASE m.role WHEN 'OWNER' THEN 3 WHEN 'ADMIN' THEN 2 ELSE 1 END) \
	   FROM memberships m JOIN anc ON m.org_id = anc.id \
	  WHERE m.account_id = ? AND m.accepted_at IS NOT NULL) \
	   FROM orgs o WHERE o.uid = ? AND o.status = 'ACTIVE'";

	let cases = [
		("account_for_token", account_for_token, 1),
		("org_membership_role", org_membership_role, 3),
	];
	for (what, sql, binds) in cases {
		let mut q = sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(format!(
			"EXPLAIN QUERY PLAN {sql}"
		)));
		for _ in 0..binds {
			q = q.bind(1_i64);
		}
		for (_, _, _, detail) in q.fetch_all(store.read_pool()).await.unwrap() {
			assert!(
				!detail.contains("SCAN memberships") && !detail.contains("SCAN orgs"),
				"{what} scans: {detail}"
			);
		}
	}
}

// vim: ts=4
