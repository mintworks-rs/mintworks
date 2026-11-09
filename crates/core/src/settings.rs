// SPDX-License-Identifier: MPL-2.0
//! The `settings` table: typed, validated, DB-backed runtime configuration.
//!
//! Every configurable thing is a **declared key**, owned by the crate that consumes it. A
//! feature crate exposes `pub static SETTINGS: &[SettingDef]`, the application registers the
//! slices it wants with [`crate::AppBuilder::settings`], and [`Registry`] composes them once at
//! boot and never changes afterwards.
//!
//! Resolution for a key is row, then environment, then the application's registered default,
//! then the registry default. The environment variable is [`env_name`]: the key uppercased with
//! `.` and `-` replaced by `_`, so `currency.base` reads `CURRENCY_BASE`. **No prefix** — one
//! rule for the whole file, matching the bare bootstrap names in [`crate::config`]; a consumer
//! application prefixes its own keys instead (`app.dist_dir` → `APP_DIST_DIR`).
//! A blank variable is *absent*, not an empty override, so a copied `.env.example` cannot
//! shadow a default.
//!
//! The composed registry is the whole vocabulary: a key that is not declared is not a setting,
//! and reading or writing one is [`Error::Setting`]. Some declarations are prefixes
//! (`pow.difficulty.`, `ratelimit.`) covering a family of per-scope keys; an exact declaration
//! always wins over a prefix, which is how `ratelimit.default` coexists with `ratelimit.<route>`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;

use crate::{
	app::App,
	auth_mw::{require_member_role_on, require_operator},
	ctx::{Actor, Ctx},
	error::{ClResult, Error},
	gencache::GenCache,
	store::{CoreStore, Role, SettingChainRow},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingType {
	Text,
	Int,
	Flag,
	Choice(&'static [&'static str]),
	/// Any JSON document, stored as its text and checked with `serde_json` on write.
	Json,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SettingValue {
	Text(String),
	Int(i64),
	Flag(bool),
	Json(serde_json::Value),
}

/// Where a value for this key may live, and so which chain [`Settings::get_for`] resolves it
/// along. Each key resolves along its own chain only; the environment is the global level's
/// alone, so no scoped value is ever settable from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Scope {
	/// One value for the deployment: `(0, 0)`, then environment, then the default chain.
	Global,
	/// `(org, 0)`, then each ancestor's `(anc, 0)` nearest first, then global.
	Org,
	/// `(0, account)`, then global.
	Account,
	/// `(org, account)`, then the [`Scope::Org`] chain from `org`.
	OrgAccount,
}

/// A declared key. `min`/`max` bound the value for `Int` and the length for `Text` and
/// `Choice`; they are ignored for `Flag`.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)] // independent declaration flags, not a state machine
pub struct SettingDef {
	pub key: &'static str,
	pub ty: SettingType,
	pub default: &'static str,
	/// What `GET /api/admin/settings` returns, and the `.env` reference. Required on every
	/// declaration: a key nobody can describe is a key nobody can configure.
	pub description: &'static str,
	pub scope: Scope,
	pub min: i64,
	pub max: i64,
	/// `key` is a prefix matching a family (`pow.difficulty.login`), not one key.
	pub prefix: bool,
	/// A blank value is a misconfiguration, not a default. Enforced by
	/// [`Settings::check_required`] at boot rather than by [`parse`]: an unconfigured
	/// deployment must still be able to *read* the key.
	pub required: bool,
	/// Runs after the type check, for a `Text`/`Json` shape `min`/`max` cannot express.
	/// `Err` here is the same `E-CORE-SETTING` a range violation is — the point is that it
	/// happens when the operator writes the row, not on every request afterwards.
	pub check: Option<fn(&str) -> ClResult<()>>,
	/// An `app.*` `Scope::Account` key the account may write for itself through
	/// `PUT /api/account/settings/{key}`. Off by default: reading is not consent to writing.
	pub user_writable: bool,
	/// Readable along its `Scope` chain like any key, but [`write_scoped`] lets only the
	/// operator write it, at any level: a cap an org must not raise for itself.
	pub operator_only: bool,
}

impl SettingDef {
	const fn new(
		key: &'static str,
		ty: SettingType,
		default: &'static str,
		description: &'static str,
	) -> Self {
		Self {
			key,
			ty,
			default,
			description,
			scope: Scope::Global,
			min: i64::MIN,
			max: i64::MAX,
			prefix: false,
			required: false,
			check: None,
			user_writable: false,
			operator_only: false,
		}
	}

	pub const fn operator_only(mut self) -> Self {
		self.operator_only = true;
		self
	}

	pub const fn user_writable(mut self) -> Self {
		self.user_writable = true;
		self
	}

	pub const fn check(mut self, f: fn(&str) -> ClResult<()>) -> Self {
		self.check = Some(f);
		self
	}

	pub const fn text(key: &'static str, default: &'static str, description: &'static str) -> Self {
		Self::new(key, SettingType::Text, default, description)
	}

	pub const fn int(key: &'static str, default: &'static str, description: &'static str) -> Self {
		Self::new(key, SettingType::Int, default, description)
	}

	pub const fn flag(key: &'static str, default: &'static str, description: &'static str) -> Self {
		Self::new(key, SettingType::Flag, default, description)
	}

	pub const fn choice(
		key: &'static str,
		allowed: &'static [&'static str],
		default: &'static str,
		description: &'static str,
	) -> Self {
		Self::new(key, SettingType::Choice(allowed), default, description)
	}

	pub const fn json(key: &'static str, default: &'static str, description: &'static str) -> Self {
		Self::new(key, SettingType::Json, default, description)
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

	pub const fn org_scoped(self) -> Self {
		self.scope(Scope::Org)
	}

	pub const fn scope(mut self, scope: Scope) -> Self {
		self.scope = scope;
		self
	}
}

