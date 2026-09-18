//! Boot-time seeding, run from `on_init` on every start and therefore idempotent.

use std::sync::Arc;

use saas_auth::store::{AuthStore, LegalKind, NewLegalDoc};
use saas_core::{App, Ctx, prelude::*};
use saas_invoice::{
	Invoices, SELLER_ID, Seller, VatCode,
	store::{InvoiceStore, SellerVersionPatch, ServiceDef},
};
use sha2::{Digest, Sha256};

/// Bump whenever `legal/*.md` changes — forwards only: a new version re-gates every account
/// until it consents again, and an already-published version is left alone.
const LEGAL_VERSION: &str = "2026-09-17";

pub async fn run(app: App) -> ClResult<()> {
	let ctx = Ctx::system("seed");
	// First, before the database is touched: a bad `email.template_dir` or an undecryptable
	// `smtp.password` otherwise surfaces only as `SEND_EMAIL` jobs retrying on backoff, so a
	// deployment can drop every activation link it sends and nothing says so.
	saas_email::check_email_settings(&app).await?;
	seller(&app).await?;
	services(&app, &ctx).await?;
	legal(&app).await?;
	secret_from_env(&app, "smtp.password", "SMTP_PASSWORD").await?;
	nav(&app).await
}

fn invoice_store(app: &App) -> ClResult<Arc<dyn InvoiceStore>> {
	app.extensions
		.get::<Arc<dyn InvoiceStore>>()
		.cloned()
		.ok_or_else(|| Error::internal("Arc<dyn InvoiceStore> is not registered as an extension"))
}

/// `base:TaxNumberType`: 11 digits, the first 8 being the `taxpayerId` NAV splits off and
/// cross-checks every reported invoice against. Punctuation is how it is usually written
/// (`12345678-2-42`), so it is stripped rather than refused.
fn tax_number(raw: &str) -> ClResult<String> {
	let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
	if digits.len() != 11 {
		return Err(Error::internal(format!(
			"SELLER_TAX_NUMBER must be 11 digits (e.g. 12345678-2-42), got {}",
			digits.len()
		)));
	}
	Ok(digits)
}

/// Upsert on `sellers.id = 1`, the id every call site in v1 hardcodes.
async fn seller(app: &App) -> ClResult<()> {
	let store = invoice_store(app)?;
	// The operational half is env-driven on every boot: it is deployment configuration, and a
	// redeploy pointing at a new NAV endpoint has to take effect.
	store
		.put_seller(&Seller {
			id: SELLER_ID,
			// Blank on purpose: it falls back to `settings['nav.base_url']`, so the demo's
			// NAV endpoint is configured in one place and not baked into this row.
			nav_base_url: String::new(),
			nav_login: env_opt("NAV_LOGIN"),
			series_code: "EX".to_owned(),
			created_at: Timestamp::now(),
		})
		.await?;

	// The statutory half is seeded **once**. After the first boot the database is the source of
	// truth: an operator edits it through `PATCH /api/seller/draft` + `POST /api/seller/publish`,
	// and re-publishing the env on every restart would either undo that or stack up an identical
	// version per boot.
	if store.current_seller_version(SELLER_ID).await?.is_some() {
		return Ok(());
	}
	// Fatal, unlike the incomplete NAV block `nav()` below merely warns about: filing is
	// optional here, a seller identity is not. A placeholder still issues numbered, immutable
	// invoices under a taxpayer that is not the operator's, and those cannot be corrected.
	let tax_number = tax_number(&env_req("SELLER_TAX_NUMBER")?)?;
	// Not derived from the tax number: an EU VAT id exists only once a company registers for
	// intra-Community trade, and `issue::vies` sends this one as the requester — a fabricated
	// id asks VIES about a registration that was never made.
	let eu_vat_id = match env_opt("SELLER_EU_VAT_ID") {
		Some(raw) => Some(saas_invoice::vies::normalise(&raw)?.0),
		None => None,
	};
	store
		.save_seller_version_draft(
			SELLER_ID,
			&SellerVersionPatch {
				name: Some(env_req("SELLER_NAME")?),
				country: Some("HU".to_owned()),
				tax_number: Some(tax_number),
				eu_vat_id: eu_vat_id.map_or(Patch::Undefined, Patch::Value),
				postcode: Some(env_req("SELLER_POSTCODE")?),
				city: Some(env_req("SELLER_CITY")?),
				street: Some(env_req("SELLER_STREET")?),
				// Optional: an invoice with no bank account is legal, one with a wrong one is not.
				bank_account: env_opt("SELLER_BANK_ACCOUNT").map_or(Patch::Undefined, Patch::Value),
				bank_name: env_opt("SELLER_BANK_NAME").map_or(Patch::Undefined, Patch::Value),
				..Default::default()
			},
		)
		.await?;
	store.publish_seller_version(SELLER_ID, Timestamp::now(), &|_| Ok(())).await?;
	Ok(())
}

/// The two things this demo business sells. `sync_services` upserts by `code`, so editing a
/// price here is an update and never a second row.
async fn services(app: &App, ctx: &Ctx) -> ClResult<()> {
	// `Money` is minor units at two decimals for every currency, HUF included: 15 000 HUF is
	// `Money(1_500_000)`, not `Money(15_000)`.
	Invoices::new(app.clone())
		.sync_services(
			ctx,
			&[
				ServiceDef {
					code: "CONSULT".to_owned(),
					name: "Consulting".to_owned(),
					description: Some("Advisory work, billed by the hour.".to_owned()),
					unit: "hour".to_owned(),
					unit_price: Money(1_500_000),
					vat_code: VatCode::Std27,
				},
				ServiceDef {
					code: "SITEVISIT".to_owned(),
					name: "Site visit".to_owned(),
					description: Some("An on-site visit, billed by the occasion.".to_owned()),
					unit: "occasion".to_owned(),
					unit_price: Money(2_500_000),
					vat_code: VatCode::Std27,
				},
			],
		)
		.await
}

