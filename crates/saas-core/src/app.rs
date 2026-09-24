//! `AppState` and `AppBuilder` — the wiring every consumer application goes through.
//!
//! ```ignore
//! let store = Arc::new(SqliteStore::open(&config).await?);
//! AppBuilder::new()
//!     .config(config)
//!     .store(store.clone() as Arc<dyn CoreStore>)
//!     .extension(store.clone() as Arc<dyn InvoiceStore>)
//!     .routes(Router::new().merge(saas_auth::routes::public()))
//!     .jobs(|runner, app| runner.register("SEND_EMAIL", move |job| send(app.clone(), job)))
//!     .run()
//!     .await
//! ```
//!
//! `saas-core` opens no pools and runs no SQL: [`AppBuilder::store`] takes an
//! `Arc<dyn CoreStore>` and the store adapter stays a leaf crate, so dependencies still
//! point inward only. Feature-crate stores keep arriving through [`AppBuilder::extension`];
//! only [`crate::store::CoreStore`] gets a field, because the middleware that runs on every
//! request cannot fall back on a runtime error for a store nobody registered.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use axum::Router;
use axum::http::Extensions;
use axum::response::IntoResponse;
use tokio::task::JoinHandle;

use crate::alert::Alert;
use crate::config::Config;
use crate::error::{ClResult, Error};
use crate::job;
use crate::ratelimit::RateLimiter;
use crate::secrets::SecretStore;
use crate::settings::{Registry, SettingDef, Settings};
use crate::store::CoreStore;
use crate::types::Timestamp;

type InitCallback =
	Box<dyn FnOnce(App) -> Pin<Box<dyn Future<Output = ClResult<()>> + Send>> + Send>;
type JobRegistrar = Box<dyn FnOnce(&mut job::Runner, App) -> ClResult<()> + Send>;
/// One feature crate's contribution to [`crate::alert::alerts`]. `Fn`, not `FnOnce`: a source
/// is re-evaluated on every sweep, which is what makes the pull model idempotent.
pub type AlertSource =
	Arc<dyn Fn(App) -> Pin<Box<dyn Future<Output = ClResult<Vec<Alert>>> + Send>> + Send + Sync>;

pub struct AppState {
	pub config: Config,
	/// Everything the framework itself persists. The reader/writer split lives inside the
	/// adapter now — it was two pool fields here, and handing `audit::log` the reader
	/// compiled and silently bypassed the single writer.
	pub store: Arc<dyn CoreStore>,
	pub settings: Settings,
	pub secrets: SecretStore,
	pub limits: RateLimiter,
	/// Consumer and feature-crate state, keyed by type.
	pub extensions: Extensions,
	/// Condition alerts the feature crates registered, read by [`crate::alert::alerts`].
	pub alert_sources: Vec<AlertSource>,
	/// Every scope prefix the mounted bundles registered, served by
	/// `GET /api/api-keys/scopes` for the mint UI.
	///
	/// **Not** the scope decision: a flat set cannot say which prefix a request path belongs
	/// to. `auth_mw` reads the `ScopePrefix` the bundle annotated itself with instead.
	pub route_scopes: BTreeSet<&'static str>,
	pub started_at: Timestamp,
	/// Set once, immediately after the runner's workers are spawned. Behind a `Mutex` so
	/// [`AppState::shutdown_jobs`] can take the handles out and await them — a `OnceLock` alone
	/// only lends them.
	jobs: OnceLock<Mutex<Vec<JoinHandle<()>>>>,
	/// [`job::Runner::stopper`], parked here for the same shutdown.
	stop_jobs: OnceLock<tokio::sync::watch::Sender<bool>>,
}

impl AppState {
	/// `/readyz`'s job check: **every** worker loop is still running. `true` during the window
	/// between building the state and spawning them, and for a `jobs.workers = 0` process.
	///
	/// `all`, not `any`: with `any`, three of four workers dying was a 4x capacity collapse that
	/// nothing reported.
	pub fn jobs_alive(&self) -> bool {
		self.jobs.get().is_none_or(|hs| hs.lock().iter().all(|h| !h.is_finished()))
	}