/// `mintworks-core`'s own keys. Always registered; every other crate's slice is opt-in through
/// [`crate::AppBuilder::settings`], which is what keeps a consumer embedding only `mintworks-auth`
/// from being asked to configure NAV.
pub static SETTINGS: &[SettingDef] = &[
	// Which environment every external system runs against, as a name rather than a URL: each
	// adapter maps it onto its own vocabulary. One key, not one per system, because a deployment
	// filing into NAV's test system while taking real card payments is not a configuration
	// anyone wants — with two keys it was reachable by forgetting one. `production` by default,
	// never `test` — a deployment that inherited the NAV test endpoint answered OK, reached DONE
	// and reported nothing.
	SettingDef::choice(
		"deployment.env",
		&["test", "production"],
		"production",
		"Which environment every external system runs against.",
	),
	// One job per worker; one alone starves every other kind behind a NAV sweep's timeouts.
	// Bounded because `AppBuilder::build` spawns exactly this many tasks before returning. `0`
	// means this process runs no jobs and reclaims nothing — per-process that is `JOBS_WORKERS`.
	SettingDef::int("jobs.workers", "2", "Job worker tasks this deployment runs.").range(0, 64),
	// How long a finished job with **no** `dedup_key` is kept. A keyed row is never swept
	// whatever this says: its `dedup_key` is the once-only guarantee. Bounded at a century
	// because the sweep's `days * 86_400` panics under `overflow-checks`.
	SettingDef::int("jobs.retention_days", "90", "Days a finished, unkeyed job row is kept.")
		.range(1, 36_500),
	// How recent the newest `FAILED` job must be for `A-JOB-FAILED` to be raised. Rows live until
	// `jobs.retention_days`, so without a window one failure pinned the dashboard at ERROR.
	SettingDef::int(
		"jobs.failed_alert_hours",
		"24",
		"How recent a FAILED job must be to raise A-JOB-FAILED.",
	)
	.range(1, 8_760),
	// Pending jobs above this raise `A-JOB-BACKLOG`. Nothing has failed at that point — the
	// queue is simply not keeping up, which `jobs.workers` is the lever for.
	SettingDef::int(
		"jobs.backlog_warn",
		"100",
		"Pending jobs above which A-JOB-BACKLOG is raised.",
	)
	.range(1, 1_000_000),
	// Per-kind retry policy, families rather than one number because the kinds want opposite
	// things: a statutory obligation must not be given up on, a one-off notification must be.
	// The bare stem resolves to nothing — always ask for `jobs.<family>.<KIND>`. `0` is
	// unbounded. "8" restates `job::DEFAULT_MAX_ATTEMPTS`; a default must be a string literal.
	SettingDef::int(
		"jobs.max_attempts.",
		"8",
		"Attempts before a job kind is given up on; 0 is unbounded.",
	)
	.family()
	.range(0, 1_000),
	// The ceiling `backoff_secs` clamps `2^attempts` to. One hour by default.
	SettingDef::int(
		"jobs.backoff_cap.",
		"3600",
		"Ceiling in seconds on a job kind's retry backoff.",
	)
	.family()
	.range(1, 86_400),
	// How long a kind may keep failing before it is worth telling a human about. Read by
	// `alert::alerts` for `A-JOB-STALE`. `0` disables.
	SettingDef::int(
		"jobs.alert_after.",
		"3600",
		"Seconds a job kind may keep failing before A-JOB-STALE; 0 disables.",
	)
	.family()
	.range(0, 2_592_000),
	// How long a handler may run before the runner gives up on it. `0` disables the deadline.
	// Without one, a relay that answers every 29 s holds a worker forever and the row stays
	// `RUNNING` — invisible to `job_claim`, `job_stale` and every alert until a restart.
	SettingDef::int(
		"jobs.timeout_secs.",
		"900",
		"Seconds a job kind's handler may run; 0 disables the deadline.",
	)
	.family()
	.range(0, 86_400),
	SettingDef::int(
		"auth.stepup_window",
		"300",
		"Seconds a step-up re-auth stays valid on destructive routes.",
	)
	.range(1, i64::MAX),
	SettingDef::choice(
		"refs.slug_by",
		&["operator", "admin"],
		"operator",
		"Who may choose a slug as a ref code instead of a random one.",
	),
	// How old `secrets['auth.jwt_key']` may get before `A-SECRET-STALE` says so. Advisory only:
	// nothing rotates the key automatically, because rotating it signs every session out.
	SettingDef::int(
		"auth.key_max_age_days",
		"365",
		"Age at which the JWT signing key raises A-SECRET-STALE.",
	)
	.range(1, 36_500),
	SettingDef::text(
		"ratelimit.default",
		"120/min/ip",
		"The blanket budget every unscoped route gets.",
	)
	.check(crate::ratelimit::check_limit),
	SettingDef::text("ratelimit.", "120/min/ip", "Per-scope rate limit, as `count/window/unit`.")
		.family()
		.check(crate::ratelimit::check_limit),
	// Free space on the `DATA_DIR` filesystem below which `alert::alerts` raises `A-DISK-LOW`,
	// against `statvfs` on `config.data_dir`. `0` disables the check.
	SettingDef::int(
		"storage.free_warn_mb",
		"512",
		"Free MB on DATA_DIR below which A-DISK-LOW is raised; 0 disables.",
	)
	.range(0, 1_000_000),
	// Where `ALERT_SWEEP` mails newly appeared alerts. Empty — the default — disables alert
	// mail entirely; `alert::alerts` still computes the list, and the admin dashboard still
	// shows it. A deployment without an operator mailbox is normal, not misconfigured.
	SettingDef::text(
		"admin.alert_email",
		"",
		"Where new alerts are mailed; empty disables alert mail.",
	),
	// Floor on what is worth an email. `ERROR` mails only what has actually failed; `WARN`
	// mails everything `alerts()` returns.
	SettingDef::choice(
		"admin.alert_min_severity",
		&["WARN", "ERROR"],
		"ERROR",
		"Lowest alert severity worth an email.",
	),
	// How often the alert set is recomputed, and so the re-notify floor per code. `ALERT_SWEEP`
	// ticks every minute and returns early until this many have passed, so a change applies
	// without a restart — `Runner::register_periodic` fixes its period at boot.
	SettingDef::int("admin.alert_interval_minutes", "60", "Minutes between alert recomputations.")
		.range(1, 1_440),
	// Comma-separated reverse proxies in front of this process. `X-Forwarded-For` is read **only**
	// when the direct peer is one of them; empty means trust nothing, because an unvalidated
	// header forges a fresh rate-limit bucket per request. Unset behind a real proxy, every
	// per-IP bucket keys on the proxy and the limits collapse into one.
	SettingDef::text(
		"http.trusted_proxy",
		"",
		"Comma-separated reverse proxies whose X-Forwarded-For is trusted.",
	)
	.check(crate::auth_mw::check_trusted_proxy),
];

