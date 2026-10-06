//! `DocumentStore` guarantees and the `jobs.result` read-back, run from the shared conformance suite (`store_conformance::pdf`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

store_conformance::pdf_tests!(common::SqliteHarness);

// vim: ts=4
