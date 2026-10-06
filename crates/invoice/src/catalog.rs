// SPDX-License-Identifier: MPL-2.0
//! Handlers for the reference data — currencies, the seller, the service catalogue.
//!
//! Every handler deserializes, calls one [`Invoices`] method and serializes; none of them decides
//! anything. Where a second call appears it is *rendering*, never a decision: [`Money`] carries no
//! currency of its own, so printing a priced row first needs [`Invoices::base_currency`] to know
//! how many decimals it has and what to label them.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::ids::SellerId;
use mintworks_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::currency::{Currency, RateMode};
use crate::routes::Page;
use crate::service_api::Invoices;
use crate::store::{
	Seller, SellerVersion, SellerVersionPatch, SellerVersionStatus, Service, ServiceDef,
	ServicePatch,
};
use crate::vat::VatCode;

// ---------------------------------------------------------------- wire types

/// A `currencies` row. `rate` is what one unit of it fetches in the base currency, six
/// decimals; `null` when the mode is OFFICIAL and nothing has been published for it yet.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrencyView {
	pub code: CurrencyCode,
	pub price_round_step: i64,
	pub cash_round_step: Option<i64>,
	pub mode: &'static str,
	pub rate: Option<String>,
	pub fee_bp: i64,
	pub enabled: bool,
}

impl CurrencyView {
	fn of(cur: Currency, rate_e6: Option<i64>) -> Self {
		Self {
			code: cur.code,
			price_round_step: cur.price_round_step,
			cash_round_step: cur.cash_round_step,
			mode: match cur.mode {
				RateMode::Fixed => "FIXED",
				RateMode::Official => "OFFICIAL",
			},
			// A rate is scaled 1e6 and is not money.
			rate: rate_e6.map(|r| format_scaled(r, 6)),
			fee_bp: cur.fee_bp,
			enabled: cur.enabled,
		}
	}
}

/// The seller as an org may see it. `nav_base_url` and `nav_login` are deliberately absent: they
/// are the operator's NAV credentials.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SellerView {
	pub uid: SellerId,
	pub name: String,
	pub country: String,
	pub tax_number: String,
	pub group_member_tax_no: Option<String>,
	pub eu_vat_id: Option<String>,
	pub postcode: String,
	pub city: String,
	pub street: String,
	pub bank_account: Option<String>,
	pub bank_name: Option<String>,
	pub small_business: bool,
	pub vat_scheme: String,
	pub income_regime: String,
	pub expense_ratio_pct: Option<i64>,
	pub regime_since: Option<String>,
	pub series_code: String,
	/// Which `seller_versions` row this is, and where it stands in the lifecycle. An invoice
	/// names its own through `seller_ver`, so a reader can tell what it was issued under.
	pub seller_ver: i64,
	pub status: SellerVersionStatus,
	pub valid_from: Option<Timestamp>,
	pub superseded_at: Option<Timestamp>,
	/// The acting org invoices under an ancestor's seller rather than one of its own.
	pub inherited: bool,
	/// Read-only since: only payments may be recorded.
	pub closed_at: Option<Timestamp>,
	/// An invoice has a number, so the tax number is the company's NAV identity and is fixed.
	pub tax_number_locked: bool,
	/// The org's own payment term; `None` inherits `default_payment_days`.
	pub payment_days: Option<i64>,
	/// `settings['invoice.default_payment_days']`, what an unset term falls back to.
	pub default_payment_days: i64,
}

impl SellerView {
	/// The two halves recomposed: the statutory data from one `seller_versions` row, the
	/// `series_code` from the live `sellers` row. The wire shape is the same one `GET
	/// /api/seller` has always served, with the version's own bookkeeping added.
	pub(crate) fn of(
		seller: &Seller,
		v: SellerVersion,
		acting_org: i64,
		issued: bool,
		default_payment_days: i64,
	) -> Self {
		Self {
			uid: seller.uid.clone(),
			name: v.name,
			country: v.country,
			tax_number: v.tax_number,
			group_member_tax_no: v.group_member_tax_no,
			eu_vat_id: v.eu_vat_id,
			postcode: v.postcode,
			city: v.city,
			street: v.street,
			bank_account: v.bank_account,
			bank_name: v.bank_name,
			small_business: v.small_business,
			vat_scheme: v.vat_scheme,
			income_regime: v.income_regime,
			expense_ratio_pct: v.expense_ratio_pct,
			regime_since: v.regime_since,
			series_code: seller.series_code.clone(),
			seller_ver: v.seller_ver,
			status: v.status,
			valid_from: v.valid_from,
			superseded_at: v.superseded_at,
			inherited: seller.org_id != acting_org,
			closed_at: seller.closed_at,
			tax_number_locked: issued,
			payment_days: seller.payment_days,
			default_payment_days,
		}
	}
}