	/// Stop the job workers and wait for them to drain. [`AppBuilder::run`] calls it once the
	/// server has shut down; a consumer driving its own server calls it itself.
	///
	/// Each worker finishes the job it is holding and claims no further one. Unbounded on
	/// purpose — the orchestrator's own kill timer is the deadline, and that is one fewer knob.
	/// Idempotent, and a no-op for a `jobs.workers = 0` process.
	pub async fn shutdown_jobs(&self) {
		if let Some(stop) = self.stop_jobs.get() {
			// `send_replace`, not `send`: the latter is an error when no receiver is left, and a
			// worker that already exited is exactly the case this must not fail on.
			stop.send_replace(true);
		}
		let Some(handles) = self.jobs.get() else { return };
		// Taken out of the lock before the first await: `jobs_alive` runs on every `/readyz`.
		let handles: Vec<_> = std::mem::take(&mut *handles.lock());
		if handles.is_empty() {
			return;
		}
		tracing::info!(workers = handles.len(), "draining the job runner");
		for handle in handles {
			let _ = handle.await;
		}
	}
}

/// Cheap to clone — an `Arc<AppState>` with the state's fields reachable directly.
#[derive(Clone)]
pub struct App(Arc<AppState>);

impl Deref for App {
	type Target = AppState;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

/// A route bundle together with the scope prefixes it registered, so [`AppBuilder::routes`]
/// can stack them onto [`AppState::route_scopes`].
///
/// [`RouterScopeExt::scope`] **annotates; it does not layer**, and that is load-bearing. A layer
/// applied where bundles are merged sits *outside* every bundle's `require_auth` — axum's
/// last-applied layer is outermost, which is why [`crate::auth_mw::RouteGate::apply`] and
/// `consent_gated_router` both end by layering `require_auth` — so it would run before any `Ctx`
/// exists. The annotation parks an `axum::Extension` instead, which
/// [`crate::auth_mw::authenticate`] reads once `verify` has produced an
/// [`crate::ctx::Actor::Key`].
///
/// `.scope(…)` must sit **outside** whatever layer runs `auth_mw::authenticate`, or the
/// annotation is never seen and every key gets `403 E-AUTH-SCOPE`.
pub struct Scoped {
	router: Router<App>,
	scopes: BTreeSet<&'static str>,
}

impl Scoped {
	/// Merges another bundle's routes **and** unions its registered prefixes. Takes
	/// `impl Into<Scoped>` so a bare `Router<App>` merges into a scoped bundle unchanged.
	#[must_use]
	pub fn merge(mut self, other: impl Into<Scoped>) -> Self {
		let other = other.into();
		self.router = self.router.merge(other.router);
		self.scopes.extend(other.scopes);
		self
	}

	/// Applies `f` to the bundle's router — the way to reach a `Router` method
	/// (`.fallback_service(spa())`) without dropping the registered prefixes. Converting with
	/// `From<Scoped> for Router<App>` does drop them.
	#[must_use]
	pub fn with(mut self, f: impl FnOnce(Router<App>) -> Router<App>) -> Self {
		self.router = f(self.router);
		self
	}
}

impl From<Router<App>> for Scoped {
	fn from(router: Router<App>) -> Self {
		Self { router, scopes: BTreeSet::new() }
	}
}

/// The way back out, for a consumer that still has to apply a `Router` method at the point it
/// hands the bundle to [`AppBuilder::routes`]. Prefer [`Scoped::with`], which keeps the prefixes.
impl From<Scoped> for Router<App> {
	fn from(scoped: Scoped) -> Self {
		if !scoped.scopes.is_empty() {
			tracing::error!(
				"a scoped bundle was converted to a bare `Router`; its scope prefixes are lost, \
				 so the scope listing is empty and every mint is refused — use `Scoped::with`"
			);
		}
		scoped.router
	}
}

/// See [`Scoped`] for why `scope` is an annotation and not a middleware, and why it must be
/// applied **after** the layer that runs `auth_mw::authenticate`.
pub trait RouterScopeExt {
	/// Registers `prefix` for this bundle: an API key may call it only with
	/// `<prefix>:<read|write>` in its scope set, the verb derived from the HTTP method.
	fn scope(self, prefix: &'static str) -> Scoped;
}

impl RouterScopeExt for Router<App> {
	fn scope(self, prefix: &'static str) -> Scoped {
		Scoped {
			router: self.layer(axum::Extension(crate::auth_mw::ScopePrefix(prefix))),
			scopes: BTreeSet::from([prefix]),
		}
	}
}

#[derive(Default)]
pub struct AppBuilder {
	config: Option<Config>,
	store: Option<Arc<dyn CoreStore>>,
	routes: Option<Router<App>>,
	scopes: BTreeSet<&'static str>,
	extensions: Extensions,
	jobs: Vec<JobRegistrar>,
	alert_sources: Vec<AlertSource>,
	on_init: Vec<InitCallback>,
	settings: Vec<&'static [SettingDef]>,
	setting_defaults: Vec<(&'static str, &'static str)>,
	secrets: Vec<&'static [&'static str]>,
}

impl AppBuilder {
	/// Installs the tracing subscriber as a side effect; calling it twice is harmless.
	pub fn new() -> Self {
		crate::log::init();
		Self::default()
	}

