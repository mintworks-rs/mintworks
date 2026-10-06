// SPDX-License-Identifier: MPL-2.0
//! The generic composition root: what `examples/booking/app-rust/src/main.rs` writes by hand for one
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

use async_trait::async_trait;
#[cfg(feature = "postgres")]
use mintworks_appdb_postgres::PgAppDb;
use mintworks_appdb_sqlite::SqliteAppDb;
use mintworks_auth::store::{AuthStore, LegalKind, NewLegalDoc};
use mintworks_billing::{BillingStore, PaymentProviders};
use mintworks_core::{
	App, AppBuilder, ClResult,
	config::Config,
	error::Error,
	objects::ObjectStore,
	settings::{SettingDef, env_name},
	store::CoreStore,
};
use mintworks_invoice::store::InvoiceStore;
use mintworks_nav::store::NavStore;
use mintworks_payment_barion::BarionProvider;
use mintworks_script::{AppDb, Script, ScriptApp, TxBody, TxHook, testing::TestFn};
#[cfg(feature = "postgres")]
use mintworks_store_postgres::PgStore;
use mintworks_store_sqlite::SqliteStore;
use sha2::{Digest, Sha256};
use tower_http::services::{ServeDir, ServeFile};

/// `mintworks`'s own declared keys. Unprefixed like a framework key, but declared here so no
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
	SettingDef::text(
		"app_db_url",
		"",
		"A postgres:// URL putting the app database on PostgreSQL instead of app_db_path; environment-only, needs --features postgres.",
	),
	SettingDef::text(
		"db_url",
		"",
		"A postgres:// URL putting the framework database on PostgreSQL instead of DB_PATH; environment-only, needs --features postgres.",
	),
];

/// The framework's handlebars templates, which live at the repository root rather than inside
/// `mintworks-email`. A default, not a row, so `EMAIL_TEMPLATE_DIR` still overrides it — and it has
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
	let (config, store, app_db) = open(dir, config, None).await?;
	match store {
		CoreDb::Sqlite(store) => build_on(dir, config, store, app_db).await,
		#[cfg(feature = "postgres")]
		CoreDb::Postgres(store) => build_on(dir, config, store, app_db).await,
	}
}

async fn build_on<S: FrameworkStore + Clone>(
	dir: &Path,
	config: Config,
	store: S,
	app_db: AppDbHandle,
) -> ClResult<AppBuilder> {
	let (builder, script) = compose(dir, config, &store, &app_db, false)?;
	let gateway = gateway_configured(&store).await?;
	let builder = script
		.install(builder, move |b, features| {
			feature_crates(b, features, dir, &store, &app_db, true, gateway)
		})
		.await?;
	// Here, not in `feature_crates`, which `install` calls *before* adding the script's `on_init`:
	// the script's `seed` writes the `sellers` row `mintworks_nav::job::seed` validates, so it must
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
	urls: Option<&DbUrls>,
) -> ClResult<(AppBuilder, Arc<Script>, Vec<TestFn>, Arc<dyn FrameworkStore>)> {
	let (config, store, app_db) = open(dir, Some(config), urls).await?;
	match store {
		CoreDb::Sqlite(store) => build_tests_on(dir, config, store, app_db).await,
		#[cfg(feature = "postgres")]
		CoreDb::Postgres(store) => build_tests_on(dir, config, store, app_db).await,
	}
}

async fn build_tests_on<S: FrameworkStore + Clone>(
	dir: &Path,
	config: Config,
	store: S,
	app_db: AppDbHandle,
) -> ClResult<(AppBuilder, Arc<Script>, Vec<TestFn>, Arc<dyn FrameworkStore>)> {
	let (builder, script) = compose(dir, config, &store, &app_db, true)?;
	// Example identities are compiled in for `test` only; a suite must not need `.env` to boot.
	let builder = builder.setting_default("deployment.env", "test");
	let gateway = gateway_configured(&store).await?;
	let seeded: Arc<dyn FrameworkStore> = Arc::new(store.clone());
	let (builder, compiled, tests) = script
		.install_tests(builder, move |b, features| {
			feature_crates(b, features, dir, &store, &app_db, false, gateway)
		})
		.await?;
	Ok((builder, compiled, tests, seeded))
}

