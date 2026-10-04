//! Rune embedded over the framework's service handles.
//!
//! A bundle compiles once into a shared `Unit`; every invocation runs on a fresh `Vm` under an
//! instruction budget and a wall-clock deadline. What a script can reach is exactly what the
//! host registered into the `Context` it was compiled against — this crate grants no ambient
//! capability, and a script can never mint a `Ctx`.
#![forbid(unsafe_code)]
// Rune's binding macros fix these three shapes: a host fn takes its arguments by value, an
// instance method takes `&self` however small the type, and an inner `mod` pulls in the
// file's imports wholesale.
#![allow(
	clippy::needless_pass_by_value,
	clippy::trivially_copy_pass_by_ref,
	clippy::wildcard_imports
)]

#[cfg(feature = "ai")]
pub mod agent;
pub mod api;
pub mod ctx;
pub mod db;
pub mod entitle;
pub mod error;
pub mod io;
pub mod jobs;
#[cfg(feature = "ai")]
pub mod llm;
#[cfg(feature = "ai")]
pub mod memory;
pub mod money;
pub mod objects;
pub mod pdf;
pub mod plans;
pub mod refs;
pub mod routes;
#[cfg(feature = "ai")]
pub mod search;
#[cfg(feature = "ai")]
pub mod skills;
pub mod sys;
pub mod testing;
pub mod tx;
pub mod value;
pub mod vm;

use std::{collections::BTreeSet, pin::Pin, sync::Arc};

use async_trait::async_trait;
use rune::Context;
use saas_core::{
	App, AppBuilder,
	auth_mw::RouteGate,
	error::ClResult,
	objects::{ObjectStore, ObjectType},
	settings::SettingDef,
};

pub use api::modules as api_modules;
pub use ctx::ScriptCtx;
pub use db::{ColDef, ColType, TableDef};
pub use error::{E_BUDGET, E_COMPILE, E_DB, E_RUNTIME, E_TIMEOUT};
pub use io::IoProfile;
pub use money::{Money, Qty};
pub use objects::ObjectTypeDef;
pub use tx::{E_TX_REMOTE, E_TX_TIMEOUT};
pub use value::{ScriptError, from_json, to_json};
pub use vm::{Limits, Script};

/// The runtime's declared keys, registered through `AppBuilder::settings`.
pub static SETTINGS: &[SettingDef] = &[
	SettingDef::int(
		"script.budget",
		"10000000",
		"Rune instructions one script invocation may execute before E-SCRIPT-BUDGET.",
	)
	.range(1_000, 1_000_000_000),
	SettingDef::int(
		"script.timeout_ms",
		"5000",
		"Wall-clock milliseconds one script invocation may run before E-SCRIPT-TIMEOUT.",
	)
	.range(100, 600_000),
	SettingDef::int(
		"script.tx_timeout_ms",
		"2000",
		"Wall-clock milliseconds a tx::with block may run before E-SCRIPT-TX-TIMEOUT.",
	)
	// Far shorter than `script.timeout_ms`, and bounded above by it in spirit: the block holds
	// the process's only writer connection, so every other write waits out this number.
	.range(50, 60_000),
	SettingDef::int(
		"script.db_max_rows",
		"10000",
		"Rows one db::query may return; more is E-SCRIPT-DB, add LIMIT.",
	)
	.range(1, 1_000_000),
];

/// A script bundle, loaded onto an `AppBuilder`.
///
/// One call compiles the sources, runs `main(app)` once to collect its declarations, and turns
/// them into a route bundle, job handlers and init hooks. `main` only declares; every name it
/// uses is resolved here, so an unknown mount or a duplicate route refuses to serve rather than
/// failing on the request that reaches it.
pub struct ScriptApp {
	sources: Vec<(String, String)>,
	objects: Arc<dyn ObjectStore>,
	limits: Limits,
	tx: Option<Arc<dyn TxHook>>,
	db: Option<Arc<dyn AppDb>>,
	gate: RouteGate,
	io: IoProfile,
}