	/// Defaults to [`Config::from_env`] when not set.
	pub fn config(mut self, config: Config) -> Self {
		self.config = Some(config);
		self
	}

	/// The store adapter. Required. Migrations must already have been applied to it.
	pub fn store(mut self, store: Arc<dyn CoreStore>) -> Self {
		self.store = Some(store);
		self
	}

	/// The assembled router — the consumer merges whichever route bundles it wants. A bundle
	/// that registered scope prefixes arrives as [`Scoped`]; a bare `Router<App>` still
	/// compiles and registers nothing.
	///
	/// `saas-core`'s own probes are added by [`AppBuilder::run`].
	pub fn routes(mut self, routes: impl Into<Scoped>) -> Self {
		let scoped = routes.into();
		// Fail-closed turns a forgotten `.scope(…)` into a 403 for every key and 200 for every
		// browser, so without this notice the omission is silent until a CI job breaks.
		if scoped.scopes.is_empty() {
			tracing::warn!(
				"a route bundle registered no scope prefix; no API key is accepted on it, and it \
				 adds nothing to the scope listing `GET /api/api-keys/scopes`"
			);
		}
		self.scopes.extend(scoped.scopes);
		self.routes = Some(match self.routes {
			Some(existing) => existing.merge(scoped.router),
			None => scoped.router,
		});
		self
	}

	/// Parks state of any type in `AppState`, read back with
	/// `app.extensions.get::<T>()`.
	pub fn extension<T: Clone + Send + Sync + 'static>(mut self, val: T) -> Self {
		self.extensions.insert(val);
		self
	}

	/// Declares a crate's settings: `.settings(saas_nav::SETTINGS)`. `saas-core`'s own are
	/// always registered, and registering a slice is what opts this deployment into being
	/// asked for that crate's configuration at boot — a consumer that never registers
	/// `saas_nav::SETTINGS` is never asked for NAV settings.
	#[must_use]
	pub fn settings(mut self, defs: &'static [SettingDef]) -> Self {
		self.settings.push(defs);
		self
	}

	/// A value for an already-declared key, below the environment and above the registry
	/// default. For a constant the application compiles in — `nav.software_id` — rather than a
	/// row, which would sit above everything and shadow the environment forever.
	#[must_use]
	pub fn setting_default(mut self, key: &'static str, value: &'static str) -> Self {
		self.setting_defaults.push((key, value));
		self
	}

