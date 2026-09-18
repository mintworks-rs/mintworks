//! The `settings` table: typed, validated, DB-backed runtime configuration.
//!
//! Resolution order for a key is row, then environment, then the registry default. The
//! environment variable is `SAAS_` followed by the key uppercased with `.` replaced by
//! `_`, so `currency.base` reads `SAAS_CURRENCY_BASE`.
//!
//! The registry is the whole vocabulary: a key that is not declared here is not a
//! setting, and reading or writing one is [`Error::Setting`]. Two declarations are
//! prefixes (`pow.difficulty.`, `ratelimit.`) covering a family of per-scope keys; an
//! exact declaration always wins over a prefix, which is how `ratelimit.default`
//! coexists with `ratelimit.<route>`.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::{
	error::{ClResult, Error},
	gencache::GenCache,
	store::CoreStore,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingType {
	Text,
	Int,
	Flag,
	Choice(&'static [&'static str]),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SettingValue {
	Text(String),
	Int(i64),
	Flag(bool),
}

/// A declared key. `min`/`max` bound the value for `Int` and the length for `Text` and
/// `Choice`; they are ignored for `Flag`.
#[derive(Clone, Debug)]
pub struct SettingDef {
	pub key: &'static str,
	pub ty: SettingType,
	pub default: &'static str,
	pub min: i64,
	pub max: i64,
	/// `key` is a prefix matching a family (`pow.difficulty.login`), not one key.
	pub prefix: bool,
	/// A blank value is a misconfiguration, not a default. Enforced by
	/// [`Settings::check_required`] at boot rather than by [`parse`]: an unconfigured
	/// deployment must still be able to *read* the key.
	pub required: bool,
	/// Ran after the type check, for a `Text` value whose shape `min`/`max` cannot express.
	/// `Err` here is the same `E-CORE-SETTING` a range violation is — the point is that it
	/// happens when the operator writes the row, not on every request afterwards.
	pub check: Option<fn(&str) -> ClResult<()>>,
}

impl SettingDef {
	const fn new(key: &'static str, ty: SettingType, default: &'static str) -> Self {
		Self {
			key,
			ty,
			default,
			min: i64::MIN,
			max: i64::MAX,
			prefix: false,
			required: false,
			check: None,
		}
	}

	pub const fn check(mut self, f: fn(&str) -> ClResult<()>) -> Self {
		self.check = Some(f);
		self
	}

	pub const fn text(key: &'static str, default: &'static str) -> Self {
		Self::new(key, SettingType::Text, default)
	}

	pub const fn int(key: &'static str, default: &'static str) -> Self {
		Self::new(key, SettingType::Int, default)
	}

	pub const fn flag(key: &'static str, default: &'static str) -> Self {
		Self::new(key, SettingType::Flag, default)
	}

	pub const fn choice(
		key: &'static str,
		allowed: &'static [&'static str],
		default: &'static str,
	) -> Self {
		Self::new(key, SettingType::Choice(allowed), default)
	}

	pub const fn range(mut self, min: i64, max: i64) -> Self {
		self.min = min;
		self.max = max;
		self
	}

	pub const fn family(mut self) -> Self {
		self.prefix = true;
		self
	}

	pub const fn required(mut self) -> Self {
		self.required = true;
		self
	}
}

