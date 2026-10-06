// SPDX-License-Identifier: MPL-2.0
//! `EntitleStore` conformance, run from the shared conformance suite (`mintworks_store_conformance::entitle`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

mintworks_store_conformance::entitle_tests!(common::SqliteHarness);

// vim: ts=4
