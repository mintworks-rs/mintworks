//! `RefStore` conformance, run from the shared conformance suite (`store_conformance::refs`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

store_conformance::refs_tests!(common::SqliteHarness);

// vim: ts=4