/// Every key the framework recognises; the `settings` table is the schema of record.
pub static REGISTRY: &[SettingDef] = &[
	SettingDef::text("currency.base", "HUF").range(3, 3),
	// MNB rates are legal only with a prior election notified to NAV; the statutory default is
	// a bank selling rate. `MANUAL` is listed because both CHECK constraints allow it, and
	// without it a deployment entering its own rates got `BANK` frozen onto the invoice.
	SettingDef::choice("currency.rate_source", &["MNB", "ECB", "BANK", "MANUAL"], "BANK"),
	// How far back the dated rate lookup may reach. Seven days, not fewer: MNB publishes nothing
	// on weekends and can miss four consecutive days over Easter and Christmas. Without a bound,
	// a stalled fetch froze a months-old rate onto an invoice as its `exchangeRate` and HUF VAT.
	SettingDef::int("currency.max_rate_age_days", "7").range(1, i64::MAX),
	// The UTC hour `FETCH_RATES` works in. Fixed, not "24h after seeding": a chain seeded before
	// the source publishes fetched ahead of it every day. MNB publishes around 11:00 CET, and
	// 11:00 UTC clears that year-round; `10` lands on publication in winter and arrives late.
	SettingDef::int("currency.rate_fetch_hour", "11").range(0, 23),
	// Both bounded at a century for the same reason as `jobs.retention_days`: the consumers
	// multiply by 86_400. `numbering::add_days` fed `time::Duration::days`, whose own
	// `expect` fires on overflow — a panic at issue time, on a job-runner task.
	SettingDef::int("invoice.default_payment_days", "8").range(0, 36_500),
	SettingDef::int("invoice.draft_ttl_days", "30").range(1, 36_500),
	// How many issued-but-undocumented invoices the daily draft sweep re-enqueues a
	// RENDER_PDF for. A missing PDF blocks the statutory NAV filing — `saas_nav::job::report`
	// answers `Unavailable` until the document row lands — so the sweep is a backstop, not the
	// recovery; `jobs.max_attempts.RENDER_PDF` is what keeps the render itself alive.
	SettingDef::int("invoice.pdf_sweep_batch", "50").range(1, i64::MAX),
	// How long NAV_REPORT waits behind RENDER_PDF. `saas_nav::job::report` answers `Unavailable`
	// until the document row lands, so the same `run_at` made every invoice pay a `2^attempts`
	// backoff step for a race it always loses. `0` queues both at once. "15" restates
	// `saas_invoice::issue::DEFAULT_NAV_REPORT_DELAY_SECS`; a default must be a string literal.
	SettingDef::int("invoice.nav_report_delay_secs", "15").range(0, 3_600),
	// No default on purpose: defaulting to the *test* endpoint let a production deployment file
	// every invoice into a system that answers OK and reaches DONE, so nothing looked wrong and
	// nothing statutory was reported. `nav::auth::check_software_settings` refuses to boot blank.
	SettingDef::text("nav.base_url", "").required(),
	SettingDef::text("nav.software_id", "").range(0, 18).required(),
	SettingDef::text("nav.software_name", "").required(),
	SettingDef::choice(
		"nav.software_operation",
		&["LOCAL_SOFTWARE", "ONLINE_SERVICE"],
		"LOCAL_SOFTWARE",
	),
	SettingDef::text("nav.software_main_version", "").required(),
	SettingDef::text("nav.software_dev_name", "").required(),
	SettingDef::text("nav.software_dev_contact", "").required(),
	SettingDef::text("nav.software_dev_tax_number", ""),
	SettingDef::text("nav.software_dev_country", "HU").range(2, 2),
	// Áfa tv. 175. § makes an invoice electronic only with the buyer's acceptance, and
	// `manageInvoice` files once — a deployment that delivers on paper must be able to stop
	// asserting it. On by default: the archived hash is the only independent proof that the
	// buyer's PDF is the issued one.
	SettingDef::flag("nav.electronic_invoice", "1"),
	// The ceiling is NAV's, not ours: `invoiceOperation maxOccurs="100"`
	// (`saas-nav/xsd/invoiceApi.xsd:1082`), and one token covers one request however many
	// invoices it carries (interface specification §1.1).
	SettingDef::int("nav.batch_max", "100").range(1, 100),
	SettingDef::int("pow.difficulty.", "18").range(1, 32).family(),
	SettingDef::text("ratelimit.default", "120/min/ip").check(crate::ratelimit::check_limit),
	SettingDef::text("ratelimit.", "120/min/ip")
		.family()
		.check(crate::ratelimit::check_limit),
	SettingDef::text("email.from", "").required().check(check_address),
	SettingDef::text("email.from.name", ""),
	SettingDef::text("email.smtp.host", "").required(),
	SettingDef::int("email.smtp.port", "587").range(1, 65535),
	SettingDef::text("email.smtp.username", ""),
	SettingDef::choice("email.smtp.tls_mode", &["none", "starttls", "tls"], "starttls"),
	SettingDef::int("email.smtp.timeout_seconds", "30").range(1, 600),
	SettingDef::text("email.template_dir", "./templates/email"),
	// Bounded at a century for the same reason as `jobs.retention_days`: `vies::cached`
	// computes `days * 86_400`, which `i64::MAX` overflowed — a panic under
	// `[profile.release] overflow-checks`.
	SettingDef::int("vies.cache_days", "30").range(1, 36_500),
	SettingDef::int("auth.stepup_window", "300").range(1, i64::MAX),
	// How long a session may be renewed for, from its `auth_at`. With no session table and no
	// denylist, without this a captured refresh token renewed itself indefinitely. An hour is
	// the floor because below the refresh TTL it is a logout.
	SettingDef::int("auth.session_max_seconds", "2592000").range(3_600, 31_536_000),
	// Bounded at 20: `RateLimiter::consumed` reads the `AUTH_FAILED` bucket, which `ratelimit.rs`
	// fixes at `20/5min/ip`. A higher threshold is unreachable and silently disables the gate.
	SettingDef::int("auth.pow_after_failures", "3").range(0, 20),
	// Bounded because `totp::confirm` argon2id-hashes each one in a loop while holding a
	// `HASH_SLOTS` permit: an unbounded count hung `POST /api/auth/totp/verify` and starved
	// every other password hash in the process with it.
	SettingDef::int("auth.recovery_codes", "8").range(1, 64),
	// Whether `POST /api/auth/register` accepts new accounts. Login, activation and reset
	// stay up when it is off, so an operator can close signups during an abuse wave without
	// locking out the accounts that already exist. See `saas_auth::Auth::register`.
	SettingDef::flag("auth.registration_open", "1"),
	// How old `secrets['auth.jwt_key']` may get before `A-SECRET-STALE` says so. Advisory only:
	// nothing rotates the key automatically, because rotating it signs every session out.
	SettingDef::int("auth.key_max_age_days", "365").range(1, 36_500),
	// One job per worker; one alone starves every other kind behind a NAV sweep's timeouts.
	// Bounded because `AppBuilder::build` spawns exactly this many tasks before returning. `0`
	// means this process runs no jobs and reclaims nothing — per-process that is `JOBS_WORKERS`.
	SettingDef::int("jobs.workers", "2").range(0, 64),
	// How long a finished job with **no** `dedup_key` is kept. A keyed row is never swept
	// whatever this says: its `dedup_key` is the once-only guarantee. Bounded at a century
	// because the sweep's `days * 86_400` panics under `overflow-checks`.
	SettingDef::int("jobs.retention_days", "90").range(1, 36_500),
	// How recent the newest `FAILED` job must be for `A-JOB-FAILED` to be raised. Rows live until
	// `jobs.retention_days`, so without a window one failure pinned the dashboard at ERROR.
	SettingDef::int("jobs.failed_alert_hours", "24").range(1, 8_760),
	// Pending jobs above this raise `A-JOB-BACKLOG`. Nothing has failed at that point — the
	// queue is simply not keeping up, which `jobs.workers` is the lever for.
	SettingDef::int("jobs.backlog_warn", "100").range(1, 1_000_000),
	// Per-kind retry policy, families rather than one number because the kinds want opposite
	// things: a statutory obligation must not be given up on, a one-off notification must be.
	// The bare stem resolves to nothing — always ask for `jobs.<family>.<KIND>`. `0` is
	// unbounded. "8" restates `job::DEFAULT_MAX_ATTEMPTS`; a default must be a string literal.
	SettingDef::int("jobs.max_attempts.", "8").family().range(0, 1_000),
	// The ceiling `backoff_secs` clamps `2^attempts` to. One hour by default.
	SettingDef::int("jobs.backoff_cap.", "3600").family().range(1, 86_400),
	// How long a kind may keep failing before it is worth telling a human about. Read by
	// `alert::alerts` for `A-JOB-STALE`. `0` disables.
	SettingDef::int("jobs.alert_after.", "3600").family().range(0, 2_592_000),
	// How long a handler may run before the runner gives up on it. `0` disables the deadline.
	// Without one, a relay that answers every 29 s holds a worker forever and the row stays
	// `RUNNING` — invisible to `job_claim`, `job_stale` and every alert until a restart.
	SettingDef::int("jobs.timeout_secs.", "900").family().range(0, 86_400),
	// A filing is a statutory obligation, so both kinds are unbounded and the alert — not the
	// runner — ends the loop; `Nav::cancel_filing` is how a person stops one NAV will never
	// accept. The 10-minute ceiling is NAV's own poll rhythm.
	SettingDef::int("jobs.max_attempts.NAV_REPORT", "0").range(0, 1_000),
	SettingDef::int("jobs.backoff_cap.NAV_REPORT", "600").range(1, 86_400),
	SettingDef::int("jobs.max_attempts.NAV_POLL", "0").range(0, 1_000),
	SettingDef::int("jobs.backoff_cap.NAV_POLL", "600").range(1, 86_400),
	SettingDef::int("jobs.alert_after.NAV_POLL", "86400").range(0, 2_592_000),
	// Explicit, not inherited: `max_attempts` is 0 for both, so A-JOB-STALE is their only alert
	// and it lands at ERROR — the number an operator is paged on belongs where they can read it.
	SettingDef::int("jobs.alert_after.NAV_REPORT", "3600").range(0, 2_592_000),
	// `0`, not the family's 8: giving up on reconciliation leaves the batch in exactly the state
	// reconciliation exists to resolve — a `manageInvoice` whose reply was lost, with up to
	// `nav.batch_max` invoices whose status with NAV nothing else can establish.
	SettingDef::int("jobs.max_attempts.NAV_RECONCILE", "0").range(0, 1_000),
	SettingDef::int("jobs.alert_after.NAV_RECONCILE", "3600").range(0, 2_592_000),
	// Explicit like its two siblings: with `max_attempts` at 0 the cap *is* the retry rhythm
	// against the tax authority, so it belongs where an operator can read it.
	SettingDef::int("jobs.backoff_cap.NAV_RECONCILE", "600").range(1, 86_400),
	SettingDef::int("jobs.alert_after.RENDER_PDF", "3600").range(0, 2_592_000),
	// `NAV_REPORT` takes the family's 900, not two minutes: a leader builds up to
	// `nav.batch_max` invoiceData documents and archives a request row per member before the
	// POST. `NAV_POLL` still makes one call, and aborting it cannot lose a `transactionId`.
	SettingDef::int("jobs.timeout_secs.NAV_POLL", "120").range(0, 86_400),
	// `0`, not the family's 8: `saas_nav::job::report` answers `Unavailable` until this render
	// lands, and a terminally FAILED one keeps `pdf:invoice:{id}` forever — so giving up on a
	// render gave up on a statutory filing that never gives up itself.
	SettingDef::int("jobs.max_attempts.RENDER_PDF", "0").range(0, 1_000),
	// A mail waits out a misconfiguration rather than dying on attempt one: nothing re-drives a
	// `SEND_EMAIL` row, so a terminal failure here loses an activation link for good. 14 attempts
	// under the 3600 s cap is roughly half a day.
	SettingDef::int("jobs.max_attempts.SEND_EMAIL", "14").range(0, 1_000),
	SettingDef::int("jobs.max_attempts.AUTH_LINK_EMAIL", "14").range(0, 1_000),
	// Free space on the `DATA_DIR` filesystem below which `alert::alerts` raises `A-DISK-LOW`,
	// against `statvfs` on `config.data_dir`. `0` disables the check.
	SettingDef::int("storage.free_warn_mb", "512").range(0, 1_000_000),
	// Where `ALERT_SWEEP` mails newly appeared alerts. Empty — the default — disables alert
	// mail entirely; `alert::alerts` still computes the list, and the admin dashboard still
	// shows it. A deployment without an operator mailbox is normal, not misconfigured.
	SettingDef::text("admin.alert_email", ""),
	// Floor on what is worth an email. `ERROR` mails only what has actually failed; `WARN`
	// mails everything `alerts()` returns.
	SettingDef::choice("admin.alert_min_severity", &["WARN", "ERROR"], "ERROR"),
	// How often the alert set is recomputed, and so the re-notify floor per code. `ALERT_SWEEP`
	// ticks every minute and returns early until this many have passed, so a change applies
	// without a restart — `Runner::register_periodic` fixes its period at boot.
	SettingDef::int("admin.alert_interval_minutes", "60").range(1, 1_440),
	// Comma-separated reverse proxies in front of this process. `X-Forwarded-For` is read **only**
	// when the direct peer is one of them; empty means trust nothing, because an unvalidated
	// header forges a fresh rate-limit bucket per request. Unset behind a real proxy, every
	// per-IP bucket keys on the proxy and the limits collapse into one.
	SettingDef::text("http.trusted_proxy", "").check(crate::auth_mw::check_trusted_proxy),
];

