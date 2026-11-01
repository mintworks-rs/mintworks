// SPDX-License-Identifier: MPL-2.0
//! What `main(app)` declares, and the `axum` router it becomes.
//!
//! The declaration object is the Rune-side mirror of `AppBuilder`: `main` only declares, and
//! every name it uses is resolved here, at load, so an unknown mount or a duplicate route
//! refuses to serve rather than 404ing at runtime.
//!
//! A generated handler deserializes, invokes exactly one script function and serializes —
//! the framework's own layering rule, unchanged by the entry point being a script.

use std::{
	collections::{BTreeMap, BTreeSet},
	sync::{Arc, Mutex, MutexGuard, PoisonError},
	time::Duration,
};

use axum::{
	Json, Router,
	body::{Body, Bytes},
	extract::{Query, RawPathParams, State},
	http::{HeaderMap, Method, StatusCode, Uri, header},
	response::{IntoResponse, Response},
	routing::MethodFilter,
};
use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use mintworks_core::{
	App, Ctx,
	app::{RouterScopeExt, Scoped},
	auth_mw::{ClientIp, RouteGate},
	blob::{SpoolError, Upload},
	error::{ClResult, Error},
	ratelimit,
	settings::{Scope, SettingDef, SettingType},
};
use rune::{
	Any, ContextError, Hash, Module, Value,
	runtime::{FromValue, Function, Object, RuntimeError, ToValue},
};

use crate::{
	ScriptCtx,
	db::{Migration, TableDef},
	error,
	jobs::JobDecl,
	objects::ObjectTypeDef,
	stream::{Source, Streamed},
	value::{ScriptError, from_json, to_json},
	vm::Script,
};

/// The response keys `resp::` marks a value with. `$` cannot start a Rune identifier, so
/// shorthand object syntax can never produce this shape by accident.
const K_STATUS: &str = "$status";
const K_BODY: &str = "$body";

/// `&'static str` is what `AppState::route_scopes` and `Actor::System { source }` both need, and
/// a name declared in a script is a `String`. Interning is bounded and happens once, at load:
/// past the cap the load fails rather than leaking per request.
const MAX_INTERNED: usize = 256;

/// Shared by route scope prefixes, rate tiers and job sources.
///
/// # Errors
/// `E-SCRIPT-COMPILE` past [`MAX_INTERNED`] distinct names.
pub fn intern(name: &str) -> ClResult<&'static str> {
	static NAMES: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
	let mut set = lock(&NAMES);
	if let Some(found) = set.get(name) {
		return Ok(found);
	}
	if set.len() >= MAX_INTERNED {
		return Err(error::compile(format!("more than {MAX_INTERNED} distinct names interned")));
	}
	let leaked: &'static str = String::leak(name.to_string());
	set.insert(leaked);
	Ok(leaked)
}

/// [`intern`] for a declared name list: `AppBuilder::secrets` wants `'static`, and a suite
/// installs once per case, so an identical list is leaked once.
pub fn intern_slice(names: &[&'static str]) -> &'static [&'static str] {
	static SLICES: Mutex<BTreeSet<&'static [&'static str]>> = Mutex::new(BTreeSet::new());
	let mut set = lock(&SLICES);
	if let Some(found) = set.get(names) {
		return found;
	}
	let leaked: &'static [&'static str] = Vec::leak(names.to_vec());
	set.insert(leaked);
	leaked
}

/// [`intern_slice`] for `app.setting` declarations, whose strings [`intern`] already shares.
pub fn intern_settings(defs: &[SettingDef]) -> &'static [SettingDef] {
	static SLICES: Mutex<Vec<&'static [SettingDef]>> = Mutex::new(Vec::new());
	// Every field but `check`, which a script never sets; exhaustive, so a new field must be listed.
	let same = |a: &SettingDef, b: &SettingDef| {
		let k = |d: &SettingDef| {
			let SettingDef {
				key,
				ty,
				default,
				description,
				scope,
				min,
				max,
				prefix,
				required,
				user_writable,
				check: _,
			} = *d;
			(key, ty, default, description, scope, min, max, prefix, required, user_writable)
		};
		k(a) == k(b)
	};
	let mut all = lock(&SLICES);
	let found = all
		.iter()
		.find(|s| s.len() == defs.len() && s.iter().zip(defs).all(|(a, b)| same(a, b)))
		.copied();
	found.unwrap_or_else(|| {
		let leaked: &'static [SettingDef] = Vec::leak(defs.to_vec());
		all.push(leaked);
		leaked
	})
}

/// Declaration runs single-threaded at load, so a poisoned lock means an earlier panic has
/// already failed the load; recovering the guard reports that failure instead of a second one.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One `app.get/post/…` declaration.
pub struct RouteDecl {
	pub method: Method,
	pub path: String,
	pub entry: Hash,
	/// Drops the auth and consent layers, and keys the rate tier on IP rather than account.
	pub public: bool,
	pub tier: Option<String>,
	/// `None` is fail-closed for an `Actor::Key`, which is the framework's own default.
	pub scope: Option<String>,
	/// `.timeout_ms(n)`: replaces `script.timeout_ms` for this route, clamped to
	/// `script.max_timeout_ms` at dispatch.
	pub timeout_ms: Option<i64>,
	/// `.max_body(n)`: this route's body cap in bytes, clamped to `script.max_body_bytes` at
	/// dispatch. `None` is [`DEFAULT_MAX_BODY`].
	pub max_body: Option<i64>,
	/// `.raw()`: any content type, and `req.body` is a [`crate::blob::RequestBody`].
	pub raw: bool,
}

/// axum's own implicit body limit, kept for every route without `.max_body`.
pub const DEFAULT_MAX_BODY: i64 = 2 * 1024 * 1024;