/// A `services` row. `unitPrice` is in the base currency, which is where all master data is priced.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceView {
	pub uid: ServiceId,
	pub code: Option<String>,
	pub name: String,
	pub description: Option<String>,
	pub unit: String,
	pub unit_price: MoneyWire,
	pub vat_code: VatCode,
	pub active: bool,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
}

impl ServiceView {
	fn of(s: Service, base: &Currency) -> Self {
		Self {
			uid: s.uid,
			code: s.code,
			name: s.name,
			description: s.description,
			unit: s.unit,
			unit_price: s.unit_price.to_wire(&base.code),
			vat_code: s.vat_code,
			active: s.active,
			created_at: s.created_at,
			updated_at: s.updated_at,
		}
	}
}

/// A wire amount read back into [`Money`]. The currency code must match the one the value is
/// priced in, so a client cannot quietly send EUR where the row is HUF and have the digits
/// taken at face value.
pub(crate) fn money_in(wire: &MoneyWire, cur: &Currency) -> ClResult<Money> {
	if wire.currency != cur.code {
		return Err(Error::validation("amount is not in the currency this value is priced in"));
	}
	Money::parse(&wire.amount)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewServiceBody {
	pub code: String,
	pub name: String,
	#[serde(default)]
	pub description: Option<String>,
	pub unit: String,
	pub unit_price: MoneyWire,
	pub vat_code: VatCode,
}

impl NewServiceBody {
	fn into_def(self, base: &Currency) -> ClResult<ServiceDef> {
		Ok(ServiceDef {
			unit_price: money_in(&self.unit_price, base)?,
			code: self.code,
			name: self.name,
			description: self.description,
			unit: self.unit,
			vat_code: self.vat_code,
		})
	}
}

/// Three-state on `code` and `description`, which are nullable columns; plain `Option` on the
/// rest, where absent and null mean the same thing because the column is `NOT NULL`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePatchBody {
	#[serde(default)]
	pub code: Patch<String>,
	#[serde(default)]
	pub name: Option<String>,
	#[serde(default)]
	pub description: Patch<String>,
	#[serde(default)]
	pub unit: Option<String>,
	#[serde(default)]
	pub unit_price: Option<MoneyWire>,
	#[serde(default)]
	pub vat_code: Option<VatCode>,
	#[serde(default)]
	pub active: Option<bool>,
}

impl ServicePatchBody {
	fn into_patch(self, base: &Currency) -> ClResult<ServicePatch> {
		Ok(ServicePatch {
			unit_price: match &self.unit_price {
				Some(w) => Some(money_in(w, base)?),
				None => None,
			},
			code: self.code,
			name: self.name,
			description: self.description,
			unit: self.unit,
			vat_code: self.vat_code,
			active: self.active,
		})
	}
}

#[derive(Debug, Default, Deserialize)]
pub struct AllQuery {
	/// Operator-only: include the disabled currencies too.
	#[serde(default)]
	pub all: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct ServiceQuery {
	/// `?active=0` shows deactivated rows as well. Anything else, including absent, hides
	/// them — a catalogue listing is about what can be sold today.
	#[serde(default)]
	pub active: Option<i64>,
}

// ---------------------------------------------------------------- handlers

/// `GET /api/currencies`
pub async fn currencies(
	State(app): State<App>,
	ctx: Ctx,
	Query(q): Query<AllQuery>,
) -> ClResult<Json<Page<CurrencyView>>> {
	let rows = Invoices::new(app).list_currencies(&ctx, q.all).await?;
	Ok(Json(Page::all(rows.into_iter().map(|(c, r)| CurrencyView::of(c, r)).collect())))
}

/// `GET /api/seller`
pub async fn seller(State(app): State<App>, ctx: Ctx) -> ClResult<Json<SellerView>> {
	Ok(Json(Invoices::new(app).seller(&ctx).await?))
}

/// `POST /api/seller` body: the statutory fields plus the one operational field a mint sets.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSellerBody {
	pub series_code: Option<String>,
	#[serde(flatten)]
	pub seller: SellerVersionPatch,
}