/// RFC 5322-shaped enough to catch an operator's typo: one `@`, a non-empty local part, and a
/// domain carrying a dot. `saas-core` cannot depend on `lettre`, so `sender::build`'s own
/// `Mailbox` parse stays the backstop — but that one fails inside a job handler, hours after
/// the row was written.
///
/// Empty passes: `required` is what refuses a blank one, and [`Settings::get`] parses on every
/// read, so an unconfigured deployment must still be able to read the key.
fn check_address(raw: &str) -> ClResult<()> {
	let mut parts = raw.split('@');
	let shaped = match (parts.next(), parts.next(), parts.next()) {
		(Some(""), ..) => raw.is_empty(),
		(Some(_), Some(domain), None) => {
			domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
		}
		_ => false,
	};
	if shaped { Ok(()) } else { Err(Error::Setting(format!("'{raw}' is not an email address"))) }
}

/// Resolves a key against [`REGISTRY`]. Exact declarations beat prefix families.
pub fn definition(key: &str) -> ClResult<&'static SettingDef> {
	REGISTRY
		.iter()
		.find(|d| !d.prefix && d.key == key)
		.or_else(|| {
			REGISTRY
				.iter()
				.filter(|d| d.prefix && key.starts_with(d.key) && key.len() > d.key.len())
				.max_by_key(|d| d.key.len())
		})
		.ok_or_else(|| Error::Setting(format!("unknown setting '{key}'")))
}