/// Everything `main(app)` declared, read back by [`crate::ScriptApp`] once it returns.
#[derive(Default)]
pub struct Decls {
	pub mounts: Vec<String>,
	pub features: BTreeSet<String>,
	pub setting_defaults: Vec<(Option<String>, String, String)>,
	/// What `app.test_default(…)` declared; applied only under `mintworks test`.
	pub test_defaults: Vec<(String, String)>,
	/// What `app.test_env(…)` declared; `env::get`'s fallback only under `mintworks test`.
	pub test_env: Vec<(String, String)>,
	/// What `app.secret(…)` declared, interned for `AppBuilder::secrets`.
	pub secrets: Vec<&'static str>,
	/// What `app.setting(…)` declared, every string interned, for `AppBuilder::settings`.
	pub settings: Vec<SettingDef>,
	pub types: Vec<ObjectTypeDef>,
	/// What `app.table(…)` declared, for `AppDb::reconcile`.
	pub tables: Vec<TableDef>,
	/// What `app.migration(…)` declared, sorted by version and contiguous from 1 after `take`.
	pub migrations: Vec<Migration>,
	/// What `app.entitlement(…)` declared, for `mintworks_entitle::install`.
	pub entitlements: Vec<mintworks_entitle::EntitlementDef>,
	/// What `app.offer(…)` declared, for `mintworks_plans::install`.
	pub offers: Vec<mintworks_plans::OfferDef>,
	/// What `app.member_role(…)` declared, installed as `mintworks_auth::org::MemberRoles`.
	pub member_roles: BTreeMap<String, mintworks_core::store::Role>,
	/// What `app.account_role(…)` declared, installed as `mintworks_auth::org::AccountRoles`.
	pub account_roles: BTreeSet<String>,
	/// What `app.consent(…)` declared, installed as `mintworks_auth::consent::ConsentKinds`.
	pub consents: BTreeMap<String, mintworks_auth::consent::ConsentKind>,
	pub routes: Vec<RouteDecl>,
	pub jobs: Vec<JobDecl>,
	/// `app.tool(…)`: the agent tools this script defines.
	#[cfg(feature = "ai")]
	pub tools: Vec<crate::agent::ToolDecl>,
	pub init: Vec<Hash>,
	/// `app.on_event(kind, fn)`, in declaration order.
	pub events: Vec<(String, Hash)>,
	/// `app.on_account_export` / `app.on_account_erase` / `app.on_org_deleted`: at most one each.
	pub account_export: Option<Hash>,
	pub account_erase: Option<Hash>,
	pub org_deleted: Option<Hash>,
	/// A declaration error cannot be returned from a chained builder call without changing the
	/// script-side spelling, so it is collected here and fails the load afterwards.
	pub errors: Vec<String>,
}

impl Decls {
	fn fail(&mut self, msg: impl Into<String>) {
		self.errors.push(msg.into());
	}
}

/// Which of the `AccountHooks` slots a declaration fills.
pub(crate) enum AccountHook {
	Export,
	Erase,
	OrgDeleted,
}

/// The `app` handle `main` receives.
#[derive(Any, Clone)]
pub struct Decl(Arc<Mutex<Decls>>);

/// What `app.get(…)` returns, so `.public()`, `.tier(…)` and `.scope(…)` chain onto it.
#[derive(Any, Clone)]
pub struct RouteBuilder {
	decls: Arc<Mutex<Decls>>,
	idx: usize,
}

impl Decl {
	#[must_use]
	pub fn new() -> Self {
		Self(Arc::new(Mutex::new(Decls::default())))
	}

	/// The declarations, taken once `main` has returned. The migration versions are checked
	/// here, where the whole set is known: exactly `1..=n`, no gap and no duplicate.
	#[must_use]
	pub fn take(&self) -> Decls {
		let mut d = std::mem::take(&mut *lock(&self.0));
		// Only a spooled body may pass the default: a buffered one stays this small, so no RAM gate.
		let big: Vec<String> = (d.routes.iter())
			.filter(|r| !r.raw && r.max_body.is_some_and(|n| n > DEFAULT_MAX_BODY))
			.map(|r| format!("{} {}: .max_body above 2 MiB requires .raw()", r.method, r.path))
			.collect();
		d.errors.extend(big);
		d.migrations.sort_by_key(|m| m.version);
		if d.migrations.iter().zip(1..).any(|(m, n)| m.version != n) {
			let got: Vec<String> = d.migrations.iter().map(|m| m.version.to_string()).collect();
			d.fail(format!(
				"app.migration versions must be exactly 1..={}, got {}",
				d.migrations.len(),
				got.join(", ")
			));
		}
		d
	}

	pub(crate) fn push_job(&self, decl: JobDecl) {
		let mut d = lock(&self.0);
		if d.jobs.iter().any(|j| j.kind == decl.kind) {
			d.fail(format!("job kind '{}' is declared twice", decl.kind));
		}
		d.jobs.push(decl);
	}

	#[cfg(feature = "ai")]
	pub(crate) fn push_tool(&self, decl: crate::agent::ToolDecl) {
		let mut d = lock(&self.0);
		if d.tools.iter().any(|t| t.name == decl.name) {
			d.fail(format!("tool '{}' is declared twice", decl.name));
		}
		d.tools.push(decl);
	}

	#[cfg(feature = "ai")]
	pub(crate) fn fail(&self, msg: impl Into<String>) {
		lock(&self.0).fail(msg);
	}

	pub(crate) fn push_init(&self, entry: Hash) {
		lock(&self.0).init.push(entry);
	}

	pub(crate) fn push_event(&self, kind: String, entry: Hash) {
		let mut d = lock(&self.0);
		if !mintworks_core::event::Event::KINDS.contains(&kind.as_str()) {
			d.fail(format!("app.on_event: unknown event kind '{kind}'"));
		}
		d.events.push((kind, entry));
	}

	/// A second declaration of any one account hook fails.
	pub(crate) fn push_account_hook(&self, which: AccountHook, entry: Hash) {
		let mut d = lock(&self.0);
		let (slot, name) = match which {
			AccountHook::Export => (&mut d.account_export, "on_account_export"),
			AccountHook::Erase => (&mut d.account_erase, "on_account_erase"),
			AccountHook::OrgDeleted => (&mut d.org_deleted, "on_org_deleted"),
		};
		if slot.replace(entry).is_some() {
			d.fail(format!("app.{name} is declared twice"));
		}
	}

	fn route(&self, method: Method, path: String, handler: &Function) -> RouteBuilder {
		let mut d = lock(&self.0);
		if d.routes.iter().any(|r| r.method == method && r.path == path) {
			d.fail(format!("route {method} {path} is declared twice"));
		}
		d.routes.push(RouteDecl {
			method,
			path,
			entry: handler.type_hash(),
			public: false,
			tier: None,
			scope: None,
			timeout_ms: None,
			max_body: None,
			raw: false,
		});
		RouteBuilder { decls: Arc::clone(&self.0), idx: d.routes.len() - 1 }
	}
}

impl Default for Decl {
	fn default() -> Self {
		Self::new()
	}
}

impl RouteBuilder {
	fn edit(&self, f: impl FnOnce(&mut RouteDecl)) -> Self {
		let mut d = lock(&self.decls);
		if let Some(r) = d.routes.get_mut(self.idx) {
			f(r);
		}
		drop(d);
		self.clone()
	}
}

