// SPDX-License-Identifier: MPL-2.0
//! `DUNNING_SWEEP`: the daily reminder that an issued invoice is past its `due_date`, and the
//! aging list the same query feeds to the admin API.

use std::sync::Arc;

use mintworks_core::app::App;
use mintworks_core::job::{self, Job, Runner};
use mintworks_core::prelude::*;
use mintworks_core::store::CoreStore;

use crate::store::{OverdueInvoice, store};

/// The job kind. The application registers the handler with
/// `AppBuilder::jobs(mintworks_billing::dunning::register)` and seeds it once with [`seed`].
pub const KIND: &str = "DUNNING_SWEEP";

const SWEEP_EVERY_SECS: i64 = 86_400;

/// One page of the sweep, not its total: the query is not filtered by "already reminded", so a
/// flat cap meant the same oldest 500 came back every day and invoice 501 onward was never
/// dunned at all for as long as the backlog held.
pub const BATCH: i64 = 500;

/// The most one tick will walk, so a runaway org cannot make a single sweep unbounded.
const MAX_PER_TICK: i64 = 20_000;

/// Days past `due_date` at which a reminder goes out, ascending and comma separated.
/// `SettingDef` has no list type, so the list is text rather than an `int[]`.
pub const SCHEDULE_SETTING: &str = "dunning.schedule_days";

/// Template base name: `payment_reminder.{hu.,}{html,txt}.hbs`.
const TEMPLATE: &str = "payment_reminder";

fn schedule(raw: &str) -> Vec<i64> {
	let mut days: Vec<i64> = raw
		.split(',')
		.filter_map(|s| s.trim().parse::<i64>().ok())
		.filter(|d| *d >= 0)
		.collect();
	days.sort_unstable();
	days
}

/// Every issued invoice past its due date and not yet fully paid, oldest first — the admin
/// API's aging list. `org_id` `None` is every org, which is what the sweep asks for.
pub async fn aging(app: &App, org_id: Option<i64>, limit: i64) -> ClResult<Vec<OverdueInvoice>> {
	store(app)?.overdue_invoices(org_id, None, limit).await
}

/// `AppBuilder::jobs(mintworks_billing::dunning::register)`.
pub fn register(runner: &mut Runner, app: App) {
	// The schedule is read inside the tick: `register_periodic` fixes the period at boot, but
	// an operator's change to `dunning.schedule_days` must reach the next sweep unrestarted.
	runner.register_periodic(KIND, SWEEP_EVERY_SECS, move |_job: Job| {
		let app = app.clone();
		async move { sweep(&app).await }
	});
}

/// Seeds the periodic sweep. Call once at boot.
pub async fn seed(store: &Arc<dyn CoreStore>) -> ClResult<()> {
	job::seed_periodic(store, KIND).await
}

/// One pass of the reminder sweep. Public so a test can drive it without the job runner; the
/// application reaches it through [`register`].
pub async fn sweep(app: &App) -> ClResult<()> {
	let steps = schedule(&app.settings.text(SCHEDULE_SETTING).await?);
	if steps.is_empty() {
		return Ok(());
	}
	let bstore = store(app)?;
	let mut after: Option<i64> = None;
	let mut seen: i64 = 0;
	loop {
		let batch = bstore.overdue_invoices(None, after, BATCH).await?;
		let short = i64::try_from(batch.len()).unwrap_or(i64::MAX) < BATCH;
		after = batch.last().map(|i| i.invoice_id);
		seen += i64::try_from(batch.len()).unwrap_or(0);
		for inv in batch {
			// The furthest step reached, not every step passed: an invoice first seen three weeks
			// late sends one reminder, not all three at once.
			let Some(step) = steps.iter().copied().rfind(|d| *d <= inv.days_overdue) else {
				continue;
			};
			let Some(to) = inv.buyer_email.clone() else {
				tracing::debug!(invoice = %inv.invoice_uid.as_str(), "overdue, but no buyer e-mail");
				continue;
			};

			let owed = inv.outstanding.to_wire(&inv.currency);
			// The `SEND_EMAIL` payload is `mintworks_email::SendEmail`, built by hand because this
			// crate does not depend on `mintworks-email` — `mintworks_core::alert` enqueues mail
			// the same way.
			let payload = serde_json::json!({
				"to": to,
				"template": TEMPLATE,
				// The frozen buyer snapshot's country is the only locale signal an issued invoice
				// carries; anything without its own file falls back to the English template.
				"lang": if inv.buyer_country.as_deref() == Some("HU") { "hu" } else { "" },
				"vars": {
					"name": inv.buyer_name,
					"invoice_number": inv.number,
					"due_date": inv.due_date,
					"days_overdue": inv.days_overdue,
					"amount": owed.amount,
					"currency": owed.currency,
				},
			})
			.to_string();

			// The `dedup_key` *is* the "already sent" record, and a permanent one: `job_sweep`
			// deletes a finished row only when its key is NULL or `periodic:`-prefixed, so one
			// step can never mail twice. Same cost as `RENDER_PDF`'s key — a terminally FAILED
			// reminder holds it for good and is never resent.
			let key = format!("dunning:{}:{}", inv.invoice_id, step);
			let queued = job::enqueue(
				&app.store,
				job::KIND_SEND_EMAIL,
				&payload,
				Some(&key),
				Timestamp::now(),
			)
			.await;
			match queued {
				Ok(Some(_)) => {
					tracing::info!(invoice = %inv.invoice_uid.as_str(), step, "dunning reminder queued");
				}
				Ok(None) => {}
				// One bad row must not abandon the rest of the sweep.
				Err(e) => {
					tracing::warn!(invoice = %inv.invoice_uid.as_str(), error = %e, "reminder not queued");
				}
			}
		}
		if short || seen >= MAX_PER_TICK {
			if !short {
				tracing::warn!(
					seen,
					"dunning sweep hit its per-tick ceiling; the rest waits a day"
				);
			}
			break;
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::schedule;

	#[test]
	fn schedule_parses_sorts_and_drops_junk() {
		assert_eq!(schedule("10, 3,20"), vec![3, 10, 20]);
		assert_eq!(schedule(""), Vec::<i64>::new());
		assert_eq!(schedule("7,,x,-1"), vec![7]);
	}

	#[test]
	fn furthest_step_reached_wins() {
		let steps = schedule("3,10,20");
		let pick = |d: i64| steps.iter().copied().rfind(|s| *s <= d);
		assert_eq!(pick(2), None);
		assert_eq!(pick(3), Some(3));
		assert_eq!(pick(19), Some(10));
		assert_eq!(pick(99), Some(20));
	}
}

// vim: ts=4