/// One [`crate::AppBuilder::setting_default`] (`env: None`) or `setting_default_for` value.
#[derive(Clone, Copy, Debug)]
pub struct SettingDefault {
	pub env: Option<&'static str>,
	pub key: &'static str,
	pub value: &'static str,
}

/// Every declaration this process knows, composed at [`crate::AppBuilder::build`] and never
/// changed after.
pub struct Registry {
	exact: HashMap<&'static str, &'static SettingDef>,
	/// Longest key first, so `jobs.max_attempts.` beats `jobs.` if both are ever declared.
	families: Vec<&'static SettingDef>,
	/// Below the environment, above `def.default`. Scanned, not hashed: read on a cache miss only.
	defaults: Vec<SettingDefault>,
	/// Declared secret key names — [`crate::secrets::SecretStore`] has no typed definitions,
	/// only names, which is what the collision check and the admin key list need.
	secrets: Vec<&'static str>,
	/// Only the variables this registry names. Blank values are dropped: blank is *absent*.
	env: HashMap<String, String>,
}

impl Registry {
	/// Composes the declarations into the process registry, returning every problem found
	/// rather than the first: an operator fixing configuration wants one restart, not four.
	///
	/// Infallible by construction so a caller that cannot fail — [`Settings::core`] — needs no
	/// `unwrap`; [`crate::AppBuilder::build`] is what turns a non-empty error list into a
	/// refusal to boot.
	#[must_use]
	pub fn build(
		slices: &[&'static [SettingDef]],
		defaults: &[SettingDefault],
		secrets: &[&'static str],
	) -> (Self, Vec<String>) {
		let mut errors = Vec::new();
		let mut exact: HashMap<&'static str, &'static SettingDef> = HashMap::new();
		let mut families: Vec<&'static SettingDef> = Vec::new();
		// `env_name` maps both `.` and `-` to `_`, so `a.b` and `a-b` share a variable and
		// nothing would notice. Secret names share the namespace and so share this check.
		let mut by_var: HashMap<String, &'static str> = HashMap::new();
		let claim = |errors: &mut Vec<String>,
		             by_var: &mut HashMap<String, &'static str>,
		             key: &'static str| {
			if let Some(other) = by_var.insert(env_name(key), key) {
				errors.push(format!("'{key}' and '{other}' both map to {}", env_name(key)));
			}
		};
		for def in slices.iter().flat_map(|s| s.iter()) {
			if def.description.trim().is_empty() {
				errors.push(format!("'{}' has no description", def.key));
			}
			if def.prefix {
				if families.iter().any(|d| d.key == def.key) {
					errors.push(format!("'{}' is declared twice", def.key));
					continue;
				}
				families.push(def);
			} else {
				if let Some(other) = exact.insert(def.key, def) {
					errors.push(format!(
						"'{}' is declared twice (default '{}' and '{}')",
						def.key, other.default, def.default
					));
					continue;
				}
				claim(&mut errors, &mut by_var, def.key);
			}
		}
		// A name ending in `.` is a family (`llm.api_key.` covers `llm.api_key.<p>`), matched like
		// a setting family. Overlapping a setting family would let a secret be written as one.
		let mut secret_families = Vec::new();
		for &key in secrets {
			if !key.ends_with('.') {
				claim(&mut errors, &mut by_var, key);
			} else if let Some(d) =
				families.iter().find(|d| d.key.starts_with(key) || key.starts_with(d.key))
			{
				errors.push(format!("secret family '{key}' overlaps setting family '{}'", d.key));
			} else {
				secret_families.push(env_name(key));
			}
		}
		families.sort_by_key(|d| std::cmp::Reverse(d.key.len()));

		let mut names: HashSet<String> = by_var.keys().cloned().collect();
		let prefixes: Vec<String> =
			families.iter().map(|d| env_name(d.key)).chain(secret_families).collect();
		let mut registry = Self {
			exact,
			families,
			defaults: Vec::new(),
			secrets: secrets.to_vec(),
			env: HashMap::new(),
		};
		// A typo'd key is a boot error, not a silent no-op, and a value that cannot parse is
		// caught here rather than on the first read of it.
		let env_def = registry.definition("deployment.env").ok();
		let mut seen: HashSet<(Option<&str>, &str)> = HashSet::new();
		for &d in defaults {
			let SettingDefault { env, key, value } = d;
			// Refused, not last-wins: two layers defaulting one key is a composition bug.
			if !seen.insert((env, key)) {
				let scope = env.map(|e| format!(" for '{e}'")).unwrap_or_default();
				errors.push(format!(
					"default for '{key}' registered twice{scope} (the host binary and the \
					 application may both set it)"
				));
				continue;
			}
			// It would resolve against itself.
			if env.is_some() && key == "deployment.env" {
				errors.push("default for 'deployment.env' cannot be env-scoped".into());
				continue;
			}
			if let Some(e) = env
				&& env_def.is_none_or(|d| parse(d, e).is_err())
			{
				errors.push(format!("default for '{key}': unknown deployment.env '{e}'"));
				continue;
			}
			let Ok(def) = registry.definition(key) else {
				errors.push(format!("default for undeclared setting '{key}'"));
				continue;
			};
			// Blank is absent, so it is not parsed: a `range(2, 2)` or choice key refuses "".
			if !value.trim().is_empty()
				&& let Err(e) = parse(def, value)
			{
				errors.push(format!("default for '{key}': {e}"));
				continue;
			}
			registry.defaults.push(d);
			names.insert(env_name(key));
		}
		registry.env = env_snapshot(&names, &prefixes);
		// Names only, never values: the same snapshot is what `secrets.rs` reads. The env name
		// is unprefixed (`DEPLOYMENT_ENV`, `JOBS_WORKERS`), so there is nothing to filter an
		// ambient CI or container variable out by, and env beats the application's own default
		// — this line is the only place an operator can see that it happened.
		if !registry.env.is_empty() {
			let mut vars: Vec<&str> = registry.env.keys().map(String::as_str).collect();
			vars.sort_unstable();
			tracing::info!(vars = vars.join(", "), "settings taken from the environment");
		}
		(registry, errors)
	}

	/// Resolves a key. Exact declarations beat prefix families, and a family matches only a key
	/// strictly longer than its stem — so the bare `pow.difficulty.` resolves to nothing.
	///
	/// # Errors
	/// `E-CORE-SETTING` for a key no registered slice declares.
	pub fn definition(&self, key: &str) -> ClResult<&'static SettingDef> {
		self.exact
			.get(key)
			.copied()
			.or_else(|| {
				self.families
					.iter()
					.find(|d| key.starts_with(d.key) && key.len() > d.key.len())
					.copied()
			})
			.ok_or_else(|| Error::Setting(format!("unknown setting '{key}'")))
	}