macro_rules! verb {
	($name:ident, $method:ident) => {
		#[rune::function(instance)]
		fn $name(this: &Decl, path: String, handler: Function) -> RouteBuilder {
			this.route(Method::$method, path, &handler)
		}
	};
}

verb!(get, GET);
verb!(post, POST);
verb!(put, PUT);
verb!(patch, PATCH);
verb!(delete, DELETE);

/// `.public()` — drops auth and consent from this one route. An explicit `.tier(…)` is
/// required with it, because the `AUTHENTICATED` default keys on an account the
/// request no longer has.
#[rune::function(instance)]
fn public(this: &RouteBuilder) -> RouteBuilder {
	this.edit(|r| r.public = true)
}

#[rune::function(instance)]
fn tier(this: &RouteBuilder, name: String) -> RouteBuilder {
	this.edit(|r| r.tier = Some(name))
}

#[rune::function(instance)]
fn scope(this: &RouteBuilder, prefix: String) -> RouteBuilder {
	this.edit(|r| r.scope = Some(prefix))
}

/// `.timeout_ms(n)` — this route's wall-clock deadline. The instruction budget is unchanged.
#[rune::function(instance)]
fn timeout_ms(this: &RouteBuilder, ms: i64) -> RouteBuilder {
	if ms <= 0 {
		lock(&this.decls).fail(format!(".timeout_ms({ms}) must be positive"));
		return this.clone();
	}
	this.edit(|r| r.timeout_ms = Some(ms))
}

/// `.max_body(n)` — this route's request-body cap in bytes; past it the request is a 413.
#[rune::function(instance)]
fn max_body(this: &RouteBuilder, bytes: i64) -> RouteBuilder {
	if bytes <= 0 {
		lock(&this.decls).fail(format!(".max_body({bytes}) must be positive"));
		return this.clone();
	}
	this.edit(|r| r.max_body = Some(bytes))
}

/// `.raw()` — accepts any content type and spools the body to disk unparsed, for `blob::put`.
#[rune::function(instance)]
fn raw(this: &RouteBuilder) -> RouteBuilder {
	this.edit(|r| r.raw = true)
}

/// A mount implies its feature: otherwise mounting `invoice.org_read` without
/// `app.feature("invoice")` boots with no `Arc<dyn InvoiceStore>` extension.
#[rune::function(instance)]
fn mount(this: &Decl, name: String) {
	let mut d = lock(&this.0);
	let feat = name.split_once('.').map_or(name.as_str(), |(f, _)| f);
	if FEATURES.contains(&feat) || (cfg!(feature = "ai") && AI_FEATURES.contains(&feat)) {
		enable(&mut d.features, feat);
	}
	d.mounts.push(name);
}

/// The crates whose settings, secrets, jobs and alerts an app opts into. `auth` and `email` are
/// always on, so naming one is a declaration error rather than a no-op.
const FEATURES: &[&str] = &["invoice", "nav", "billing", "pdf", "entitle", "plans"];

/// Features whose crates compile only with the `ai` Cargo feature; each AI plan appends its own.
const AI_FEATURES: &[&str] = &["llm", "memory", "agent", "search"];

/// `agent` runs the LLM loop over memory tools, so it switches both on; `search` records its
/// calls in the LLM ledger, so it switches `llm` on.
fn enable(features: &mut BTreeSet<String>, name: &str) {
	if name == "agent" {
		features.extend(["llm".to_owned(), "memory".to_owned()]);
	}
	if name == "search" {
		features.insert("llm".to_owned());
	}
	features.insert(name.to_owned());
}

#[rune::function(instance)]
fn feature(this: &Decl, name: String) {
	let mut d = lock(&this.0);
	if AI_FEATURES.contains(&name.as_str()) {
		if cfg!(feature = "ai") {
			enable(&mut d.features, &name);
		} else {
			d.fail(format!("feature '{name}' needs mintworks built with `--features ai`"));
		}
		return;
	}
	if !FEATURES.contains(&name.as_str()) {
		d.fail(format!("unknown feature '{name}'; one of {FEATURES:?}"));
		return;
	}
	d.features.insert(name);
}

/// `app.setting_default("nav.software_id", "…")` — `AppBuilder::setting_default` for a script:
/// an undeclared key, or a second default for the same key, fails the boot.
#[rune::function(instance)]
fn setting_default(this: &Decl, key: String, value: String) {
	lock(&this.0).setting_defaults.push((None, key, value));
}

/// `app.setting_default_for("test", "nav.software_id", "…")` — applies only while
/// `deployment.env` is `env`, above `app.setting_default`.
#[rune::function(instance)]
fn setting_default_for(this: &Decl, env: String, key: String, value: String) {
	lock(&this.0).setting_defaults.push((Some(env), key, value));
}

/// `app.test_default("llm.kind.fake", "fake")` — a default for the automated suite only:
/// `mintworks test` applies it as `setting_default_for("test", …)`, a served app never does.
/// Fake providers belong here, not under `setting_default_for("test", …)`: `deployment.env =
/// test` is a real sandbox.
#[rune::function(instance)]
fn test_default(this: &Decl, key: String, value: String) {
	lock(&this.0).test_defaults.push((key, value));
}

/// `app.test_env("APP_SELLER_NAME", "…")` — what `env::get` answers under `mintworks test`, ahead
/// of the process environment, so a suite needs no `.env`. A served app never sees it. A
/// non-`APP_*` name or a second value for a name fails the boot.
#[rune::function(instance)]
fn test_env(this: &Decl, name: String, value: String) {
	let mut d = lock(&this.0);
	if !name.starts_with("APP_") {
		d.fail(format!("app.test_env: '{name}' is not an APP_* name, which env::get never reads"));
	} else if d.test_env.iter().any(|(n, _)| *n == name) {
		d.fail(format!("app.test_env '{name}' is declared twice"));
	} else {
		d.test_env.push((name, value));
	}
}

/// Bounds what one app may declare; past it the load fails.
const MAX_APP_SECRETS: usize = 32;

/// `app.secret("app.<name>")` — a secret `secret::get` may read. Env `APP_<NAME>` wins over the
/// encrypted row, as for every secret. A non-`app.` key, a family (`…`.) or a repeat fails the boot.
#[rune::function(instance)]
fn secret(this: &Decl, key: String) {
	let mut d = lock(&this.0);
	if !app_key(&key) {
		d.fail(format!("app.secret: '{key}' is not an app.<name> key"));
	} else if d.secrets.contains(&key.as_str()) {
		d.fail(format!("app.secret '{key}' is declared twice"));
	} else if d.secrets.len() >= MAX_APP_SECRETS {
		d.fail(format!("app.secret: more than {MAX_APP_SECRETS} secrets declared"));
	} else {
		match intern(&key) {
			Ok(k) => d.secrets.push(k),
			Err(e) => d.fail(e.to_string()),
		}
	}
}

