//! `EntitleStore` conformance, run from the shared conformance suite (`store_conformance::entitle`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

store_conformance::entitle_tests!(common::SqliteHarness);

// vim: ts=4
