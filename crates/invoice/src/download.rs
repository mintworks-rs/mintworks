// SPDX-License-Identifier: MPL-2.0
//! The public download link an invoice email carries instead of the PDF.
//!
//! A link is a `refs` row of type [`REF_TYPE`] owned by the invoice's org: `target` is the invoice
//! uid and `params.sha256` the file, so it only ever serves the exact PDF that was issued, until
//! `expires_at` (`invoice.download_ttl_days` after minting). The org's admin lists and revokes
//! them through `/api/refs` like any other ref.

use mintworks_core::app::App;
use mintworks_core::prelude::*;

/// Listed in `mintworks_core::refs::RESERVED_TYPES`: a hand-crafted one would serve any target.
pub const REF_TYPE: &str = "invoice_download";

/// `{base_url}/api/public/invoices/{code}/pdf`.
#[must_use]
pub fn url(app: &App, code: &str) -> String {
	let base = app.config.base_url.trim_end_matches('/');
	format!("{base}/api/public/invoices/{code}/pdf")
}

/// The expiry a link minted now carries: `invoice.download_ttl_days` from now.
///
/// # Errors
/// Whatever the settings store raises.
pub async fn expiry(app: &App) -> ClResult<i64> {
	Ok(Timestamp::now().0 + app.settings.int("invoice.download_ttl_days").await? * 86_400)
}

// vim: ts=4