	/// Every non-family declaration, for the boot check and the admin listing.
	pub fn exact_defs(&self) -> impl Iterator<Item = &'static SettingDef> + '_ {
		self.exact.values().copied()
	}

	/// The declared secret key names; one ending in `.` is a family prefix.
	#[must_use]
	pub fn secrets(&self) -> &[&'static str] {
		&self.secrets
	}

	/// The environment's value for a key, or `None` when the variable is unset or blank.
	#[must_use]
	pub fn env(&self, key: &str) -> Option<&str> {
		self.env.get(&env_name(key)).map(String::as_str)
	}

	/// Whether `key` has a `setting_default_for` default, so resolving it needs `deployment.env`.
	fn env_scoped(&self, key: &str) -> bool {
		self.defaults.iter().any(|d| d.env.is_some() && d.key == key)
	}

	/// Environment, then the application's default for `env`, then its plain default. A blank
	/// application default is absent.
	fn fallback(&self, key: &str, env: Option<&str>) -> Option<&str> {
		let app = |scope: Option<&str>| {
			self.defaults
				.iter()
				.find(|d| d.env == scope && d.key == key && !d.value.trim().is_empty())
				.map(|d| d.value)
		};
		self.env(key).or_else(|| env.and_then(|e| app(Some(e)))).or_else(|| app(None))
	}
}

/// RFC 5322-shaped enough to catch an operator's typo: one `@`, a non-empty local part, and a
/// domain carrying a dot. `mintworks-core` cannot depend on `lettre`, so `sender::build`'s own
/// `Mailbox` parse stays the backstop — but that one fails inside a job handler, hours after
/// the row was written.
///
/// Empty passes: `required` is what refuses a blank one, and [`Settings::get`] parses on every
/// read, so an unconfigured deployment must still be able to read the key.
///
/// # Errors
/// `E-CORE-SETTING` when `raw` is neither empty nor address-shaped.
pub fn check_address(raw: &str) -> ClResult<()> {
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

/// The key uppercased with `.` and `-` replaced by `_`, and **no prefix**: one rule for the
/// whole environment, matching the bare bootstrap names. A consumer application prefixes its
/// own keys instead — `app.dist_dir` is `APP_DIST_DIR`.
#[must_use]
pub fn env_name(key: &str) -> String {
	key.to_uppercase().replace(['.', '-'], "_")
}

/// `vars_os`, not `vars`, which panics on a non-UTF-8 variable — this runs in
/// `AppBuilder::build`, before any `CatchPanicLayer`, so the process dies at startup.
///
/// Only variables the composed registry names are copied: with no prefix there is nothing to
/// filter on, so an undeclared ambient variable must never be readable as a setting.
fn env_snapshot(names: &HashSet<String>, prefixes: &[String]) -> HashMap<String, String> {
	std::env::vars_os()
		.filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
		// Blank is absent, for settings as it already was for secrets: a copied `.env.example`
		// must not shadow a registry default with `""`.
		.filter(|(_, v)| !v.trim().is_empty())
		.filter(|(k, _)| {
			names.contains(k) || prefixes.iter().any(|p| k.len() > p.len() && k.starts_with(p))
		})
		.collect()
}

/// Ceiling on any stored value: a Json or unbounded `app.*` text key is user-writable.
const MAX_VALUE_BYTES: usize = 65_536;

/// A wire or script value as the raw text [`parse`] reads: a json key keeps any value as its
/// JSON text, every other type takes a string, number or boolean as written.
///
/// # Errors
/// `E-CORE-SETTING` for an array, object or null under a non-json type.
pub fn raw_value(ty: SettingType, v: &serde_json::Value) -> ClResult<String> {
	use serde_json::Value;
	match (ty, v) {
		(SettingType::Json, v) => Ok(v.to_string()),
		(_, Value::String(s)) => Ok(s.clone()),
		(_, Value::Number(_) | Value::Bool(_)) => Ok(v.to_string()),
		_ => Err(Error::Setting(format!("{v} is not a {ty:?} value"))),
	}
}

/// Parses and validates `raw` against the declaration. This is the only validation path:
/// writes go through it before they reach the table, so a stored row always parses.
///
/// # Errors
/// `E-CORE-SETTING` on a type, range, choice or [`SettingDef::check`] violation.
pub fn parse(def: &SettingDef, raw: &str) -> ClResult<SettingValue> {
	let bad = |what: &str| Error::Setting(format!("setting '{}': {what}", def.key));
	if raw.len() > MAX_VALUE_BYTES {
		return Err(bad(&format!("value exceeds {MAX_VALUE_BYTES} bytes")));
	}
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
		SettingType::Json => {
			let v = serde_json::from_str(raw).map_err(|_| bad("must be JSON"))?;
			if let Some(check) = def.check {
				check(raw).map_err(|e| bad(&e.to_string()))?;
			}
			Ok(SettingValue::Json(v))
		}
	}
}

/// The `GenCache` key for one level: `"{org}:{account}:{key}"`, so the global `(0, 0)` entries
/// [`Settings::get`] fills are the ones [`Settings::set_at`] invalidates.
fn cache_key(org_id: i64, account_id: i64, key: &str) -> String {
	format!("{org_id}:{account_id}:{key}")
}

/// The level a read from `(org_id, account_id)` starts at under `scope`: the coordinates the
/// scope does not use are dropped, and an `OrgAccount` read without an org is a global one.
fn read_level(scope: Scope, org_id: i64, account_id: i64) -> (i64, i64) {
	match scope {
		Scope::Global => (0, 0),
		Scope::Org => (org_id, 0),
		Scope::Account => (0, account_id),
		Scope::OrgAccount if org_id == 0 => (0, 0),
		Scope::OrgAccount => (org_id, account_id),
	}
}

