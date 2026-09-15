//! Accounts, tenants, memberships, API keys, TOTP credentials and consent records.
//!
//! Authentication is pure stateless JWT: there is no session table, no token denylist and
//! no activation- or reset-token table. Activation and reset tokens are HMAC-signed and
//! single-use because their signature covers the state the action changes
//! (`accounts.status`, `accounts.pwd_hash`); a live token pair is revoked by bumping
//! `accounts.token_epoch`, or globally by rotating the `auth.jwt_key` secret.

#![forbid(unsafe_code)]

// The crate's interface is [`Auth`], the route bundles and the store trait; the private
// modules below are handlers over one `Auth` method each, so nothing outside names them.
pub(crate) mod activate;
pub mod consent;
pub mod gdpr;
pub mod job;
pub(crate) mod login;
pub mod pow;
pub mod register;
pub(crate) mod reset;
pub mod routes;
pub mod service_api;
pub(crate) mod stepup;
pub mod store;
pub mod tenant;
pub(crate) mod token;
pub(crate) mod totp;

pub use consent::{ConsentBody, PublishLegalDoc};
pub use job::{LinkMail, render};
pub use pow::{Challenge, Proof};
pub use routes::{authenticated, consent_gated_router, public};
pub use service_api::{Auth, ConsentGrant, Credentials, Erasure, LoginOutcome, Registration};
pub use stepup::StepUpResponse;
pub use store::{AuthStore, LegalKind, Role, TenantKind};
pub use tenant::{MemberBody, SwitchResponse, TenantDetail, TenantPatch, TenantSummary};
pub use token::{AccountBody, LoginBody, LoginTenant, TenantBody, Tokens};
pub use totp::{Enrolment, RecoveryCodes};

// vim: ts=4
