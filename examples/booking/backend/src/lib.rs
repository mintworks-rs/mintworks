#![forbid(unsafe_code)]
//! Everything the example is except its composition root, which stays in `main.rs`.
//!
//! The library target exists so `tests/flow.rs` can drive the real router: without it the
//! modules had to be pulled in by `#[path]`, which left `routes.rs` — auth, the consent gate,
//! the error envelope, the checkout's payment-method choice — compiled by nothing but the
//! binary.

pub mod bookings;
pub mod routes;
pub mod seed;
pub mod store;

use saas_core::settings::SettingDef;

/// This application's own keys. `dist_dir` reads `DIST_DIR`: unprefixed, like a framework key,
/// but declared here rather than in `saas-core`, so no framework crate reads it.
pub static SETTINGS: &[SettingDef] = &[SettingDef::text(
	"dist_dir",
	DIST_DIR_DEFAULT,
	"Directory the built SPA is served from; environment-only, a row does nothing.",
)];

const DIST_DIR_DEFAULT: &str = "../frontend/dist";

/// Placeholder identity for NAV's test system only.
pub const NAV_SOFTWARE_TEST: [(&str, &str); 7] = [
	("nav.software_id", "SAASEXAMPLE0000001"),
	("nav.software_name", "saas-framework example"),
	("nav.software_operation", "ONLINE_SERVICE"),
	("nav.software_main_version", "0.1"),
	("nav.software_dev_name", "saas-framework"),
	("nav.software_dev_contact", "dev@example.com"),
	("nav.software_dev_country", "HU"),
];
/// Template: fill in the identity NAV registered for your software. Blank is absent, so a blank
/// required key refuses to boot until this or `NAV_SOFTWARE_*` supplies it.
pub const NAV_SOFTWARE_PROD: [(&str, &str); 7] = [
	("nav.software_id", ""),
	("nav.software_name", ""),
	("nav.software_operation", "ONLINE_SERVICE"),
	("nav.software_main_version", ""),
	("nav.software_dev_name", ""),
	("nav.software_dev_contact", ""),
	("nav.software_dev_country", ""),
];

/// Where `main.rs`'s SPA fallback serves from.
///
/// Resolved from the environment rather than through `Settings`, because the router is
/// assembled before any `App` exists — the same constraint that keeps the Barion gate an
/// environment read. The declaration above is what types and documents it.
#[must_use]
pub fn dist_dir() -> String {
	std::env::var(saas_core::settings::env_name("dist_dir"))
		.ok()
		.filter(|v| !v.trim().is_empty())
		.unwrap_or_else(|| DIST_DIR_DEFAULT.to_owned())
}

// vim: ts=4
