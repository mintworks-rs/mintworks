//! `PlanStore` conformance, run from the shared conformance suite (`mintworks_store_conformance::plans`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

mintworks_store_conformance::plans_tests!(common::SqliteHarness);

// vim: ts=4
