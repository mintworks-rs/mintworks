//! Barion Smart Gateway, behind `saas_billing::PaymentProvider`.
//!
//! Everything Barion-shaped lives here and stops here: [`map`] holds the wire structs and the
//! status vocabulary, [`client`] the calls. A leaf — it depends on `saas-billing` and
//! `saas-core` and on no other framework crate, takes no store and reads no table of ours.

#![forbid(unsafe_code)]

pub mod client;
pub mod map;

pub use client::{
	BarionProvider, Credentials, PRODUCTION_BASE_URL, PROVIDER_ID, SANDBOX_BASE_URL, base_url_for,
};

/// This adapter's secret, declared so the unprefixed environment namespace cannot collide with
/// a setting's variable. `payment.barion.payee` needs no declaration of its own: it lives under
/// `saas_billing::SETTINGS`' `payment.` family.
pub static SECRETS: &[&str] = &[client::POS_KEY_SECRET];

// vim: ts=4