/// `POST /api/seller` — the acting org's Admin mints its own seller, once.
pub async fn create_seller(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<NewSellerBody>,
) -> ClResult<(StatusCode, Json<SellerView>)> {
	let view = Invoices::new(app)
		.create_seller(&ctx, body.series_code.as_deref(), &body.seller)
		.await?;
	Ok((StatusCode::CREATED, Json(view)))
}

/// `PUT /api/seller` — a new version, or `204` when nothing changed.
pub async fn sync_seller(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<SellerVersionPatch>,
) -> ClResult<axum::response::Response> {
	use axum::response::IntoResponse;
	Ok(match Invoices::new(app).sync_seller(&ctx, &body).await? {
		Some(view) => Json(view).into_response(),
		None => StatusCode::NO_CONTENT.into_response(),
	})
}

#[derive(Debug, Deserialize)]
pub struct SellerClosedBody {
	pub closed: bool,
}

/// `PUT /api/seller/closed` — the Owner makes the company read-only, or reopens it.
pub async fn set_seller_closed(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<SellerClosedBody>,
) -> ClResult<Json<SellerView>> {
	Ok(Json(Invoices::new(app).set_seller_closed(&ctx, body.closed).await?))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SellerPaymentDaysBody {
	pub payment_days: Option<i64>,
}

/// `PUT /api/seller/payment-days` — the Admin sets or clears the org's payment term.
pub async fn set_seller_payment_days(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<SellerPaymentDaysBody>,
) -> ClResult<Json<SellerView>> {
	Ok(Json(Invoices::new(app).set_seller_payment_days(&ctx, body.payment_days).await?))
}

/// `GET /api/services`
pub async fn list(
	State(app): State<App>,
	ctx: Ctx,
	Query(q): Query<ServiceQuery>,
) -> ClResult<Json<Page<ServiceView>>> {
	let inv = Invoices::new(app);
	let base = inv.base_currency(&ctx).await?;
	let rows = inv.list_services(&ctx, q.active != Some(0)).await?;
	Ok(Json(Page::all(rows.into_iter().map(|s| ServiceView::of(s, &base)).collect())))
}

/// `GET /api/services/{uid}`
pub async fn get(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<Json<ServiceView>> {
	let inv = Invoices::new(app);
	let base = inv.base_currency(&ctx).await?;
	Ok(Json(ServiceView::of(inv.service(&ctx, &uid).await?, &base)))
}

/// `POST /api/services` — `Admin` on the acting org's seller org.
pub async fn create(
	State(app): State<App>,
	ctx: Ctx,
	Json(body): Json<NewServiceBody>,
) -> ClResult<(StatusCode, Json<ServiceView>)> {
	let inv = Invoices::new(app);
	let base = inv.base_currency(&ctx).await?;
	let svc = inv.create_service(&ctx, &body.into_def(&base)?).await?;
	Ok((StatusCode::CREATED, Json(ServiceView::of(svc, &base))))
}

/// `PATCH /api/services/{uid}` — `Admin` on the acting org's seller org.
pub async fn patch(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
	Json(body): Json<ServicePatchBody>,
) -> ClResult<Json<ServiceView>> {
	let inv = Invoices::new(app);
	let base = inv.base_currency(&ctx).await?;
	let svc = inv.update_service(&ctx, &uid, &body.into_patch(&base)?).await?;
	Ok(Json(ServiceView::of(svc, &base)))
}

/// `DELETE /api/services/{uid}` — `Admin` on the acting org's seller org, and a deactivation
/// rather than a delete:
/// `invoice_lines.service_id` references the row, so it is never removed. One service call, because
/// "deactivate" *is* a patch.
pub async fn deactivate(
	State(app): State<App>,
	ctx: Ctx,
	Path(uid): Path<String>,
) -> ClResult<StatusCode> {
	let patch = ServicePatch { active: Some(false), ..Default::default() };
	Invoices::new(app).update_service(&ctx, &uid, &patch).await?;
	Ok(StatusCode::NO_CONTENT)
}

// vim: ts=4
