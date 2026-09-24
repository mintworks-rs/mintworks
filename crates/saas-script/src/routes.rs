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
};

use axum::{
	Json, Router,
	body::Bytes,
	extract::{Query, RawPathParams, State},
	http::{HeaderMap, Method, StatusCode, Uri, header},
	response::{IntoResponse, Response},
	routing::MethodFilter,
};
use rune::{
	Any, ContextError, Hash, Module, Value,
	runtime::{FromValue, Function, RuntimeError, ToValue},
};
use saas_core::{
	App, Ctx,
	app::{RouterScopeExt, Scoped},
	auth_mw::RouteGate,
	error::{ClResult, Error},
	ratelimit,
};

use crate::{
	ScriptCtx,
	db::TableDef,
	error,
	jobs::JobDecl,
	objects::ObjectTypeDef,
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
}

/// Everything `main(app)` declared, read back by [`crate::ScriptApp`] once it returns.
#[derive(Default)]
pub struct Decls {
	pub mounts: Vec<String>,
	pub features: BTreeSet<String>,
	pub types: Vec<ObjectTypeDef>,
	/// What `app.table(…)` declared, for `ScriptDb::reconcile`.
	pub tables: Vec<TableDef>,
	pub routes: Vec<RouteDecl>,
	pub jobs: Vec<JobDecl>,
	pub init: Vec<Hash>,
	/// A declaration error cannot be returned from a chained builder call without changing the
	/// script-side spelling, so it is collected here and fails the load afterwards.
	pub errors: Vec<String>,
}

impl Decls {
	fn fail(&mut self, msg: impl Into<String>) {
		self.errors.push(msg.into());
	}
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

	/// The declarations, taken once `main` has returned.
	#[must_use]
	pub fn take(&self) -> Decls {
		std::mem::take(&mut *lock(&self.0))
	}

	pub(crate) fn push_job(&self, decl: JobDecl) {
		let mut d = lock(&self.0);
		if d.jobs.iter().any(|j| j.kind == decl.kind) {
			d.fail(format!("job kind '{}' is declared twice", decl.kind));
		}
		d.jobs.push(decl);
	}

