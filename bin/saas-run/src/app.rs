//! The generic composition root: what `examples/booking/backend/src/main.rs` writes by hand for one
//! application, driven instead by what the script declared.
//!
//! `auth` and `email` are on without asking; `invoice`, `nav`
//! and `billing` are registered only when the script asked for them, through the `features`
//! callback `ScriptApp::install` hands back.

use std::{
	collections::BTreeSet,
	path::{Path, PathBuf},
	sync::Arc,
};

use appdb_adapter_sqlite::{Module, SqliteAppDb};
use async_trait::async_trait;
use payment_adapter_barion::BarionProvider;
use saas_auth::store::{AuthStore, LegalKind, NewLegalDoc};
use saas_billing::{BillingStore, PaymentProviders};
use saas_core::{
	App, AppBuilder, ClResult,
	config::Config,
	error::Error,
	objects::ObjectStore,
	settings::{SettingDef, env_name},
	store::CoreStore,
};
use saas_invoice::store::InvoiceStore;
use saas_nav::store::NavStore;
use saas_script::{Script, ScriptApp, TxBody, TxHook, testing::TestFn};
use sha2::{Digest, Sha256};
use store_adapter_sqlite::{FRAMEWORK, SqliteStore};
use tower_http::services::{ServeDir, ServeFile};

/// `saas-run`'s own declared keys. Unprefixed like a framework key, but declared here so no
/// framework crate reads it.
pub static SETTINGS: &[SettingDef] = &[
	SettingDef::text(
		"dist_dir",
		"",
		"Directory the built SPA is served from; environment-only, defaults to <app-dir>/dist.",
	),
	SettingDef::text(
		"script_fs_root",
		"",
		"Directory the script's fs module is confined to; environment-only, defaults to <data-dir>/script.",
	),
	SettingDef::text(
		"app_db_path",
		"",
		"The app database file (the script's db module); environment-only, defaults to <data-dir>/app.db.",
	),
];

/// The framework's handlebars templates, which live at the repository root rather than inside
/// `saas-email`. A default, not a row, so `EMAIL_TEMPLATE_DIR` still overrides it — and it has
/// to, for a binary copied out of its checkout: an image carries `templates/email/` and points
/// the variable at it, as `examples/booking/Dockerfile` already does for the Rust example.
const TEMPLATE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/email");

/// Opens the store, migrates it, composes everything the script declared and hands back the
/// builder. Returning the builder rather than serving is what lets a test harness reach the
/// same composition.
///
/// # Errors
/// Whatever the store raised opening or migrating; `E-SCRIPT-COMPILE` for a source that does not
/// compile, an unknown mount name or a job kind that will not intern.
pub async fn build(dir: &Path, config: Option<Config>) -> ClResult<AppBuilder> {
	let (builder, script, store, app_db) = compose(dir, config, false).await?;
	let gateway = gateway_configured(&store).await?;
	let builder = script
		.install(builder, move |b, features| {
			feature_crates(b, features, dir, &store, &app_db, true, gateway)
		})
		.await?;
	// Here, not in `feature_crates`, which `install` calls *before* adding the script's `on_init`:
	// the script's `seed` writes the `sellers` row `saas_nav::job::seed` validates, so it must
	// run after it.
	Ok(builder.on_init(|app: App| async move { nav_seed(&app).await }))
}

/// [`build`], plus the compiled bundle, its `#[test]` functions and the store to seed through.
///
/// The `test::` module reaches a bundle only along this path, so a served application cannot
/// call the in-process client.
///
/// # Errors
/// As [`build`].
pub async fn build_tests(
	dir: &Path,
	config: Config,
) -> ClResult<(AppBuilder, Arc<Script>, Vec<TestFn>, SqliteStore)> {
	let (builder, script, store, app_db) = compose(dir, Some(config), true).await?;
	// Example identities are compiled in for `test` only; a suite must not need `.env` to boot.
	let builder = builder.setting_default("deployment.env", "test");
	let gateway = gateway_configured(&store).await?;
	let seeded = store.clone();
	let (builder, compiled, tests) = script
		.install_tests(builder, move |b, features| {
			feature_crates(b, features, dir, &store, &app_db, false, gateway)
		})
		.await?;
	Ok((builder, compiled, tests, seeded))
}

