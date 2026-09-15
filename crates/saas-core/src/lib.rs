//! Shared foundation for every `saas-*` crate: the error type and HTTP envelope, the
//! three-state `Patch`, timestamps, prefixed-ULID identifiers and fixed-point money.
//!
//! Dependencies point inward only — `saas-core` depends on no other `saas-*` crate.

#![forbid(unsafe_code)]

pub mod alert;
pub mod app;
pub mod audit;
pub mod auth_mw;
pub mod config;
pub mod ctx;
pub mod error;
pub mod gencache;
pub mod health;
pub mod http;
pub mod ids;
pub mod job;
// Request-id plumbing, mounted by `AppBuilder::run` itself. Nothing outside the crate names it.
pub(crate) mod log;
pub mod money;
pub mod prelude;
pub mod ratelimit;
pub mod secrets;
pub mod settings;
pub mod store;
pub mod str_enum;
pub mod types;

pub use app::{App, AppBuilder, AppState};
pub use ctx::{Actor, Ctx};
pub use error::{ClResult, Error, Retry};

// There is deliberately no `saas_core::routes` bundle to merge: `AppBuilder::run` already mounts
// `health::public()`, so merging it too trips axum's duplicate-route check and panics at process
// start, outside `CatchPanicLayer`.

// vim: ts=4
