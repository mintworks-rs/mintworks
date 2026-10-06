#![forbid(unsafe_code)]
//! The composition root: it opens the store, migrates it, mounts the route bundles this
//! application wants and serves the built SPA from the same binary.

use std::sync::Arc;

use mintworks_auth::store::AuthStore;
use mintworks_billing::{BillingStore, PaymentProviders};
use mintworks_core::{AppBuilder, ClResult, config::Config, store::CoreStore};
use mintworks_invoice::store::InvoiceStore;
use mintworks_nav::store::NavStore;
use mintworks_payment_barion::BarionProvider;
use mintworks_store_sqlite::{FRAMEWORK, SqliteStore};
use tower_http::services::{ServeDir, ServeFile};

use booking::store::{BookingStore, EXAMPLE};
use booking::{NAV_SOFTWARE_PROD, NAV_SOFTWARE_TEST, bookings, routes, seed};

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
	// dropped the customer on a DRAFT invoice with no redirect and no explanation — and half a
	// configuration is the same failure one step later, so the payee counts too: `start` sends it.
	let configured = ["PAYMENT_BARION_POS_KEY", "PAYMENT_BARION_PAYEE"]
		.iter()
		.all(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()));
	let mut providers = PaymentProviders::new();
	if configured {
		providers = providers.with(barion);
	}

	let builder = AppBuilder::new()
		.config(config)
		.store(Arc::new(store.clone()) as Arc<dyn CoreStore>)
		// Registering a crate's slice is what opts this deployment into being asked for that
		// crate's configuration at boot; `mintworks-core`'s own is always registered.
		.settings(mintworks_auth::SETTINGS)
		.settings(mintworks_email::SETTINGS)
		.settings(mintworks_invoice::SETTINGS)
		.settings(mintworks_nav::SETTINGS)
		.settings(mintworks_billing::SETTINGS)
		.settings(booking::SETTINGS)
		.secrets(mintworks_auth::SECRETS)
		.secrets(mintworks_email::SECRETS)
		.secrets(mintworks_nav::SECRETS)
		.secrets(mintworks_payment_barion::SECRETS);
	let builder = NAV_SOFTWARE_TEST
		.iter()
		.fold(builder, |b, &(k, v)| b.setting_default_for("test", k, v));
	let builder = NAV_SOFTWARE_PROD
		.iter()
		.fold(builder, |b, &(k, v)| b.setting_default_for("production", k, v));
	builder
		// `Nav` and `Invoices` resolve their stores out of this map at call time, so a missing
		// extension is a job failing at runtime rather than a compile error.
		.extension(Arc::new(store.clone()) as Arc<dyn AuthStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn mintworks_core::refs::RefStore>)
		.extension(Arc::clone(&invoices))
		// The same handle a fourth time, behind this application's own trait: consumer tables
		// share the framework's database file and its transactions.
		.extension(Arc::new(store.clone()) as Arc<dyn BookingStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn BillingStore>)
		// Registered even when empty: `mintworks_billing::provider::providers` is an error when the
		// extension is missing, and every billing route reads it.
		.extension(Arc::new(providers))
		.extension(Arc::new(store) as Arc<dyn NavStore>)
		// Assembled in `routes::api` so `tests/flow.rs` drives the same thing the browser does.
		// `api()` hands back a `Scoped`, which carries the registered scope prefixes; `Scoped::with`
		// applies the SPA fallback without dropping them.
		.routes(
			routes::api()
				.merge(mintworks_core::refs::routes(&mintworks_auth::routes::consent_gate()))
				.with(|r| r.fallback_service(spa(&booking::dist_dir()))),
		)
		// `ALERT_SWEEP` only knows about jobs; a NAV rejection ends its job successfully, so
		// without these three the sweep never sees `A-NAV-REJECTED`, `A-RATE-MISSING` or
		// `A-BOOKING-ORPHANED` at all.
		.alerts(mintworks_nav::alerts)
		.alerts(mintworks_invoice::service_api::alerts)
		.alerts(mintworks_billing::alerts)
		.alerts(bookings::alerts)
		.jobs(move |runner, app| {
			mintworks_auth::job::register(runner, app.clone());
			mintworks_email::job::register(runner, app.clone());
			mintworks_invoice::mnb::register(runner, app.clone());
			mintworks_invoice::draft::register(runner, app.clone(), Arc::clone(&invoices));
			mintworks_invoice::pdf::register(runner, app.clone(), invoices);
			mintworks_billing::dunning::register(runner, app.clone());
			mintworks_billing::sweep::register(runner, app.clone());
			mintworks_nav::job::register(runner, app);
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