const MAX_APP_SETTINGS: usize = 32;

/// What the admin listing shows for an `app.setting` key; the registry refuses a blank one.
const APP_SETTING_DESCRIPTION: &str = "declared by the application with app.setting";

/// `app.setting("app.<name>", #{scope: "global"|"org"|"account"|"orgAccount", type:
/// "int"|"bool"|"text"|"json", default, user?})` — a key `settings::get`/`set` resolve along its
/// scope's chain. `default` must parse as `type`; a json default is any value, stored as its JSON
/// text. `user: true` lets an account write its own value of an `account` key over HTTP.
#[rune::function(instance)]
fn setting(this: &Decl, key: String, decl: Value) {
	#[derive(serde::Deserialize)]
	#[serde(deny_unknown_fields)]
	struct Arg {
		scope: String,
		#[serde(rename = "type")]
		ty: String,
		default: serde_json::Value,
		#[serde(default)]
		user: bool,
	}
	let mut d = lock(&this.0);
	let a: Arg = match decl_arg(&decl) {
		Ok(a) => a,
		Err(e) => return d.fail(format!("app.setting '{key}': {e}")),
	};
	if !app_key(&key) {
		return d.fail(format!("app.setting: '{key}' is not an app.<name> key"));
	}
	if d.settings.iter().any(|s| s.key == key) {
		return d.fail(format!("app.setting '{key}' is declared twice"));
	}
	if d.settings.len() >= MAX_APP_SETTINGS {
		return d.fail(format!("app.setting: more than {MAX_APP_SETTINGS} settings declared"));
	}
	let scope = match a.scope.as_str() {
		"global" => Scope::Global,
		"org" => Scope::Org,
		"account" => Scope::Account,
		"orgAccount" => Scope::OrgAccount,
		other => return d.fail(format!("app.setting '{key}': unknown scope '{other}'")),
	};
	let ty = match a.ty.as_str() {
		"int" => SettingType::Int,
		"bool" => SettingType::Flag,
		"text" => SettingType::Text,
		"json" => SettingType::Json,
		other => return d.fail(format!("app.setting '{key}': unknown type '{other}'")),
	};
	let def = mintworks_core::settings::raw_value(ty, &a.default)
		.and_then(|raw| Ok((intern(&key)?, intern(&raw)?)))
		.map(|(k, raw)| {
			let def = match ty {
				SettingType::Int => SettingDef::int(k, raw, APP_SETTING_DESCRIPTION),
				SettingType::Flag => SettingDef::flag(k, raw, APP_SETTING_DESCRIPTION),
				SettingType::Json => SettingDef::json(k, raw, APP_SETTING_DESCRIPTION),
				_ => SettingDef::text(k, raw, APP_SETTING_DESCRIPTION),
			};
			let def = def.scope(scope);
			if a.user { def.user_writable() } else { def }
		})
		.and_then(|def| mintworks_core::settings::parse(&def, def.default).map(|_| def));
	match def {
		Ok(def) => d.settings.push(def),
		Err(e) => d.fail(format!("app.setting '{key}': default: {e}")),
	}
}

