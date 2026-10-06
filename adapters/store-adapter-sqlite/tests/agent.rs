//! `AgentRunStore` conformance, run from the shared conformance suite (`store_conformance::agent`).

#![cfg(feature = "ai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

store_conformance::agent_tests!(common::SqliteHarness);

// vim: ts=4