	/// Declares a crate's secret key names: `.secrets(saas_nav::SECRETS)`. Names only — a
	/// secret has no type, range or default — so this buys collision detection against the
	/// settings namespace and the key list `PUT /api/admin/secrets/{key}` needs.
	#[must_use]
	pub fn secrets(mut self, keys: &'static [&'static str]) -> Self {
		self.secrets.push(keys);
		self
	}

	/// Registers job handlers. Runs after `on_init`, just before the runner is spawned;
	/// the `App` handed in is the live one, so a handler can capture and clone it.
	pub fn jobs(self, f: impl FnOnce(&mut job::Runner, App) + Send + 'static) -> Self {
		self.try_jobs(move |runner, app| {
			f(runner, app);
			Ok(())
		})
	}

	/// [`Self::jobs`] for a registrar that can refuse; its error fails the boot.
	pub fn try_jobs(
		mut self,
		f: impl FnOnce(&mut job::Runner, App) -> ClResult<()> + Send + 'static,
	) -> Self {
		self.jobs.push(Box::new(f));
		self
	}

	/// Registers a source of condition alerts, the same reflex as [`AppBuilder::jobs`]:
	/// `.alerts(saas_nav::service_api::alerts)`. Called on every sweep, so it must only read.
	pub fn alerts<F, Fut>(mut self, f: F) -> Self
	where
		F: Fn(App) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = ClResult<Vec<Alert>>> + Send + 'static,
	{
		self.alert_sources.push(Arc::new(move |app| Box::pin(f(app))));
		self
	}

	/// Runs once `AppState` exists, before the job runner starts.
	pub fn on_init<F, Fut>(mut self, f: F) -> Self
	where
		F: FnOnce(App) -> Fut + Send + 'static,
		Fut: Future<Output = ClResult<()>> + Send + 'static,
	{
		self.on_init.push(Box::new(move |app| Box::pin(f(app))));
		self
	}

	/// Builds the state and starts the job runner — everything [`AppBuilder::run`] does
	/// except bind a port. Integration tests use it to get a live `App` without serving;
	/// `run` is this plus the router.
	pub async fn build(mut self) -> ClResult<App> {
		let config = match self.config.take() {
			Some(c) => c,
			None => Config::from_env(),
		};
		// Nothing else creates it, and `alert::A-DISK-LOW` only ever stats it — a missing
		// directory silently disabled the disk check instead of failing the boot.
		std::fs::create_dir_all(&config.data_dir).map_err(|e| {
			Error::internal(format!("cannot create data dir '{}': {e}", config.data_dir))
		})?;

		let store = self
			.store
			.take()
			.ok_or_else(|| Error::internal("AppBuilder::store() was not called"))?;

		// Composed once, before anything can read a setting, and never changed after. Every
		// problem at once: an operator fixing configuration wants one restart, not four.
		let mut slices = vec![crate::settings::SETTINGS];
		slices.append(&mut self.settings);
		let secrets: Vec<&'static str> = crate::secrets::SECRETS
			.iter()
			.copied()
			.chain(self.secrets.iter().flat_map(|s| s.iter().copied()))
			.collect();
		let (registry, errors) = Registry::build(&slices, &self.setting_defaults, &secrets);
		if !errors.is_empty() {
			return Err(Error::internal(format!("configuration registry: {}", errors.join("; "))));
		}
		let registry = Arc::new(registry);

		let app = App(Arc::new(AppState {
			settings: Settings::new(Arc::clone(&store), Arc::clone(&registry)),
			secrets: SecretStore::new(Arc::clone(&store), config.master_key, Arc::clone(&registry)),
			limits: RateLimiter::new(),
			extensions: std::mem::take(&mut self.extensions),
			alert_sources: std::mem::take(&mut self.alert_sources),
			route_scopes: std::mem::take(&mut self.scopes),
			started_at: Timestamp::now(),
			jobs: OnceLock::new(),
			stop_jobs: OnceLock::new(),
			config,
			store,
		}));

		for cb in self.on_init.drain(..) {
			cb(app.clone()).await?;
		}

		// After `on_init`, so a seeded row counts, and over the whole composed registry: the
		// application used to have to remember `saas_email::check_email_settings` and friends
		// by hand, and forgetting one was silent until a job handler hit it.
		app.settings.check_required("").await?;

		// `jobs.workers` is a setting and cannot differ between two processes sharing a database;
		// `JOBS_WORKERS` can. `0` skips `reclaim` too, because it flips *every* `RUNNING` row
		// back and cannot tell a crashed worker's from a live sibling process's.
		let workers = match app.config.jobs_workers {
			Some(n) => n,
			None => app.settings.int("jobs.workers").await?,
		};
		if workers > 0 {
			let mut runner =
				job::Runner::with_registry(Arc::clone(&app.store), Arc::clone(&registry));
			// Registered here rather than left to the consumer: `jobs` is this crate's table and
			// a deployment that forgot to wire the sweep up grew a row per job forever.
			{
				let app = app.clone();
				runner.register_periodic(job::KIND_SWEEP, 86_400, move |_job| {
					let app = app.clone();
					async move {
						let days = app.settings.int("jobs.retention_days").await?;
						// Total arithmetic, not just a bounded setting: under release
						// `overflow-checks` a trapping multiply here is a panic inside a job
						// handler, and the retention chain dies with it.
						let cutoff = Timestamp(
							Timestamp::now().0.saturating_sub(days.saturating_mul(86_400)),
						);
						let gone = app.store.job_sweep(cutoff).await?;
						tracing::info!(gone, "swept finished jobs");
						Ok(())
					}
				});
			}
			// A one-minute tick rather than `admin.alert_interval_minutes`, because
			// `register_periodic` fixes the period at boot; `alert::sweep` gates on the setting
			// itself, so the interval changes without a restart.
			{
				let app = app.clone();
				runner.register_periodic(job::KIND_ALERT_SWEEP, 60, move |_job| {
					let app = app.clone();
					async move {
						// Logged, never returned: an `Error::internal` here is `Retry::Never`,
						// which terminates the row on attempt one and leaves the periodic chain
						// dead. A database blip must not cost the deployment its alerting.
						if let Err(e) = crate::alert::sweep(&app).await {
							tracing::error!(error = %e, "alert sweep failed");
						}
						Ok(())
					}
				});
			}
			for register in self.jobs.drain(..) {
				register(&mut runner, app.clone())?;
			}
			// Before the seeds, and fatal: `seed_periodic` counts `RUNNING` as live, so a
			// crash-left row was neither reclaimed nor re-seeded and the chain stayed dead with
			// `/readyz` green.
			let reclaimed = runner.reclaim().await?;
			if reclaimed > 0 {
				tracing::warn!(jobs = reclaimed, "reclaimed jobs interrupted by a restart");
			}
			// Seeded here for the same reason it is registered here — and after the registrars, so
			// a consumer that overrode the kind gets its own handler on the seeded row.
			job::seed_periodic(&app.store, job::KIND_SWEEP).await?;
			job::seed_periodic(&app.store, job::KIND_ALERT_SWEEP).await?;
			// Several workers over one runner, because each processes one job at a time and a NAV
			// sweep would hold up every `SEND_EMAIL` behind it. The claim is a single
			// `UPDATE … RETURNING`, so a row still goes to exactly one worker. Parked before the
			// spawn so a racing `shutdown_jobs` still stops them.
			app.stop_jobs
				.set(runner.stopper())
				.map_err(|_| Error::internal("the job stopper was already set"))?;
			let runner = std::sync::Arc::new(runner);
			let handles: Vec<_> = (0..workers)
				.map(|i| tokio::spawn(std::sync::Arc::clone(&runner).run(i)))
				.collect();
			// Not discarded: a failed `set` drops the handles, which detaches every worker from
			// supervision — and `jobs_alive` reads `is_none_or`, so an unset cell reports
			// `/readyz` healthy forever.
			app.jobs
				.set(Mutex::new(handles))
				.map_err(|_| Error::internal("the job handles were already set"))?;
		}

		Ok(app)
	}

	/// Builds the state and composes the router production serves, without binding a port.
	///
	/// [`run`](Self::run) is this plus the listener, so a test harness drives the same
	/// middleware stack — rate limit included — that a request meets in production.
	pub async fn into_service(mut self) -> ClResult<(App, axum::Router)> {
		let routes = self.routes.take();
		let app = self.build().await?;

		let router = routes
			.unwrap_or_default()
			.merge(crate::health::public())
			// Applied outermost-last so a rejected token is still logged under its own request
			// id and a public route still gets a resolved `ClientIp`. Authentication is **not**
			// layered here — a route bundle declares it. What stays global is the state those
			// bundle middlewares read: a `Router<App>` is built before any `App` exists, so
			// `from_fn_with_state` is unavailable to a bundle.
			.layer(axum::Extension(app.clone()))
			// The outermost of three rate-limit tiers and the only global one; the others live
			// inside `auth_mw::require_auth`. **Outside authentication**, so the budget is
			// charged before `verify` runs its two queries — inside it, one valid token drove
			// the reader pool at line rate and still paid those queries for its eventual 429.
			.layer(axum::middleware::from_fn_with_state(
				app.clone(),
				crate::ratelimit::default_mw,
			))
			// Must stay outside `default_mw`, which keys on `ClientIp`.
			.layer(axum::middleware::from_fn_with_state(
				app.clone(),
				crate::auth_mw::client_ip_mw,
			))
			.layer(axum::middleware::from_fn(crate::log::request_id_mw))
			// Outermost. `overflow-checks = true` in release turns an out-of-envelope money
			// computation into a panic on purpose (`crate::money`), and without this a panic
			// is a dropped connection rather than a `500` in the error envelope.
			.layer(tower_http::catch_panic::CatchPanicLayer::custom(panic_response))
			.with_state(app.clone());

		Ok((app, router))
	}

	/// Builds the state, starts the job runner and serves until the process ends.
	pub async fn run(self) -> ClResult<()> {
		let (app, router) = self.into_service().await?;

		let listen = app.config.listen.clone();
		let listener = tokio::net::TcpListener::bind(&listen)
			.await
			.map_err(|e| Error::internal(format!("cannot bind {listen}: {e}")))?;
		tracing::info!("listening on {listen}");

		axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
			.with_graceful_shutdown(shutdown_signal())
			.await
			.map_err(|e| Error::internal(format!("server stopped: {e}")))?;

		// After the listener, not beside it: a request still in flight may enqueue a job, and
		// draining the runner first would leave it for the next boot.
		app.shutdown_jobs().await;
		Ok(())
	}
}