/// Whether `scope` permits a row at `(org_id, account_id)`. The global level always.
fn writable(scope: Scope, org_id: i64, account_id: i64) -> bool {
	match scope {
		_ if (org_id, account_id) == (0, 0) => true,
		Scope::Global => false,
		Scope::Org => account_id == 0,
		Scope::Account => org_id == 0,
		Scope::OrgAccount => org_id != 0,
	}
}

/// The winning non-global row of a [`CoreStore::setting_rows_chain`] answer for a read at
/// `org_id` (already [`read_level`]-normalized): `(0, a)` or `(o, a)` first, then `(o, 0)`, then
/// `(anc, 0)` nearest first. `(0, 0)` is never picked — the global level goes through
/// [`Settings::configured`], which is where the environment sits.
fn pick(org_id: i64, rows: Vec<SettingChainRow>) -> Option<String> {
	rows.into_iter()
		.filter_map(|r| {
			let rank = match r.depth {
				None if r.account_id != 0 && org_id == 0 => 0,
				Some(d) if r.account_id == 0 => d.saturating_mul(2).saturating_add(1),
				Some(0) => 0,
				None | Some(_) => return None,
			};
			Some((rank, r.value))
		})
		.min_by_key(|(rank, _)| *rank)
		.map(|(_, v)| v)
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
	registry: Arc<Registry>,
}

impl Settings {
	/// The environment is read when the [`Registry`] is composed, so changing a variable at
	/// runtime needs a restart. Settings rows, the operator-facing lever, stay live.
	pub fn new(store: Arc<dyn CoreStore>, registry: Arc<Registry>) -> Self {
		Self { store, rows: GenCache::new(), values: GenCache::new(), registry }
	}

	/// A `Settings` over `mintworks-core`'s own declarations alone, for a caller with no composed
	/// registry to hand — `job::Runner::new`, and tests.
	pub fn core(store: Arc<dyn CoreStore>) -> Self {
		Self::new(store, Arc::new(Registry::build(&[SETTINGS], &[], &[]).0))
	}

	#[must_use]
	pub fn registry(&self) -> &Arc<Registry> {
		&self.registry
	}

	/// Row, then environment, then the application's default for the resolved `deployment.env`,
	/// then its plain default, then the registry default.
	///
	/// # Errors
	/// `E-CORE-SETTING` for an undeclared key or a value that does not parse; whatever the
	/// store raises for the row read.
	pub async fn get(&self, key: &str) -> ClResult<SettingValue> {
		self.get_for(0, 0, key).await
	}

	/// The value for `key` read from `(org_id, account_id)` (`0` = none), resolved along the
	/// key's own [`Scope`] chain and ending at [`Settings::get`]'s global resolution. Coordinates
	/// the scope does not use are ignored, so a `Global` key answers its global value anywhere.
	///
	/// # Errors
	/// As [`Settings::get`].
	pub async fn get_for(&self, org_id: i64, account_id: i64, key: &str) -> ClResult<SettingValue> {
		let def = self.registry.definition(key)?;
		let (org_id, account_id) = read_level(def.scope, org_id, account_id);
		let ck = cache_key(org_id, account_id, key);
		let miss = match self.values.lookup(&ck) {
			Ok(v) => return Ok(v),
			Err(miss) => miss,
		};
		let scoped = if (org_id, account_id) == (0, 0) {
			None
		} else {
			pick(org_id, self.store.setting_rows_chain(org_id, account_id, key).await?)
		};
		let raw = match scoped {
			Some(v) => v,
			None => self.configured(key).await?.unwrap_or_else(|| def.default.to_owned()),
		};
		let value = parse(def, &raw)?;
		self.values.store(&ck, miss, value.clone());
		Ok(value)
	}

	/// [`Settings::get`]'s raw value without the registry default: row, then environment, then
	/// the application's defaults. `None` means nobody configured the key.
	///
	/// # Errors
	/// Whatever the store raises; `E-CORE-SETTING` when `deployment.env` cannot be resolved.
	pub async fn configured(&self, key: &str) -> ClResult<Option<String>> {
		if let Some(v) = self.row(key).await? {
			return Ok(Some(v));
		}
		let env = if self.registry.env_scoped(key) {
			Some(Box::pin(self.text("deployment.env")).await?)
		} else {
			None
		};
		Ok(self.registry.fallback(key, env.as_deref()).map(ToOwned::to_owned))
	}

	/// The `settings` row exactly as an operator wrote it, or `None` when the key has no row.
	/// Deliberately **no** environment or registry-default fallback.
	///
	/// [`Settings::get`] cannot answer this. For a prefix family such as `ratelimit.` it
	/// returns the family default for a key nobody ever set, so a caller that treated that as
	/// an operator override would apply `ratelimit.default` to every named scope —
	/// `RateLimiter::check` needs to tell the two apart before it can honour an override.
	///
	/// # Errors
	/// Whatever the store raises.
	pub async fn row(&self, key: &str) -> ClResult<Option<String>> {
		self.row_at(0, 0, key).await
	}

	/// [`Settings::row`] at exactly one level — so an admin UI can tell "no override" from "an
	/// override equal to the parent's".
	///
	/// # Errors
	/// Whatever the store raises.
	pub async fn row_at(
		&self,
		org_id: i64,
		account_id: i64,
		key: &str,
	) -> ClResult<Option<String>> {
		let ck = cache_key(org_id, account_id, key);
		let miss = match self.rows.lookup(&ck) {
			Ok(v) => return Ok(v),
			Err(miss) => miss,
		};
		let value = self.store.setting_get(org_id, account_id, key).await?;
		self.rows.store(&ck, miss, value.clone());
		Ok(value)
	}

	/// Validates, writes and invalidates the cached entry.
	///
	/// # Errors
	/// `E-CORE-SETTING` for an undeclared key or a value that does not parse.
	pub async fn set(&self, key: &str, raw: &str, updated_by: Option<i64>) -> ClResult<()> {
		self.set_at(0, 0, key, Some(raw), updated_by).await
	}

