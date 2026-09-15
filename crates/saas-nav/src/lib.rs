//! NAV Online Számla reporting for the saas-framework.
//!
//! Reporting is asynchronous: the invoice issue path enqueues a job, and nothing here
//! is ever called from it. The official XSDs this crate's XML is validated against are
//! vendored in `xsd/` — see `xsd/README.md` for their provenance.

#![forbid(unsafe_code)]

pub mod auth;
pub mod client;
// NAV's signature and token crypto. Nothing outside the crate calls it.
pub(crate) mod crypto;
pub mod export;
pub mod job;
pub(crate) mod service_api;
pub mod store;
pub mod submission;
pub mod xml;

pub use service_api::{E_NAV_CANCELLED, Nav, alerts};
pub use store::NavStore;
pub use submission::{NavOp, NavSubmission, NavVerdict};

// vim: ts=4
