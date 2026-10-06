//! What needs a person, computed from state that already exists.
//!
//! **Pull, never push**: there is no alert table, no unread flag and no alert-raising side effect
//! on any failure path. [`alerts`] asks the database what is wrong *now*, so it loses nothing, is
//! idempotent, and costs a failing job nothing.
//!
//! [`Alert`]'s field set is the `alerts` array of `GET /api/admin/stats` and is a frozen contract —
//! the admin dashboard serializes this struct as-is.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{app::App, auth_mw::JWT_SECRET_KEY, prelude::*};

/// Ordered: `Error` is above `Warn`, which is what "the severity rose since the previous
/// sweep" compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Severity {
	Warn,
	Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
	pub code: &'static str,
	pub severity: Severity,
	pub count: i64,
	/// Rendered for a human, already carrying `count`.
	pub message: String,
	/// When the oldest instance of this condition started; `None` when the condition has no
	/// onset (a threshold crossed by an aggregate).
	pub since: Option<Timestamp>,
	/// Where an operator goes to act on it.
	pub link: Option<String>,
}

/// Every alert that currently holds, most severe first.
///
/// Three come from the `jobs` table — the work did not happen — and two from core's own state:
/// nothing failed there, the answer is just bad. Feature crates append theirs through
/// [`crate::app::AppBuilder::alerts`].
///
/// Takes the whole `App` rather than the store and settings it started with: `A-DISK-LOW` needs
/// `config.data_dir` and the contributed sources live on `AppState`, and both always arrive from
/// the same place anyway.
pub async fn alerts(app: &App) -> ClResult<Vec<Alert>> {
	let (store, settings) = (&app.store, &app.settings);
	let mut out = Vec::new();
	let counts = store.job_status_counts().await?;
	let find = |status: &str| counts.iter().find(|(s, ..)| s == status).cloned();

	// A-JOB-FAILED — a job gave up, `E-NAV-CANCELLED` included: a cancelled statutory filing is
	// what somebody should still see. Age-bounded on the newest *transition* into `FAILED`:
	// unbounded, one year-old failure pinned the dashboard at ERROR; bounded on the newest
	// *enqueue*, a filing cancelled today on an old job was hidden.
	let failed_window = settings.int("jobs.failed_alert_hours").await? * 3600;
	if let Some((_, count, since, newest_transition)) = find("FAILED")
		&& newest_transition.0 > Timestamp::now().0 - failed_window
	{
		out.push(Alert {
			code: "A-JOB-FAILED",
			severity: Severity::Error,
			count,
			message: format!("{count} job(s) failed"),
			since: Some(since),
			link: Some("/api/admin/jobs?status=FAILED".into()),
		});
	}

	// A-JOB-BACKLOG — the queue is not keeping up. Nothing has failed; there is just more
	// work than workers.
	let backlog_warn = settings.int("jobs.backlog_warn").await?;
	match find("PENDING") {
		Some((_, count, since, _)) if count > backlog_warn => out.push(Alert {
			code: "A-JOB-BACKLOG",
			severity: Severity::Warn,
			count,
			message: format!("{count} jobs pending, above the {backlog_warn} threshold"),
			since: Some(since),
			link: Some("/api/admin/jobs?status=PENDING".into()),
		}),
		_ => {}
	}

	// A-JOB-STALE — still retrying, past its kind's `jobs.alert_after`. Waiting for `FAILED`
	// would mean never telling anyone: a kind whose `max_attempts` is 0 never fails at all, and
	// "this invoice is still not filed" persists for hours.
	let now = Timestamp::now();
	for kind in store.job_retrying_kinds().await? {
		let after = settings.int(&format!("jobs.alert_after.{kind}")).await?;
		if after == 0 {
			continue; // 0 disables the alert for this kind.
		}
		if let Some((count, since)) = store.job_stale(&kind, Timestamp(now.0 - after)).await? {
			// ERROR, not WARN, when the kind is unbounded: `max_attempts = 0` never reaches FAILED,
			// so A-JOB-STALE is the only alert it can ever raise — and `admin.alert_min_severity`
			// defaults to ERROR, which no WARN clears. That set is the statutory kinds:
			// NAV_REPORT, NAV_POLL, RENDER_PDF.
			let severity = match settings.int(&format!("jobs.max_attempts.{kind}")).await? {
				0 => Severity::Error,
				_ => Severity::Warn,
			};
			let age =
				if after >= 3600 { format!("{}h", after / 3600) } else { format!("{after}s") };
			out.push(Alert {
				code: "A-JOB-STALE",
				severity,
				count,
				message: format!("{count} {kind} job(s) still retrying after {age}"),
				since: Some(since),
				link: Some(format!("/api/admin/jobs?kind={kind}")),
			});
		}
	}

	// A-SECRET-STALE — the JWT signing key has not been rotated in a year. `None` is not this
	// alert's business: an unset key is a deployment that has not booted auth yet, not a stale
	// one.
	let key_max_age = settings.int("auth.key_max_age_days").await?;
	if let Some(changed) = store.secret_updated_at(0, JWT_SECRET_KEY).await? {
		let age_days = (now.0 - changed.0) / 86_400;
		if age_days > key_max_age {
			out.push(Alert {
				code: "A-SECRET-STALE",
				severity: Severity::Warn,
				count: 1,
				message: format!(
					"the JWT signing key was last rotated {age_days} days ago, above the \
					 {key_max_age} day threshold"
				),
				since: Some(changed),
				link: None,
			});
		}
	}

	// A-DISK-LOW — the `DATA_DIR` filesystem is filling. A rendered PDF and an emailed
	// attachment both land there, so a full disk fails invoice issue.
	let free_warn_mb = settings.int("storage.free_warn_mb").await?;
	if free_warn_mb > 0 {
		match rustix::fs::statvfs(app.config.data_dir.as_str()) {
			// `f_bavail`, not `f_bfree`: the root-reserved blocks are not space this process
			// can write into, and on a default ext4 that is 5% of the device.
			Ok(vfs) => {
				let free_mb = vfs.f_bavail.saturating_mul(vfs.f_frsize) / (1024 * 1024);
				let free_mb = i64::try_from(free_mb).unwrap_or(i64::MAX);
				if free_mb < free_warn_mb {
					out.push(Alert {
						code: "A-DISK-LOW",
						severity: Severity::Warn,
						count: 1,
						message: format!(
							"{free_mb} MB free on the data directory, below the {free_warn_mb} MB \
							 threshold"
						),
						since: None,
						link: None,
					});
				}
			}
			// Not an error return: a data directory that cannot be stat'd is its own problem,
			// and losing every other alert to it is the worse outcome.
			Err(e) => {
				tracing::warn!(error = %e, dir = %app.config.data_dir, "cannot stat data dir");
			}
		}
	}

	// What the feature crates registered. `mintworks-core` cannot depend on them, so
	// `A-NAV-REJECTED` and `A-RATE-MISSING` arrive here from `mintworks-nav` and
	// `mintworks-invoice` themselves.
	for source in &app.alert_sources {
		out.extend(source(app.clone()).await?);
	}

	out.sort_by_key(|a| std::cmp::Reverse(a.severity));
	Ok(out)
}