impl ScriptApp {
	/// `sources` are `(name, text)` pairs — every `.rn` in the application directory. `root` is a
	/// **separate** directory the caller chooses, and is what the default [`IoProfile`] confines
	/// `fs` to: rooting it at the application directory would put its `.env` in a script's reach.
	#[must_use]
	pub fn new(
		sources: Vec<(String, String)>,
		objects: Arc<dyn ObjectStore>,
		root: std::path::PathBuf,
	) -> Self {
		Self {
			sources,
			objects,
			limits: Limits::default(),
			tx: None,
			db: None,
			gate: saas_auth::routes::consent_gate(),
			io: IoProfile::app(root),
		}
	}

	/// Overrides the compiled-in defaults. `Limits::load` needs a live `Settings`, which does not
	/// exist until `AppBuilder::build`, so the bundle's bounds are supplied rather than read.
	#[must_use]
	pub fn limits(mut self, limits: Limits) -> Self {
		self.limits = limits;
		self
	}

	#[must_use]
	pub fn tx_hook(mut self, hook: Arc<dyn TxHook>) -> Self {
		self.tx = Some(hook);
		self
	}

	/// Registering one is what puts `db::` in the bundle's reach at all — see
	/// [`context`](Self::context).
	#[must_use]
	pub fn app_db(mut self, db: Arc<dyn AppDb>) -> Self {
		self.db = Some(db);
		self
	}

	/// `RouteGate::none()` for a deployment that publishes no legal documents; the default is
	/// `saas_auth::routes::consent_gate()`.
	///
	/// `none()` drops the consent check but not authentication: each non-public script route
	/// carries its own `require_auth`, so only a `.public()` route sees `Ctx::public("script")`.
	#[must_use]
	pub fn gate(mut self, gate: RouteGate) -> Self {
		self.gate = gate;
		self
	}

	/// `IoProfile::sandboxed()` registers no `fs`, `http` or `env` — the shape a future
	/// org-level script profile takes.
	#[must_use]
	pub fn io(mut self, io: IoProfile) -> Self {
		self.io = io;
		self
	}

	/// Compiles the bundle and applies everything `main(app)` declared to `builder`.
	///
	/// `features` receives the names from `app.feature(…)`: registering a feature crate's
	/// settings, secrets, jobs and alerts means naming those crates' symbols, which belongs to
	/// the composition root rather than here. `|b, _| Ok(b)` opts out.
	///
	/// # Errors
	/// `E-SCRIPT-COMPILE` for a source that does not compile, a declaration error `main` left
	/// behind, or an unknown mount name; whatever `main` itself raises.
	pub async fn install<F>(self, builder: AppBuilder, features: F) -> ClResult<AppBuilder>
	where
		F: FnOnce(AppBuilder, &BTreeSet<String>) -> ClResult<AppBuilder>,
	{
		Ok(self.install_visited(builder, features, None).await?.0)
	}

