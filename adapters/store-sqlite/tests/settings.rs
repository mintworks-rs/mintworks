// SPDX-License-Identifier: MPL-2.0
//! Scoped `settings` conformance, run from the shared conformance suite
//! (`mintworks_store_conformance::settings`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

mintworks_store_conformance::settings_tests!(common::SqliteHarness);

// vim: ts=4
