// SPDX-License-Identifier: MPL-2.0
//! Markdown memory: org-scoped **spaces** hold path-addressed **docs** whose content is a chain
//! of immutable **versions**, with full-text search over each doc's current body.
#![forbid(unsafe_code)]

mod hook;
pub mod service;
pub mod store;

pub use service::Memory;
pub use store::{Doc, MemoryStore, NewVersion, SearchHit, Space, Version, WriteMode};

// vim: ts=4