pub(crate) fn env_name(key: &str) -> String {
	format!("SAAS_{}", key.to_uppercase().replace(['.', '-'], "_"))
}

/// `vars_os`, not `vars`, which panics on a non-UTF-8 variable — this runs in
/// `AppBuilder::build`, before any `CatchPanicLayer`, so the process dies at startup.
/// [`env_name`] only ever builds `SAAS_*`, so nothing else is worth a process-lifetime copy.
fn env_snapshot() -> HashMap<String, String> {
	std::env::vars_os()
		.filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
		.filter(|(k, _)| k.starts_with("SAAS_"))
		.collect()
}

/// Parses and validates `raw` against the declaration. This is the only validation path:
/// writes go through it before they reach the table, so a stored row always parses.
pub fn parse(def: &SettingDef, raw: &str) -> ClResult<SettingValue> {
	let bad = |what: &str| Error::Setting(format!("setting '{}': {what}", def.key));
	let raw = raw.trim();
	match def.ty {
		SettingType::Text | SettingType::Choice(_) => {
			if let SettingType::Choice(allowed) = def.ty
				&& !allowed.contains(&raw)
			{
				return Err(bad(&format!("must be one of {allowed:?}")));
			}
			let len = i64::try_from(raw.chars().count()).unwrap_or(i64::MAX);
			if len < def.min || len > def.max {
				return Err(bad(&format!("length must be {}..={}", def.min, def.max)));
			}
			if let Some(check) = def.check {
				check(raw).map_err(|e| bad(&e.to_string()))?;
			}
			Ok(SettingValue::Text(raw.to_string()))
		}
		SettingType::Int => {
			let n: i64 = raw.parse().map_err(|_| bad("must be an integer"))?;
			if n < def.min || n > def.max {
				return Err(bad(&format!("must be {}..={}", def.min, def.max)));
			}
			Ok(SettingValue::Int(n))
		}
		SettingType::Flag => match raw {
			"1" | "true" | "yes" | "on" => Ok(SettingValue::Flag(true)),
			"0" | "false" | "no" | "off" => Ok(SettingValue::Flag(false)),
			_ => Err(bad("must be a boolean")),
		},
	}
}