	/// [`install`](Self::install) with `#[test]` discovery and the `test::` module switched on.
	///
	/// `tests` being `Some` is the *only* thing that puts the in-process client in a bundle's
	/// reach, so a served application can never call it.
	///
	/// # Errors
	/// As [`install`](Self::install).
	pub(crate) async fn install_visited<F>(
		self,
		builder: AppBuilder,
		features: F,
		tests: Option<&mut testing::TestVisitor>,
	) -> ClResult<(AppBuilder, Arc<Script>)>
	where
		F: FnOnce(AppBuilder, &BTreeSet<String>) -> ClResult<AppBuilder>,
	{
		let suite = tests.is_some();
		let mut context = self.context()?;
		if suite {
			context
				.install(testing::module().map_err(|e| error::compile(e.to_string()))?)
				.map_err(|e| error::compile(e.to_string()))?;
		}
		let script = Arc::new(Script::compile(
			&context,
			&self.sources,
			self.limits,
			tests.map(|v| v as &mut dyn rune::compile::CompileVisitor),
		)?);

		let decl = routes::Decl::new();
		// The declaration pass. `rune::Value` is not `Send`, but this runs once at boot on the
		// thread that builds the app, so the non-`Send` future never reaches a task.
		let _: rune::Value = script.invoke(["main"], (decl.clone(),)).await?;
		let decls = decl.take();
		if !decls.errors.is_empty() {
			return Err(error::compile(decls.errors.join("; ")));
		}

		let mut runtime = ScriptRuntime::new(Arc::clone(&self.objects), decls.types.clone());
		if let Some(hook) = self.tx {
			runtime = runtime.with_tx_hook(hook);
		}
		if let Some(db) = self.db {
			runtime = runtime.with_app_db(db, decls.tables.clone());
		}
		let scoped = routes::build(&script, &decls, &self.gate)?;
		// Here as well as in the `try_jobs` closure: with `jobs.workers = 0` that closure never
		// runs, and a kind that will not intern must still fail the boot.
		for job in &decls.jobs {
			routes::intern(&job.kind)?;
		}

		let mut builder = features(builder, &decls.features)?;
		// Here, not in the `features` closure: that one sees the feature names, not `decls`.
		if decls.features.contains("entitle") {
			builder = saas_entitle::install(builder, decls.entitlements.clone());
		}
		for (env, key, value) in &decls.setting_defaults {
			let (key, value) = (routes::intern(key)?, routes::intern(value)?);
			builder = match env {
				Some(env) => builder.setting_default_for(routes::intern(env)?, key, value),
				None => builder.setting_default(key, value),
			};
		}
		if suite {
			for (key, value) in &decls.test_defaults {
				let (key, value) = (routes::intern(key)?, routes::intern(value)?);
				builder = builder.setting_default_for("test", key, value);
			}
		}
		let hook_app = Arc::new(std::sync::OnceLock::new());
		if decls.account_export.is_some() || decls.account_erase.is_some() {
			builder = builder.account_data_hook(Arc::new(jobs::AccountHooks {
				script: Arc::clone(&script),
				export: decls.account_export,
				erase: decls.account_erase,
				app: Arc::clone(&hook_app),
			}));
		}
		#[cfg(feature = "ai")]
		{
			let rune = agent::RuneTools::new(&script, &decls.tools, &hook_app);
			// `saas_agent::Agent` reads `Tools`; it cannot name `RuneTools`, this crate depends on it.
			let mut tools = saas_agent::Tools::default();
			rune.0.iter().for_each(|t| tools.add(Arc::clone(t)));
			builder = builder.extension(rune).extension(tools);
		}
		let decls = Arc::new(decls);
		let (events_script, events_decls) = (Arc::clone(&script), Arc::clone(&decls));
		let (jobs_script, jobs_decls) = (Arc::clone(&script), Arc::clone(&decls));
		let init_script = Arc::clone(&script);
		let init_runtime = runtime.clone();

		// After the script's `on_init` below, whose seed creates the `services` offers name.
		let plans = decls.features.contains("plans").then(|| decls.offers.clone());
		let builder = builder
			.settings(SETTINGS)
			.extension(runtime)
			.routes(scoped)
			.try_jobs(move |runner, app| jobs::register(runner, &app, &jobs_script, &jobs_decls))
			.on_init(move |app: App| async move {
				let _ = hook_app.set(app.clone());
				// Before the object index, and before `jobs::init`: a script's own `on_init`
				// seed may already write to the tables `app.table` declared.
				if let Some(db) = &init_runtime.db {
					db.reconcile(&init_runtime.tables).await?;
				}
				// Before anything queries: the index follows the declarations, so a path added
				// to `app.object_type` is queryable on the first request after the restart.
				init_runtime.objects.object_index_reconcile(&init_runtime.declared()).await?;
				jobs::init(&init_script, &app, &decls.init).await
			});
		let mut builder = match plans {
			Some(offers) => saas_plans::install(builder, offers),
			None => builder,
		};
		// After `install`: handlers run in registration order, so a script's `PaymentSettled`
		// handler sees the grants `saas-plans` wrote for it.
		if !events_decls.events.is_empty() {
			builder = builder.on_event(move |app, ev| {
				let (script, decls) = (Arc::clone(&events_script), Arc::clone(&events_decls));
				async move { jobs::dispatch(&script, &app, &decls.events, ev).await }
			});
		}
		Ok((builder, script))
	}

