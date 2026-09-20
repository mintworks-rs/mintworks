//! `PAYMENT_SWEEP`: the payer who paid and never came back. The return leg settles one who does
//! come back, and the callback settles one whose gateway can reach us; this is what catches the
//! tab that closed and the callback that was never delivered.

use std::sync::Arc;

use saas_core::app::App;
use saas_core::error::Retry;
use saas_core::job::{self, Job, Runner};
use saas_core::prelude::*;
use saas_core::store::CoreStore;

use crate::allocate;
use crate::provider::PaymentState;
use crate::store::store;

/// The job kind. The application registers the handler with
/// `AppBuilder::jobs(saas_billing::sweep::register)` and seeds it once with [`seed`].
pub const KIND: &str = "PAYMENT_SWEEP";

const SWEEP_EVERY_SECS: i64 = 300;

/// A row the gateway has not been re-asked about for this long. Well past the return leg's ask,
/// so a payer who is still on the page is not swept underneath them.
const STALE_AFTER_SECS: i64 = 120;

/// The age past which the sweep stops asking the gateway and rules on the row itself: a gateway
/// expires an unfinished payment long before, so an older row's verdict is already known.
const MAX_AGE_SECS: i64 = 7 * 86_400;

/// How long past its own deadline a payment is given before we call it expired ourselves. The
/// gateway is meant to do this at `expires_at`; an hour later it is not going to, and the
/// invoice would otherwise stay locked at PENDING with no way back to DRAFT.
const GIVE_UP_AFTER_SECS: i64 = 3_600;

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
	// Read once per tick, beside `now`: the deadline the stored row carries and the one a legacy
	// row would have are both derived from this window.
	let window_secs = app.settings.int("payment.window_minutes").await? * 60;
	let bstore = store(app)?;
	// Paged, as `dunning::sweep` is and for the same reason: `apply_state` writes nothing when
	// the fetched state equals the stored one, so `updated_at` never moves and a flat `LIMIT`
	// re-read the same oldest 200 rows every five minutes.
	let mut after: Option<i64> = None;
	let mut seen: i64 = 0;
	'pages: loop {
		let rows = bstore.live_payments(Timestamp(now.0 - STALE_AFTER_SECS), after, BATCH).await?;
		let short = i64::try_from(rows.len()).unwrap_or(i64::MAX) < BATCH;
		after = rows.last().map(|p| p.id);
		seen += i64::try_from(rows.len()).unwrap_or(0);
		for p in &rows {
			// Seven days is far past any window plus `GIVE_UP_AFTER_SECS`, so the verdict is already
			// known and the gateway is not worth a round trip: expire locally and let the row leave
			// the live set for good.
			let fresh = if p.created_at.0 < now.0 - MAX_AGE_SECS {
				p.clone()
			} else {
				// One bad row must not abandon the rest of the sweep; a gateway-wide refusal must,
				// because every remaining row would be the same round trip — and the batch is what
				// spends the quota the refusal is about.
				match allocate::refresh(app, p).await {
					Ok(fresh) => fresh,
					Err(e) => {
						if e.retry() == Retry::Backoff {
							tracing::warn!(
								payment = %p.uid.as_str(),
								error = %e,
								"payment sweep stopped: the gateway is not answering"
							);
							break 'pages;
						}
						tracing::warn!(payment = %p.uid.as_str(), error = %e, "payment not refreshed");
						continue;
					}
				}
			};
			// Our own verdict, not the gateway's: `settle_full` therefore accepts EXPIRED as a
			// `from`, so a gateway that captures afterwards is still heard.
			//
			// A row written before the window existed carries no deadline of its own; `created_at +
			// window` is the deadline it would have.
			let deadline = fresh.expires_at.map_or(fresh.created_at.0 + window_secs, |e| e.0);
			let too_old = fresh.created_at.0 < now.0 - MAX_AGE_SECS;
			if allocate::LIVE.contains(&fresh.status)
				&& (too_old || deadline + GIVE_UP_AFTER_SECS < now.0)
			{
				// One bad row must not abandon the rest of the page any more than the refresh above
				// may.
				if let Err(e) = allocate::apply_state(app, &fresh, PaymentState::Expired).await {
					tracing::warn!(payment = %fresh.uid.as_str(), error = %e, "payment not expired");
				}
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