/// Reads settings through a process-local cache, invalidated on write.
///
/// The reader/writer split this needs — a service method holding `write_tx()` may read an
/// uncached setting, and routing that read to the writer would make it wait on the connection
/// it is itself holding — is the adapter's obligation behind [`CoreStore`].
pub struct Settings {
	store: Arc<dyn CoreStore>,
	/// The raw row behind [`Settings::row`]. [`Settings::get`] is built on it, so the table
	/// is cached once rather than once per question asked of it.
	rows: GenCache<Option<String>>,
	/// The parsed result of [`Settings::get`]. Without it `parse` — and with it the
	/// declaration's `check` hook — ran on every read: `auth_mw::client_ip_mw` re-parsed every
	/// configured proxy address on every request.
	values: GenCache<SettingValue>,
	/// The `SAAS_*` variables, snapshotted once: `std::env::var` takes a process-wide lock, and
	/// the "no row" case is the normal one, so [`Settings::get`] paid for it on every
	/// rate-limited request.
	env: HashMap<String, String>,
}

impl Settings {
	/// The `SAAS_*` environment is read **here**, so changing one of those variables at runtime
	/// needs a restart — as `ratelimit::env_limits` already required. Settings rows, the
	/// operator-facing lever, stay live.
	pub fn new(store: Arc<dyn CoreStore>) -> Self {
		Self { store, rows: GenCache::new(), values: GenCache::new(), env: env_snapshot() }
	}