/// `app.<name>` in `[a-z0-9._-]`, not a family: what `app.secret` and `app.setting` accept.
fn app_key(key: &str) -> bool {
	key.len() > "app.".len()
		&& key.starts_with("app.")
		&& !key.ends_with('.')
		&& key
			.bytes()
			.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// `app.object_type(name, #{ prefix: "prj_", paths: ["$.partyUid"] })`.
#[rune::function(instance)]
fn object_type(this: &Decl, name: String, decl: Value) {
	let mut d = lock(&this.0);
	let json = match to_json(&decl) {
		Ok(json) => json,
		Err(e) => return d.fail(format!("object type '{name}': {e}")),
	};
	let mut def = ObjectTypeDef::new(&name);
	if let Some(prefix) = json.get("prefix").and_then(serde_json::Value::as_str) {
		def = def.prefix(prefix);
	}
	for path in json.get("paths").and_then(serde_json::Value::as_array).into_iter().flatten() {
		match path.as_str() {
			Some(p) => def = def.path(p),
			None => return d.fail(format!("object type '{name}': a path is not a string")),
		}
	}
	d.types.push(def);
}

/// `app.table("ledger", #{ cols: #{ uid: "TEXT PRIMARY KEY" }, indexes: [["org_id"]] })`.
#[rune::function(instance)]
fn table(this: &Decl, name: String, decl: Value) {
	let mut d = lock(&this.0);
	let json = match to_json(&decl) {
		Ok(json) => json,
		Err(e) => return d.fail(format!("table '{name}': {e}")),
	};
	match TableDef::parse(&name, &json) {
		Err(e) => d.fail(e),
		Ok(def) if d.tables.iter().any(|t| t.name == def.name) => {
			d.fail(format!("table '{name}' is declared twice"));
		}
		Ok(def) => d.tables.push(def),
	}
}

/// `app.migration(1, "CREATE TABLE notes (…); CREATE INDEX …")` — raw dialect DDL, applied once.
#[rune::function(instance)]
fn migration(this: &Decl, version: i64, sql: String) {
	let mut d = lock(&this.0);
	match Migration::new(version, sql) {
		Err(e) => d.fail(e),
		Ok(m) => d.migrations.push(m),
	}
}

/// `app.entitlement("ai_credits", "meter")` — declares a key and its kind (`feature`, `limit` or
/// `meter`) and switches the `entitle` feature on.
#[rune::function(instance)]
fn entitlement(this: &Decl, key: String, kind: String) {
	let mut d = lock(&this.0);
	let Ok(kind) = kind.parse::<mintworks_entitle::Kind>() else {
		return d.fail(format!("entitlement '{key}': unknown kind '{kind}'"));
	};
	if d.entitlements.iter().any(|e| e.key == key) {
		return d.fail(format!("entitlement '{key}' is declared twice"));
	}
	d.features.insert("entitle".to_owned());
	d.entitlements.push(mintworks_entitle::EntitlementDef { key, kind });
}

/// A declaration's `#{…}` argument as `T`.
fn decl_arg<T: serde::de::DeserializeOwned>(decl: &Value) -> Result<T, String> {
	let json = to_json(decl).map_err(|e| e.to_string())?;
	serde_json::from_value(json).map_err(|e| e.to_string())
}

/// `[a-z][a-z0-9_]{0,31}`, or `[A-Z][A-Z0-9_]{0,31}` when `upper`.
fn ident(name: &str, upper: bool) -> bool {
	let case = |c: char| if upper { c.is_ascii_uppercase() } else { c.is_ascii_lowercase() };
	let mut chars = name.chars();
	chars.next().is_some_and(case)
		&& name.len() <= 32
		&& chars.all(|c| case(c) || c.is_ascii_digit() || c == '_')
}

/// `app.member_role("teacher", "ADMIN")` — an app role name (`[a-z][a-z0-9_]{0,31}`) and the
/// rank (`OWNER`, `ADMIN` or `MEMBER`) setting it grants.
#[rune::function(instance)]
fn member_role(this: &Decl, name: String, rank: String) {
	let mut d = lock(&this.0);
	if !ident(&name, false) {
		return d.fail(format!("member_role '{name}': the name must match [a-z][a-z0-9_]{{0,31}}"));
	}
	let Ok(rank) = rank.parse::<mintworks_core::store::Role>() else {
		return d.fail(format!("member_role '{name}': unknown rank '{rank}'"));
	};
	if d.member_roles.insert(name.clone(), rank).is_some() {
		d.fail(format!("member_role '{name}' is declared twice"));
	}
}

/// `app.account_role("teacher")` — an account-level app role name (`[a-z][a-z0-9_]{0,31}`),
/// granted to an account rather than a membership, so it carries no rank.
#[rune::function(instance)]
fn account_role(this: &Decl, name: String) {
	let mut d = lock(&this.0);
	if !ident(&name, false) {
		return d
			.fail(format!("account_role '{name}': the name must match [a-z][a-z0-9_]{{0,31}}"));
	}
	if !d.account_roles.insert(name.clone()) {
		d.fail(format!("account_role '{name}' is declared twice"));
	}
}

/// `app.consent("AGE16", #{required, withdrawable})` — an app consent kind
/// (`[A-Z][A-Z0-9_]{0,31}`, not a framework one). Its text is `<app-dir>/legal/<name lowercased>.md`;
/// a required kind gates login and registration exactly like `TOS`.
#[rune::function(instance)]
fn consent(this: &Decl, name: String, decl: Value) {
	#[derive(serde::Deserialize)]
	#[serde(deny_unknown_fields)]
	struct Arg {
		required: bool,
		withdrawable: bool,
	}
	let mut d = lock(&this.0);
	let a: Arg = match decl_arg(&decl) {
		Ok(a) => a,
		Err(e) => return d.fail(format!("app.consent '{name}': {e}")),
	};
	if !ident(&name, true) {
		return d.fail(format!("app.consent '{name}': the name must match [A-Z][A-Z0-9_]{{0,31}}"));
	}
	if mintworks_auth::consent::KINDS.iter().any(|k| k.as_str() == name) {
		return d.fail(format!("app.consent '{name}' is a framework kind"));
	}
	let kind =
		mintworks_auth::consent::ConsentKind { required: a.required, withdrawable: a.withdrawable };
	if d.consents.insert(name.clone(), kind).is_some() {
		d.fail(format!("app.consent '{name}' is declared twice"));
	}
}

/// `app.offer("pro", #{name, kind: "RECURRING", service, family?, rank?, interval?: "MONTH",
/// intervalCount?, validityDays?, trialDays?, prices: #{HUF: 499000}, entitlements: #{key:
/// amount | #{amount, perSeat}}})` — amounts in minor units; switches `plans` on.
#[rune::function(instance)]
fn offer(this: &Decl, code: String, decl: Value) {
	#[derive(serde::Deserialize)]
	#[serde(rename_all = "camelCase", deny_unknown_fields)]
	struct Arg {
		name: String,
		kind: String,
		service: String,
		family: Option<String>,
		#[serde(default)]
		rank: i64,
		interval: Option<String>,
		interval_count: Option<i64>,
		validity_days: Option<i64>,
		#[serde(default)]
		trial_days: i64,
		#[serde(default)]
		prices: std::collections::BTreeMap<String, i64>,
		#[serde(default)]
		entitlements: std::collections::BTreeMap<String, serde_json::Value>,
	}
	let mut d = lock(&this.0);
	let a: Arg = match decl_arg(&decl) {
		Ok(a) => a,
		Err(e) => return d.fail(format!("offer '{code}': {e}")),
	};
	if d.offers.iter().any(|o| o.code == code) {
		return d.fail(format!("offer '{code}' is declared twice"));
	}
	let mut def = match a.kind.as_str() {
		"ONE_TIME" => mintworks_plans::OfferDef::one_time(&code, &a.name, &a.service),
		"RECURRING" => {
			let interval = match a.interval.as_deref() {
				Some("MONTH") => mintworks_plans::store::Interval::Month,
				Some("YEAR") => mintworks_plans::store::Interval::Year,
				other => {
					return d.fail(format!("offer '{code}': interval {other:?} is not MONTH|YEAR"));
				}
			};
			mintworks_plans::OfferDef::recurring(
				&code,
				&a.name,
				&a.service,
				interval,
				a.interval_count.unwrap_or(1),
			)
		}
		other => {
			return d.fail(format!("offer '{code}': kind '{other}' is not ONE_TIME|RECURRING"));
		}
	};
	(def.family, def.rank, def.validity_days, def.trial_days) =
		(a.family, a.rank, a.validity_days, a.trial_days);
	for (cur, amount) in a.prices {
		match mintworks_core::money::CurrencyCode::parse(&cur) {
			Ok(c) => def = def.price(c, amount),
			Err(_) => return d.fail(format!("offer '{code}': bad currency '{cur}'")),
		}
	}
	for (key, e) in a.entitlements {
		let (amount, per_seat) = match &e {
			serde_json::Value::Number(n) => (n.as_i64(), false),
			serde_json::Value::Object(o) => (
				o.get("amount").and_then(serde_json::Value::as_i64),
				o.get("perSeat").and_then(serde_json::Value::as_bool).unwrap_or(false),
			),
			_ => (None, false),
		};
		let Some(amount) = amount else {
			return d.fail(format!(
				"offer '{code}': entitlement '{key}' is amount | #{{amount, perSeat}}"
			));
		};
		def = def.entitle(key, amount, per_seat);
	}
	d.features.insert("plans".to_owned());
	d.offers.push(def);
}

/// The names `resolve` accepts, for the unknown-mount error. The test walks this, so a name that
/// stopped resolving cannot stay advertised here. The access matrix composes every one of them.
pub const MOUNTS: &[&str] = &[
	"auth.public",
	"auth.authenticated",
	"auth.operator",
	"invoice.org_read",
	"invoice.org_parties",
	"invoice.org_services",
	"invoice.org_invoices",
	"invoice.org_seller",
	"nav.org_credentials",
	"billing.public",
	"billing.org",
	"billing.operator",
	"pdf.documents",
	"refs",
	"entitle",
	"plans",
	#[cfg(feature = "ai")]
	"agent.runs",
];

/// Resolves a mount name to the bundle it names, with its gate and its API-key scope prefix
/// already applied; the names are the Rust symbols.
///
/// A `match` rather than a name → `fn` table: the bundles have two shapes (bare and
/// `fn(&RouteGate)`), which no single fn-pointer type expresses. Public so the access matrix
/// composes through the same mount → scope map rather than a copy of it.
pub fn resolve(name: &str, gate: &RouteGate) -> Option<Scoped> {
	Some(match name {
		// The two auth bundles stay unscoped deliberately: fail-closed for an
		// `Actor::Key`, which keeps a leaked API key out of the routes that mint other keys.
		"auth.public" => mintworks_auth::routes::public().into(),
		"auth.authenticated" => mintworks_auth::routes::authenticated().into(),
		"auth.operator" => mintworks_auth::routes::operator().into(),
		"invoice.org_read" => mintworks_invoice::routes::org_read(gate).scope("invoice"),
		"invoice.org_parties" => mintworks_invoice::routes::org_parties(gate).scope("invoice"),
		"invoice.org_services" => mintworks_invoice::routes::org_services(gate).scope("invoice"),
		"invoice.org_invoices" => mintworks_invoice::routes::org_invoices(gate).scope("invoice"),
		// Unscoped, like `auth.*`: an API key must not mint a seller or set NAV credentials.
		"invoice.org_seller" => mintworks_invoice::routes::org_seller(gate).into(),
		"nav.org_credentials" => mintworks_nav::routes::org_credentials(gate).into(),
		"billing.public" => mintworks_billing::routes::public().scope("billing"),
		"billing.org" => mintworks_billing::routes::org(gate).scope("billing"),
		"billing.operator" => mintworks_billing::routes::operator(gate).scope("billing"),
		"pdf.documents" => mintworks_pdf::routes().scope("pdf"),
		"refs" => mintworks_core::refs::routes(gate),
		// Scoped inside: `/api/entitlements` as `entitlements`, the operator routes not at all.
		"entitle" => mintworks_entitle::routes(gate),
		// Scoped inside: `plans`, the offers list public.
		"plans" => mintworks_plans::routes(gate),
		#[cfg(feature = "ai")]
		"agent.runs" => mintworks_agent::routes::runs(gate).scope("agent"),
		_ => return None,
	})
}

/// The declared routes and mounts as one bundle.
///
/// # Errors
/// `E-SCRIPT-COMPILE` for an unknown mount name, an unknown HTTP method, a `.public()` route
/// with no explicit tier, or more than [`MAX_INTERNED`] distinct scope and tier names.
pub fn build(script: &Arc<Script>, decls: &Decls, gate: &RouteGate) -> ClResult<Scoped> {
	let mut bundle: Scoped = Router::<App>::new().into();
	for name in &decls.mounts {
		let mounted = resolve(name, gate).ok_or_else(|| {
			error::compile(format!("unknown mount '{name}'; expected one of {}", MOUNTS.join(", ")))
		})?;
		bundle = bundle.merge(mounted);
	}

	// Grouped by scope prefix, because `RouterScopeExt::scope` annotates a whole router; the
	// auth and tier layers are per-route, so they go on the `MethodRouter` instead.
	let mut by_scope: BTreeMap<Option<&'static str>, Router<App>> = BTreeMap::new();
	for r in &decls.routes {
		let scope = r.scope.as_deref().map(intern).transpose()?;
		let tier = match (&r.tier, r.public) {
			(Some(t), _) => intern(t)?,
			(None, false) => ratelimit::AUTHENTICATED,
			(None, true) => {
				return Err(error::compile(format!(
					"public route {} {} names no tier; `.public()` requires `.tier(…)`",
					r.method, r.path
				)));
			}
		};

		// `require_auth` is bundled here, not left to the gate: `RouteGate::none()` drops consent
		// only, and must not turn an authenticated route anonymous.
		let one = Router::new().route(r.path.as_str(), method(r, script, tier, r.public)?);
		let one = if r.public {
			one
		} else {
			gate.apply(one.layer(axum::middleware::from_fn(mintworks_core::auth_mw::require_auth)))
		};
		let entry = by_scope.entry(scope).or_default();
		*entry = std::mem::take(entry).merge(one);
	}

	for (scope, router) in by_scope {
		bundle = match scope {
			Some(prefix) => bundle.merge(router.scope(prefix)),
			None => bundle.merge(router),
		};
	}
	Ok(bundle)
}

fn method(
	r: &RouteDecl,
	script: &Arc<Script>,
	tier: &'static str,
	ip_keyed: bool,
) -> ClResult<axum::routing::MethodRouter<App>> {
	let filter = MethodFilter::try_from(r.method.clone())
		.map_err(|_| error::compile(format!("unsupported HTTP method {}", r.method)))?;
	let (script, entry, timeout) = (Arc::clone(script), r.entry, r.timeout_ms);
	let (max_body, raw) = (r.max_body, r.raw);

	// `Body`, not `Bytes`: the cap is read from settings per request, so axum's must not apply.
	let handler = axum::routing::on(
		filter,
		move |State(app): State<App>,
		      ctx: Option<Ctx>,
		      params: RawPathParams,
		      Query(query): Query<BTreeMap<String, String>>,
		      uri: Uri,
		      method: Method,
		      headers: HeaderMap,
		      ip: Option<axum::Extension<ClientIp>>,
		      body: Body| {
			let script = Arc::clone(&script);
			async move {
				let (q, u, m) = (&query, &uri, &method);
				let target = (entry, timeout, max_body, raw);
				let ip = ip.map(|axum::Extension(ClientIp(ip))| ip);
				dispatch(&script, target, app, ctx, &params, q, u, m, &headers, ip, body).await
			}
		},
	);
	// `scoped_ip_mw` wherever no auth layer runs: the account-keyed tier keys on a claim the
	// request no longer carries, so every anonymous caller would share one bucket.
	Ok(if ip_keyed {
		handler.layer(axum::middleware::from_fn_with_state(tier, ratelimit::scoped_ip_mw))
	} else {
		handler.layer(axum::middleware::from_fn_with_state(tier, ratelimit::scoped_account_mw))
	})
}

/// Fixed 2 min to receive a body; make it a setting if slow uploads hit it
const BODY_READ_TIMEOUT: Duration = Duration::from_mins(2);

#[allow(clippy::too_many_arguments)]
async fn dispatch(
	script: &Script,
	(entry, timeout_ms, max_body, raw): (Hash, Option<i64>, Option<i64>, bool),
	app: App,
	ctx: Option<Ctx>,
	params: &RawPathParams,
	query: &BTreeMap<String, String>,
	uri: &Uri,
	method: &Method,
	headers: &HeaderMap,
	ip: Option<std::net::IpAddr>,
	body: Body,
) -> ClResult<Response> {
	let limit = match max_body {
		Some(n) => n.min(app.settings.int("script.max_body_bytes").await?),
		None => DEFAULT_MAX_BODY,
	};
	let body = Limited::new(body, usize::try_from(limit).unwrap_or(0));
	let too_large = || error::toolarge("request body exceeds this route's limit");
	let late = || error::body_timeout("request body not received in time");
	// Bounded in time too: a trickled body would otherwise hold its connection forever.
	let (body, upload) = if raw {
		let spooled = mintworks_core::blob::spool(&app.config.data_dir, body);
		match tokio::time::timeout(BODY_READ_TIMEOUT, spooled).await.map_err(|_| late())? {
			Ok(up) => (Bytes::new(), Some(up)),
			Err(SpoolError::Body(e)) if e.is::<LengthLimitError>() => return Err(too_large()),
			Err(SpoolError::Body(e)) => return Err(Error::validation(format!("body: {e}"))),
			Err(SpoolError::Io(e)) => return Err(e),
		}
	} else {
		match tokio::time::timeout(BODY_READ_TIMEOUT, body.collect())
			.await
			.map_err(|_| late())?
		{
			Ok(b) => (b.to_bytes(), None),
			Err(e) if e.is::<LengthLimitError>() => return Err(too_large()),
			Err(e) => return Err(Error::validation(format!("body: {e}"))),
		}
	};
	let path: serde_json::Map<String, serde_json::Value> = params
		.iter()
		.map(|(k, v)| (k.to_string(), serde_json::Value::from(v)))
		.collect();
	let parsed = if raw || body.is_empty() {
		serde_json::Value::Null
	} else if is_json(headers) {
		serde_json::from_slice(&body).map_err(|e| Error::validation(format!("body: {e}")))?
	} else {
		return Err(Error::Unsupported("expected Content-Type: application/json".into()));
	};

	let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
	let req = serde_json::json!({
		"path": path,
		"query": query,
		"body": parsed,
		"contentType": content_type,
		"headers": script_headers(headers),
		"ip": ip.map(|ip| ip.to_string()),
		"method": method.as_str(),
		"path_str": uri.path(),
	});
	// A `.public()` route has no token, and `Ctx`'s required extractor treats that as an error.
	let ctx = ctx.unwrap_or_else(|| Ctx::public("script"));
	let deadline = match timeout_ms {
		Some(ms) => {
			let max = app.settings.int("script.max_timeout_ms").await?;
			Some(Duration::from_millis(u64::try_from(ms.min(max)).unwrap_or(0)))
		}
		None => None,
	};
	let args = (ScriptCtx::new(app, ctx), Request { json: req, raw: upload });
	let reply: Reply = script.invoke_with(entry, args, deadline).await?;
	match reply {
		Reply::Json(json) => {
			let (status, body) = json?;
			Ok(if status == StatusCode::NO_CONTENT {
				status.into_response()
			} else {
				(status, Json(body)).into_response()
			})
		}
		Reply::Stream(Source::Http(status, body, encoding), ct) => {
			Ok(crate::stream::http_response(status, ct.as_deref(), encoding.as_deref(), body))
		}
		Reply::Stream(Source::File(path, name, attach), ct) => {
			let (name, ct) = (name.as_deref(), ct.as_deref());
			crate::stream::file_response(&path, name, attach, ct, method, uri, headers).await
		}
	}
}

/// Credential headers never shown to a script, so it cannot echo a token into a response or a
/// log: any name carrying a credential-shaped word. Over-hiding (`x-author`) is the
/// deliberate fail-closed side.
/// No `signature`: a webhook HMAC is bound to its body, and a route must read it to verify one.
/// Forwarding headers are client-set unless a trusted proxy rewrote them; `req.ip` is the
/// resolved address.
const HIDDEN_WORDS: &[&str] = &[
	"auth",
	"token",
	"secret",
	"password",
	"session",
	"api-key",
	"apikey",
	"access-key",
	"private-key",
	"credential",
	"bearer",
	"jwt",
	"cookie",
	"csrf",
];

fn hidden_header(name: &str) -> bool {
	HIDDEN_WORDS.iter().any(|w| name.contains(w))
		|| matches!(name, "forwarded" | "x-real-ip")
		|| name.starts_with("x-forwarded-")
}

/// `req.headers`: lowercase name → value, repeats joined with `", "`, non-UTF-8 values dropped.
fn script_headers(headers: &HeaderMap) -> serde_json::Map<String, serde_json::Value> {
	headers
		.keys()
		.filter(|name| !hidden_header(name.as_str()))
		.filter_map(|name| {
			let values: Vec<&str> =
				headers.get_all(name).iter().filter_map(|v| v.to_str().ok()).collect();
			(!values.is_empty()).then(|| (name.to_string(), values.join(", ").into()))
		})
		.collect()
}

/// `application/json` or `application/*+json`, the set axum's `Json` extractor accepts: a form
/// body is refused, not parsed as JSON behind a CORS-simple content type.
fn is_json(headers: &HeaderMap) -> bool {
	let Some(ct) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
		return false;
	};
	let mime = ct.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
	mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"))
}

