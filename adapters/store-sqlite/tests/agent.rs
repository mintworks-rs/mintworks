//! `AgentRunStore` conformance, run from the shared conformance suite (`mintworks_store_conformance::agent`).

#![cfg(feature = "ai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

mintworks_store_conformance::agent_tests!(common::SqliteHarness);

// vim: ts=4