	/// The execution context: the base surface, plus the I/O modules the profile allows.
	///
	/// `fs`, `http` and `env` are installed from [`IoProfile`] and are never in the base set —
	/// that single edit is what would foreclose org-level scripting.
	fn context(&self) -> ClResult<Context> {
		let ce = |e: rune::ContextError| error::compile(e.to_string());
		let mut modules = api::modules().map_err(ce)?;
		modules.push(ctx::module().map_err(ce)?);
		modules.push(value::module().map_err(ce)?);
		modules.push(value::json_module().map_err(ce)?);
		modules.push(money::module().map_err(ce)?);
		modules.push(objects::module().map_err(ce)?);
		modules.push(tx::module().map_err(ce)?);
		modules.push(pdf::module().map_err(ce)?);
		modules.push(refs::module().map_err(ce)?);
		modules.push(entitle::module().map_err(ce)?);
		modules.push(plans::module().map_err(ce)?);
		#[cfg(feature = "ai")]
		modules.push(llm::module().map_err(ce)?);
		#[cfg(feature = "ai")]
		modules.push(memory::module().map_err(ce)?);
		#[cfg(feature = "ai")]
		modules.push(agent::module().map_err(ce)?);
		#[cfg(feature = "ai")]
		modules.push(skills::module().map_err(ce)?);
		#[cfg(feature = "ai")]
		modules.push(search::module().map_err(ce)?);
		// Gated like `fs` and `http`: no adapter, no module, so a `db::` reference is
		// `E-SCRIPT-COMPILE` rather than a runtime permission check.
		if self.db.is_some() {
			modules.push(db::module().map_err(ce)?);
		}
		modules.extend(routes::modules().map_err(ce)?);
		modules.extend(self.io.modules()?);

		let mut c = Context::with_default_modules().map_err(ce)?;
		for m in modules {
			c.install(m).map_err(ce)?;
		}
		Ok(c)
	}
}

/// What a Rune host function cannot capture: a host function is a free `fn`, so the object
/// store, the transaction hook and the declared object types travel in the `App`'s type-map.
///
/// [`ScriptApp::install`] registers one through `AppBuilder::extension`. It is the same instance
/// inside a `tx::with` block as outside: the transaction is joined by task, so the object store
/// handle in here needs no rebinding.
#[derive(Clone)]
pub struct ScriptRuntime {
	pub objects: Arc<dyn ObjectStore>,
	/// `None` until the application supplies one; `tx::with` then fails with a message naming
	/// the missing hook rather than panicking.
	pub tx: Option<Arc<dyn TxHook>>,
	/// `None` leaves the `db::` module uninstalled, which is the whole permission system.
	pub db: Option<Arc<dyn AppDb>>,
	pub types: Arc<Vec<ObjectTypeDef>>,
	/// What `app.table(…)` declared, reconciled once at startup.
	pub tables: Arc<Vec<db::TableDef>>,
}

impl ScriptRuntime {
	#[must_use]
	pub fn new(objects: Arc<dyn ObjectStore>, types: Vec<ObjectTypeDef>) -> Self {
		Self { objects, tx: None, db: None, types: Arc::new(types), tables: Arc::new(Vec::new()) }
	}

	#[must_use]
	pub fn with_tx_hook(mut self, hook: Arc<dyn TxHook>) -> Self {
		self.tx = Some(hook);
		self
	}

	#[must_use]
	pub fn with_app_db(mut self, db: Arc<dyn AppDb>, tables: Vec<db::TableDef>) -> Self {
		self.db = Some(db);
		self.tables = Arc::new(tables);
		self
	}

	#[must_use]
	pub fn type_def(&self, type_name: &str) -> Option<&ObjectTypeDef> {
		self.types.iter().find(|d| d.type_name == type_name)
	}

	/// What `ObjectStore::object_index_reconcile` takes, run at startup before anything queries.
	#[must_use]
	pub fn declared(&self) -> Vec<ObjectType> {
		self.types.iter().map(ObjectTypeDef::declared).collect()
	}
}