/// Everything both entry points share: the store, the sources and the builder, stopping one
/// step short of `install` so the test path can ask for a different one.
async fn compose(
	dir: &Path,
	config: Option<Config>,
	with_tests: bool,
) -> ClResult<(AppBuilder, ScriptApp, SqliteStore, Arc<SqliteAppDb>)> {
	// Resolved against the application directory rather than the cwd: the `.env` is part of the
	// application. Missing is not an error — a real deployment sets real environment variables.
	// Loaded even when the caller passes a `Config`: `saas-run test` builds a literal one for the
	// bootstrap keys, but the declared keys behind it still resolve through the environment.
	// Fatal on a parse error: dotenvy drops every line after a bad one, silently unsetting keys.
	match dotenvy::from_path(dir.join(".env")) {
		Ok(()) => {}
		Err(dotenvy::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
		Err(e) => return Err(Error::internal(format!("{}/.env: {e}", dir.display()))),
	}
	let config = config.unwrap_or_else(Config::from_env);
	// Both read `config`, which `AppBuilder::config` takes by value below.
	let fs_root = fs_root(&config)?;
	let store = SqliteStore::open(&config).await?;
	// After the open: the framework database's directory has to exist to be canonicalised.
	let db_path = app_db_path(&config)?;
	store.migrate(&[FRAMEWORK]).await?;

	let sources = sources(dir, with_tests)?;

	// `auth.public` and `auth.authenticated` merge **unscoped**, which `auth_mw` reads as a
	// fail-closed 403 for an `Actor::Key`: it keeps a leaked key out of the endpoints that mint
	// other keys and start erasures.
	let mut api = saas_auth::routes::public()
		.merge(saas_auth::routes::authenticated())
		// Before the SPA fallback: an unmatched /api path is a 404 in the error envelope, not
		// index.html with a 200 the client parses as an empty success.
		.route("/api/{*rest}", axum::routing::any(|| async { Error::NotFound }));
	if let Some(dist) = dist_dir(dir)? {
		api = api.fallback_service(spa(&dist));
	}

	let builder = AppBuilder::new()
		.config(config)
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		.settings(SETTINGS)
		.settings(saas_auth::SETTINGS)
		.settings(saas_email::SETTINGS)
		.secrets(saas_auth::SECRETS)
		.secrets(saas_email::SECRETS)
		.setting_default("email.template_dir", TEMPLATE_DIR)
		.extension(Arc::new(store.clone()) as Arc<dyn AuthStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn saas_core::refs::RefStore>)
		.routes(api)
		.jobs(|runner, app| {
			saas_auth::job::register(runner, app.clone());
			saas_email::job::register(runner, app);
		})
		// No framework crate calls it, so a bad `email.template_dir` is otherwise a job failing
		// at send time rather than a boot failure.
		.on_init(|app: App| async move { saas_email::check_email_settings(&app).await })
		.on_init({
			let legal = dir.join("legal");
			move |app: App| async move { publish_legal(&app, &legal).await }
		})
		.on_init(|app: App| async move { saas_auth::bootstrap_operator(&app).await });

	let objects: Arc<dyn ObjectStore> = Arc::new(store.clone());
	// The framework's transaction, over the framework's database.
	let hook = Arc::new(SqliteTxHook(store.clone()));
	// The script's own database: a different file, so no `db::` statement can reach a framework
	// table. Its tables are **not** a schema `Module` either — they carry no version, are
	// reconciled declaratively from the declaration, and never reach `schema_version`.
	let app_db = Arc::new(SqliteAppDb::new(db_path));
	let app = ScriptApp::new(sources, objects, fs_root).tx_hook(hook).app_db(app_db.clone());
	Ok((builder, app, store, app_db))
}

/// Registers what a feature crate needs beyond its routes: its settings, secrets, store
/// extension, jobs and alert source. `ScriptApp::install` mounts the bundles; naming these
/// symbols is the composition root's job.
///
/// `live` is false on the test path: with real NAV credentials in the application's `.env`, the
/// suite's job drain would file its throwaway invoices to the tax authority on every run. Only
/// the `NAV_REPORT` handler is gated on it — the settings, secrets, store extension and alerts
/// stay registered, so `GET /api/invoices/{uid}/nav` still answers. ([`nav_seed`] is on the
/// live path alone because [`build`] is the only caller that registers it.)
// Only the `ai` build's `Prompts::load` can fail.
#[cfg_attr(not(feature = "ai"), allow(clippy::unnecessary_wraps))]
fn feature_crates(
	mut b: AppBuilder,
	features: &BTreeSet<String>,
	dir: &Path,
	store: &SqliteStore,
	app_db: &Arc<SqliteAppDb>,
	live: bool,
	gateway: bool,
) -> ClResult<AppBuilder> {
	// Runs before the script's `app.table` reconcile: `install` calls this closure first and
	// registers its own `on_init` after, and `AppBuilder::build` runs them in order.
	let modules = app_db_modules(features);
	let db = Arc::clone(app_db);
	b = b.on_init(move |_: App| async move { db.migrate(&modules).await });
	// NAV reports invoices, so `nav` without `invoice` would resolve its `InvoiceStore` out of
	// an empty extension map at job time rather than at boot.
	if features.contains("invoice") || features.contains("nav") {
		let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
		let (draft, pdf) = (Arc::clone(&invoices), Arc::clone(&invoices));
		b = b
			.settings(saas_invoice::SETTINGS)
			.extension(invoices)
			.alerts(saas_invoice::service_api::alerts)
			.jobs(move |runner, app| {
				saas_invoice::mnb::register(runner, app.clone());
				saas_invoice::draft::register(runner, app.clone(), draft);
				saas_invoice::pdf::register(runner, app, pdf);
			})
			// `register` installs handlers only — the same reason the billing seeds below exist.
			// Without these, abandoned drafts are never swept, an invoice whose RENDER_PDF was
			// never enqueued never gets one, and no MNB rate is ever fetched.
			.on_init(|app: App| async move {
				saas_invoice::draft::seed(&app.store).await?;
				saas_core::job::seed_periodic(&app.store, saas_invoice::mnb::KIND).await
			});
	}
	if features.contains("nav") {
		// `saas-run` compiles in no `nav.software_*` identity: NAV registers software per
		// developer and `saas-run` is not one deployment. An application compiles its own in
		// with `app.setting_default_for`, or its environment supplies it.
		b = b
			.settings(saas_nav::SETTINGS)
			.secrets(saas_nav::SECRETS)
			.extension(Arc::new(store.clone()) as Arc<dyn NavStore>)
			.alerts(saas_nav::alerts);
		if live {
			b = b.jobs(saas_nav::job::register);
		}
	}
	if features.contains("billing") {
		// Constructed before `build()` because the extension map is frozen by it, and handed the
		// `App` from `on_init`, where the credentials it reads become resolvable.
		let barion = Arc::new(BarionProvider::deferred());
		let attach = Arc::clone(&barion);
		// Registered unconditionally, the SPA's card button appears with no POS key behind it.
		// See [`gateway_configured`] for why the two layers are read by hand.
		let mut providers = PaymentProviders::new();
		if gateway {
			providers = providers.with(barion);
		}
		// `saas-run test` only: the scripted gateway `test::payments` feeds.
		if !live {
			let fake = Arc::new(saas_script::testing::FakePayments::default());
			providers = providers
				.with(Arc::clone(&fake) as Arc<dyn saas_billing::provider::PaymentProvider>);
			b = b.extension(fake);
		}
		b = b
			.settings(saas_billing::SETTINGS)
			.secrets(payment_adapter_barion::SECRETS)
			.extension(Arc::new(store.clone()) as Arc<dyn BillingStore>)
			// Registered even when empty: every billing route reads it.
			.extension(Arc::new(providers))
			.alerts(saas_billing::alerts)
			.jobs(|runner, app| {
				saas_billing::dunning::register(runner, app.clone());
				saas_billing::sweep::register(runner, app);
			})
			// `register` installs handlers only. With no first job the periodic chains never
			// start, so an abandoned card payment never expires, its invoice stays PENDING and
			// discard and pay-by-transfer are refused forever.
			.on_init(move |app: App| {
				let attach = Arc::clone(&attach);
				async move {
					attach.attach(&app);
					saas_billing::dunning::seed(&app.store).await?;
					saas_billing::sweep::seed(&app.store).await
				}
			});
	}
	if features.contains("pdf") {
		// The store, not a `Documents` handle: the extension map is frozen before an `App` exists.
		let docs: Arc<dyn saas_pdf::DocumentStore> = Arc::new(store.clone());
		let (root, job_docs) = (dir.to_path_buf(), Arc::clone(&docs));
		let data_dir = b.configured().map(|c| c.data_dir.clone()).unwrap_or_default();
		b = b
			.account_data_hook(Arc::new(saas_pdf::DocumentHook {
				docs: Arc::clone(&docs),
				data_dir,
			}))
			.extension(docs)
			.jobs(move |runner, app| saas_pdf::register(runner, app, root, Some(job_docs)));
	}
	if features.contains("entitle") {
		// The declarations are `saas-script`'s to install: this closure sees no `Decls`.
		b = b.extension(Arc::new(store.clone()) as Arc<dyn saas_entitle::EntitleStore>);
	}
	if features.contains("plans") {
		// `saas_plans::install` (the offers, their reconcile) is `saas-script`'s, like `entitle`.
		b = b
			.settings(saas_plans::SETTINGS)
			.secrets(saas_plans::SECRETS)
			.extension(Arc::new(store.clone()) as Arc<dyn saas_plans::PlanStore>)
			.extension(Arc::new(saas_plans::events::Recurrence)
				as Arc<dyn saas_billing::provider::RecurrenceHook>)
			.jobs(saas_plans::renew::register)
			.on_init(|app: App| async move { saas_plans::renew::seed(&app.store).await });
	}
	#[cfg(feature = "ai")]
	if features.contains("llm") {
		// One `LlmState` per app: every `fake`-kind provider shares its queue, which is what
		// `test::llm_script` pushes into. Prompts load once, at boot, from `<app-dir>/packs`.
		b = b
			.settings(saas_llm::SETTINGS)
			.secrets(saas_llm::SECRETS)
			.extension(saas_llm::LlmState::default())
			.extension(Arc::new(store.clone()) as Arc<dyn saas_llm::LlmStore>)
			.extension(Arc::new(saas_llm::Prompts::load(dir)?))
			.alerts(saas_llm::ledger::alerts);
	}
	#[cfg(feature = "ai")]
	if features.contains("memory") {
		// One handle for `memory::`, the erase hook and the agent tools.
		let memory = Arc::new(saas_memory::Memory::new(
			Arc::clone(app_db) as Arc<dyn saas_memory::MemoryStore>,
			Arc::new(store.clone()) as Arc<dyn AuthStore>,
		));
		b = b.extension(Arc::clone(&memory)).account_data_hook(memory);
	}
	#[cfg(feature = "ai")]
	if features.contains("agent") {
		// `app.feature("agent")` implies `llm` and `memory`, so both blocks above ran. The
		// script registers its `app.tool`s as the `saas_agent::Tools` extension itself.
		let threads = Arc::clone(app_db) as Arc<dyn saas_agent::ThreadStore>;
		b = b
			.settings(saas_agent::SETTINGS)
			.extension(saas_agent::RunPool::default())
			.extension(Arc::new(store.clone()) as Arc<dyn saas_agent::AgentRunStore>)
			.extension(Arc::clone(&threads))
			.account_data_hook(Arc::new(saas_agent::AgentHook {
				threads,
				orgs: Arc::new(store.clone()) as Arc<dyn AuthStore>,
			}))
			// No silent resume: whatever a previous process left live ends `interrupted`.
			.on_init(|app: App| async move { saas_agent::pool::sweep(&app).await.map(|_| ()) });
	}
	#[cfg(feature = "ai")]
	if features.contains("search") {
		// `search` implies `llm`, whose `LlmStore` the ledger rows go to. One shared `Fixtures`,
		// so `test::search_fixture` feeds what a `fake` provider answers. A `SearchStore` is
		// also what makes `Agent` offer `web_search` and `fetch`.
		let fixtures = saas_search::Fixtures::default();
		let backends = saas_search::SearchBackends::new()
			.with_provider(Arc::new(search_adapter_linkup::Linkup))
			.with_provider(Arc::new(search_adapter_searxng::Searxng))
			.with_provider(Arc::new(fetch_adapter_jina::JinaSearch))
			.with_provider(Arc::new(fixtures.clone()))
			.with_fetcher(Arc::new(fetch_adapter_jina::Jina))
			.with_fetcher(Arc::new(fixtures.clone()));
		b = b
			.settings(saas_search::SETTINGS)
			.secrets(saas_search::SECRETS)
			// The crate names no engine; a suite selects its fakes with `app.test_default`.
			.setting_default("search.provider", "linkup")
			.setting_default("search.fetcher", "jina")
			// jina.ai/pricing (2026-09-29): $50 per 1B tokens at 0.8807 EUR/USD, rounded up.
			.setting_default("search.price_per_mtok.jina", "44100")
			// docs.linkup.so pricing: $0.005 per standard `searchResults` call, rounded up.
			.setting_default("search.price.linkup", "4500")
			.extension(fixtures)
			.extension(Arc::new(backends))
			.extension(Arc::new(store.clone()) as Arc<dyn saas_search::SearchStore>);
	}
	Ok(b)
}

/// NAV reporting is optional, so an incomplete configuration warns instead of failing:
/// `saas_nav::job::seed` refuses to start on a `sellers` row NAV would reject, and that error
/// out of `on_init` would take the whole application down with it.
async fn nav_seed(app: &App) -> ClResult<()> {
	// The `nav` feature's own gate, read back from the extension map: registration happens inside
	// `install`, and this `on_init` is added after it, where the declared set is out of reach.
	if app.extensions.get::<Arc<dyn NavStore>>().is_none() {
		return Ok(());
	}
	let mut ready = true;
	for key in ["nav.tech_password", "nav.sign_key", "nav.exchange_key"] {
		// Not short-circuited: all three are reported even when one is missing, so filling the
		// gap in is one restart rather than three.
		ready &= app.secrets.status(key).await?.set;
	}
	// No early return: the sweep still seeds, and a tenant seller files with its own org's
	// secrets, never these.
	if !ready {
		tracing::warn!(
			"global NAV secrets are incomplete — the root seller's invoices will not be filed"
		);
	}
	if let Err(e) = saas_nav::job::seed(app).await {
		tracing::warn!(error = %e, "NAV is not fully configured — invoices will not be filed");
	}
	Ok(())
}

/// `<app-dir>/legal/{terms,privacy}.md`, published the way the binary already ships email
/// templates and serves `dist/`. A script has no `AuthStore` binding, and publishing a legal
/// document is a deployment act rather than business logic.
///
/// Without a published document `consents_required` fails **closed**: every consent-gated route
/// answers 403 and the SPA cannot register anyone. No `legal/` directory means an application
/// that mounts no consent gate, so nothing is published and nothing changes.
async fn publish_legal(app: &App, dir: &Path) -> ClResult<()> {
	if !dir.is_dir() {
		return Ok(());
	}
	let store = saas_auth::routes::store(app)?;
	for (kind, file, title) in [
		(LegalKind::Tos, "terms.md", "Terms of Service"),
		(LegalKind::Privacy, "privacy.md", "Privacy Policy"),
	] {
		let path = dir.join(file);
		let Ok(body) = std::fs::read_to_string(&path) else {
			continue;
		};
		let sha256 = hex::encode(Sha256::digest(body.as_bytes()));
		// The digest *is* the version, so editing a file republishes and re-gates automatically
		// and there is no constant to forget to bump. Idempotent on the row's existence: an
		// unchanged file is the same version and the `(kind, locale, version)` UNIQUE refuses it.
		let res = store
			.insert_legal_doc(&NewLegalDoc {
				kind,
				locale: "en".to_owned(),
				version: sha256[..12].to_owned(),
				title: title.to_owned(),
				body,
				sha256,
				effective_from: saas_core::types::Timestamp::now(),
			})
			.await;
		match res {
			Ok(_) | Err(Error::Conflict(_)) => {}
			Err(e) => return Err(e),
		}
	}
	Ok(())
}

/// Every `.rn` in the application directory, in name order — minus the test sources unless the
/// caller is the test runner.
///
/// `test::` is registered only on the test path, so a served bundle must drop the test sources:
/// every `test::request` in them would be an unresolved item and fail the compile.
fn sources(dir: &Path, with_tests: bool) -> ClResult<Vec<(String, String)>> {
	let io = |e: std::io::Error, p: &Path| Error::internal(format!("{}: {e}", p.display()));
	let mut out = Vec::new();
	for entry in std::fs::read_dir(dir).map_err(|e| io(e, dir))? {
		let path = entry.map_err(|e| io(e, dir))?.path();
		if path.extension().is_none_or(|e| e != "rn") {
			continue;
		}
		let stem = path.file_stem().map_or_else(String::new, |n| n.to_string_lossy().into());
		if with_tests || !(stem == "tests" || stem.ends_with("_tests")) {
			let text = std::fs::read_to_string(&path).map_err(|e| io(e, &path))?;
			let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into());
			out.push((name, text));
		}
	}
	if out.is_empty() {
		return Err(Error::internal(format!("no .rn source in {}", dir.display())));
	}
	// A directory listing has no order, and a source name is what a Rune diagnostic reports.
	out.sort();
	Ok(out)
}