	/// Row, then environment, then registry default.
	pub async fn get(&self, key: &str) -> ClResult<SettingValue> {
		let miss = match self.values.lookup(key) {
			Ok(v) => return Ok(v),
			Err(miss) => miss,
		};
		let def = definition(key)?;
		let raw = match self.row(key).await? {
			Some(v) => v,
			None => {
				self.env.get(&env_name(key)).cloned().unwrap_or_else(|| def.default.to_string())
			}
		};
		let value = parse(def, &raw)?;
		self.values.store(key, miss, value.clone());
		Ok(value)
	}

	/// The `settings` row exactly as an operator wrote it, or `None` when the key has no row.
	/// Deliberately **no** environment or registry-default fallback.
	///
	/// [`Settings::get`] cannot answer this. For a prefix family such as `ratelimit.` it
	/// returns the family default for a key nobody ever set, so a caller that treated that as
	/// an operator override would apply `ratelimit.default` to every named scope —
	/// `RateLimiter::check` needs to tell the two apart before it can honour an override.
	pub async fn row(&self, key: &str) -> ClResult<Option<String>> {
		let miss = match self.rows.lookup(key) {
			Ok(v) => return Ok(v),
			Err(miss) => miss,
		};
		let value = self.store.setting_get(key).await?;
		self.rows.store(key, miss, value.clone());
		Ok(value)
	}