/// A JSON argument — a request object, or a job's payload — converted to Rune **inside**
/// `ToValue`: `Vm::send_execute` bounds its arguments `Send` and `rune::Value` is not. The
/// mirror of `tx::Bridged` on the way out.
pub(crate) struct Arg(pub serde_json::Value);

impl ToValue for Arg {
	fn to_value(self) -> Result<Value, RuntimeError> {
		// This crate has no `RuntimeError` constructor, and `from_json` on plain JSON fails
		// only on allocation; a handler then traps reading `req.body`, which is a 5xx either way.
		Ok(from_json(&self.0).unwrap_or_else(|_| Value::from(())))
	}
}

/// A route's request object; on a `.raw()` route `body` is a [`crate::blob::RequestBody`],
/// which cannot travel inside the JSON.
struct Request {
	json: serde_json::Value,
	raw: Option<Upload>,
}

impl ToValue for Request {
	fn to_value(self) -> Result<Value, RuntimeError> {
		let Some(upload) = self.raw else {
			return Arg(self.json).to_value();
		};
		let mut o = Object::new();
		if let serde_json::Value::Object(m) = &self.json {
			for (k, v) in m.iter().filter(|(k, _)| *k != "body") {
				// As in `Arg`: these fail only on allocation.
				if let (Ok(k), Ok(v)) = (rune::alloc::String::try_from(k.as_str()), from_json(v)) {
					let _ = o.insert(k, v);
				}
			}
		}
		if let Ok(k) = rune::alloc::String::try_from("body") {
			let _ = o.insert(k, rune::to_value(crate::blob::RequestBody::new(upload))?);
		}
		rune::to_value(o)
	}
}

