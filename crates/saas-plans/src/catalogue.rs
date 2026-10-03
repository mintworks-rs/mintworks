//! The declared offer catalogue and its boot reconcile into `offers` rows.
//!
//! A declaration is reconciled by `code`: inserted, updated if changed, and an offer the seller
//! has in the DB but no longer declares is deactivated — never deleted. The declaration wins
//! over an admin edit at the next boot.

use std::collections::BTreeSet;
use std::sync::Arc;

use saas_core::app::App;
use saas_core::error::{ClResult, Error};
use saas_core::ids::OfferId;
use saas_core::money::{CurrencyCode, Money};
use saas_entitle::EntitlementRegistry;

use crate::store::{Interval, NewOffer, OfferEntitlement, OfferKind, OfferPrice, PlanStore};

/// One declared offer. `service` is the seller's `services.code`, which carries the VAT code,
/// unit and line text onto the invoice.
#[derive(Clone, Debug)]
pub struct OfferDef {
	pub code: String,
	pub name: String,
	pub kind: OfferKind,
	pub service: String,
	pub family: Option<String>,
	pub rank: i64,
	pub interval: Option<Interval>,
	pub interval_count: Option<i64>,
	pub validity_days: Option<i64>,
	pub trial_days: i64,
	pub prices: Vec<OfferPrice>,
	pub entitlements: Vec<OfferEntitlement>,
}

impl OfferDef {
	pub fn one_time(
		code: impl Into<String>,
		name: impl Into<String>,
		service: impl Into<String>,
	) -> Self {
		Self {
			code: code.into(),
			name: name.into(),
			kind: OfferKind::OneTime,
			service: service.into(),
			family: None,
			rank: 0,
			interval: None,
			interval_count: None,
			validity_days: None,
			trial_days: 0,
			prices: Vec::new(),
			entitlements: Vec::new(),
		}
	}

	pub fn recurring(
		code: impl Into<String>,
		name: impl Into<String>,
		service: impl Into<String>,
		interval: Interval,
		interval_count: i64,
	) -> Self {
		Self {
			kind: OfferKind::Recurring,
			interval: Some(interval),
			interval_count: Some(interval_count),
			..Self::one_time(code, name, service)
		}
	}

	/// `amount` in the currency's minor units.
	#[must_use]
	pub fn price(mut self, currency: CurrencyCode, amount: i64) -> Self {
		self.prices.push(OfferPrice { currency, amount: Money(amount) });
		self
	}

	#[must_use]
	pub fn entitle(mut self, key: impl Into<String>, amount: i64, per_seat: bool) -> Self {
		self.entitlements.push(OfferEntitlement { key: key.into(), amount, per_seat });
		self
	}

	/// Shape rules the schema's CHECKs would otherwise raise as an opaque boot failure.
	fn check(&self, registry: Option<&EntitlementRegistry>) -> ClResult<()> {
		let bad = |why: &str| Err(Error::internal(format!("offer '{}': {why}", self.code)));
		match self.kind {
			OfferKind::Recurring
				if self.interval.is_none() || self.interval_count.is_none_or(|n| n <= 0) =>
			{
				return bad("a recurring offer needs an interval and intervalCount > 0");
			}
			OfferKind::OneTime
				if self.interval.is_some()
					|| self.interval_count.is_some()
					|| self.trial_days != 0 =>
			{
				return bad("a one-time offer takes no interval, intervalCount or trialDays");
			}
			_ => {}
		}
		if self.trial_days < 0 || self.validity_days.is_some_and(|d| d <= 0) {
			return bad("trialDays must be >= 0 and validityDays > 0");
		}
		if self.prices.iter().any(|p| p.amount.0 < 0)
			|| self.entitlements.iter().any(|e| e.amount < 0)
		{
			return bad("prices and entitlement amounts must be >= 0");
		}
		for e in &self.entitlements {
			if registry.and_then(|r| r.kind(&e.key)).is_none() {
				return bad(&format!("entitlement '{}' is not declared in saas-entitle", e.key));
			}
		}
		Ok(())
	}
}

/// The declarations, read back from `app.extensions` by [`reconcile`].
#[derive(Clone, Debug, Default)]
pub struct OfferCatalogue(pub Arc<Vec<OfferDef>>);

/// The boot entry point: reconciles the [`OfferCatalogue`] extension for the root org, which
/// is the seller of every declared offer. [`crate::install`] registers it as an `on_init`.
pub async fn reconcile(app: App) -> ClResult<()> {
	let Some(catalogue) = app.extensions.get::<OfferCatalogue>().cloned() else {
		return Ok(());
	};
	let store = app
		.extensions
		.get::<Arc<dyn PlanStore>>()
		.cloned()
		.ok_or_else(|| Error::internal("saas-plans: no PlanStore extension registered"))?;
	let invoices = saas_invoice::invoice_store(&app)?;
	for d in catalogue.0.iter() {
		for p in &d.prices {
			let cur = invoices.currency_get(p.currency.as_str()).await?.ok_or_else(|| {
				Error::internal(format!("offer '{}': unknown currency '{}'", d.code, p.currency))
			})?;
			saas_invoice::currency::ensure_price_on_step(p.amount, cur.price_round_step)
				.map_err(|e| Error::internal(format!("offer '{}': {e}", d.code)))?;
		}
	}
	let seller = app.store.root_org_id().await?;
	reconcile_with(&*store, app.extensions.get::<EntitlementRegistry>(), seller, &catalogue.0).await
}

/// [`reconcile`] with its inputs explicit. Every declaration is checked before anything is
/// written, so a bad catalogue fails the boot without a half-applied one.
pub async fn reconcile_with(
	store: &dyn PlanStore,
	registry: Option<&EntitlementRegistry>,
	seller_org_id: i64,
	defs: &[OfferDef],
) -> ClResult<()> {
	let mut codes = BTreeSet::new();
	let mut rows = Vec::with_capacity(defs.len());
	for d in defs {
		d.check(registry)?;
		if !codes.insert(d.code.clone()) {
			return Err(Error::internal(format!("offer '{}' is declared twice", d.code)));
		}
		let service_id =
			store.plan_service_id(seller_org_id, &d.service).await?.ok_or_else(|| {
				Error::internal(format!("offer '{}': no service with code '{}'", d.code, d.service))
			})?;
		rows.push(NewOffer {
			uid: OfferId::generate(),
			seller_org_id,
			code: d.code.clone(),
			name: d.name.clone(),
			kind: d.kind,
			service_id,
			family: d.family.clone(),
			rank: d.rank,
			interval: d.interval,
			interval_count: d.interval_count,
			validity_days: d.validity_days,
			trial_days: d.trial_days,
			prices: d.prices.clone(),
			entitlements: d.entitlements.clone(),
		});
	}
	for row in &rows {
		store.offer_upsert(row).await?;
	}
	let keep: Vec<String> = codes.into_iter().collect();
	store.offers_deactivate_except(seller_org_id, &keep).await?;
	Ok(())
}

// vim: ts=4