	/// Validates, writes and invalidates the cached entry.
	pub async fn set(&self, key: &str, raw: &str, updated_by: Option<i64>) -> ClResult<()> {
		let def = definition(key)?;
		parse(def, raw)?;
		self.store.setting_set(key, raw.trim(), updated_by).await?;
		self.rows.invalidate(key);
		self.values.invalidate(key);
		Ok(())
	}

	/// Refuse to boot while a key under `prefix` is blank-but-`required` or does not parse.
	///
	/// Per feature prefix (`"email."`, `"nav."`), never globally: [`REGISTRY`] is one
	/// vocabulary over every feature crate, but the crates are optional — a consumer
	/// embedding only `saas-auth` must not be made to configure NAV. Each feature calls this
	/// from the boot hook it already has.
	///
	/// Every declared key goes through [`Settings::get`], which resolves row → environment →
	/// registry default and parses: one pass catches a bad `settings` row, a bad `SAAS_*`
	/// override and a bad default alike. Without it the first place a misconfiguration
	/// surfaced was inside a job handler, as an `Error::Setting` — `Retry::Never`, which
	/// destroyed every queued activation link rather than delaying it.
	///
	/// Every failure at once: naming one at a time makes an operator restart per key.
	/// Family declarations are skipped — a prefix has no single value to check.
	pub async fn check_required(&self, prefix: &str) -> ClResult<()> {
		let mut missing = Vec::new();
		let mut invalid = Vec::new();
		for def in REGISTRY.iter().filter(|d| !d.prefix && d.key.starts_with(prefix)) {
			match self.get(def.key).await {
				Err(e) => invalid.push(format!("{} ({e})", def.key)),
				Ok(SettingValue::Text(s)) if def.required && s.trim().is_empty() => {
					missing.push(def.key.to_owned());
				}
				Ok(_) => {}
			}
		}
		let mut refusals = Vec::new();
		if !missing.is_empty() {
			refusals.push(format!("these settings must be configured: {}", missing.join(", ")));
		}
		if !invalid.is_empty() {
			refusals.push(format!("these settings are invalid: {}", invalid.join("; ")));
		}
		if refusals.is_empty() {
			return Ok(());
		}
		Err(Error::Setting(refusals.join("; ")))
	}

	pub async fn text(&self, key: &str) -> ClResult<String> {
		match self.get(key).await? {
			SettingValue::Text(s) => Ok(s),
			_ => Err(Error::Setting(format!("setting '{key}' is not textual"))),
		}
	}

	pub async fn int(&self, key: &str) -> ClResult<i64> {
		match self.get(key).await? {
			SettingValue::Int(n) => Ok(n),
			_ => Err(Error::Setting(format!("setting '{key}' is not an integer"))),
		}
	}