/// The one `vars` row alerting keeps. There is no alert table behind this name.
const VAR_PREV: &str = "alert.prev";

/// The previous sweep, as stored under [`VAR_PREV`]: when it ran, and what held then.
#[derive(Default, Deserialize)]
struct Prev {
	at: i64,
	codes: BTreeMap<String, Severity>,
}

/// The memo key for one alert: its code *and* its link.
///
/// `A-JOB-STALE` is pushed once per job kind, discriminated only by `link`, so a memo keyed on
/// the code alone collapsed them: while `NAV_POLL` stayed stale, `SEND_EMAIL` going stale was
/// never mailed. A unit separator, so it cannot occur in either half.
fn memo_key(a: &Alert) -> String {
	match &a.link {
		Some(link) => format!("{}\u{1f}{link}", a.code),
		None => a.code.to_owned(),
	}
}

/// One `ALERT_SWEEP` tick: recompute [`alerts`], email what is new, remember the result.
///
/// *State-free by design* — the comparison set is one `vars` row, so there is no alert table and no
/// unread flag. An alert is mailed when its code was absent from the previous sweep or its severity
/// has risen since; one that merely persists is not mailed again, which is what makes
/// `admin.alert_interval_minutes` the re-notify floor as well as the period.
///
/// Most ticks return early. The job fires every minute because
/// [`Runner::register_periodic`](crate::job::Runner::register_periodic) fixes its period at
/// registration, and gating here instead is what lets an operator change the interval without
/// restarting the process.
pub async fn sweep(app: &App) -> ClResult<()> {
	let now = Timestamp::now();
	let prev: Prev = match app.store.var_get(VAR_PREV).await? {
		// A row that no longer parses is a format change, not a reason to stop alerting: it
		// counts as "no previous sweep", so everything currently holding is mailed once.
		Some(raw) => serde_json::from_str(&raw).unwrap_or_default(),
		None => Prev::default(),
	};
	let every = app.settings.int("admin.alert_interval_minutes").await?;
	if now.0 - prev.at < every.saturating_mul(60) {
		return Ok(());
	}

	let current = alerts(app).await?;
	let min = match app.settings.text("admin.alert_min_severity").await?.as_str() {
		"WARN" => Severity::Warn,
		_ => Severity::Error,
	};
	let fresh: Vec<&Alert> = current
		.iter()
		.filter(|a| a.severity >= min)
		// Absent before, or worse than it was: "was not there at all" has to pass too.
		.filter(|a| prev.codes.get(&memo_key(a)).is_none_or(|was| *was < a.severity))
		.collect();

	// The one alarm this channel cannot deliver is the one about itself: the push path is
	// `SEND_EMAIL`, so an alert naming a stuck email kind joins the queue it reports on. Matched
	// on the rendered text, because `A-JOB-STALE`'s kind only appears there.
	if fresh.iter().any(|a| {
		a.message.contains("EMAIL") || a.link.as_deref().is_some_and(|l| l.contains("EMAIL"))
	}) {
		tracing::error!(alerts = ?fresh,
			"email delivery is among the alerts, so the alert mail may never be delivered");
	}

	// Empty is the normal state of a deployment with no operator mailbox, not a
	// misconfiguration: the sweep still runs, and the dashboard still shows the list.
	let to = app.settings.text("admin.alert_email").await?;
	if !fresh.is_empty() && !to.is_empty() {
		// Built as JSON rather than as a `mintworks_email::SendEmail`: dependencies point inward,
		// so `mintworks-core` cannot name that type. The field names are the contract, and
		// `the_alert_sweeps_hand_built_payload_still_deserialises` is the other half.
		let payload = serde_json::json!({
			"to": to,
			"template": "alert",
			"lang": "",
			"vars": { "alerts": fresh },
		});
		// No dedup key, so this cannot collide and the id is of no use here.
		let _id = crate::job::enqueue(
			&app.store,
			crate::job::KIND_SEND_EMAIL,
			&payload.to_string(),
			None,
			now,
		)
		.await?;
	}

	// The whole computed set, not just `fresh`: it is what the *next* sweep compares against.
	let codes: BTreeMap<String, Severity> =
		current.iter().map(|a| (memo_key(a), a.severity)).collect();
	let raw = serde_json::json!({ "at": now.0, "codes": codes }).to_string();
	app.store.var_set(VAR_PREV, &raw).await
}

// vim: ts=4