/// Where the SPA fallback serves from, or `None` for an application that serves no SPA.
///
/// Read from the environment rather than through `Settings`, because the router is assembled
/// before any `App` exists to resolve a row against; [`SETTINGS`] is what types and documents it.
fn dist_dir(dir: &Path) -> ClResult<Option<PathBuf>> {
	// Configured and wrong is a mistake worth a startup failure; unconfigured just means there is
	// no SPA to serve.
	let Some(v) = std::env::var(env_name("dist_dir")).ok().filter(|v| !v.trim().is_empty()) else {
		let path = dir.join("dist");
		return Ok(path.is_dir().then_some(path));
	};
	// App-relative like the `.env` in `compose`; `Path::join` returns an absolute `v` unchanged.
	let path = dir.join(v);
	if path.is_dir() {
		Ok(Some(path))
	} else {
		Err(Error::Setting(format!("setting 'dist_dir': '{}' is not a directory", path.display())))
	}
}

/// Whether a payment gateway is configured at all, across **both** layers.
///
/// `build` freezes the extension map before any `App` exists to resolve a key through, so the two
/// configuration layers are read by hand: a secret is env → row, a setting is row →
/// env. Half a configuration counts as none — `Credentials::resolve` needs the payee and `start`
/// is what sends it.
async fn gateway_configured(store: &SqliteStore) -> ClResult<bool> {
	let set = |v: Option<String>| v.is_some_and(|v| !v.trim().is_empty());
	let env = |key: &str| std::env::var(env_name(key)).ok();
	let pos_key = payment_adapter_barion::SECRETS[0];
	let pos = set(env(pos_key)) || store.secret_get(0, pos_key).await?.is_some();
	let payee =
		set(store.setting_get("payment.barion.payee").await?) || set(env("payment.barion.payee"));
	Ok(pos && payee)
}

