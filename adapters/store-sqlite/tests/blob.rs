// SPDX-License-Identifier: MPL-2.0
//! `blobs`/`blob_refs` conformance, run from the shared conformance suite
//! (`mintworks_store_conformance::blob`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

mintworks_store_conformance::blob_tests!(common::SqliteHarness);

// vim: ts=4