/// The body of a `tx::with` block, as [`TxHook`] receives it — and of a `db::tx` block, as
/// [`AppDb::transaction`] does.
///
/// Boxed rather than generic because `TxHook` is used as a trait object: the application
/// registers one `Arc<dyn TxHook>` for the whole process. **Not `Send`**: it drives a Rune
/// `Function`, and Rune's futures borrow `rune::Value`, which is not `Send` — `Vm::send_execute`
/// is what confines that for axum, and it is upstream of here.
pub type TxBody<'a> = Pin<Box<dyn Future<Output = ClResult<serde_json::Value>> + 'a>>;

/// How a `tx::with` block runs inside a transaction, supplied by the consumer application.
///
/// `saas-script` cannot reach the store adapter's transaction API — the adapter is a leaf, and
/// naming it here would make the script runtime SQLite-only. So the application opens the
/// transaction, runs `body` to completion inside it, and commits on `Ok` or rolls back on `Err`.
/// Everything `body` calls joins that transaction by **task**, so there is no rebound `App` to
/// hand back and no store handle for the hook to swap.
#[async_trait(?Send)]
pub trait TxHook: Send + Sync + 'static {
	/// # Errors
	/// Whatever `body` returned, or whatever the store raised opening or committing.
	async fn run(&self, app: &App, body: TxBody<'_>) -> ClResult<serde_json::Value>;
}

/// The script's own database, supplied by the consumer application.
///
/// The mirror of [`TxHook`], and for the same reason: `saas-script` cannot name a store adapter
/// without becoming SQLite-only.
///
/// **This is not the framework's database.** A statement here cannot reach `invoices`, `accounts`
/// or any other framework table — the adapter opens a different file, so a script's SQL is not a
/// framework-data capability at all. `adapters/appdb-adapter-sqlite` is the implementation.
///
/// The impl is **adapter-dependent by design**: a statement's dialect is the script's, so a
/// bundle that uses `db::` does not port to another one. A bundle that does not still does.
#[async_trait]
pub trait AppDb: Send + Sync + 'static {
	/// # Errors
	/// `E-SCRIPT-DB` for a refused statement or an unrepresentable column; whatever the database
	/// raised otherwise — a busy one stays `E-CORE-UNAVAILABLE` and retryable. `E-SCRIPT-DB` too
	/// for a statement that would write, and for one returning more than `max_rows` rows: an
	/// error, never a silent truncation.
	async fn query(
		&self,
		sql: &str,
		args: &[serde_json::Value],
		max_rows: usize,
	) -> ClResult<Vec<serde_json::Value>>;

	/// Rows affected.
	///
	/// # Errors
	/// As [`query`](Self::query).
	async fn exec(&self, sql: &str, args: &[serde_json::Value]) -> ClResult<u64>;

	/// The startup reconcile of the declared tables. **Additive**: it creates what is missing and
	/// never drops a table or a column, because a declaration file people edit casually must not
	/// be able to lose data.
	///
	/// # Errors
	/// Whatever the database raised; `Error::Internal` for a declared column that already exists
	/// with a different type.
	async fn reconcile(&self, tables: &[db::TableDef]) -> ClResult<()>;

	/// One transaction on the script database, committed on `Ok` and rolled back on `Err`.
	/// It covers **this** database only: a framework `tx::with` rollback does not undo what this
	/// block wrote, and this block rolling back does not undo a framework write.
	///
	/// Hand-written rather than `async fn`, because the two halves of this trait need opposite
	/// bounds: `body` drives a Rune closure, whose future is not `Send`, while `reconcile` runs
	/// from `AppBuilder::on_init`, which requires one.
	///
	/// # Errors
	/// Whatever `body` returned, or whatever the database raised opening or committing.
	fn transaction<'a>(
		&'a self,
		body: TxBody<'a>,
	) -> Pin<Box<dyn Future<Output = ClResult<serde_json::Value>> + 'a>>;
}

// vim: ts=4
