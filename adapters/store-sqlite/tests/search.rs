// SPDX-License-Identifier: MPL-2.0
//! `SearchStore` conformance, run from the shared conformance suite (`mintworks_store_conformance::search`).

#![cfg(feature = "ai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

mintworks_store_conformance::search_tests!(common::SqliteHarness);

// vim: ts=4
