// SPDX-License-Identifier: MPL-2.0
//! NAV Online Számla reporting for Mintworks.
//!
//! Reporting is asynchronous: the invoice issue path enqueues a job, and nothing here
//! is ever called from it. The official XSDs this crate's XML is validated against are
//! vendored in `xsd/` — see `xsd/README.md` for their provenance.

#![forbid(unsafe_code)]

pub mod auth;
pub mod client;
// NAV's signature and token crypto. Nothing outside the crate calls it.
pub(crate) mod crypto;
pub mod diff;
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
	E_NAV_SUBMISSION_STATE, FilingPage, Nav, NavCredentials, NavCredentialsStatus,
	NavCredentialsTest, NavHeld, alerts,
};
pub use store::NavStore;
pub use submission::{
	AdoptResult, BatchState, FilingState, NavBatch, NavFiling, NavOp, NavSellerHold, NavSubmission,
	NavVerdict, RowState,
};

use std::sync::{Arc, OnceLock};

use mintworks_core::settings::SettingDef;
use mintworks_invoice::{FilingProbe, FilingSummary};

/// This crate's declared settings. An application registers them with
/// `AppBuilder::settings(mintworks_nav::SETTINGS)`, which is also what opts it into being asked for
/// NAV configuration at boot.
///
/// The `software` block's XSD lengths and charsets are here rather than in a boot check: the
/// registry could not express them only while it lived in `mintworks-core`, which may not depend on
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
	// (`mintworks-nav/xsd/invoiceApi.xsd:1082`), and one token covers one request however many
	// invoices it carries (interface specification §1.1).
	SettingDef::int(
		"nav.batch_max",
		"100",
		"Invoices per manageInvoice request; NAV's own ceiling is 100.",
	)
	.range(1, 100),
	// A filing is a statutory obligation, so every NAV kind is unbounded and the alert — not the
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
	// Explicit, not inherited: `max_attempts` is 0 for all of them, so A-JOB-STALE is their only
	// alert and it lands at ERROR — the number an operator is paged on belongs where they can read it.
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
	// `reconcile` paces itself at 1.1 s per transaction, up to 2000 in a slice, and a timeout restarts
	// it at the newest slice. The longest of these keys is also every kind's reclaim lease.
	SettingDef::int(
		"jobs.timeout_secs.NAV_RECONCILE",
		"3600",
		"Seconds reconciliation may run before the runner gives up.",
	)
	.range(0, 86_400),
	// `NAV_REPORT` and `NAV_SEND` take the family's 900, not two minutes: each builds up to
	// `nav.batch_max` invoiceData documents. `NAV_POLL` makes one call, and aborting it cannot
	// lose a `transactionId`.
	SettingDef::int(
		"jobs.timeout_secs.NAV_POLL",
		"120",
		"Seconds a status poll may run before the runner gives up.",
	)
	.range(0, 86_400),
	SettingDef::int(
		"jobs.max_attempts.NAV_SEND",
		"0",
		"Attempts before sending a batch is given up on; 0 is unbounded.",
	)
	.range(0, 1_000),
	SettingDef::int(
		"jobs.backoff_cap.NAV_SEND",
		"600",
		"Retry backoff ceiling for sending a batch, in seconds.",
	)
	.range(1, 86_400),
	SettingDef::int(
		"jobs.alert_after.NAV_SEND",
		"3600",
		"Seconds sending a batch may keep failing before A-JOB-STALE.",
	)
	.range(0, 2_592_000),
	SettingDef::int(
		"jobs.max_attempts.NAV_ADOPT",
		"0",
		"Attempts before an adoption check is given up on; 0 is unbounded.",
	)
	.range(0, 1_000),
	SettingDef::int(
		"jobs.backoff_cap.NAV_ADOPT",
		"600",
		"Retry backoff ceiling for an adoption check, in seconds.",
	)
	.range(1, 86_400),
	SettingDef::int(
		"jobs.alert_after.NAV_ADOPT",
		"3600",
		"Seconds an adoption check may keep failing before A-JOB-STALE.",
	)
	.range(0, 2_592_000),
	// `NAV_ADOPT` paces itself at one `queryInvoiceData` per 1.1 s (§1.6.11), across a whole batch.
	SettingDef::int(
		"jobs.timeout_secs.NAV_ADOPT",
		"900",
		"Seconds an adoption check may run before the runner gives up.",
	)
	.range(0, 86_400),
	// The grace before a fault on a `CLAIMED` batch needs a person, so a transient one pages nobody.
	SettingDef::int(
		"nav.fault_alert_after",
		"3600",
		"Seconds a batch's fault may last before it counts as needing a person.",
	)
	.range(0, 2_592_000),
];

