// SPDX-License-Identifier: MPL-2.0
//! The one extension point in `mintworks-invoice`.
//!
//! It exists because the framework originates some invoices itself — the payment-succeeded job, and
//! subscription renewal later — with no consumer in the loop. An application that never does that
//! never registers one.

use std::sync::Arc;

use async_trait::async_trait;
use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;

use crate::money::DraftLine;

/// Last word on what a draft's lines cost, called after the `services` rows have been
/// resolved and converted into the invoice currency, and before VAT is computed.
///
/// Registered with `AppBuilder::extension(Arc::new(MyPricing) as Arc<dyn PricingHook>)`.
/// The default is the no-op, so an implementation only overrides what it cares about.
///
/// **Called exactly once per draft**, in `Invoices::draft`, when the caller's lines are first
/// resolved. Never again: not on `add_line`/`edit_line`/`patch`, and not at issue. The stored
/// lines are the hook's output, so a hook is free to append lines or scale prices without
/// being idempotent — which it was not, when `issue::run` used to re-run it over its own
/// already-persisted result and put the doubled amount on a numbered, immutable invoice.
///
/// The corollary is that a hook does not see edits made after the draft was created. Changing
/// that needs either an idempotency contract or hook-authored lines segregated from
/// caller-authored ones; re-running it on the edit path is not the fix.
#[async_trait]
pub trait PricingHook: Send + Sync + 'static {
	async fn price(&self, _ctx: &Ctx, _lines: &mut Vec<DraftLine>) -> ClResult<()> {
		Ok(())
	}
}

/// Runs the registered hook, or nothing when the application registered none.
pub async fn apply(app: &App, ctx: &Ctx, lines: &mut Vec<DraftLine>) -> ClResult<()> {
	match app.extensions.get::<Arc<dyn PricingHook>>().cloned() {
		Some(hook) => hook.price(ctx, lines).await,
		None => Ok(()),
	}
}

// vim: ts=4