	pub async fn flag(&self, key: &str) -> ClResult<bool> {
		match self.get(key).await? {
			SettingValue::Flag(b) => Ok(b),
			_ => Err(Error::Setting(format!("setting '{key}' is not a boolean"))),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_default_parses() {
		for def in REGISTRY {
			assert!(parse(def, def.default).is_ok(), "bad default for {}", def.key);
		}
	}

	#[test]
	fn exact_key_beats_prefix_family() {
		assert!(!definition("ratelimit.default").unwrap().prefix);
		assert!(definition("ratelimit.invoice_create").unwrap().prefix);
		assert!(definition("pow.difficulty.login").unwrap().prefix);
		assert!(definition("pow.difficulty.").is_err());
		assert!(definition("nope.nope").is_err());
	}

	#[test]
	fn validation_rejects_out_of_range_and_unknown_choices() {
		let port = definition("email.smtp.port").unwrap();
		assert!(parse(port, "65536").is_err());
		assert!(parse(port, "587").is_ok());
		assert!(parse(definition("currency.rate_source").unwrap(), "FED").is_err());
		// `Flag` has no registered key since `email.enabled` was dropped — email is not
		// optional — but the type stays for a consumer's own settings.
		let flag = SettingDef::flag("test.flag", "0");
		assert_eq!(parse(&flag, "yes").unwrap(), SettingValue::Flag(true));
		assert!(parse(&flag, "maybe").is_err());
	}

	/// `SettingDef` bounds a `Text` only by length, so a value that will not parse used to be
	/// stored happily and fail far from the operator — `ratelimit.login.ip = "10/5week"` 400'd
	/// every request in that scope with login down until someone found the row, and an
	/// unparseable `email.from` failed in `sender::build`, inside a job handler, on every
	/// queued mail. `parse` is the only validation path, so this is what `Settings::set`
	/// refuses.
	///
	/// The numeric ceilings are the same rule against an arithmetic trap: every one of these
	/// keys feeds `now + n`, `n * 86_400` or a loop count, and `[profile.release]` turns the
	/// overflow into a panic. `jobs.workers` spawns that many tasks inside `AppBuilder::build`;
	/// `auth.recovery_codes` argon2id-hashes that many while holding a `HASH_SLOTS` permit;
	/// `invoice.default_payment_days` reaches `time::Duration::days`, whose own
	/// `days.checked_mul(86_400).expect(..)` fires before `numbering::shift`'s `checked_add` is
	/// ever consulted — a panic at issue time, on a job-runner task.
	#[test]
	fn a_value_that_cannot_be_used_is_refused_where_it_is_written() {
		let max = i64::MAX.to_string();
		for (key, bad, good) in [
			(
				"ratelimit.login.ip",
				&["10/5week", "nan/min/ip", "0/min/ip", "nope"][..],
				&["10/5min/ip"][..],
			),
			("ratelimit.default", &[], &["120/min/ip"]),
			("jobs.retention_days", &[&max, "36501"], &["90"]),
			("invoice.default_payment_days", &[&max, "36501"], &["30"]),
			("invoice.draft_ttl_days", &[&max, "36501"], &["30"]),
			("vies.cache_days", &[&max, "36501", "0"], &["36500"]),
			("auth.recovery_codes", &[&max, "65", "0"], &["64"]),
			// `jobs.workers` shares the ceiling and not the floor: `0` is how a process says it
			// runs no jobs, which is what keeps `Runner::reclaim` single-process.
			("jobs.workers", &[&max, "65", "-1"], &["64", "0"]),
			(
				"email.from",
				&["no-at-sign", "a@b@c.com", "@example.com", "a@example", "a@.com", "a@com."],
				// Blank is the *unconfigured* deployment `required` refuses at boot, not a bad
				// address: `Settings::get` parses on every read, so the key stays readable.
				&["billing@example.com", ""],
			),
		] {
			let def = definition(key).unwrap();
			for value in bad {
				assert_eq!(
					parse(def, value).unwrap_err().parts().1,
					"E-CORE-SETTING",
					"{key} accepted {value:?}"
				);
			}
			for value in good {
				assert!(parse(def, value).is_ok(), "{key} refused {value:?}");
			}
		}
	}

	#[test]
	fn env_names_are_prefixed_and_uppercased() {
		assert_eq!(env_name("currency.base"), "SAAS_CURRENCY_BASE");
	}

	#[test]
	fn the_snapshot_holds_only_saas_keys() {
		// The non-UTF-8 half of the same change cannot be injected portably (`set_var` is
		// `unsafe` on edition 2024 and the workspace forbids it); this asserts the reachable
		// half — `MASTER_KEY` and the rest are not copied for the process lifetime.
		let env = env_snapshot();
		assert!(env.keys().all(|k| k.starts_with("SAAS_")), "{:?}", env.keys().collect::<Vec<_>>());
		assert!(!env.contains_key("PATH"));
	}
}

// vim: ts=4