/// Where the script's `fs` module is confined. Deliberately **not** the application directory:
/// that is where `compose` loads `.env` from, so an app-rooted `fs` would make `MASTER_KEY`
/// readable from any script route. Created here because `under_root` canonicalizes the root.
fn fs_root(config: &Config) -> ClResult<PathBuf> {
	let root = std::env::var(env_name("script_fs_root"))
		.ok()
		.filter(|v| !v.trim().is_empty())
		.map_or_else(|| PathBuf::from(&config.data_dir).join("script"), PathBuf::from);
	std::fs::create_dir_all(&root)
		.map_err(|e| Error::internal(format!("{}: {e}", root.display())))?;
	Ok(root)
}

/// The framework's app-DB content modules the script's features need, in apply order.
#[cfg_attr(not(feature = "ai"), allow(unused_variables, unused_mut))]
fn app_db_modules(features: &BTreeSet<String>) -> Vec<Module> {
	let mut modules = Vec::new();
	#[cfg(feature = "ai")]
	if features.contains("memory") {
		modules.push(appdb_adapter_sqlite::MEMORY);
	}
	#[cfg(feature = "ai")]
	if features.contains("agent") {
		modules.push(appdb_adapter_sqlite::AGENT);
	}
	modules
}

/// Where the app database file lives. Read by hand like `dist_dir` and `script_fs_root`: the
/// value is needed before any `App` exists to resolve a row against, and [`SETTINGS`] is what
/// types and documents it. Hanging the default off `data_dir` is what gives `saas-run test` its
/// per-case isolation and cleanup for free.
fn app_db_path(config: &Config) -> ClResult<PathBuf> {
	let var = |key| std::env::var(env_name(key)).ok().filter(|v| !v.trim().is_empty());
	resolve_app_db_path(
		var("app_db_path"),
		var("script_db_path"),
		Path::new(&config.data_dir),
		&config.db_path,
	)
}