	/// [`Settings::set`] at `(org_id, account_id)`; `raw: None` deletes that level's row, so
	/// the read falls through to the next level of the chain.
	///
	/// # Errors
	/// `E-CORE-SETTING` for an undeclared key, a value that does not parse, or a level the key's
	/// [`Scope`] has no place for.
	pub async fn set_at(
		&self,
		org_id: i64,
		account_id: i64,
		key: &str,
		raw: Option<&str>,
		updated_by: Option<i64>,
	) -> ClResult<()> {
		let def = self.registry.definition(key)?;
		if !writable(def.scope, org_id, account_id) {
			return Err(Error::Setting(format!(
				"setting '{key}' is {:?}-scoped and has no value at ({org_id}, {account_id})",
				def.scope
			)));
		}
		match raw {
			Some(raw) => {
				parse(def, raw)?;
				self.store.setting_set(org_id, account_id, key, raw.trim(), updated_by).await?;
			}
			None => self.store.setting_delete(org_id, account_id, key).await?,
		}
		self.rows.invalidate(&cache_key(org_id, account_id, key));
		// Env-scoped defaults resolve against `deployment.env`, so every key may have moved;
		// otherwise every resolved level of this one key, since descendants inherit the write.
		if key == "deployment.env" {
			self.values.clear();
		} else {
			let suffix = format!(":{key}");
			self.values.invalidate_where(|k| k.ends_with(&suffix));
		}
		Ok(())
	}

