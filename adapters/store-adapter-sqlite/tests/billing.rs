//! `BillingStore` guarantees, run from the shared conformance suite (`store_conformance::billing`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

store_conformance::billing_tests!(common::SqliteHarness);

// vim: ts=4