/// `APP_DB_PATH`, then the deprecated `SCRIPT_DB_PATH`, then `<data_dir>/app.db` — taking over a
/// legacy `<data_dir>/script.db` by renaming it.
fn resolve_app_db_path(
	app: Option<String>,
	legacy: Option<String>,
	data_dir: &Path,
	db_path: &str,
) -> ClResult<PathBuf> {
	let path = match (app, legacy) {
		(Some(p), _) => PathBuf::from(p),
		// Legacy for one release after the script.db → app.db rename: delete this arm and adopt_legacy_db then.
		(None, Some(p)) => {
			tracing::warn!("SCRIPT_DB_PATH is deprecated; set APP_DB_PATH instead");
			PathBuf::from(p)
		}
		(None, None) => adopt_legacy_db(data_dir)?,
	};
	not_the_framework_db(path, db_path)
}

/// `<data_dir>/app.db`, renamed from `script.db` when only the legacy file exists. Never delete or
/// recreate it: the invoicing example's database holds NAV invoice numbers already filed.
/// Siblings move before the main file, so a crash midway still finds `script.db` next boot.
fn adopt_legacy_db(data_dir: &Path) -> ClResult<PathBuf> {
	let path = data_dir.join("app.db");
	let legacy = data_dir.join("script.db");
	if path.exists() || !legacy.exists() {
		return Ok(path);
	}
	for suffix in ["-wal", "-shm", ""] {
		let from = PathBuf::from(format!("{}{suffix}", legacy.display()));
		if from.exists() {
			let to = PathBuf::from(format!("{}{suffix}", path.display()));
			std::fs::rename(&from, &to).map_err(|e| {
				Error::internal(format!("{} -> {}: {e}", from.display(), to.display()))
			})?;
		}
	}
	tracing::info!(path = %path.display(), "renamed the legacy script.db to app.db");
	Ok(path)
}

