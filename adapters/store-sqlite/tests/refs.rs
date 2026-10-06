// SPDX-License-Identifier: MPL-2.0
//! `RefStore` conformance, run from the shared conformance suite
//! (`mintworks_store_conformance::refs`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

mintworks_store_conformance::refs_tests!(common::SqliteHarness);

// vim: ts=4