/// Per-case `DB_URL`/`APP_DB_URL` that win over the environment: `mintworks test` cannot set
/// either, because `std::env::set_var` is `unsafe` in edition 2024.
pub struct DbUrls {
	pub db: String,
	pub app_db: String,
}

/// The framework store [`open`] selected: `DB_URL` names a PostgreSQL one, else `DB_PATH` a file.
enum CoreDb {
	Sqlite(SqliteStore),
	#[cfg(feature = "postgres")]
	Postgres(PgStore),
}

/// Loads `.env`, resolves the config, then opens and migrates the framework store and opens the
/// app database.
async fn open(
	dir: &Path,
	config: Option<Config>,
	urls: Option<&DbUrls>,
) -> ClResult<(Config, CoreDb, AppDbHandle)> {
	// Resolved against the application directory rather than the cwd: the `.env` is part of the
	// application. Missing is not an error — a real deployment sets real environment variables.
	// Loaded even when the caller passes a `Config`: `mintworks test` builds a literal one for the
	// bootstrap keys, but the declared keys behind it still resolve through the environment.
	// Fatal on a parse error: dotenvy drops every line after a bad one, silently unsetting keys.
	match dotenvy::from_path(dir.join(".env")) {
		Ok(()) => {}
		Err(dotenvy::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
		Err(e) => return Err(Error::internal(format!("{}/.env: {e}", dir.display()))),
	}
	let config = config.unwrap_or_else(Config::from_env);
	let db_url = match urls {
		Some(u) => Some(u.db.clone()),
		None => std::env::var(env_name("db_url")).ok().filter(|v| !v.trim().is_empty()),
	};
	let store = match &db_url {
		Some(url) => postgres_store(url).await?,
		None => CoreDb::Sqlite(SqliteStore::open(&config).await?),
	};
	// After the open: the framework database's directory has to exist to be canonicalised.
	let app_db = match urls {
		Some(u) => postgres_app_db(&u.app_db, db_url.as_deref())?,
		None => open_app_db(&config, db_url.as_deref())?,
	};
	match &store {
		CoreDb::Sqlite(s) => s.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await?,
		#[cfg(feature = "postgres")]
		CoreDb::Postgres(s) => s.migrate(&[mintworks_store_postgres::FRAMEWORK]).await?,
	}
	Ok((config, store, app_db))
}

/// Only a PostgreSQL URL is accepted: a SQLite framework database is named by `DB_PATH`.
#[cfg_attr(not(feature = "postgres"), allow(clippy::unused_async))]
async fn postgres_store(url: &str) -> ClResult<CoreDb> {
	if !is_postgres_url(url) {
		return Err(Error::Setting(
			"setting 'db_url': not a postgres:// or postgresql:// URL".into(),
		));
	}
	#[cfg(not(feature = "postgres"))]
	return Err(Error::Setting(
		"setting 'db_url': a PostgreSQL framework database needs mintworks built with --features postgres"
			.into(),
	));
	#[cfg(feature = "postgres")]
	return Ok(CoreDb::Postgres(PgStore::open(url).await?));
}

fn is_postgres_url(url: &str) -> bool {
	url.starts_with("postgres://") || url.starts_with("postgresql://")
}

/// Everything both entry points share: the sources and the builder over the opened store,
/// stopping one step short of `install` so the test path can ask for a different one.
fn compose<S: FrameworkStore + Clone>(
	dir: &Path,
	config: Config,
	store: &S,
	app_db: &AppDbHandle,
	with_tests: bool,
) -> ClResult<(AppBuilder, ScriptApp)> {
	// Reads `config`, which `AppBuilder::config` takes by value below.
	let fs_root = fs_root(&config)?;
	let sources = sources(dir, with_tests)?;

	// `auth.public` and `auth.authenticated` merge **unscoped**, which `auth_mw` reads as a
	// fail-closed 403 for an `Actor::Key`: it keeps a leaked key out of the endpoints that mint
	// other keys and start erasures.
	let mut api = mintworks_auth::routes::public()
		.merge(mintworks_auth::routes::authenticated())
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
		.settings(mintworks_auth::SETTINGS)
		.settings(mintworks_email::SETTINGS)
		.secrets(mintworks_auth::SECRETS)
		.secrets(mintworks_email::SECRETS)
		.setting_default("email.template_dir", TEMPLATE_DIR)
		.extension(Arc::new(store.clone()) as Arc<dyn AuthStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_core::refs::RefStore>)
		.routes(api)
		.jobs(|runner, app| {
			mintworks_auth::job::register(runner, app.clone());
			mintworks_email::job::register(runner, app);
		})
		// No framework crate calls it, so a bad `email.template_dir` is otherwise a job failing
		// at send time rather than a boot failure.
		.on_init(|app: App| async move { mintworks_email::check_email_settings(&app).await })
		.on_init({
			let legal = dir.join("legal");
			move |app: App| async move { publish_legal(&app, &legal).await }
		})
		.on_init(|app: App| async move { mintworks_auth::bootstrap_operator(&app).await });

	let objects: Arc<dyn ObjectStore> = Arc::new(store.clone());
	// The framework's transaction, over the framework's database.
	let hook = Arc::new(StoreTxHook(store.clone()));
	// The script's own database: a different file, so no `db::` statement can reach a framework
	// table. Its tables are **not** a schema `Module` either — they carry no version, are
	// reconciled declaratively from the declaration, and never reach `schema_version`.
	let app = ScriptApp::new(sources, objects, fs_root).tx_hook(hook).app_db(app_db.db());
	Ok((builder, app))
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
fn feature_crates<S: FrameworkStore + Clone>(
	mut b: AppBuilder,
	features: &BTreeSet<String>,
	dir: &Path,
	store: &S,
	app_db: &AppDbHandle,
	live: bool,
	gateway: bool,
) -> ClResult<AppBuilder> {
	// Runs before the script's `app.table` reconcile: `install` calls this closure first and
	// registers its own `on_init` after, and `AppBuilder::build` runs them in order.
	let (db, declared) = (app_db.clone(), features.clone());
	b = b.on_init(move |_: App| async move { db.migrate(&declared).await });
	// NAV reports invoices, so `nav` without `invoice` would resolve its `InvoiceStore` out of
	// an empty extension map at job time rather than at boot.
	if features.contains("invoice") || features.contains("nav") {
		let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
		let (draft, pdf) = (Arc::clone(&invoices), Arc::clone(&invoices));
		b = b
			.settings(mintworks_invoice::SETTINGS)
			.extension(invoices)
			.alerts(mintworks_invoice::service_api::alerts)
			.jobs(move |runner, app| {
				mintworks_invoice::mnb::register(runner, app.clone());
				mintworks_invoice::draft::register(runner, app.clone(), draft);
				mintworks_invoice::pdf::register(runner, app, pdf);
			})
			// `register` installs handlers only — the same reason the billing seeds below exist.
			// Without these, abandoned drafts are never swept, an invoice whose RENDER_PDF was
			// never enqueued never gets one, and no MNB rate is ever fetched.
			.on_init(|app: App| async move {
				mintworks_invoice::draft::seed(&app.store).await?;
				mintworks_core::job::seed_periodic(&app.store, mintworks_invoice::mnb::KIND).await
			});
	}
	if features.contains("nav") {
		// `mintworks` compiles in no `nav.software_*` identity: NAV registers software per
		// developer and `mintworks` is not one deployment. An application compiles its own in
		// with `app.setting_default_for`, or its environment supplies it.
		b = b
			.settings(mintworks_nav::SETTINGS)
			.secrets(mintworks_nav::SECRETS)
			.extension(Arc::new(store.clone()) as Arc<dyn NavStore>)
			.alerts(mintworks_nav::alerts);
		if live {
			b = b.jobs(mintworks_nav::job::register);
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
		// `mintworks test` only: the scripted gateway `test::payments` feeds.
		if !live {
			let fake = Arc::new(mintworks_script::testing::FakePayments::default());
			providers = providers
				.with(Arc::clone(&fake) as Arc<dyn mintworks_billing::provider::PaymentProvider>);
			b = b.extension(fake);
		}
		b = b
			.settings(mintworks_billing::SETTINGS)
			.secrets(mintworks_payment_barion::SECRETS)
			.extension(Arc::new(store.clone()) as Arc<dyn BillingStore>)
			// Registered even when empty: every billing route reads it.
			.extension(Arc::new(providers))
			.alerts(mintworks_billing::alerts)
			.jobs(|runner, app| {
				mintworks_billing::dunning::register(runner, app.clone());
				mintworks_billing::sweep::register(runner, app);
			})
			// `register` installs handlers only. With no first job the periodic chains never
			// start, so an abandoned card payment never expires, its invoice stays PENDING and
			// discard and pay-by-transfer are refused forever.
			.on_init(move |app: App| {
				let attach = Arc::clone(&attach);
				async move {
					attach.attach(&app);
					mintworks_billing::dunning::seed(&app.store).await?;
					mintworks_billing::sweep::seed(&app.store).await
				}
			});
	}
	if features.contains("pdf") {
		// The store, not a `Documents` handle: the extension map is frozen before an `App` exists.
		let docs: Arc<dyn mintworks_pdf::DocumentStore> = Arc::new(store.clone());
		let (root, job_docs) = (dir.to_path_buf(), Arc::clone(&docs));
		let data_dir = b.configured().map(|c| c.data_dir.clone()).unwrap_or_default();
		b = b
			.account_data_hook(Arc::new(mintworks_pdf::DocumentHook {
				docs: Arc::clone(&docs),
				data_dir,
			}))
			.extension(docs)
			.jobs(move |runner, app| mintworks_pdf::register(runner, app, root, Some(job_docs)));
	}
	if features.contains("entitle") {
		// The declarations are `mintworks-script`'s to install: this closure sees no `Decls`.
		b = b.extension(Arc::new(store.clone()) as Arc<dyn mintworks_entitle::EntitleStore>);
	}
	if features.contains("plans") {
		// `mintworks_plans::install` (the offers, their reconcile) is `mintworks-script`'s, like
		// `entitle`.
		b = b
			.settings(mintworks_plans::SETTINGS)
			.secrets(mintworks_plans::SECRETS)
			.extension(Arc::new(store.clone()) as Arc<dyn mintworks_plans::PlanStore>)
			.extension(Arc::new(mintworks_plans::events::Recurrence)
				as Arc<dyn mintworks_billing::provider::RecurrenceHook>)
			.jobs(mintworks_plans::renew::register)
			.on_init(|app: App| async move { mintworks_plans::renew::seed(&app.store).await });
	}
	#[cfg(feature = "ai")]
	if features.contains("llm") {
		// One `LlmState` per app: every `fake`-kind provider shares its queue, which is what
		// `test::llm_script` pushes into. Prompts load once, at boot, from `<app-dir>/packs`.
		b = b
			.settings(mintworks_llm::SETTINGS)
			.secrets(mintworks_llm::SECRETS)
			.extension(mintworks_llm::LlmState::default())
			.extension(Arc::new(store.clone()) as Arc<dyn mintworks_llm::LlmStore>)
			.extension(Arc::new(mintworks_llm::Prompts::load(dir)?))
			.alerts(mintworks_llm::ledger::alerts);
	}
	#[cfg(feature = "ai")]
	if features.contains("memory") {
		// One handle for `memory::`, the erase hook and the agent tools.
		let memory = Arc::new(mintworks_memory::Memory::new(
			app_db.memory(),
			Arc::new(store.clone()) as Arc<dyn AuthStore>,
		));
		b = b.extension(Arc::clone(&memory)).account_data_hook(memory);
	}
	#[cfg(feature = "ai")]
	if features.contains("agent") {
		// `app.feature("agent")` implies `llm` and `memory`, so both blocks above ran. The
		// script registers its `app.tool`s as the `mintworks_agent::Tools` extension itself.
		let threads = app_db.threads();
		// From `<app-dir>/skills`, at boot; `Agent` offers `skill_read` over it.
		let skills = mintworks_agent::Skills::load(dir)?;
		let list = skills.list();
		let names: Vec<String> = list
			.iter()
			.map(|s| format!("{} (+{} refs)", s.name, s.references.len()))
			.collect();
		tracing::info!(count = list.len(), skills = names.join(", "), "skills loaded");
		b = b
			.settings(mintworks_agent::SETTINGS)
			.extension(mintworks_agent::RunPool::default())
			.extension(Arc::new(skills))
			.extension(Arc::new(store.clone()) as Arc<dyn mintworks_agent::AgentRunStore>)
			.extension(Arc::clone(&threads))
			.account_data_hook(Arc::new(mintworks_agent::AgentHook {
				threads,
				orgs: Arc::new(store.clone()) as Arc<dyn AuthStore>,
			}))
			// No silent resume: whatever a previous process left live ends `interrupted`.
			.on_init(|app: App| async move { mintworks_agent::pool::sweep(&app).await.map(|_| ()) });
	}
	#[cfg(feature = "ai")]
	if features.contains("search") {
		// `search` implies `llm`, whose `LlmStore` the ledger rows go to. One shared `Fixtures`,
		// so `test::search_fixture` feeds what a `fake` provider answers. A `SearchStore` is
		// also what makes `Agent` offer `web_search` and `fetch`.
		let fixtures = mintworks_search::Fixtures::default();
		let backends = mintworks_search::SearchBackends::new()
			.with_provider(Arc::new(mintworks_search_linkup::Linkup))
			.with_provider(Arc::new(mintworks_search_searxng::Searxng))
			.with_provider(Arc::new(mintworks_fetch_jina::JinaSearch))
			.with_provider(Arc::new(fixtures.clone()))
			.with_fetcher(Arc::new(mintworks_fetch_jina::Jina))
			.with_fetcher(Arc::new(fixtures.clone()));
		b = b
			.settings(mintworks_search::SETTINGS)
			.secrets(mintworks_search::SECRETS)
			// The crate names no engine; a suite selects its fakes with `app.test_default`.
			.setting_default("search.provider", "linkup")
			.setting_default("search.fetcher", "jina")
			// jina.ai/pricing (2026-09-29): $50 per 1B tokens at 0.8807 EUR/USD, rounded up.
			.setting_default("search.price_per_mtok.jina", "44100")
			// docs.linkup.so pricing: $0.005 per standard `searchResults` call, rounded up.
			.setting_default("search.price.linkup", "4500")
			.extension(fixtures)
			.extension(Arc::new(backends))
			.extension(Arc::new(store.clone()) as Arc<dyn mintworks_search::SearchStore>);
	}
	Ok(b)
}

/// NAV reporting is optional, so an incomplete configuration warns instead of failing:
/// `mintworks_nav::job::seed` refuses to start on a `sellers` row NAV would reject, and that error
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
	if let Err(e) = mintworks_nav::job::seed(app).await {
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
	let store = mintworks_auth::routes::store(app)?;
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
				effective_from: mintworks_core::types::Timestamp::now(),
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
async fn gateway_configured(store: &impl CoreStore) -> ClResult<bool> {
	let set = |v: Option<String>| v.is_some_and(|v| !v.trim().is_empty());
	let env = |key: &str| std::env::var(env_name(key)).ok();
	let pos_key = mintworks_payment_barion::SECRETS[0];
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

/// The framework's app-DB content modules the script's features need, in apply order, taken
/// from `$adapter`: each adapter crate has its own `Module` type, so one function cannot name both.
macro_rules! app_db_modules {
	($features:expr, $adapter:ident) => {{
		let features: &BTreeSet<String> = $features;
		#[allow(unused_mut)]
		let mut modules: Vec<$adapter::Module> = Vec::new();
		#[cfg(feature = "ai")]
		if features.contains("memory") {
			modules.push($adapter::MEMORY);
		}
		#[cfg(feature = "ai")]
		if features.contains("agent") {
			modules.push($adapter::AGENT);
		}
		let _ = features;
		modules
	}};
}

/// The app database behind whichever adapter [`open_app_db`] selected. An enum, not a trait
/// object: the content-module migration is typed per adapter.
#[derive(Clone)]
enum AppDbHandle {
	Sqlite(Arc<SqliteAppDb>),
	#[cfg(feature = "postgres")]
	Postgres(Arc<PgAppDb>),
}

impl AppDbHandle {
	fn db(&self) -> Arc<dyn AppDb> {
		match self {
			Self::Sqlite(db) => Arc::clone(db) as Arc<dyn AppDb>,
			#[cfg(feature = "postgres")]
			Self::Postgres(db) => Arc::clone(db) as Arc<dyn AppDb>,
		}
	}

	#[cfg(feature = "ai")]
	fn memory(&self) -> Arc<dyn mintworks_memory::MemoryStore> {
		match self {
			Self::Sqlite(db) => Arc::clone(db) as Arc<dyn mintworks_memory::MemoryStore>,
			#[cfg(feature = "postgres")]
			Self::Postgres(db) => Arc::clone(db) as Arc<dyn mintworks_memory::MemoryStore>,
		}
	}

	#[cfg(feature = "ai")]
	fn threads(&self) -> Arc<dyn mintworks_agent::ThreadStore> {
		match self {
			Self::Sqlite(db) => Arc::clone(db) as Arc<dyn mintworks_agent::ThreadStore>,
			#[cfg(feature = "postgres")]
			Self::Postgres(db) => Arc::clone(db) as Arc<dyn mintworks_agent::ThreadStore>,
		}
	}

	/// The content modules `features` needs — before the script's `app.table` reconcile.
	async fn migrate(&self, features: &BTreeSet<String>) -> ClResult<()> {
		match self {
			Self::Sqlite(db) => {
				db.migrate(&app_db_modules!(features, mintworks_appdb_sqlite)).await
			}
			#[cfg(feature = "postgres")]
			Self::Postgres(db) => db.migrate(&app_db_modules!(features, mintworks_appdb_postgres)).await,
		}
	}
}

/// A PostgreSQL `APP_DB_URL` selects that adapter; unset, the app DB is the SQLite file
/// [`app_db_path`] resolves. Read by hand for the same reason as `app_db_path`.
fn open_app_db(config: &Config, db_url: Option<&str>) -> ClResult<AppDbHandle> {
	match std::env::var(env_name("app_db_url")).ok().filter(|v| !v.trim().is_empty()) {
		Some(url) => postgres_app_db(&url, db_url),
		None => Ok(AppDbHandle::Sqlite(Arc::new(SqliteAppDb::new(app_db_path(config)?)))),
	}
}

/// Only a PostgreSQL URL is accepted: a SQLite app DB is named by `APP_DB_PATH`. `db_url` is the
/// framework store's `DB_URL`, which it must not name.
fn postgres_app_db(url: &str, db_url: Option<&str>) -> ClResult<AppDbHandle> {
	if !is_postgres_url(url) {
		return Err(Error::Setting(
			"setting 'app_db_url': not a postgres:// or postgresql:// URL".into(),
		));
	}
	#[cfg(not(feature = "postgres"))]
	return {
		let _ = db_url;
		Err(Error::Setting(
			"setting 'app_db_url': a PostgreSQL app database needs mintworks built with --features postgres"
				.into(),
		))
	};
	#[cfg(feature = "postgres")]
	return {
		if let Some(db_url) = db_url {
			if same_pg_database(url, db_url)? {
				return Err(Error::internal(
					"APP_DB_URL names the framework database DB_URL; it must be a separate database",
				));
			}
			// Script SQL under the core DB's role could `pg_terminate_backend` its sessions.
			if pg_options(url)?.get_username() == pg_options(db_url)?.get_username() {
				return Err(Error::internal(
					"APP_DB_URL and DB_URL share a role; give the app database a role of its own",
				));
			}
		}
		Ok(AppDbHandle::Postgres(Arc::new(PgAppDb::new(url.to_owned()))))
	};
}

/// Host, port and database name compared after parsing, so spelling, credentials and query
/// parameters cannot disguise one database as two. An unnamed database defaults to the user's.
#[cfg(feature = "postgres")]
fn same_pg_database(a: &str, b: &str) -> ClResult<bool> {
	let key = |url: &str| {
		let o = pg_options(url)?;
		// Loopback spellings only; another name resolving to the same server still
		// passes, and catching it needs a DNS lookup or asking each server for its identity.
		let host = o.get_host().to_ascii_lowercase();
		let host = match host.strip_suffix('.').unwrap_or(&host) {
			"localhost" | "127.0.0.1" | "::1" | "[::1]" => "local",
			h if h.starts_with('/') => "local",
			h => h,
		};
		let db = o.get_database().unwrap_or(o.get_username()).to_owned();
		Ok::<_, Error>((host.to_owned(), o.get_port(), db))
	};
	Ok(key(a)? == key(b)?)
}

#[cfg(feature = "postgres")]
pub(crate) fn pg_options(
	url: &str,
) -> ClResult<mintworks_store_postgres::sqlx::postgres::PgConnectOptions> {
	use std::str::FromStr;
	mintworks_store_postgres::sqlx::postgres::PgConnectOptions::from_str(url)
		.map_err(|e| Error::Setting(format!("not a valid PostgreSQL URL: {e}")))
}

/// Where the app database file lives. Read by hand like `dist_dir` and `script_fs_root`: the
/// value is needed before any `App` exists to resolve a row against, and [`SETTINGS`] is what
/// types and documents it. Hanging the default off `data_dir` is what gives `mintworks test` its
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
		// Legacy for one release after the script.db → app.db rename: delete this arm and the adopt_legacy_db call then.
		(None, Some(p)) => {
			tracing::warn!("SCRIPT_DB_PATH is deprecated; set APP_DB_PATH instead");
			PathBuf::from(p)
		}
		(None, None) => {
			let path = data_dir.join("app.db");
			mintworks_core::config::adopt_legacy_db(&data_dir.join("script.db"), &path)?;
			path
		}
	};
	not_the_framework_db(path, db_path)
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

#[cfg(feature = "ai")]
pub trait AiStores:
	mintworks_agent::AgentRunStore + mintworks_llm::LlmStore + mintworks_search::SearchStore
{
}
#[cfg(feature = "ai")]
impl<T: mintworks_agent::AgentRunStore + mintworks_llm::LlmStore + mintworks_search::SearchStore>
	AiStores for T
{
}
#[cfg(not(feature = "ai"))]
pub trait AiStores {}
#[cfg(not(feature = "ai"))]
impl<T> AiStores for T {}

/// Every store trait the composition hands out, plus the two operations no trait carries: one
/// adapter per database, each implementing all of them.
#[async_trait(?Send)]
pub trait FrameworkStore:
	CoreStore
	+ AuthStore
	+ mintworks_core::refs::RefStore
	+ InvoiceStore
	+ NavStore
	+ BillingStore
	+ mintworks_entitle::EntitleStore
	+ mintworks_plans::PlanStore
	+ mintworks_pdf::DocumentStore
	+ ObjectStore
	+ AiStores
	+ 'static
{
	/// Runs `body` in one write transaction: committed on `Ok`, rolled back on `Err`.
	async fn run_scoped(&self, body: TxBody<'_>) -> ClResult<serde_json::Value>;

	/// One raw statement on the write pool — the test runner's seed, nothing else.
	async fn execute(&self, sql: &'static str) -> ClResult<()>;
}

/// The two adapters' transaction APIs are the same shape but distinct types.
macro_rules! framework_store {
	($store:ty) => {
		#[async_trait(?Send)]
		impl FrameworkStore for $store {
			async fn run_scoped(&self, body: TxBody<'_>) -> ClResult<serde_json::Value> {
				let tx = self.write_tx().await?;
				// No timeout here: `tx::with` puts `script.tx_timeout_ms` on `body` itself, and a
				// second one around `run` would abandon an open transaction mid-commit.
				match <$store>::scope_writes(&tx, body).await {
					Ok(v) => {
						tx.commit().await?;
						Ok(v)
					}
					Err(e) => {
						// `e` is the answer, not the rollback's own failure: losing the block's
						// errCode to a secondary fault turns a 409 the client handles into a 500.
						if let Err(fault) = tx.rollback().await {
							tracing::error!(error = %fault, "rolling back a tx::with block failed");
						}
						Err(e)
					}
				}
			}

			async fn execute(&self, sql: &'static str) -> ClResult<()> {
				sqlx::query(sql)
					.execute(self.write_pool())
					.await
					.map(|_| ())
					.map_err(|e| Error::internal(format!("{sql}: {e}")))
			}
		}
	};
}

framework_store!(SqliteStore);
#[cfg(feature = "postgres")]
framework_store!(PgStore);

/// How a script's `tx::with` block becomes a real transaction.
///
/// Nothing is rebound: every store trait object the `App` holds is this same store, and
/// `scope_writes` makes each pooled handle on the task join `tx` for the duration of the body.
struct StoreTxHook<S>(S);

#[async_trait(?Send)]
impl<S: FrameworkStore> TxHook for StoreTxHook<S> {
	async fn run(&self, _app: &App, body: TxBody<'_>) -> ClResult<serde_json::Value> {
		self.0.run_scoped(body).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[cfg(feature = "postgres")]
	#[test]
	fn the_app_db_must_not_be_the_pg_framework_db() {
		let core = "postgres://core:pw@db.example:5432/mintworks";
		for (url, same) in [
			(core, true),
			("postgresql://app:other@db.example/mintworks?sslmode=require", true),
			("postgres://core:pw@db.example:5432/app", false),
			("postgres://core:pw@db.example:5433/mintworks", false),
			("postgres://core:pw@DB.example.:5432/mintworks", true),
		] {
			assert_eq!(same_pg_database(url, core).unwrap(), same, "{url}");
		}
		let local = "postgres://u@localhost/mintworks";
		for url in [
			"postgres://u@127.0.0.1/mintworks",
			"postgres://u@[::1]/mintworks",
			"postgres:///mintworks?host=/run/postgresql&user=u",
		] {
			assert!(same_pg_database(url, local).unwrap(), "{url}");
		}
	}

	#[cfg(feature = "postgres")]
	#[test]
	fn the_app_db_must_not_share_the_framework_role() {
		let core = Some("postgres://core:pw@db.example/mintworks");
		assert!(postgres_app_db("postgres://core:pw@db.example/app", core).is_err());
		assert!(postgres_app_db("postgres://app:pw@db.example/app", core).is_ok());
	}

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
		let dir = std::env::temp_dir().join(format!("mintworks-{name}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	#[test]
	fn app_db_url_takes_only_a_postgres_url() {
		assert!(postgres_app_db("sqlite://app.db", None).is_err());
		let pg = postgres_app_db("postgresql://localhost/app", None);
		#[cfg(feature = "postgres")]
		assert!(matches!(pg, Ok(AppDbHandle::Postgres(_))));
		#[cfg(not(feature = "postgres"))]
		assert!(matches!(pg, Err(e) if e.to_string().contains("--features postgres")));
	}

	#[tokio::test]
	async fn db_url_takes_only_a_postgres_url() {
		assert!(postgres_store("sqlite://core.db").await.is_err());
		#[cfg(not(feature = "postgres"))]
		assert!(matches!(
			postgres_store("postgres://localhost/core").await,
			Err(e) if e.to_string().contains("--features postgres")
		));
	}

	#[cfg(feature = "postgres")]
	#[test]
	fn the_pg_app_db_must_not_be_the_framework_db() {
		let core = "postgres://u:p@db.example:5432/core";
		for same in [
			"postgresql://other:pw@db.example/core",
			"postgres://u@db.example:5432/core?sslmode=require",
		] {
			assert!(postgres_app_db(same, Some(core)).is_err(), "{same}");
		}
		for other in [
			"postgres://a@db.example/app",
			"postgres://a@db.example:5433/core",
			"postgres://a@elsewhere/core",
		] {
			assert!(postgres_app_db(other, Some(core)).is_ok(), "{other}");
		}
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