	pub(crate) fn push_init(&self, entry: Hash) {
		lock(&self.0).init.push(entry);
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

/// A mount implies its feature: otherwise mounting `invoice.org_read` without
/// `app.feature("invoice")` boots with no `Arc<dyn InvoiceStore>` extension.
#[rune::function(instance)]
fn mount(this: &Decl, name: String) {
	let mut d = lock(&this.0);
	let feat = name.split_once('.').map_or(name.as_str(), |(f, _)| f);
	if FEATURES.contains(&feat) {
		d.features.insert(feat.to_owned());
	}
	d.mounts.push(name);
}

/// The crates whose settings, secrets, jobs and alerts an app opts into. `auth` and `email` are
/// always on, so naming one is a declaration error rather than a no-op.
const FEATURES: &[&str] = &["invoice", "nav", "billing"];

#[rune::function(instance)]
fn feature(this: &Decl, name: String) {
	let mut d = lock(&this.0);
	if !FEATURES.contains(&name.as_str()) {
		d.fail(format!("unknown feature '{name}'; one of {FEATURES:?}"));
		return;
	}
	d.features.insert(name);
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

/// The names `resolve` accepts, for the unknown-mount error. The test walks this, so a name that
/// stopped resolving cannot stay advertised here.
const MOUNTS: &[&str] = &[
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
];

/// Resolves a mount name to the bundle it names, with its gate and its API-key scope prefix
/// already applied; the names are the Rust symbols.
///
/// A `match` rather than a name → `fn` table: the bundles have two shapes (bare and
/// `fn(&RouteGate)`), which no single fn-pointer type expresses.
fn resolve(name: &str, gate: &RouteGate) -> Option<Scoped> {
	Some(match name {
		// The two auth bundles stay unscoped deliberately: fail-closed for an
		// `Actor::Key`, which keeps a leaked API key out of the routes that mint other keys.
		"auth.public" => saas_auth::routes::public().into(),
		"auth.authenticated" => saas_auth::routes::authenticated().into(),
		"auth.operator" => saas_auth::routes::operator().into(),
		"invoice.org_read" => saas_invoice::routes::org_read(gate).scope("invoice"),
		"invoice.org_parties" => saas_invoice::routes::org_parties(gate).scope("invoice"),
		"invoice.org_services" => saas_invoice::routes::org_services(gate).scope("invoice"),
		"invoice.org_invoices" => saas_invoice::routes::org_invoices(gate).scope("invoice"),
		// Unscoped, like `auth.*`: an API key must not mint a seller or set NAV credentials.
		"invoice.org_seller" => saas_invoice::routes::org_seller(gate).into(),
		"nav.org_credentials" => saas_nav::routes::org_credentials(gate).into(),
		"billing.public" => saas_billing::routes::public().scope("billing"),
		"billing.org" => saas_billing::routes::org(gate).scope("billing"),
		"billing.operator" => saas_billing::routes::operator(gate).scope("billing"),
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
			gate.apply(one.layer(axum::middleware::from_fn(saas_core::auth_mw::require_auth)))
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
	let (script, entry) = (Arc::clone(script), r.entry);

	let handler = axum::routing::on(
		filter,
		move |State(app): State<App>,
		      ctx: Option<Ctx>,
		      params: RawPathParams,
		      Query(query): Query<BTreeMap<String, String>>,
		      uri: Uri,
		      method: Method,
		      headers: HeaderMap,
		      body: Bytes| {
			let script = Arc::clone(&script);
			async move {
				let (q, u, m) = (&query, &uri, &method);
				dispatch(&script, entry, app, ctx, &params, q, u, m, &headers, &body).await
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

#[allow(clippy::too_many_arguments)]
async fn dispatch(
	script: &Script,
	entry: Hash,
	app: App,
	ctx: Option<Ctx>,
	params: &RawPathParams,
	query: &BTreeMap<String, String>,
	uri: &Uri,
	method: &Method,
	headers: &HeaderMap,
	body: &Bytes,
) -> ClResult<Response> {
	let path: serde_json::Map<String, serde_json::Value> = params
		.iter()
		.map(|(k, v)| (k.to_string(), serde_json::Value::from(v)))
		.collect();
	let parsed = if body.is_empty() {
		serde_json::Value::Null
	} else if is_json(headers) {
		serde_json::from_slice(body).map_err(|e| Error::validation(format!("body: {e}")))?
	} else {
		return Err(Error::Unsupported("expected Content-Type: application/json".into()));
	};

	let req = serde_json::json!({
		"path": path,
		"query": query,
		"body": parsed,
		"method": method.as_str(),
		"path_str": uri.path(),
	});
	// A `.public()` route has no token, and `Ctx`'s required extractor treats that as an error.
	let ctx = ctx.unwrap_or_else(|| Ctx::public("script"));
	let reply: Reply = script.invoke(entry, (ScriptCtx::new(app, ctx), Arg(req))).await?;
	let (status, body) = reply.0?;
	Ok(if status == StatusCode::NO_CONTENT {
		status.into_response()
	} else {
		(status, Json(body)).into_response()
	})
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

/// A handler's return value, bridged the same way on the way back.
struct Reply(ClResult<(StatusCode, serde_json::Value)>);

impl FromValue for Reply {
	fn from_value(value: Value) -> Result<Self, RuntimeError> {
		Ok(Self(to_json(&value).map(|json| {
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
	m.function_meta(mount)?;
	m.function_meta(feature)?;
	m.function_meta(object_type)?;
	m.function_meta(table)?;
	m.function_meta(crate::jobs::job)?;
	m.function_meta(crate::jobs::every)?;
	m.function_meta(crate::jobs::next)?;
	m.function_meta(crate::jobs::on_init)?;

	let mut r = Module::with_item(["resp"])?;
	r.function_meta(resp::json)?;
	r.function_meta(resp::created)?;
	r.function_meta(resp::no_content)?;
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
			}],
			..Decls::default()
		};
		let script = Arc::new(
			Script::compile(
				&rune::Context::with_default_modules().unwrap(),
				&[("t".to_string(), "pub fn main() {}".to_string())],
				crate::Limits::default(),
				None,
			)
			.unwrap(),
		);
		let err = build(&script, &decls, &RouteGate::none()).map(|_| ()).unwrap_err();
		assert_eq!(err.parts().1, error::E_COMPILE);
	}
}

// vim: ts=4
