// SPDX-License-Identifier: MPL-2.0
//! What this crate may know about an invoice's NAV filing.
//!
//! `mintworks-nav` depends on this crate, never the other way round, so the storno gate and the
//! invoice view read the filing through [`FilingProbe`], which `mintworks-nav` implements over
//! `Nav::filing` and registers with `mintworks_nav::install`. With no probe registered (the nav
//! feature off) nothing here is asked, and the storno gate skips its NAV checks.

use async_trait::async_trait;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use serde::Serialize;

/// The filing of one invoice. Field names mirror the Rune `nav::filing` record
/// (`mintworks_nav::routes::filing_json`); the tags are `NavVerdict` / `FilingState` /
/// `AdoptResult` strings.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilingSummary {
	/// `DONE` / `WARN` / `REJECTED` / `FAILED`, absent until NAV answered.
	pub verdict: Option<String>,
	/// A `FilingState` tag, e.g. `SENT`, `ACCEPTED`, `HELD`.
	pub state: Option<String>,
	pub error_code: Option<String>,
	pub error_msg: Option<String>,
	pub created_at: Timestamp,
	pub done_at: Option<Timestamp>,
	pub resolved_at: Option<Timestamp>,
	pub adopt_result: Option<String>,
	pub adopt_checked_at: Option<Timestamp>,
	pub adopt_diff: Option<serde_json::Value>,
}

/// Registered with `AppBuilder::extension(Arc::new(p) as Arc<dyn FilingProbe>)`.
///
/// `Ok(None)` is "no filing yet". Readers treat an error as no filing, so a NAV outage never
/// breaks a read; the storno gate treats it as `NAV_UNKNOWN`.
#[async_trait]
pub trait FilingProbe: Send + Sync + 'static {
	async fn filing(&self, ctx: &Ctx, invoice_uid: &str) -> ClResult<Option<FilingSummary>>;

	/// Whether the seller (`sellers.id`) files with NAV at all. `false` lets a storno through
	/// with no filing to wait for: a seller that never connected has none coming.
	async fn files(&self, seller_id: i64) -> ClResult<bool>;
}

// vim: ts=4
