#![forbid(unsafe_code)]
//! Everything the example is except its composition root, which stays in `main.rs`.
//!
//! The library target exists so `tests/flow.rs` can drive the real router: without it the
//! modules had to be pulled in by `#[path]`, which left `routes.rs` — auth, the consent gate,
//! the error envelope, the required cancel body — compiled by nothing but the binary.

pub mod bookings;
pub mod routes;
pub mod seed;
pub mod store;

// vim: ts=4