/// A handler's return value, bridged the same way on the way back: JSON, or a `resp::stream`
/// handle taken out of the VM before it is dropped.
enum Reply {
	Json(ClResult<(StatusCode, serde_json::Value)>),
	Stream(Source, Option<String>),
}

impl FromValue for Reply {
	fn from_value(value: Value) -> Result<Self, RuntimeError> {
		if let Ok(streamed) = value.borrow_ref::<Streamed>() {
			return Ok(match streamed.take() {
				Some((source, ct)) => Self::Stream(source, ct),
				None => Self::Json(Err(error::runtime("a resp::stream value was returned twice"))),
			});
		}
		Ok(Self::Json(to_json(&value).map(|json| {
			let marked = json
				.get(K_STATUS)
				.and_then(serde_json::Value::as_u64)
				.and_then(|s| u16::try_from(s).ok())
				.and_then(|s| StatusCode::from_u16(s).ok());
			match marked {
				Some(status) => (status, json.get(K_BODY).cloned().unwrap_or_default()),
				None => (StatusCode::OK, json),
			}
		})))
	}
}

/// `resp::` — the three non-200 response shapes. A bare returned value is 200.
mod resp {
	use super::{K_BODY, K_STATUS, ScriptError, Value, from_json, to_json};

	fn marked(status: u16, body: Option<&Value>) -> Result<Value, ScriptError> {
		let body = match body {
			Some(v) => to_json(v).map_err(ScriptError)?,
			None => serde_json::Value::Null,
		};
		from_json(&serde_json::json!({ K_STATUS: status, K_BODY: body })).map_err(ScriptError)
	}

