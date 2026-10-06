// SPDX-License-Identifier: MPL-2.0
//! Transaction binding, run from the shared conformance suite (`mintworks_store_conformance::tx`),
//! plus what only SQLite's single writer connection does.

#![allow(clippy::unwrap_used)]

mod common;

use mintworks_core::prelude::*;
use mintworks_core::store::{AuditEntry, CoreStore};
use mintworks_store_conformance::Harness;

mintworks_store_conformance::tx_tests!(common::SqliteHarness);

/// The one writer connection is the open transaction's, so `audit_detached` cannot commit before
/// it ends: the row is buffered on the bound handle and flushed after the rollback.
#[tokio::test]
async fn audit_detached_is_buffered_while_open() {
	let h = common::SqliteHarness::fresh("audit-buffered").await.unwrap();
	h.exec(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0)",
		&[],
	)
	.await;
	let issued = "SELECT COUNT(*) FROM audit_logs WHERE action = 'ISSUE'";

	let (tx, bound) = h.store().begin().await.unwrap();
	bound
		.audit_detached(&AuditEntry {
			at: Timestamp::now(),
			account_id: Some(1),
			org_id: Some(1),
			ip: None,
			entity: "invoice".into(),
			entity_id: None,
			action: "ISSUE".into(),
			detail: None,
			request_id: None,
		})
		.await
		.unwrap();
	assert_eq!(h.scalar_i64(issued, &[]).await, 0, "buffered, not written, while open");
	tx.rollback().await.unwrap();

	assert_eq!(h.scalar_i64(issued, &[]).await, 1);
}

// vim: ts=4
