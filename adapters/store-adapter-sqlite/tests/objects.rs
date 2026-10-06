//! `ObjectStore` guarantees, run from the shared conformance suite (`store_conformance::objects`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

store_conformance::objects_tests!(common::SqliteHarness);

// vim: ts=4