	/// Refuse to boot while a key under `prefix` is blank-but-`required` or does not parse.
	///
	/// [`crate::AppBuilder::build`] runs this over the **whole** composed registry once
	/// `on_init` has finished, so registering a crate's slice is what opts a deployment into
	/// being asked for its configuration. `prefix` stays for a consumer that wants an earlier,
	/// narrower gate; `""` is every declared key.
	///
	/// Every declared key goes through [`Settings::get`], which resolves row → environment →
	/// app default → registry default and parses: one pass catches a bad `settings` row, a bad
	/// environment override and a bad default alike. Without it the first place a
	/// misconfiguration surfaced was inside a job handler, as an `Error::Setting` —
	/// `Retry::Never`, which destroyed every queued activation link rather than delaying it.
	///
	/// Every failure at once: naming one at a time makes an operator restart per key.
	/// Family declarations are skipped — a prefix has no single value to check.
	///
	/// # Errors
	/// `E-CORE-SETTING` naming every blank-but-required and every unparseable key.
	pub async fn check_required(&self, prefix: &str) -> ClResult<()> {
		let mut missing = Vec::new();
		let mut invalid = Vec::new();
		let mut keys: Vec<&'static str> = self
			.registry
			.exact_defs()
			.filter(|d| d.key.starts_with(prefix))
			.map(|d| d.key)
			.collect();
		// Sorted because the registry is a `HashMap`: an operator comparing two boots' refusals
		// should not have to diff a shuffled list.
		keys.sort_unstable();
		for key in keys {
			let def = self.registry.definition(key)?;
			match self.get(key).await {
				Err(e) => invalid.push(format!("{key} ({e})")),
				Ok(SettingValue::Text(s)) if def.required && s.trim().is_empty() => {
					missing.push(key.to_owned());
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

	/// # Errors
	/// `E-CORE-SETTING` when the key is undeclared, unparseable or not textual.
	pub async fn text(&self, key: &str) -> ClResult<String> {
		match self.get(key).await? {
			SettingValue::Text(s) => Ok(s),
			_ => Err(Error::Setting(format!("setting '{key}' is not textual"))),
		}
	}

	/// # Errors
	/// `E-CORE-SETTING` when the key is undeclared, unparseable or not an integer.
	pub async fn int(&self, key: &str) -> ClResult<i64> {
		match self.get(key).await? {
			SettingValue::Int(n) => Ok(n),
			_ => Err(Error::Setting(format!("setting '{key}' is not an integer"))),
		}
	}

	/// # Errors
	/// `E-CORE-SETTING` when the key is undeclared, unparseable or not a boolean.
	pub async fn flag(&self, key: &str) -> ClResult<bool> {
		match self.get(key).await? {
			SettingValue::Flag(b) => Ok(b),
			_ => Err(Error::Setting(format!("setting '{key}' is not a boolean"))),
		}
	}
}

/// The caller is `account_id` itself. `System` is the application's own code and passes, as it
/// does [`crate::auth_mw::member_role_on`].
fn is_self(ctx: &Ctx, account_id: i64) -> bool {
	matches!(ctx.actor, Actor::System { .. }) || ctx.actor.account_id() == Some(account_id)
}

/// The `(org, account)` gate, applied after `read_level` so an `Account` key never reaches it.
/// Another account must be a member of `org`, and the caller an `ADMIN` of it.
async fn org_account(app: &App, ctx: &Ctx, org_id: i64, account_id: i64) -> ClResult<()> {
	// Not `is_self`: `System` must still find the target in the org.
	if ctx.actor.account_id() == Some(account_id) {
		require_member_role_on(app, ctx, org_id, Role::Member).await?;
		return Ok(());
	}
	require_member_role_on(app, ctx, org_id, Role::Admin).await?;
	if app.store.org_role(account_id, org_id).await?.is_none() {
		return Err(Error::NotFound);
	}
	Ok(())
}

/// [`Settings::get_for`] on behalf of `ctx`. `org`/`account` are internal ids (`None` = that
/// coordinate unset), first narrowed to the level the key's scope reads. Reads at an org need an effective `MEMBER` of it; at an account, the
/// account itself; at `(org, account)`, a `MEMBER` of the org for its own row (its chain falls
/// through to the org's) and an `ADMIN` for another member's. The global level is open to every actor, `Public`
/// included: it is what [`Settings::get`] hands any code path without a `Ctx`, so gating it here
/// guards nothing, and what a route exposes is the route's decision.
///
/// # Errors
/// `E-CORE-NOTFOUND` for an org the caller is not in, another account, or (at `(org, account)`)
/// an account outside the org; `E-AUTH-FORBIDDEN` for a member below the role needed; otherwise
/// as [`Settings::get_for`].
pub async fn read_scoped(
	app: &App,
	ctx: &Ctx,
	key: &str,
	org: Option<i64>,
	account: Option<i64>,
) -> ClResult<SettingValue> {
	// Gate the level the read actually lands on: `read_level` drops the org of an `Account` key.
	let scope = app.settings.registry().definition(key)?.scope;
	let (o, a) = read_level(scope, org.unwrap_or(0), account.unwrap_or(0));
	match (o, a) {
		(0, 0) => {}
		(o, 0) => {
			require_member_role_on(app, ctx, o, Role::Member).await?;
		}
		(0, a) => self_or_notfound(ctx, a)?,
		(o, a) => org_account(app, ctx, o, a).await?,
	}
	app.settings.get_for(o, a, key).await
}

/// [`Settings::set_at`] on behalf of `ctx`; `value: None` deletes that level's row. The global
/// level is operator only, as is every level of an [`SettingDef::operator_only`] key; an org needs an effective `ADMIN` of it; an account, the account
/// itself; `(org, account)`, a `MEMBER` for its own row and an `ADMIN` for another member's.
/// `updated_by` is the caller's account.
///
/// # Errors
/// `E-CORE-NOTFOUND` / `E-AUTH-FORBIDDEN` as [`read_scoped`]; the global level's operator check;
/// otherwise as [`Settings::set_at`].
pub async fn write_scoped(
	app: &App,
	ctx: &Ctx,
	key: &str,
	value: Option<&str>,
	org: Option<i64>,
	account: Option<i64>,
) -> ClResult<()> {
	if ctx.impersonated && account.is_some() && account == ctx.actor.account_id() {
		return Err(Error::coded(
			axum::http::StatusCode::FORBIDDEN,
			"E-AUTH-FORBIDDEN",
			"not while impersonating",
		));
	}
	if app.settings.registry().definition(key).is_ok_and(|d| d.operator_only) {
		require_operator(app, ctx).await?;
	} else {
		match (org, account) {
			(None, None) => require_operator(app, ctx).await?,
			(Some(o), None) => require_member_role_on(app, ctx, o, Role::Admin).await.map(drop)?,
			(None, Some(a)) => self_or_notfound(ctx, a)?,
			(Some(o), Some(a)) => org_account(app, ctx, o, a).await?,
		}
	}
	let (o, a) = (org.unwrap_or(0), account.unwrap_or(0));
	app.settings.set_at(o, a, key, value, ctx.actor.account_id()).await
}

fn self_or_notfound(ctx: &Ctx, account_id: i64) -> ClResult<()> {
	if is_self(ctx, account_id) {
		return Ok(());
	}
	Err(Error::NotFound)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn row(org_id: i64, account_id: i64, depth: Option<i64>, value: &str) -> SettingChainRow {
		SettingChainRow { org_id, account_id, depth, value: value.to_owned() }
	}

	/// Each scope's chain order, and the rows a chain must never pick: global (it goes through the
	/// environment), an account row of another scope, and an account row at an ancestor.
	#[test]
	fn the_chain_picks_the_nearest_level_of_its_own_scope() {
		let class_pupil = || {
			vec![
				row(0, 0, None, "global"),
				row(0, 7, None, "account"),
				row(1, 0, Some(2), "root"),
				row(5, 0, Some(1), "school"),
				row(5, 7, Some(1), "school-pupil"),
				row(9, 0, Some(0), "class"),
				row(9, 7, Some(0), "class-pupil"),
			]
		};
		assert_eq!(pick(9, class_pupil()).as_deref(), Some("class-pupil"));
		let without = |drop: &[&str]| {
			class_pupil()
				.into_iter()
				.filter(|r| !drop.contains(&r.value.as_str()))
				.collect()
		};
		assert_eq!(pick(9, without(&["class-pupil"])).as_deref(), Some("class"));
		assert_eq!(pick(9, without(&["class-pupil", "class"])).as_deref(), Some("school"));
		assert_eq!(pick(9, without(&["class-pupil", "class", "school", "root"])), None);
		assert_eq!(pick(0, vec![row(0, 0, None, "g"), row(0, 7, None, "a")]).as_deref(), Some("a"));
		assert_eq!(pick(0, vec![row(0, 0, None, "g")]), None);

		assert_eq!(read_level(Scope::Global, 9, 7), (0, 0));
		assert_eq!(read_level(Scope::Org, 9, 7), (9, 0));
		assert_eq!(read_level(Scope::Account, 9, 7), (0, 7));
		assert_eq!(read_level(Scope::OrgAccount, 0, 7), (0, 0));
		assert!(writable(Scope::Global, 0, 0) && !writable(Scope::Global, 9, 0));
		assert!(writable(Scope::Org, 9, 0) && !writable(Scope::Org, 9, 7));
		assert!(writable(Scope::Account, 0, 7) && !writable(Scope::Account, 9, 7));
		assert!(writable(Scope::OrgAccount, 9, 7) && !writable(Scope::OrgAccount, 0, 7));
		assert!(parse(&SettingDef::json("t.j", "{}", "J."), "{\"a\":[1]}").is_ok());
		assert!(parse(&SettingDef::json("t.j", "{}", "J."), "{nope").is_err());
	}

	static EXTRA: &[SettingDef] = &[
		SettingDef::int("pow.difficulty.", "18", "Proof-of-work leading zero bits per scope.")
			.range(1, 32)
			.family(),
		SettingDef::text("email.smtp.host", "", "SMTP host.").required(),
		SettingDef::int("email.smtp.port", "587", "SMTP port.").range(1, 65535),
	];

	/// `env_name` maps both `.` and `-` to `_`, so this collides with `http.trusted_proxy`.
	static DASHED: &[SettingDef] =
		&[SettingDef::text("http.trusted-proxy", "", "Collides with http.trusted_proxy.")];

	fn registry() -> Registry {
		let (registry, errors) = Registry::build(&[SETTINGS, EXTRA], &[], &[]);
		assert!(errors.is_empty(), "{errors:?}");
		registry
	}

	#[test]
	fn every_default_parses() {
		for def in SETTINGS {
			assert!(parse(def, def.default).is_ok(), "bad default for {}", def.key);
		}
	}

	/// The composition rules the unprefixed namespace rests on: a key nobody can describe is a
	/// key nobody can configure, and two keys sharing one variable is a silent mis-resolution
	/// — `a.b` and `a-b` both map to `A_B`.
	#[test]
	fn every_key_is_described_and_owns_its_variable() {
		let mut seen = HashSet::new();
		for def in SETTINGS.iter().chain(EXTRA) {
			assert!(!def.description.trim().is_empty(), "{} has no description", def.key);
			assert!(seen.insert(env_name(def.key)), "{} shares a variable", def.key);
		}
		let (_, errors) = Registry::build(&[SETTINGS, DASHED], &[], &[]);
		assert_eq!(errors.len(), 1, "{errors:?}");
		assert!(errors[0].contains("HTTP_TRUSTED_PROXY"), "{errors:?}");
	}

	fn dflt(env: Option<&'static str>, key: &'static str, value: &'static str) -> SettingDefault {
		SettingDefault { env, key, value }
	}

	/// Blank is absent, so a blank default skips the parse a choice key would fail.
	#[test]
	fn a_blank_default_is_not_parsed() {
		let (_, errors) = Registry::build(&[SETTINGS], &[dflt(None, "deployment.env", "")], &[]);
		assert!(errors.is_empty(), "{errors:?}");
	}

	/// Each of the ways composition refuses to boot, named once.
	#[test]
	fn composition_refuses_a_registry_it_cannot_resolve() {
		static DUP: &[SettingDef] = &[SettingDef::int("jobs.workers", "4", "A second claim.")];
		let cases: Vec<(&str, Vec<String>)> = vec![
			("declared twice", Registry::build(&[SETTINGS, DUP], &[], &[]).1),
			(
				"undeclared setting",
				Registry::build(&[SETTINGS], &[dflt(None, "nope.nope", "1")], &[]).1,
			),
			(
				"default for 'jobs.workers'",
				Registry::build(&[SETTINGS], &[dflt(None, "jobs.workers", "99")], &[]).1,
			),
			(
				"registered twice",
				Registry::build(
					&[SETTINGS],
					&[dflt(None, "jobs.workers", "4"), dflt(None, "jobs.workers", "")],
					&[],
				)
				.1,
			),
			(
				"registered twice for 'test'",
				Registry::build(
					&[SETTINGS],
					&[
						dflt(Some("test"), "jobs.workers", "4"),
						dflt(Some("test"), "jobs.workers", "2"),
					],
					&[],
				)
				.1,
			),
			(
				"sandbox",
				Registry::build(&[SETTINGS], &[dflt(Some("sandbox"), "jobs.workers", "4")], &[]).1,
			),
			(
				"cannot be env-scoped",
				Registry::build(&[SETTINGS], &[dflt(Some("test"), "deployment.env", "test")], &[])
					.1,
			),
			("both map to JOBS_WORKERS", Registry::build(&[SETTINGS], &[], &["jobs.workers"]).1),
			(
				"overlaps setting family 'jobs.max_attempts.'",
				Registry::build(&[SETTINGS], &[], &["jobs.max_attempts."]).1,
			),
		];
		for (want, errors) in cases {
			assert_eq!(errors.len(), 1, "{want}: {errors:?}");
			assert!(errors[0].contains(want), "{want}: {errors:?}");
		}
	}

	#[test]
	fn exact_key_beats_prefix_family() {
		let r = registry();
		assert!(!r.definition("ratelimit.default").unwrap().prefix);
		assert!(r.definition("ratelimit.invoice_create").unwrap().prefix);
		assert!(r.definition("pow.difficulty.login").unwrap().prefix);
		assert!(r.definition("pow.difficulty.").is_err());
		assert!(r.definition("nope.nope").is_err());
	}

	#[test]
	fn validation_rejects_out_of_range_and_unknown_choices() {
		let r = registry();
		let port = r.definition("email.smtp.port").unwrap();
		assert!(parse(port, "65536").is_err());
		assert!(parse(port, "587").is_ok());
		assert!(parse(r.definition("deployment.env").unwrap(), "staging").is_err());
		// `Flag` has no registered core key, but the type stays for a feature crate's and a
		// consumer's own settings.
		let flag = SettingDef::flag("test.flag", "0", "A flag.");
		assert_eq!(parse(&flag, "yes").unwrap(), SettingValue::Flag(true));
		assert!(parse(&flag, "maybe").is_err());
	}

	/// `SettingDef` bounds a `Text` only by length, so a value that will not parse used to be
	/// stored happily and fail far from the operator — `ratelimit.login.ip = "10/5week"` 400'd
	/// every request in that scope with login down until someone found the row. `parse` is the
	/// only validation path, so this is what `Settings::set` refuses.
	///
	/// The numeric ceilings are the same rule against an arithmetic trap: every one of these
	/// keys feeds `now + n`, `n * 86_400` or a loop count, and `[profile.release]` turns the
	/// overflow into a panic. `jobs.workers` spawns that many tasks inside `AppBuilder::build`.
	#[test]
	fn a_value_that_cannot_be_used_is_refused_where_it_is_written() {
		let r = registry();
		let max = i64::MAX.to_string();
		for (key, bad, good) in [
			(
				"ratelimit.login.ip",
				&["10/5week", "nan/min/ip", "0/min/ip", "nope"][..],
				&["10/5min/ip"][..],
			),
			("ratelimit.default", &[], &["120/min/ip"]),
			("jobs.retention_days", &[&max, "36501"], &["90"]),
			("jobs.failed_alert_hours", &[&max, "8761", "0"], &["24"]),
			// `jobs.workers` shares the ceiling and not the floor: `0` is how a process says it
			// runs no jobs, which is what keeps `Runner::reclaim` single-process.
			("jobs.workers", &[&max, "65", "-1"], &["64", "0"]),
		] {
			let def = r.definition(key).unwrap();
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
	fn env_names_are_unprefixed_and_uppercased() {
		assert_eq!(env_name("currency.base"), "CURRENCY_BASE");
		assert_eq!(env_name("app.dist_dir"), "APP_DIST_DIR");
	}

	/// With no prefix to filter on, only a *declared* key's variable may ever be copied — an
	/// ambient `HOME` or `PATH` must not be readable as a setting.
	#[test]
	fn the_snapshot_holds_only_declared_variables() {
		let r = registry();
		assert!(!r.env.contains_key("PATH"), "{:?}", r.env.keys().collect::<Vec<_>>());
		assert!(!r.env.contains_key("HOME"));
	}
}

// vim: ts=4