/// The three NAV credentials, in `secrets`. Declared so the unprefixed environment namespace
/// cannot collide with a setting's variable.
pub static SECRETS: &[&str] = &["nav.tech_password", "nav.sign_key", "nav.exchange_key"];

/// Registers what `mintworks-invoice` reads NAV through: the [`FilingProbe`] behind the storno
/// gate and `canStorno`. Call it on the builder of every app that registers [`SETTINGS`];
/// without it the gate skips its NAV checks.
#[must_use]
pub fn install(b: mintworks_core::app::AppBuilder) -> mintworks_core::app::AppBuilder {
	// Constructed before `build()` freezes the extension map, handed the `App` from `on_init`
	// (the `BarionProvider::deferred` pattern).
	let probe = Arc::new(NavProbe::default());
	let attach = Arc::clone(&probe);
	b.extension(probe as Arc<dyn FilingProbe>).on_init(move |app| async move {
		let _ = attach.app.set(app);
		Ok(())
	})
}

/// [`FilingProbe`] over [`Nav::filing`].
#[derive(Default)]
struct NavProbe {
	app: OnceLock<mintworks_core::App>,
}

#[async_trait::async_trait]
impl FilingProbe for NavProbe {
	async fn filing(
		&self,
		ctx: &mintworks_core::ctx::Ctx,
		invoice_uid: &str,
	) -> mintworks_core::prelude::ClResult<Option<FilingSummary>> {
		let app = self.app.get().ok_or_else(|| {
			mintworks_core::prelude::Error::internal(
				"mintworks-nav: the filing probe was never attached",
			)
		})?;
		let found = Nav::new(app.clone()).filing(ctx, invoice_uid).await?;
		Ok(found.map(|f| (&f).into()))
	}

	/// [`crate::auth::connected`], the predicate the filing job itself goes by.
	async fn files(&self, seller_id: i64) -> mintworks_core::prelude::ClResult<bool> {
		use mintworks_core::prelude::Error;
		let app = self
			.app
			.get()
			.ok_or_else(|| Error::internal("mintworks-nav: the filing probe was never attached"))?;
		let seller = mintworks_invoice::service_api::store(app)?
			.seller_by_id(seller_id)
			.await?
			.ok_or(Error::NotFound)?;
		crate::auth::connected(app, &seller).await
	}
}

impl From<&NavFiling> for FilingSummary {
	fn from(f: &NavFiling) -> Self {
		let (s, batch) = (&f.row, f.batch.as_ref());
		FilingSummary {
			verdict: s.verdict.map(|v| v.as_str().to_owned()),
			state: Some(f.state().as_str().to_owned()),
			// The row's own first; a batch-level fault is all a held or oversize filing has.
			error_code: s.error_code.clone().or_else(|| batch.and_then(|b| b.error_code.clone())),
			error_msg: s.error_msg.clone().or_else(|| batch.and_then(|b| b.error_msg.clone())),
			created_at: s.created_at,
			done_at: s.done_at,
			resolved_at: s.resolved_at,
			adopt_result: s.adopt_result.map(|r| r.as_str().to_owned()),
			adopt_checked_at: s.adopt_checked_at,
			adopt_diff: s.adopt_detail.as_deref().and_then(|d| {
				serde_json::from_str(d)
					.inspect_err(|e| {
						tracing::error!(submission = s.id, "adopt_detail is not a NavDiff: {e}");
					})
					.ok()
			}),
		}
	}
}

// vim: ts=4