/// Resolves on `SIGTERM` or Ctrl-C — the two ways an orchestrator asks this process to stop.
/// Without it every rolling deploy was a crash: in-flight handlers died and re-ran from the
/// top at the next boot's [`job::Runner::reclaim`].
async fn shutdown_signal() {
	let ctrl_c = async {
		// A failed registration must not resolve the future, or the server would shut down the
		// moment it started.
		match tokio::signal::ctrl_c().await {
			Ok(()) => {}
			Err(e) => {
				tracing::error!(error = %e, "cannot listen for Ctrl-C");
				std::future::pending::<()>().await;
			}
		}
	};
	#[cfg(unix)]
	let terminate = async {
		match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
			Ok(mut sig) => {
				sig.recv().await;
			}
			Err(e) => {
				tracing::error!(error = %e, "cannot listen for SIGTERM");
				std::future::pending::<()>().await;
			}
		}
	};
	#[cfg(not(unix))]
	let terminate = std::future::pending::<()>();

	tokio::select! {
		() = ctrl_c => {}
		() = terminate => {}
	}
	tracing::info!("shutdown signal received; draining");
}

/// A caught panic, as the one error envelope: `500` `E-CORE-INTERNAL`.
///
/// [`Error::Internal`]'s `IntoResponse` logs the detail and returns a generic `errStr`, which
/// is exactly the 5xx rule — so the panic payload is written to the log and never to the
/// client.
pub fn panic_response(
	panic: Box<dyn std::any::Any + Send + 'static>,
) -> axum::response::Response<axum::body::Body> {
	let detail = panic
		.downcast::<&'static str>()
		.map(|s| (*s).to_owned())
		.or_else(|p| p.downcast::<String>().map(|s| *s))
		.unwrap_or_else(|_| "unknown panic payload".to_owned());
	Error::internal(format!("handler panicked: {detail}")).into_response()
}

#[cfg(test)]
mod tests {
	use axum::{body::Body, http::Request, routing::get};
	use http_body_util::BodyExt;
	use tower::ServiceExt;

	use super::*;

	/// `overflow-checks = true` makes an out-of-envelope money computation panic on purpose,
	/// so a panic has to answer as a `500` in the envelope rather than drop the connection.
	#[tokio::test]
	async fn a_panicking_handler_answers_with_the_envelope() {
		#[allow(clippy::unnecessary_wraps)]
		async fn boom() -> &'static str {
			panic!("money overflowed")
		}
		let app = Router::new()
			.route("/", get(boom))
			.layer(tower_http::catch_panic::CatchPanicLayer::custom(panic_response));

		let res = app.oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(res.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
		let bytes = res.into_body().collect().await.unwrap().to_bytes();
		let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(body["error"]["errCode"], "E-CORE-INTERNAL");
		// The panic message is logged, never returned.
		assert_eq!(body["error"]["errStr"], "internal error");
	}
}

// vim: ts=4
