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
pub mod filing;
pub mod job;
pub mod reply;
pub mod routes;
pub(crate) mod service_api;
pub mod store;
pub mod submission;
pub mod xml;

pub use service_api::{
	E_NAV_BATCH_MEMBER, E_NAV_CANCELLED, E_NAV_SUBMISSION_STATE, Nav, NavCredentials,
	NavCredentialsStatus, alerts,
};
pub use store::NavStore;
pub use submission::{NavOp, NavSubmission, NavVerdict};

use saas_core::settings::SettingDef;

/// This crate's declared settings. An application registers them with
/// `AppBuilder::settings(saas_nav::SETTINGS)`, which is also what opts it into being asked for
/// NAV configuration at boot.
///
/// The `software` block's XSD lengths and charsets are here rather than in a boot check: the
/// registry could not express them only while it lived in `saas-core`, which may not depend on
/// this crate. A 60-character `softwareName` or a lowercase `"hu"` made every request
/// schema-invalid, and `NAV_REPORT` retries that forever.
pub static SETTINGS: &[SettingDef] = &[
	// An explicit endpoint, for a host `deployment.env` cannot name. Blank means "derive it from
	// `deployment.env`".
	SettingDef::text(
		"nav.base_url",
		"",
		"Explicit NAV endpoint; blank derives it from deployment.env.",
	),
	SettingDef::text(
		"nav.software_id",
		"",
		"NAV-registered software id: 18 characters of [0-9A-Z-].",
	)
	.range(0, 18)
	.required()
	.check(auth::check_software_id),
	SettingDef::text("nav.software_name", "", "Software name sent on every NAV request.")
		.range(0, 50)
		.required()
		.check(auth::check_software_text),
	SettingDef::choice(
		"nav.software_operation",
		&["LOCAL_SOFTWARE", "ONLINE_SERVICE"],
		"LOCAL_SOFTWARE",
		"Whether this deployment is on-premise software or an online service.",
	),
	SettingDef::text("nav.software_main_version", "", "Software main version sent to NAV.")
		.range(0, 15)
		.required()
		.check(auth::check_software_text),
	SettingDef::text("nav.software_dev_name", "", "Software developer's name.")
		.range(0, 512)
		.required()
		.check(auth::check_software_text),
	SettingDef::text("nav.software_dev_contact", "", "Software developer's contact address.")
		.range(0, 200)
		.required()
		.check(auth::check_software_text),
	SettingDef::text(
		"nav.software_dev_tax_number",
		"",
		"Software developer's tax number; optional.",
	)
	.range(0, 50)
	.check(auth::check_software_text),
	SettingDef::text(
		"nav.software_dev_country",
		"HU",
		"Software developer's ISO-3166 alpha-2 country.",
	)
	.range(2, 2)
	.check(auth::check_country_code),
	// Áfa tv. 175. § makes an invoice electronic only with the buyer's acceptance, and
	// `manageInvoice` files once — a deployment that delivers on paper must be able to stop
	// asserting it. On by default: the archived hash is the only independent proof that the
	// buyer's PDF is the issued one.
	SettingDef::flag(
		"nav.electronic_invoice",
		"1",
		"Whether filings assert the invoice is electronic.",
	),
	// The ceiling is NAV's, not ours: `invoiceOperation maxOccurs="100"`
	// (`saas-nav/xsd/invoiceApi.xsd:1082`), and one token covers one request however many
	// invoices it carries (interface specification §1.1).
	SettingDef::int(
		"nav.batch_max",
		"100",
		"Invoices per manageInvoice request; NAV's own ceiling is 100.",
	)
	.range(1, 100),
	// A filing is a statutory obligation, so both kinds are unbounded and the alert — not the
	// runner — ends the loop; `Nav::cancel_filing` is how a person stops one NAV will never
	// accept. The 10-minute ceiling is NAV's own poll rhythm.
	SettingDef::int(
		"jobs.max_attempts.NAV_REPORT",
		"0",
		"Attempts before a filing is given up on; 0 is unbounded.",
	)
	.range(0, 1_000),
	SettingDef::int(
		"jobs.backoff_cap.NAV_REPORT",
		"600",
		"Retry backoff ceiling for a filing, in seconds.",
	)
	.range(1, 86_400),
	SettingDef::int(
		"jobs.max_attempts.NAV_POLL",
		"0",
		"Attempts before a status poll is given up on; 0 is unbounded.",
	)
	.range(0, 1_000),
	SettingDef::int(
		"jobs.backoff_cap.NAV_POLL",
		"600",
		"Retry backoff ceiling for a status poll, in seconds.",
	)
	.range(1, 86_400),
	SettingDef::int(
		"jobs.alert_after.NAV_POLL",
		"86400",
		"Seconds a poll may keep failing before A-JOB-STALE.",
	)
	.range(0, 2_592_000),
	// Explicit, not inherited: `max_attempts` is 0 for both, so A-JOB-STALE is their only alert
	// and it lands at ERROR — the number an operator is paged on belongs where they can read it.
	SettingDef::int(
		"jobs.alert_after.NAV_REPORT",
		"3600",
		"Seconds a filing may keep failing before A-JOB-STALE.",
	)
	.range(0, 2_592_000),
	// `0`, not the family's 8: giving up on reconciliation leaves the batch in exactly the state
	// reconciliation exists to resolve — a `manageInvoice` whose reply was lost, with up to
	// `nav.batch_max` invoices whose status with NAV nothing else can establish.
	SettingDef::int(
		"jobs.max_attempts.NAV_RECONCILE",
		"0",
		"Attempts before reconciliation is given up on; 0 is unbounded.",
	)
	.range(0, 1_000),
	SettingDef::int(
		"jobs.alert_after.NAV_RECONCILE",
		"3600",
		"Seconds reconciliation may keep failing before A-JOB-STALE.",
	)
	.range(0, 2_592_000),
	// Explicit like its two siblings: with `max_attempts` at 0 the cap *is* the retry rhythm
	// against the tax authority, so it belongs where an operator can read it.
	SettingDef::int(
		"jobs.backoff_cap.NAV_RECONCILE",
		"600",
		"Retry backoff ceiling for reconciliation, in seconds.",
	)
	.range(1, 86_400),
	// `NAV_REPORT` takes the family's 900, not two minutes: a leader builds up to
	// `nav.batch_max` invoiceData documents and archives a request row per member before the
	// POST. `NAV_POLL` still makes one call, and aborting it cannot lose a `transactionId`.
	SettingDef::int(
		"jobs.timeout_secs.NAV_POLL",
		"120",
		"Seconds a status poll may run before the runner gives up.",
	)
	.range(0, 86_400),
];

/// The three NAV credentials, in `secrets`. Declared so the unprefixed environment namespace
/// cannot collide with a setting's variable.
pub static SECRETS: &[&str] = &["nav.tech_password", "nav.sign_key", "nav.exchange_key"];

// vim: ts=4
