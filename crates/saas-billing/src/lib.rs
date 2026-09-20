//! Payments for the saas-framework: money arrives from somewhere, and some of it settles an
//! invoice.
//!
//! Two boundaries meet here and neither is crossed. The gateway is behind
//! [`provider::PaymentProvider`], implemented in `adapters/payment-adapter-barion` and
//! registered through the `Extensions` type-map, so no gateway's name, URL or status string
//! appears in this crate. Persistence is behind [`store::BillingStore`], implemented in
//! `adapters/store-adapter-sqlite`, so no SQL and no `sqlx` type appears either.

#![forbid(unsafe_code)]

use saas_core::prelude::*;

pub mod allocate;
pub mod dunning;
pub mod provider;
pub mod refund;
pub mod routes;
pub mod store;
pub mod sweep;
pub mod webhook;

pub use allocate::{Allocation, ManualPayment, StartRequest, apply_state};
pub use provider::{
	CallbackRef, PaymentAddress, PaymentItem, PaymentProvider, PaymentProviders, PaymentState,
	ProviderCaps, RefundResult, StartPayment, StartedPayment,
};
pub use refund::refund;
pub use store::{
	BillingStore, NewPayment, OverdueInvoice, Payment, PaymentAllocation, RefundRecord, Settlement,
};

/// `A-PAY-UNALLOCATED`: money that arrived and settles no invoice. A `SUCCEEDED` or
/// `PARTIALLY_SUCCEEDED` payment whose allocations do not cover it is a customer who has been
/// charged and an invoice still reading unpaid — `allocate::settle_full` only writes a
/// `tracing::warn` when the link row is missing or ambiguous, and a partial allocates nothing
/// by design, so this is the standing record for all three.
///
/// # Errors
/// Propagates the store read; `Error::Internal` when no [`BillingStore`] was registered.
pub async fn alerts(app: saas_core::App) -> ClResult<Vec<saas_core::alert::Alert>> {
	// 48 h: shorter and every payment in flight over a long weekend raises it.
	let bstore = store::store(&app)?;
	let (count, since) =
		bstore.unallocated_payments(Timestamp(Timestamp::now().0 - 48 * 3600)).await?;
	let mut out = Vec::new();
	if count > 0 {
		out.push(saas_core::alert::Alert {
			code: "A-PAY-UNALLOCATED",
			severity: saas_core::alert::Severity::Warn,
			count,
			message: format!(
				"{count} payment(s) succeeded more than 48 hours ago and settle no invoice: the \
				 payer was charged and the invoice still reads unpaid"
			),
			since,
			link: Some("/api/admin/payments".into()),
		});
	}

	// `Error`, not `Warn`: the gateway paid out and nothing in our tables records it, so the
	// figure an operator reads is wrong and their retry sends the money a second time.
	let (count, since) = bstore.refund_discrepancies().await?;
	if count > 0 {
		out.push(saas_core::alert::Alert {
			code: "A-PAY-REFUND-UNRECORDED",
			severity: saas_core::alert::Severity::Error,
			count,
			message: format!(
				"{count} refund(s) were made at the gateway but not recorded: `refundedAmount` \
				 understates what was given back, and retrying one pays it out again"
			),
			since,
			link: Some("/api/admin/payments".into()),
		});
	}
	Ok(out)
}

use saas_core::settings::SettingDef;

/// This crate's declared settings, registered with
/// `AppBuilder::settings(saas_billing::SETTINGS)`.
pub static SETTINGS: &[SettingDef] = &[
	// A family, not a key per gateway: `saas-billing` is gateway-agnostic by invariant, so no
	// gateway may be named under `crates/` — the payment adapter owns the spelling below
	// `payment.` and the registry only has to admit it. Text, like `ratelimit.`: a family has
	// one declaration, so a per-gateway value the adapter parses itself is the price.
	//
	// There is deliberately no per-gateway environment override: `deployment.env` is the one
	// flag. If one is ever genuinely needed, this family already admits `payment.<gw>.base_url`
	// with no registry change.
	SettingDef::text("payment.", "", "A payment adapter's own key; the adapter owns the spelling.")
		.family(),
	// An exact key under the `payment.` family above, which is legal and is the shape
	// `ratelimit.default` already has under `ratelimit.`: `Registry::definition` consults the
	// exact map before scanning families, so this key carves itself out and every other
	// `payment.*` still falls to the family's text default.
	SettingDef::int(
		"payment.window_minutes",
		"10",
		"How long a gateway payment stays open before it expires. Also how long a payer who \
		 closed the gateway tab waits before they may pay another way.",
	)
	.range(1, 1440),
	// `SettingDef` has no list type, so the list is text and `dunning::schedule` parses it.
	SettingDef::text(
		"dunning.schedule_days",
		"3,10,20",
		"Days after the due date a reminder is sent; empty disables dunning.",
	),
];

// vim: ts=4
