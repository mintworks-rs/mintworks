// SPDX-License-Identifier: MPL-2.0
//! Sells what `mintworks-entitle` grants: an offer catalogue, quote → checkout through
//! `mintworks-invoice` and `mintworks-billing`, rewards, coupons and subscriptions.

#![forbid(unsafe_code)]

use mintworks_core::app::AppBuilder;
use mintworks_core::settings::SettingDef;

pub mod admin;
pub mod catalogue;
pub mod checkout;
pub mod coupon;
pub mod events;
pub mod invite_gate;
pub mod quote;
pub mod renew;
pub mod rewards;
pub mod routes;
pub mod service;
pub mod store;
pub mod subscribe;

pub use admin::{AdminCancelReq, RefundMode, RepriceReq, SubsFilter};
pub use catalogue::{OfferCatalogue, OfferDef, reconcile, reconcile_with};
pub use checkout::{Checkout, CheckoutReq};
pub use invite_gate::MeterInviteGate;
pub use quote::{Quote, QuoteLine, QuoteReq};
pub use rewards::Side;
pub use routes::routes;
pub use service::{OfferView, Plans};
pub use store::{
	Interval, LinkKind, LinkOutcome, NewOffer, NewPlanInvoice, Offer, OfferEntitlement, OfferKind,
	OfferPrice, PayMethod, PlanInvoice, PlanStore, SubStatus, Subscription,
};

/// This crate's declared settings, registered with
/// `AppBuilder::settings(mintworks_plans::SETTINGS)`.
pub static SETTINGS: &[SettingDef] = &[
	SettingDef::int(
		"plans.grace_days",
		"3",
		"Days a subscription period's grants outlast its start while its invoice is unpaid.",
	)
	.range(0, 60),
	SettingDef::int(
		"plans.suspend_after_days",
		"14",
		"Days an unpaid period invoice may stay open before the subscription is suspended.",
	)
	.range(1, 365),
	SettingDef::choice(
		"plans.reward_on",
		&["manual", "first_payment"],
		"manual",
		"When a ref use's reward offers are granted automatically.",
	),
	// `plans.reward.<ref_type>.inviter|invitee`: the reward offer code; blank = none.
	SettingDef::text("plans.reward.", "", "A ref type's reward offer code, per side.").family(),
];

/// `plans.quote_key` signs quote tokens; minted on first use.
pub static SECRETS: &[&str] = &["plans.quote_key"];

/// Registers the declared offers, their boot reconcile and the grant/cut event handler. The
/// renewal job is the app's: `.jobs(mintworks_plans::renew::register)` plus [`renew::seed`] at boot. The store is registered
/// separately, as `.extension(Arc::new(store) as Arc<dyn PlanStore>)`. `on_init`s run in
/// registration order, so call this after whatever seeds the `services` the offers name.
pub fn install(builder: AppBuilder, offers: impl IntoIterator<Item = OfferDef>) -> AppBuilder {
	builder
		.extension(OfferCatalogue(std::sync::Arc::new(offers.into_iter().collect())))
		.on_init(reconcile)
		.on_event(events::handle)
}

// vim: ts=4