async fn legal(app: &App) -> ClResult<()> {
	let store = saas_auth::routes::store(app)?;
	publish(&store, LegalKind::Tos, "Terms of Service", include_str!("../legal/terms.md")).await?;
	publish(&store, LegalKind::Privacy, "Privacy Policy", include_str!("../legal/privacy.md")).await
}

async fn publish(
	store: &Arc<dyn AuthStore>,
	kind: LegalKind,
	title: &str,
	body: &str,
) -> ClResult<()> {
	// Idempotent on the row's existence, not on "is it the current one": comparing against
	// `current_legal_doc` re-inserted `LEGAL_VERSION` whenever a *newer* version was already
	// published, and the `(kind, locale, version)` UNIQUE then killed boot.
	let res = store
		.insert_legal_doc(&NewLegalDoc {
			kind,
			locale: "en".to_owned(),
			version: LEGAL_VERSION.to_owned(),
			title: title.to_owned(),
			body: body.to_owned(),
			sha256: hex::encode(Sha256::digest(body.as_bytes())),
			effective_from: Timestamp::now(),
		})
		.await;
	match res {
		Ok(_) | Err(Error::Conflict(_)) => Ok(()),
		Err(e) => Err(e),
	}
}

/// Bootstrap one secret out of the environment; `true` when it is set afterwards.
///
/// A secret is not a setting: it lives encrypted in `secrets` and is never read back out over
/// HTTP, so `SAAS_<KEY>` does not resolve it and this is the only channel in. An already-set
/// value wins, so an operator's rotation is not undone on the next restart.
async fn secret_from_env(app: &App, key: &str, var: &str) -> ClResult<bool> {
	if app.secrets.status(key).await?.set {
		return Ok(true);
	}
	let Some(value) = env_opt(var) else { return Ok(false) };
	app.secrets.set(key, value.as_bytes(), None).await?;
	Ok(true)
}

/// The `software` block NAV requires on every request. Constants, not settings or environment:
/// they identify this program, which is the same in every deployment of it, and four of them are
/// `.required()` — a blank one used to take boot down. `software_id` is exactly 18 characters of
/// `[0-9A-Z-]`. `nav.software_dev_tax_number` stays unset: the registry does not require it, and
/// a blank optional field is simply left out of the block.
const NAV_SOFTWARE: &[(&str, &str)] = &[
	("nav.software_id", "SAASEXAMPLE0000001"),
	("nav.software_name", "saas-framework example"),
	("nav.software_operation", "LOCAL_SOFTWARE"),
	("nav.software_main_version", "0.1"),
	("nav.software_dev_name", "saas-framework"),
	("nav.software_dev_contact", "dev@example.com"),
	("nav.software_dev_country", "HU"),
];

/// NAV reporting is optional here, so an incomplete configuration warns instead of failing:
/// `saas_nav::job::seed` refuses to start on a `software` block NAV would reject, and that
/// error out of `on_init` would take the whole demo down with it.
async fn nav(app: &App) -> ClResult<()> {
	// Unconditional: the constants above are authoritative, so a row edited by hand is
	// restored on the next boot rather than silently outliving the code it describes.
	for (key, value) in NAV_SOFTWARE {
		app.settings.set(key, value, None).await?;
	}
	let mut ready = true;
	for (key, var) in [
		("nav.tech_password", "NAV_TECH_PASSWORD"),
		("nav.sign_key", "NAV_SIGN_KEY"),
		("nav.exchange_key", "NAV_EXCHANGE_KEY"),
	] {
		// Not short-circuited: all three are seeded even when one is missing, so filling the
		// gap in is one restart rather than three.
		ready &= secret_from_env(app, key, var).await?;
	}
	if !ready {
		tracing::warn!("NAV secrets are incomplete — invoices will not be filed");
		return Ok(());
	}
	// Warn, never fail: what is left for an operator to get wrong is `nav.base_url` and the
	// seller's own identity, and neither is worth taking a demo that files nothing down with.
	if let Err(e) = saas_nav::job::seed(app).await {
		tracing::warn!(error = %e, "NAV is not fully configured — invoices will not be filed");
	}
	Ok(())
}

fn env_opt(var: &str) -> Option<String> {
	std::env::var(var).ok().filter(|v| !v.is_empty())
}

/// Required, like `SELLER_TAX_NUMBER`: these land on numbered, immutable invoices and in the
/// NAV `supplierAddress`, where a placeholder cannot be corrected afterwards.
fn env_req(var: &str) -> ClResult<String> {
	env_opt(var).ok_or_else(|| {
		Error::internal(format!("{var} must be set; see example/backend/.env.example"))
	})
}

#[cfg(test)]
mod tests {
	use super::tax_number;

	#[test]
	fn a_printed_tax_number_keeps_only_its_digits() {
		assert_eq!(tax_number("12345678-2-42").unwrap(), "12345678242");
		assert_eq!(tax_number("12345678242").unwrap(), "12345678242");
	}

	#[test]
	fn a_tax_number_of_the_wrong_length_is_refused() {
		assert!(tax_number("12345678").is_err());
		assert!(tax_number("123456782421").is_err());
		assert!(tax_number("").is_err());
	}
}

// vim: ts=4
