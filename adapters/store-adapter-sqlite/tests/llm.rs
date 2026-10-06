//! `LlmStore` conformance, run from the shared conformance suite (`store_conformance::llm`).

#![cfg(feature = "ai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

store_conformance::llm_tests!(common::SqliteHarness);

// vim: ts=4
