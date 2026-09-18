#![forbid(unsafe_code)]
//! The composition root: it opens the store, migrates it, mounts the route bundles this
//! application wants and serves the built SPA from the same binary.

use std::sync::Arc;

use saas_auth::store::AuthStore;
use saas_core::{AppBuilder, ClResult, config::Config, store::CoreStore};
use saas_invoice::store::InvoiceStore;
use saas_nav::store::NavStore;
use store_adapter_sqlite::{FRAMEWORK, SqliteStore};
use tower_http::services::{ServeDir, ServeFile};

use example_backend::store::{BookingStore, EXAMPLE};
use example_backend::{bookings, routes, seed};

#[tokio::main]
async fn main() -> ClResult<()> {
	// Before anything reads the environment, and resolved against the crate rather than the
	// cwd. Missing is not an error: a real deployment sets real environment variables, and no
	// framework crate loads a file at all — `Config::from_env` reads the process environment.
	let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"));
	let config = Config::from_env();
	let store = SqliteStore::open(&config).await?;
	// One runner, one file, one transaction, two independently versioned modules. The framework
	// is listed first because `bookings.tenant_id` references `tenants(id)`.
	store.migrate(&[FRAMEWORK, EXAMPLE]).await?;

	let invoices: Arc<dyn InvoiceStore> = Arc::new(store.clone());

	AppBuilder::new()
		.config(config)
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		// `Nav` and `Invoices` resolve their stores out of this map at call time, so a missing
		// extension is a job failing at runtime rather than a compile error.
		.extension(Arc::new(store.clone()) as Arc<dyn AuthStore>)
		.extension(Arc::clone(&invoices))
		// The same handle a fourth time, behind this application's own trait: consumer tables
		// share the framework's database file and its transactions.
		.extension(Arc::new(store.clone()) as Arc<dyn BookingStore>)
		.extension(Arc::new(store) as Arc<dyn NavStore>)
		// Assembled in `routes::api` so `tests/flow.rs` drives the same thing the browser does.
		.routes(routes::api().fallback_service(spa()))
		// `ALERT_SWEEP` only knows about jobs; a NAV rejection ends its job successfully, so
		// without these three the sweep never sees `A-NAV-REJECTED`, `A-RATE-MISSING` or
		// `A-BOOKING-ORPHANED` at all.
		.alerts(saas_nav::alerts)
		.alerts(saas_invoice::service_api::alerts)
		.alerts(bookings::alerts)
		.jobs(move |runner, app| {
			saas_auth::job::register(runner, app.clone());
			saas_email::job::register(runner, app.clone());
			saas_invoice::mnb::register(runner, app.clone());
			saas_invoice::draft::register(runner, app.clone(), Arc::clone(&invoices));
			saas_invoice::pdf::register(runner, app.clone(), invoices);
			saas_nav::job::register(runner, app);
		})
		.on_init(seed::run)
		.run()
		.await
}

/// The built SPA. Every unmatched path falls back to `index.html` so a deep link reloads;
/// it is the router's fallback, so every registered API route still wins.
fn spa() -> ServeDir<ServeFile> {
	let dist = std::env::var("DIST_DIR").unwrap_or_else(|_| "../frontend/dist".to_owned());
	let index = std::path::Path::new(&dist).join("index.html");
	ServeDir::new(&dist).fallback(ServeFile::new(index))
}

// vim: ts=4
