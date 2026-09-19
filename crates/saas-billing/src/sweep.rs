//! `PAYMENT_SWEEP`: the payer who paid and never came back. Reading an invoice's payments
//! re-asks the gateway, so a returning payer settles themselves; this is what catches the one
//! who closed the tab, and a callback that was never delivered.

use std::sync::Arc;

use saas_core::app::App;
use saas_core::job::{self, Job, Runner};
use saas_core::prelude::*;
use saas_core::store::CoreStore;

use crate::allocate;
use crate::store::store;

/// The job kind. The application registers the handler with
/// `AppBuilder::jobs(saas_billing::sweep::register)` and seeds it once with [`seed`].
pub const KIND: &str = "PAYMENT_SWEEP";

const SWEEP_EVERY_SECS: i64 = 300;

/// A row the gateway has not been re-asked about for this long. Well clear of the SPA's poll,
/// so a payer who is still on the page is not swept underneath them.
const STALE_AFTER_SECS: i64 = 120;

/// The same, for a `CANCELED` row. `abandon` is our own give-up and the gateway was told
/// nothing, so it still has to be re-asked — but no payer is on a page waiting for it, and at
/// the live threshold a backlog of abandoned payments ate the whole per-tick ceiling.
const CANCELED_STALE_AFTER_SECS: i64 = 3_600;

/// A payment older than this is abandoned, not slow: a gateway expires an unfinished payment
/// long before, so anything still live here is a row no gateway answer will ever move.
const MAX_AGE_SECS: i64 = 7 * 86_400;

/// How many stale payments one page holds.
const BATCH: i64 = 200;

/// How many one tick works through before leaving the rest for the next. Each row is a gateway
/// round trip, so the ceiling is what keeps one pass off the gateway's rate limit.
const MAX_PER_TICK: i64 = 2_000;

/// `AppBuilder::jobs(saas_billing::sweep::register)`.
pub fn register(runner: &mut Runner, app: App) {
	runner.register_periodic(KIND, SWEEP_EVERY_SECS, move |_job: Job| {
		let app = app.clone();
		async move { tick(&app).await }
	});
}

/// Seeds the periodic sweep. Call once at boot.
pub async fn seed(store: &Arc<dyn CoreStore>) -> ClResult<()> {
	job::seed_periodic(store, KIND).await
}

/// One pass. Public because a test drives it, and because it is a safe manual kick.
pub async fn tick(app: &App) -> ClResult<()> {
	let now = Timestamp::now();
	let bstore = store(app)?;
	// Paged, as `dunning::sweep` is and for the same reason: `apply_state` writes nothing when
	// the fetched state equals the stored one, so `updated_at` never moves and a flat `LIMIT`
	// re-read the same oldest 200 rows every five minutes.
	let mut after: Option<i64> = None;
	let mut seen: i64 = 0;
	loop {
		let rows = bstore
			.live_payments(
				Timestamp(now.0 - STALE_AFTER_SECS),
				Timestamp(now.0 - CANCELED_STALE_AFTER_SECS),
				Timestamp(now.0 - MAX_AGE_SECS),
				after,
				BATCH,
			)
			.await?;
		let short = i64::try_from(rows.len()).unwrap_or(i64::MAX) < BATCH;
		after = rows.last().map(|p| p.id);
		seen += i64::try_from(rows.len()).unwrap_or(0);
		for p in &rows {
			// One bad row must not abandon the rest of the sweep.
			if let Err(e) = allocate::refresh(app, p).await {
				tracing::warn!(payment = %p.uid.as_str(), error = %e, "payment not refreshed");
			}
		}
		if short || seen >= MAX_PER_TICK {
			if !short {
				tracing::warn!(seen, "payment sweep hit its per-tick ceiling; the rest waits");
			}
			break;
		}
	}
	Ok(())
}

// vim: ts=4