/// The app database is a separate file so a script's raw SQL cannot reach a framework table;
/// pointed at `DB_PATH`, it would reach all of them.
fn not_the_framework_db(path: PathBuf, db_path: &str) -> ClResult<PathBuf> {
	if canonical(&path) == canonical(Path::new(db_path)) {
		return Err(Error::internal(format!(
			"APP_DB_PATH {} is the framework database DB_PATH; it must be a separate file",
			path.display()
		)));
	}
	Ok(path)
}

/// The parent canonicalised and the file name re-joined: the app database may not exist yet.
fn canonical(path: &Path) -> PathBuf {
	let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
	let parent = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
	parent.join(path.file_name().unwrap_or_default())
}

/// The built SPA. Every unmatched path falls back to `index.html` so a deep link reloads; it is
/// the router's fallback, so every registered API route still wins.
fn spa(dist: &Path) -> ServeDir<ServeFile> {
	ServeDir::new(dist).fallback(ServeFile::new(dist.join("index.html")))
}

/// How a script's `tx::with` block becomes a real transaction.
///
/// Nothing is rebound: every store trait object the `App` holds is this same `SqliteStore`, and
/// `scope_writes` makes each pooled handle on the task join `tx` for the duration of the body.
struct SqliteTxHook(SqliteStore);

