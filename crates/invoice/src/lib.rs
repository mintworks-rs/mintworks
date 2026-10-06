//! Invoicing: money and VAT arithmetic, tax rules, numbering and the invoice lifecycle.
//!
//! Every amount is an integer minor unit and every rate an integer basis point. No float
//! appears in the money path — not in computation, not in serialization, not in a fixture.

#![forbid(unsafe_code)]

// `party`'s `normalise_country` is re-exported below — it is one of the rules
// `mintworks_nav::auth::check_seller` applies to the `sellers` row, so the seller and buyer sides
// cannot drift.
pub(crate) mod catalog;
pub mod currency;
pub mod draft;
pub mod issue;
pub mod mnb;
pub mod money;
pub mod numbering;
pub(crate) mod party;
pub mod pdf;
pub mod pricing;
pub mod routes;
pub mod service_api;
pub mod store;
pub mod storno;
pub mod taxrule;
pub mod vat;
pub mod vies;

// `catalog` is `pub(crate)`, but `SellerView` is the return type of `Invoices::seller` and
// `seller_draft`, so no caller outside this crate can name what those two hand back.
pub use catalog::SellerView;
pub use currency::{Currency, RateMode, check_currency_settings, to_base};
pub use draft::{IssueNow, Line, NewDraft, Party};
// `mintworks-nav` files the invoice this crate issues, so the job kind it handles, the one seller
// and the store accessor are part of the interface rather than of `service_api`'s innards.
pub use issue::{KIND_NAV_REPORT, invoice_job_payload, tax_digits, vat_code_ok};
pub use money::{Discount, DraftLine, apportion, discount_of, discount_parts};
pub use numbering::date_of;
pub use party::normalise_country;
pub use pdf::{TEMPLATE_VERSION, render};
pub use pricing::PricingHook;
pub use routes::{org_invoices, org_parties, org_read, org_seller, org_services};
pub use service_api::{FullInvoice, Invoices, LinePatch, store as invoice_store};
pub use store::{
	DiscountKind, Invoice, InvoiceKind, InvoiceLine, InvoiceStore, InvoiceVatGroup, PartyKind,
	PaymentMethod, Seller, SellerVersion, SellerVersionPatch, SellerVersionStatus, Service,
	render_number,
};
pub use taxrule::{BuyerProfile, BuyerZone, Verdict, determine};
pub use vat::{ComputedInvoice, ComputedLine, VatClass, VatCode, VatGroup, compute};
pub use vies::ViesResult;

use mintworks_core::settings::SettingDef;

/// This crate's declared settings, registered with
/// `AppBuilder::settings(mintworks_invoice::SETTINGS)`.
pub static SETTINGS: &[SettingDef] = &[
	SettingDef::text(
		"currency.base",
		"HUF",
		"The accounting currency every invoice reconciles to.",
	)
	.range(3, 3),
	// MNB rates are legal only with a prior election notified to NAV; the statutory default is
	// a bank selling rate. `MANUAL` is listed because both CHECK constraints allow it, and
	// without it a deployment entering its own rates got `BANK` frozen onto the invoice.
	SettingDef::choice(
		"currency.rate_source",
		&["MNB", "ECB", "BANK", "MANUAL"],
		"BANK",
		"Which exchange rate an invoice is priced from.",
	),
	// How far back the dated rate lookup may reach. Seven days, not fewer: MNB publishes nothing
	// on weekends and can miss four consecutive days over Easter and Christmas. Without a bound,
	// a stalled fetch froze a months-old rate onto an invoice as its `exchangeRate` and HUF VAT.
	SettingDef::int(
		"currency.max_rate_age_days",
		"7",
		"How many days back a dated rate lookup may reach.",
	)
	.range(1, i64::MAX),
	// The UTC hour `FETCH_RATES` works in. Fixed, not "24h after seeding": a chain seeded before
	// the source publishes fetched ahead of it every day. MNB publishes around 11:00 CET, and
	// 11:00 UTC clears that year-round; `10` lands on publication in winter and arrives late.
	SettingDef::int("currency.rate_fetch_hour", "11", "UTC hour the daily rate fetch runs in.")
		.range(0, 23),
	// Both bounded at a century for the same reason as `jobs.retention_days`: the consumers
	// multiply by 86_400. `numbering::add_days` fed `time::Duration::days`, whose own
	// `expect` fires on overflow — a panic at issue time, on a job-runner task.
	SettingDef::int(
		"invoice.default_payment_days",
		"8",
		"Days until an invoice is due, when the draft names none.",
	)
	.range(0, 36_500),
	SettingDef::int(
		"invoice.draft_ttl_days",
		"30",
		"Days an untouched draft is kept before the sweep deletes it.",
	)
	.range(1, 36_500),
	// How many issued-but-undocumented invoices the daily draft sweep re-enqueues a
	// RENDER_PDF for. A missing PDF blocks the statutory NAV filing — `mintworks_nav::job::report`
	// answers `Unavailable` until the document row lands — so the sweep is a backstop, not the
	// recovery; `jobs.max_attempts.RENDER_PDF` is what keeps the render itself alive.
	SettingDef::int(
		"invoice.pdf_sweep_batch",
		"50",
		"Missing PDFs the daily sweep re-enqueues per run.",
	)
	.range(1, i64::MAX),
	// How long NAV_REPORT waits behind RENDER_PDF. `mintworks_nav::job::report` answers
	// `Unavailable` until the document row lands, so the same `run_at` made every invoice pay a
	// `2^attempts` backoff step for a race it always loses. `0` queues both at once. "15" restates
	// `issue::DEFAULT_NAV_REPORT_DELAY_SECS`; a default must be a string literal.
	SettingDef::int(
		"invoice.nav_report_delay_secs",
		"15",
		"Seconds a NAV filing waits behind its PDF render.",
	)
	.range(0, 3_600),
	// Bounded at a century for the same reason as `jobs.retention_days`: `vies::cached`
	// computes `days * 86_400`, which `i64::MAX` overflowed — a panic under
	// `[profile.release] overflow-checks`.
	SettingDef::int("vies.cache_days", "30", "Days a VIES VAT-number lookup is cached for.")
		.range(1, 36_500),
	// `0`, not the family's 8: `mintworks_nav::job::report` answers `Unavailable` until this render
	// lands, and a terminally FAILED one keeps `pdf:invoice:{id}` forever — so giving up on a
	// render gave up on a statutory filing that never gives up itself.
	SettingDef::int(
		"jobs.max_attempts.RENDER_PDF",
		"0",
		"Attempts before a PDF render is given up on; 0 is unbounded.",
	)
	.range(0, 1_000),
	SettingDef::int(
		"jobs.alert_after.RENDER_PDF",
		"3600",
		"Seconds a PDF render may keep failing before A-JOB-STALE.",
	)
	.range(0, 2_592_000),
];

// vim: ts=4
