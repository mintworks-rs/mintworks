// SPDX-License-Identifier: MIT-0
//! Boot-time seeding, run from `on_init` on every start and therefore idempotent.

use std::sync::Arc;

use mintworks_auth::store::{AuthStore, LegalKind, NewLegalDoc};
use mintworks_core::ids::SellerId;
use mintworks_core::{App, Ctx, prelude::*};
use mintworks_invoice::{
	Invoices, Seller, VatCode,
	store::{InvoiceStore, SellerVersionPatch, ServiceDef},
};
use sha2::{Digest, Sha256};

/// Bump whenever `legal/*.md` changes — forwards only: a new version re-gates every account
/// until it consents again, and an already-published version is left alone.
const LEGAL_VERSION: &str = "2026-09-17";

/// The example mints exactly one seller, under the root org. `sellers.id` is not
/// auto-assigned — `put_seller` takes it — so a fresh database needs a literal.
const SELLER_ID: i64 = 1;

pub async fn run(app: App) -> ClResult<()> {
	// Early, before the database is touched: a bad `email.template_dir` or an undecryptable
	// `email.smtp.password` otherwise surfaces only as `SEND_EMAIL` jobs retrying on backoff, so a
	// deployment can drop every activation link it sends and nothing says so.
	mintworks_email::check_email_settings(&app).await?;
	// The root org is the deployment's own: it owns the seller and the service catalogue, and
	// `Invoices` resolves both by walking up from `ctx.org()`, which a bare `Ctx::system` has not.
	let org_id = app.store.root_org_id().await?;
	let ctx = Ctx::system("seed").with_org(org_id);
	seller(&app, &ctx, org_id).await?;
	services(&app, &ctx).await?;
	legal(&app).await?;
	// `dunning::register` only installs the handler; without this seed the periodic chain has
	// no first job and no reminder is ever sent.
	mintworks_billing::dunning::seed(&app.store).await?;
	mintworks_billing::sweep::seed(&app.store).await?;
	// `register` installs handlers only — the same reason the two billing seeds above exist.
	// Without these, abandoned drafts are never swept, an invoice whose RENDER_PDF was never
	// enqueued never gets one, and no MNB rate is ever fetched.
	mintworks_invoice::draft::seed(&app.store).await?;
	mintworks_core::job::seed_periodic(&app.store, mintworks_invoice::mnb::KIND).await?;
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

/// The deployment's seller, owned by the root org.
///
/// Idempotent on the row rather than on a fixed id: `sellers.uid` and `.org_id` are
/// insert-only in `put_seller`, so a second boot has to reuse what the first minted.
async fn seller(app: &App, ctx: &Ctx, org_id: i64) -> ClResult<()> {
	let store = invoice_store(app)?;
	let existing = store.seller_for_org(org_id).await?;
	let seller_id = existing.as_ref().map_or(SELLER_ID, |s| s.id);
	// The operational half is env-driven on every boot: it is deployment configuration, and a
	// redeploy pointing at a new NAV endpoint has to take effect.
	store
		.put_seller(&Seller {
			id: seller_id,
			uid: existing.as_ref().map_or_else(SellerId::generate, |s| s.uid.clone()),
			org_id,
			// Blank on purpose: it falls back to the settings, so the demo's NAV endpoint is
			// `DEPLOYMENT_ENV` in one place and not baked into this row.
			nav_base_url: String::new(),
			nav_login: env_opt("NAV_LOGIN"),
			series_code: env_req("SELLER_SERIES_CODE")?,
			closed_at: None,
			payment_days: None,
			created_at: Timestamp::now(),
		})
		.await?;

	// The statutory half is re-read from the env on **every** boot: `Invoices::sync_seller`
	// versions only a merged row that differs from the live one, so a changed `SELLER_*` takes
	// effect and an unchanged one stacks nothing; an operator's open draft makes it refuse.
	// Fatal, unlike the incomplete NAV block `nav()` below merely warns about: filing is
	// optional here, a seller identity is not. A placeholder still issues numbered, immutable
	// invoices under a taxpayer that is not the operator's, and those cannot be corrected.
	let tax_number = tax_number(&env_req("SELLER_TAX_NUMBER")?)?;
	// Not derived from the tax number: an EU VAT id exists only once a company registers for
	// intra-Community trade, and `issue::vies` sends this one as the requester — a fabricated
	// id asks VIES about a registration that was never made.
	let eu_vat_id = match env_opt("SELLER_EU_VAT_ID") {
		Some(raw) => Some(mintworks_invoice::vies::normalise(&raw)?.0),
		None => None,
	};
	let patch = SellerVersionPatch {
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
	};
	Invoices::new(app.clone()).sync_seller(ctx, &patch).await?;
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
	let store = mintworks_auth::routes::store(app)?;
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

/// NAV reporting is optional here, so an incomplete configuration warns instead of failing:
/// `mintworks_nav::job::seed` refuses to start on a `sellers` row NAV would reject, and that error
/// out of `on_init` would take the whole demo down with it.
///
/// The `software` block is `lib.rs`'s `NAV_SOFTWARE_TEST`/`_PROD` defaults — below the
/// environment, unlike rows, which would sit above it and never be overridden.
async fn nav(app: &App) -> ClResult<()> {
	let mut ready = true;
	for key in ["nav.tech_password", "nav.sign_key", "nav.exchange_key"] {
		// Not short-circuited: all three are reported even when one is missing, so filling the
		// gap in is one restart rather than three.
		ready &= app.secrets.status(key).await?.set;
	}
	if !ready {
		tracing::warn!("NAV secrets are incomplete — invoices will not be filed");
		return Ok(());
	}
	// Warn, never fail: what is left for an operator to get wrong is `deployment.env` and the
	// seller's own identity, and neither is worth taking a demo that files nothing down with.
	if let Err(e) = mintworks_nav::job::seed(app).await {
		tracing::warn!(error = %e, "NAV is not fully configured — invoices will not be filed");
	}
	Ok(())
}

/// This application's own bootstrap: these seed a
/// **versioned** `seller_versions` row on first boot, which a setting has no version history
/// to model.
fn env_opt(var: &str) -> Option<String> {
	std::env::var(var).ok().filter(|v| !v.is_empty())
}

/// Required, like `SELLER_TAX_NUMBER`: these land on numbered, immutable invoices and
/// in the NAV `supplierAddress`, where a placeholder cannot be corrected afterwards.
fn env_req(var: &str) -> ClResult<String> {
	env_opt(var).ok_or_else(|| {
		Error::internal(format!("{var} must be set; see examples/booking/app-rust/.env.example"))
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