#[async_trait(?Send)]
impl TxHook for SqliteTxHook {
	async fn run(&self, _app: &App, body: TxBody<'_>) -> ClResult<serde_json::Value> {
		let tx = self.0.write_tx().await?;
		// No timeout here: `tx::with` puts `script.tx_timeout_ms` on `body` itself, and a
		// second one around `run` would abandon an open transaction mid-commit.
		match SqliteStore::scope_writes(&tx, body).await {
			Ok(v) => {
				tx.commit().await?;
				Ok(v)
			}
			Err(e) => {
				// `e` is the answer, not the rollback's own failure: losing the block's errCode to
				// a secondary fault turns a 409 the client handles into an opaque 500.
				if let Err(fault) = tx.rollback().await {
					tracing::error!(error = %fault, "rolling back a tx::with block failed");
				}
				Err(e)
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_app_db_must_not_be_the_framework_db() {
		let dir = std::env::temp_dir();
		let framework = dir.join("framework.db");
		let same = dir.join(".").join("framework.db");
		assert!(not_the_framework_db(same, &framework.to_string_lossy()).is_err());
		let sibling = dir.join("app.db");
		assert!(not_the_framework_db(sibling, &framework.to_string_lossy()).is_ok());
	}

	fn tmp_dir(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("saas-run-{name}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	#[test]
	fn the_app_db_defaults_to_app_db_in_the_data_dir() {
		let dir = tmp_dir("default");
		let path = resolve_app_db_path(None, None, &dir, "framework.db").unwrap();
		assert_eq!(path, dir.join("app.db"));
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn a_legacy_script_db_is_renamed_with_its_siblings() {
		let dir = tmp_dir("legacy");
		for (name, data) in
			[("script.db", "main"), ("script.db-wal", "wal"), ("script.db-shm", "shm")]
		{
			std::fs::write(dir.join(name), data).unwrap();
		}
		let path = resolve_app_db_path(None, None, &dir, "framework.db").unwrap();
		assert_eq!(path, dir.join("app.db"));
		for (name, data) in [("app.db", "main"), ("app.db-wal", "wal"), ("app.db-shm", "shm")] {
			assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), data);
		}
		for name in ["script.db", "script.db-wal", "script.db-shm"] {
			assert!(!dir.join(name).exists(), "{name} still exists");
		}
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn app_db_path_wins_over_script_db_path() {
		let dir = tmp_dir("precedence");
		let path = resolve_app_db_path(
			Some(dir.join("new.db").to_string_lossy().into_owned()),
			Some(dir.join("old.db").to_string_lossy().into_owned()),
			&dir,
			"framework.db",
		)
		.unwrap();
		assert_eq!(path, dir.join("new.db"));
		std::fs::remove_dir_all(&dir).unwrap();
	}
}

// vim: ts=4
