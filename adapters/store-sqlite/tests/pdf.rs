//! `DocumentStore` guarantees and the `jobs.result` read-back, run from the shared conformance suite (`mintworks_store_conformance::pdf`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

mintworks_store_conformance::pdf_tests!(common::SqliteHarness);

// vim: ts=4