	#[rune::function]
	pub fn json(status: u16, body: Value) -> Result<Value, ScriptError> {
		marked(status, Some(&body))
	}

	#[rune::function]
	pub fn created(body: Value) -> Result<Value, ScriptError> {
		marked(201, Some(&body))
	}

	#[rune::function]
	pub fn no_content() -> Result<Value, ScriptError> {
		marked(204, None)
	}
}

/// Registers the `app` handle, its route builder and the `resp::` module.
///
/// # Errors
/// Whatever Rune raises registering a type or a function.
pub fn modules() -> Result<Vec<Module>, ContextError> {
	let mut m = Module::new();
	m.ty::<Decl>()?;
	m.ty::<RouteBuilder>()?;
	m.function_meta(get)?;
	m.function_meta(post)?;
	m.function_meta(put)?;
	m.function_meta(patch)?;
	m.function_meta(delete)?;
	m.function_meta(public)?;
	m.function_meta(tier)?;
	m.function_meta(scope)?;
	m.function_meta(timeout_ms)?;
	m.function_meta(max_body)?;
	m.function_meta(raw)?;
	m.function_meta(mount)?;
	m.function_meta(feature)?;
	m.function_meta(setting_default)?;
	m.function_meta(setting_default_for)?;
	m.function_meta(test_default)?;
	m.function_meta(test_env)?;
	m.function_meta(secret)?;
	m.function_meta(setting)?;
	m.function_meta(object_type)?;
	m.function_meta(table)?;
	m.function_meta(migration)?;
	m.function_meta(entitlement)?;
	m.function_meta(member_role)?;
	m.function_meta(account_role)?;
	m.function_meta(consent)?;
	m.function_meta(offer)?;
	m.function_meta(crate::jobs::job)?;
	m.function_meta(crate::jobs::every)?;
	m.function_meta(crate::jobs::next)?;
	m.function_meta(crate::jobs::on_init)?;
	m.function_meta(crate::jobs::on_event)?;
	m.function_meta(crate::jobs::on_account_export)?;
	m.function_meta(crate::jobs::on_account_erase)?;
	m.function_meta(crate::jobs::on_org_deleted)?;
	#[cfg(feature = "ai")]
	m.function_meta(crate::agent::tool)?;

	let mut r = Module::with_item(["resp"])?;
	r.function_meta(resp::json)?;
	r.function_meta(resp::created)?;
	r.function_meta(resp::no_content)?;
	r.function_meta(crate::stream::stream)?;
	Ok(vec![m, r])
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_name_interns_to_one_static() {
		let a = intern("projects").unwrap();
		let b = intern("projects").unwrap();
		assert!(std::ptr::eq(a, b));
	}

	#[test]
	fn an_unknown_mount_does_not_resolve() {
		assert!(resolve("invoice.nope", &RouteGate::none()).is_none());
		for &name in MOUNTS {
			assert!(
				resolve(name, &RouteGate::none()).is_some(),
				"{name} is advertised in the error but does not resolve"
			);
		}
	}

	#[test]
	fn a_public_route_without_a_tier_fails_the_load() {
		let decls = Decls {
			routes: vec![RouteDecl {
				method: Method::GET,
				path: "/api/health".into(),
				entry: Hash::EMPTY,
				public: true,
				tier: None,
				scope: None,
				timeout_ms: None,
				max_body: None,
				raw: false,
			}],
			..Decls::default()
		};
		let script = Arc::new(
			Script::compile(
				&rune::Context::with_default_modules().unwrap(),
				&[("t".to_string(), "pub fn main() {}".to_string())],
				None,
			)
			.unwrap(),
		);
		let err = build(&script, &decls, &RouteGate::none()).map(|_| ()).unwrap_err();
		assert_eq!(err.parts().1, error::E_COMPILE);
	}
}

// vim: ts=4
