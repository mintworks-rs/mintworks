#![forbid(unsafe_code)]
//! The composition root: it opens the store, migrates it, mounts the route bundles this
//! application wants and serves the built SPA from the same binary.

use std::sync::Arc;

use payment_adapter_barion::BarionProvider;
use saas_auth::store::AuthStore;
use saas_billing::{BillingStore, PaymentProviders};
use saas_core::{AppBuilder, ClResult, config::Config, store::CoreStore};
use saas_invoice::store::InvoiceStore;
use saas_nav::store::NavStore;
use store_adapter_sqlite::{FRAMEWORK, SqliteStore};
use tower_http::services::{ServeDir, ServeFile};

use saas_example::store::{BookingStore, EXAMPLE};
use saas_example::{bookings, routes, seed};

#[tokio::main]
async fn main() -> ClResult<()> {
	// Before anything reads the environment, and resolved against the crate rather than the
	// cwd. Missing is not an error: a real deployment sets real environment variables, and no
	// framework crate loads a file at all — `Config::from_env` reads the process environment.
	let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"));
	let config = Config::from_env();
	let store = SqliteStore::open(&config).await?;
	// One runner, one file, one transaction, two independently versioned modules. The framework
	// is listed first because `bookings.org_id` references `orgs(id)`.
	store.migrate(&[FRAMEWORK, EXAMPLE]).await?;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());
	// Constructed before `build()` because the extension map is frozen by it, and handed the
	// `App` from `on_init`, which is where the credentials it reads become resolvable.
	let barion = Arc::new(BarionProvider::deferred());
	let attach = Arc::clone(&barion);
	// The gate on "is a gateway configured at all", and the env var rather than the credentials
	// because `build` freezes the extension map before any `App` exists to resolve them against.
	// Registered unconditionally, the card button showed with no POS key behind it and checkout
	// dropped the customer on a DRAFT invoice with no redirect and no explanation.
	let mut providers = PaymentProviders::new();
	if std::env::var("PAYMENT_BARION_POS_KEY").is_ok_and(|v| !v.trim().is_empty()) {
		providers = providers.with(barion);
	}

	AppBuilder::new()
		.config(config)
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		// Registering a crate's slice is what opts this deployment into being asked for that
		// crate's configuration at boot; `saas-core`'s own is always registered.
		.settings(saas_auth::SETTINGS)
		.settings(saas_email::SETTINGS)
		.settings(saas_invoice::SETTINGS)
		.settings(saas_nav::SETTINGS)
		.settings(saas_billing::SETTINGS)
		.settings(saas_example::SETTINGS)
		.secrets(saas_auth::SECRETS)
		.secrets(saas_email::SECRETS)
		.secrets(saas_nav::SECRETS)
		.secrets(payment_adapter_barion::SECRETS)
		// NAV registers software per developer, so this block is the deployment's identity and
		// is compiled in. A default, not a row: `.env` still overrides it, which a row never
		// could. `nav.software_dev_tax_number` stays unset — optional, and a blank optional
		// field is simply left out of the `software` block.
		.setting_default("nav.software_id", "SAASEXAMPLE0000001")
		.setting_default("nav.software_name", "saas-framework example")
		.setting_default("nav.software_operation", "LOCAL_SOFTWARE")
		.setting_default("nav.software_main_version", "0.1")
		.setting_default("nav.software_dev_name", "saas-framework")
		.setting_default("nav.software_dev_contact", "dev@example.com")
		.setting_default("nav.software_dev_country", "HU")
		// `Nav` and `Invoices` resolve their stores out of this map at call time, so a missing
		// extension is a job failing at runtime rather than a compile error.
		.extension(Arc::new(store.clone()) as Arc<dyn AuthStore>)
		.extension(Arc::clone(&invoices))
		// The same handle a fourth time, behind this application's own trait: consumer tables
		// share the framework's database file and its transactions.
		.extension(Arc::new(store.clone()) as Arc<dyn BookingStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn BillingStore>)
		// Registered even when empty: `saas_billing::provider::providers` is an error when the
		// extension is missing, and every billing route reads it.
		.extension(Arc::new(providers))
		.extension(Arc::new(store) as Arc<dyn NavStore>)
		// Assembled in `routes::api` so `tests/flow.rs` drives the same thing the browser does.
		// `api()` hands back a `Scoped`, which carries the registered scope prefixes; `Scoped::with`
		// applies the SPA fallback without dropping them.
		.routes(
			routes::api().with(|r| r.fallback_service(spa(&saas_example::dist_dir()))),
		)
		// `ALERT_SWEEP` only knows about jobs; a NAV rejection ends its job successfully, so
		// without these three the sweep never sees `A-NAV-REJECTED`, `A-RATE-MISSING` or
		// `A-BOOKING-ORPHANED` at all.
		.alerts(saas_nav::alerts)
		.alerts(saas_invoice::service_api::alerts)
		.alerts(saas_billing::alerts)
		.alerts(bookings::alerts)
		.jobs(move |runner, app| {
			saas_auth::job::register(runner, app.clone());
			saas_email::job::register(runner, app.clone());
			saas_invoice::mnb::register(runner, app.clone());
			saas_invoice::draft::register(runner, app.clone(), Arc::clone(&invoices));
			saas_invoice::pdf::register(runner, app.clone(), invoices);
			saas_billing::dunning::register(runner, app.clone());
			saas_billing::sweep::register(runner, app.clone());
			saas_nav::job::register(runner, app);
		})
		.on_init(move |app| {
			let attach = Arc::clone(&attach);
			async move {
				seed::run(app.clone()).await?;
				attach.attach(&app);
				Ok(())
			}
		})
		.run()
		.await
}

/// The built SPA. Every unmatched path falls back to `index.html` so a deep link reloads;
/// it is the router's fallback, so every registered API route still wins.
fn spa(dist: &str) -> ServeDir<ServeFile> {
	let index = std::path::Path::new(dist).join("index.html");
	ServeDir::new(dist).fallback(ServeFile::new(index))
}

// vim: ts=4
