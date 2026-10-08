// SPDX-License-Identifier: MPL-2.0
//! The v1 dispatch table: one Rune module per service handle, one function per exposed method.
//!
//! The table is an allowlist by intent, not a projection of the handles: `nav::` is reads only
//! because submission is the framework's own job chain, and nothing credential-shaped is bound
//! at all.
//!
//! A Rune host function is a free `fn` that captures nothing, so every binding builds its own
//! handle from the `App` the [`ScriptCtx`] carries.

use std::{
	collections::BTreeSet,
	sync::{Arc, Mutex, OnceLock},
};

use mintworks_core::{
	Ctx,
	error::{Error, StatusCode},
	ids::{AccountId, PartyId, SellerId, ServiceId},
	money::{CurrencyCode, Money as CoreMoney, Qty as CoreQty},
	types::{Patch, Timestamp},
};
use rune::{
	ContextError, Module, Value,
	runtime::{Object, Ref},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value as Json, json};

use crate::{
	ctx::ScriptCtx,
	error::{self, R, bad},
	money::{Money, Qty},
	objects::looks_like_uid,
	tx,
	value::{ScriptError, from_json, to_json},
};

// ----------------------------------------------------------------- argument plumbing

/// `()` is both an absent optional and an explicit "clear it".
/// `into_unit` is the public test rune's own `FromValue for ()` uses; `Value` is refcounted,
/// so the clone is a bump, not a copy.
pub(crate) fn is_unit(v: &Value) -> bool {
	v.clone().into_unit().is_ok()
}

fn de<T: DeserializeOwned>(v: &Value, what: &str) -> R<T> {
	let json = to_json(v).map_err(ScriptError)?;
	serde_json::from_value(json).map_err(|e| bad(format!("{what}: {e}")))
}

fn out<T: Serialize>(v: &T) -> R<Value> {
	let json = serde_json::to_value(v)
		.map_err(|e| ScriptError(error::runtime(format!("result is not representable: {e}"))))?;
	from_json(&json).map_err(ScriptError)
}

fn object<'a>(v: &'a Value, what: &str) -> R<rune::runtime::BorrowRef<'a, Object>> {
	v.borrow_ref::<Object>().map_err(|_| bad(format!("{what} must be an object")))
}

/// A present, non-`()` field. An absent key and an explicit `()` are the same "no value" here;
/// only [`tri`] and [`patch_of`] tell them apart, which is what three-state patches need.
fn get<'a>(o: &'a Object, k: &str) -> Option<&'a Value> {
	o.get(k).filter(|v| !is_unit(v))
}

fn need<'a>(o: &'a Object, k: &str) -> R<&'a Value> {
	get(o, k).ok_or_else(|| bad(format!("{k} is required")))
}

fn str_of(o: &Object, k: &str) -> R<Option<String>> {
	get(o, k).map(|v| de(v, k)).transpose()
}

fn req_str(o: &Object, k: &str) -> R<String> {
	de(need(o, k)?, k)
}

fn patch_of<T: DeserializeOwned>(o: &Object, k: &str) -> R<Patch<T>> {
	match o.get(k) {
		None => Ok(Patch::Undefined),
		Some(v) if is_unit(v) => Ok(Patch::Null),
		Some(v) => Ok(Patch::Value(de(v, k)?)),
	}
}

/// `LinePatch` spells its three states as `Option<Option<T>>` rather than [`Patch`].
#[allow(clippy::option_option)]
fn tri<T>(o: &Object, k: &str, f: impl Fn(&Value) -> R<T>) -> R<Option<Option<T>>> {
	match o.get(k) {
		None => Ok(None),
		Some(v) if is_unit(v) => Ok(Some(None)),
		Some(v) => Ok(Some(Some(f(v)?))),
	}
}

/// A script `Money` carries minor units and a currency already, so no scale-aware parse of a
/// decimal string is needed on the way in — unlike the route layer's `*Body` types.
fn money_of(v: &Value) -> R<(CoreMoney, CurrencyCode)> {
	let wire = v.borrow_ref::<Money>().map_err(|_| bad("expected a Money"))?.to_wire();
	let amount = CoreMoney::parse(&wire.amount).map_err(ScriptError)?;
	Ok((amount, wire.currency))
}

/// Mirrors `mintworks_invoice::catalog::money_in`, which is `pub(crate)`: a EUR amount on a HUF
/// invoice is refused rather than taken at face value.
fn money_in(v: &Value, cur: &CurrencyCode) -> R<CoreMoney> {
	let (m, c) = money_of(v)?;
	if &c != cur {
		return Err(bad(format!("amount is in {c}, but this value is priced in {cur}")));
	}
	Ok(m)
}

fn qty_of(v: &Value) -> R<CoreQty> {
	Ok(v.borrow_ref::<Qty>().map_err(|_| bad("expected a Qty"))?.inner())
}

fn string_of(v: &Value) -> R<String> {
	de(v, "string")
}

fn discount_of(v: &Value, cur: &CurrencyCode) -> R<mintworks_invoice::money::Discount> {
	use mintworks_invoice::money::Discount;
	let o = object(v, "discount")?;
	let value = need(&o, "value")?;
	match req_str(&o, "kind")?.as_str() {
		"PERCENT" => {
			let bp: i64 = de(value, "discount.value")?;
			let bp = u32::try_from(bp).map_err(|_| bad("discount.value is out of range"))?;
			Ok(Discount::Percent(bp))
		}
		"AMOUNT" => Ok(Discount::Amount(money_in(value, cur)?)),
		other => Err(bad(format!("discount.kind '{other}' is neither AMOUNT nor PERCENT"))),
	}
}

fn line_of(v: &Value, cur: &CurrencyCode) -> R<mintworks_invoice::draft::Line> {
	use mintworks_invoice::draft::Line;
	let o = object(v, "a line")?;
	let qty = qty_of(need(&o, "qty")?)?;
	let mut line = match str_of(&o, "serviceCode")? {
		// A catalogue line takes price, unit, description and VAT code from the `services` row;
		// asserting them here is refused by `draft::resolve`, not silently discarded.
		Some(code) => Line::code(code, qty),
		None => Line::adhoc(
			req_str(&o, "description")?,
			req_str(&o, "unit")?,
			qty,
			money_in(need(&o, "unitPrice")?, cur)?,
			de(need(&o, "vatCode")?, "vatCode")?,
		),
	};
	line.discount = get(&o, "discount").map(|v| discount_of(v, cur)).transpose()?;
	line.discount_description = str_of(&o, "discountDescription")?;
	line.note = str_of(&o, "note")?;
	Ok(line)
}

fn new_draft_of(v: &Value, cur: &CurrencyCode) -> R<mintworks_invoice::draft::NewDraft> {
	use mintworks_invoice::draft::{NewDraft, Party};
	let o = object(v, "a draft")?;
	let mut d = NewDraft { request_id: str_of(&o, "requestId")?, ..Default::default() };
	if let Some(uid) = str_of(&o, "partyUid")? {
		d.billing_party = Party::Uid(PartyId::parse(&uid).map_err(ScriptError)?);
	}
	if let Some(lines) = get(&o, "lines") {
		let lines = lines
			.borrow_ref::<rune::runtime::Vec>()
			.map_err(|_| bad("lines must be a list"))?;
		for line in lines.iter() {
			d.lines.push(line_of(line, cur)?);
		}
	}
	d.discount = get(&o, "discount").map(|v| discount_of(v, cur)).transpose()?;
	d.payment_method = get(&o, "paymentMethod").map(|v| de(v, "paymentMethod")).transpose()?;
	d.currency = get(&o, "currency").map(|v| de(v, "currency")).transpose()?;
	d.fulfilment_date = str_of(&o, "fulfilmentDate")?;
	d.due_date = str_of(&o, "dueDate")?;
	d.period_start = str_of(&o, "periodStart")?;
	d.period_end = str_of(&o, "periodEnd")?;
	d.notes = str_of(&o, "notes")?;
	Ok(d)
}

/// A draft's own `currency` field, read ahead of the full parse because its amounts are checked
/// against the currency it resolves to.
fn asked_currency(v: &Value) -> R<Option<CurrencyCode>> {
	let o = object(v, "a draft")?;
	get(&o, "currency").map(|v| de(v, "currency")).transpose()
}

fn service_def_of(v: &Value, cur: &CurrencyCode) -> R<mintworks_invoice::store::ServiceDef> {
	let o = object(v, "a service")?;
	Ok(mintworks_invoice::store::ServiceDef {
		code: req_str(&o, "code")?,
		name: req_str(&o, "name")?,
		description: str_of(&o, "description")?,
		unit: req_str(&o, "unit")?,
		unit_price: money_in(need(&o, "unitPrice")?, cur)?,
		vat_code: de(need(&o, "vatCode")?, "vatCode")?,
	})
}

fn service_patch_of(v: &Value, cur: &CurrencyCode) -> R<mintworks_invoice::store::ServicePatch> {
	let o = object(v, "a service patch")?;
	Ok(mintworks_invoice::store::ServicePatch {
		code: patch_of(&o, "code")?,
		name: str_of(&o, "name")?,
		description: patch_of(&o, "description")?,
		unit: str_of(&o, "unit")?,
		unit_price: get(&o, "unitPrice").map(|v| money_in(v, cur)).transpose()?,
		vat_code: get(&o, "vatCode").map(|v| de(v, "vatCode")).transpose()?,
		active: get(&o, "active").map(|v| de(v, "active")).transpose()?,
	})
}

fn line_patch_of(v: &Value, cur: &CurrencyCode) -> R<mintworks_invoice::service_api::LinePatch> {
	let o = object(v, "a line patch")?;
	Ok(mintworks_invoice::service_api::LinePatch {
		description: str_of(&o, "description")?,
		qty: get(&o, "qty").map(qty_of).transpose()?,
		unit_price: get(&o, "unitPrice").map(|v| money_in(v, cur)).transpose()?,
		vat_code: get(&o, "vatCode").map(|v| de(v, "vatCode")).transpose()?,
		discount: tri(&o, "discount", |v| discount_of(v, cur))?,
		discount_description: tri(&o, "discountDescription", string_of)?,
		note: tri(&o, "note", string_of)?,
	})
}

/// `InvoicePatch` is the one patch type that is **not** `rename_all = "camelCase"`, and its
/// `billing_party_id` is an internal key, so it is built by hand rather than deserialized.
/// The party and the currency ride beside it, which is what `patch_by_uid` takes.
fn invoice_patch_of(
	v: &Value,
) -> R<(Option<String>, Option<CurrencyCode>, mintworks_invoice::store::InvoicePatch)> {
	let o = object(v, "an invoice patch")?;
	let patch = mintworks_invoice::store::InvoicePatch {
		billing_party_id: None,
		payment_method: get(&o, "paymentMethod").map(|v| de(v, "paymentMethod")).transpose()?,
		fulfilment_date: patch_of(&o, "fulfilmentDate")?,
		due_date: patch_of(&o, "dueDate")?,
		period_start: patch_of(&o, "periodStart")?,
		period_end: patch_of(&o, "periodEnd")?,
		notes: patch_of(&o, "notes")?,
		currency: None,
		rate_e6: None,
		discount_value: None,
	};
	let party = str_of(&o, "partyUid")?;
	let currency = get(&o, "currency").map(|v| de(v, "currency")).transpose()?;
	Ok((party, currency, patch))
}

// ----------------------------------------------------------------- result plumbing

/// `ServiceView::of` and `CurrencyView::of` are private in a `pub(crate)` module, so these two
/// shapes are written out here. A `pub use` cannot widen a function's visibility.
fn service_json(s: &mintworks_invoice::store::Service, cur: &CurrencyCode) -> Json {
	json!({
		"uid": s.uid.as_str(),
		"code": s.code,
		"name": s.name,
		"description": s.description,
		"unit": s.unit,
		"unitPrice": s.unit_price.to_wire(cur),
		"vatCode": s.vat_code,
		"active": s.active,
		"createdAt": s.created_at,
		"updatedAt": s.updated_at,
	})
}

/// Every amount carries its own bucket's currency: nothing in the aggregate converts, so a
/// bucket rendered with any other currency would be a wrong number, not a wrong label.
fn summary_json(s: &mintworks_invoice::store::InvoiceSummary) -> Json {
	json!({
		"statuses": s.statuses.iter().map(|b| json!({
			"status": b.status.as_str(),
			"currency": b.currency.as_str(),
			"count": b.count,
			"net": b.net.to_wire(&b.currency),
			"vat": b.vat.to_wire(&b.currency),
			"gross": b.gross.to_wire(&b.currency),
			"paid": b.paid.to_wire(&b.currency),
		})).collect::<Vec<_>>(),
		"months": s.months.iter().map(|b| json!({
			"month": b.month,
			"currency": b.currency.as_str(),
			"count": b.count,
			"gross": b.gross.to_wire(&b.currency),
			"paid": b.paid.to_wire(&b.currency),
		})).collect::<Vec<_>>(),
		"overdue": s.overdue.iter().map(|b| json!({
			"currency": b.currency.as_str(),
			"count": b.count,
			"outstanding": b.outstanding.to_wire(&b.currency),
		})).collect::<Vec<_>>(),
		"paidThisMonth": s.paid_this_month.iter().map(|(cur, paid)| paid.to_wire(cur))
			.collect::<Vec<_>>(),
	})
}

fn revenue_json(year: i32, today: &str, months: &[mintworks_invoice::store::RevenueMonth]) -> Json {
	let huf = CurrencyCode::from_trusted("HUF".to_owned());
	json!({
		"year": year,
		"today": today,
		"months": months.iter().map(|m| json!({
			"month": m.month,
			"invoiced": m.invoiced_huf.to_wire(&huf),
			"received": m.received_huf.to_wire(&huf),
			"outstanding": m.outstanding_huf.to_wire(&huf),
		})).collect::<Vec<_>>(),
	})
}

fn currency_json(c: &mintworks_invoice::currency::Currency, rate_e6: Option<i64>) -> Json {
	json!({
		"code": c.code,
		"priceRoundStep": c.price_round_step,
		"cashRoundStep": c.cash_round_step,
		"mode": match c.mode {
			mintworks_invoice::currency::RateMode::Fixed => "FIXED",
			mintworks_invoice::currency::RateMode::Official => "OFFICIAL",
		},
		"fixedRateE6": c.fixed_rate_e6,
		"feeBp": c.fee_bp,
		"enabled": c.enabled,
		"rateE6": rate_e6,
	})
}

fn page(items: Vec<Json>, next: Option<String>) -> Json {
	json!({ "items": items, "nextCursor": next })
}

/// The org's base currency, which is the scale a catalogue price is rendered at — a `services`
/// row carries no currency of its own.
async fn catalogue_currency(
	inv: &mintworks_invoice::service_api::Invoices,
	ctx: &Ctx,
) -> R<CurrencyCode> {
	Ok(inv.base_currency(ctx).await.map_err(ScriptError)?.code)
}

/// Every invoice result crosses as the same `InvoiceView` the HTTP surface serves.
async fn invoice_json(
	inv: &mintworks_invoice::service_api::Invoices,
	ctx: &Ctx,
	invoice: mintworks_invoice::store::Invoice,
	with_lines: bool,
) -> R<Json> {
	let full = inv.hydrate(ctx, invoice, with_lines).await.map_err(ScriptError)?;
	let view = mintworks_invoice::routes::InvoiceView::of(full);
	serde_json::to_value(view)
		.map_err(|e| ScriptError(error::runtime(format!("invoice is not representable: {e}"))))
}

/// The handle plus an owned `Ctx`, taken before the first await: the Rune guard is not `Send`
/// and the invocation runs on axum's `Send` execution path.
fn invoices(c: &Ref<ScriptCtx>) -> R<(mintworks_invoice::service_api::Invoices, Ctx)> {
	Ok((mintworks_invoice::service_api::Invoices::new(c.app()?.clone()), c.ctx().clone()))
}

/// The one binding with no service wrapper to go through — `put_seller` deliberately has none.
fn invoice_store(app: &mintworks_core::App) -> R<Arc<dyn mintworks_invoice::store::InvoiceStore>> {
	app.extensions
		.get::<Arc<dyn mintworks_invoice::store::InvoiceStore>>()
		.cloned()
		.ok_or_else(|| {
			ScriptError(Error::internal("Arc<dyn InvoiceStore> is not registered as an extension"))
		})
}

/// Minting a `sellers` row hands an org a taxpayer id, NAV credentials and a numbering counter:
/// a grant an operator makes at boot, not an org-admin operation, so only a job or `on_init`
/// handler may ask for one. `sys::escalate` is also `Actor::System`, and is refused: a
/// request handler escalating must not reach a grant reserved for boot.
fn system_only(ctx: &Ctx) -> R<()> {
	match ctx.actor {
		mintworks_core::Actor::System { source } if source != crate::sys::SOURCE => Ok(()),
		_ => Err(ScriptError(Error::coded(
			StatusCode::FORBIDDEN,
			"E-SCRIPT-INIT-ONLY",
			"only an init or job handler may call this",
		))),
	}
}

mod invoices {
	use super::*;

	#[rune::function]
	pub async fn list_services(c: Ref<ScriptCtx>, active_only: bool) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let cur = catalogue_currency(&inv, &ctx).await?;
		let rows = inv.list_services(&ctx, active_only).await.map_err(ScriptError)?;
		out(&rows.iter().map(|s| service_json(s, &cur)).collect::<Vec<_>>())
	}

	#[rune::function]
	pub async fn service(c: Ref<ScriptCtx>, id_or_code: String) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let cur = catalogue_currency(&inv, &ctx).await?;
		let row = if looks_like_uid(&id_or_code) {
			inv.service(&ctx, &id_or_code).await
		} else {
			inv.service_by_code(&ctx, &id_or_code).await
		};
		out(&service_json(&row.map_err(ScriptError)?, &cur))
	}

	#[rune::function]
	pub async fn create_service(c: Ref<ScriptCtx>, def: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let cur = catalogue_currency(&inv, &ctx).await?;
		let def = service_def_of(&def, &cur)?;
		let row = inv.create_service(&ctx, &def).await.map_err(ScriptError)?;
		out(&service_json(&row, &cur))
	}

	#[rune::function]
	pub async fn update_service(c: Ref<ScriptCtx>, id_or_code: String, patch: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let cur = catalogue_currency(&inv, &ctx).await?;
		let patch = service_patch_of(&patch, &cur)?;
		let uid = resolve_service(&inv, &ctx, &id_or_code).await?;
		let row = inv.update_service(&ctx, &uid, &patch).await.map_err(ScriptError)?;
		out(&service_json(&row, &cur))
	}

	#[rune::function]
	pub async fn sync_services(c: Ref<ScriptCtx>, defs: Value) -> R<()> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let cur = catalogue_currency(&inv, &ctx).await?;
		let list = defs
			.borrow_ref::<rune::runtime::Vec>()
			.map_err(|_| bad("defs must be a list"))?;
		let defs = list.iter().map(|d| service_def_of(d, &cur)).collect::<R<Vec<_>>>()?;
		drop(list);
		inv.sync_services(&ctx, &defs).await.map_err(ScriptError)
	}

	#[rune::function]
	pub async fn list_parties(c: Ref<ScriptCtx>) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		out(&inv.list_parties(&ctx).await.map_err(ScriptError)?)
	}

	#[rune::function]
	pub async fn party(c: Ref<ScriptCtx>, id_or_code: String) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		out(&inv.party(&ctx, &party_uid(&id_or_code)?).await.map_err(ScriptError)?)
	}

	#[rune::function]
	pub async fn create_party(c: Ref<ScriptCtx>, patch: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let patch = de(&patch, "party")?;
		drop(c);
		out(&inv.create_party(&ctx, &patch).await.map_err(ScriptError)?)
	}

	#[rune::function]
	pub async fn update_party(c: Ref<ScriptCtx>, id_or_code: String, patch: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let patch = de(&patch, "party")?;
		drop(c);
		let uid = party_uid(&id_or_code)?;
		out(&inv.update_party(&ctx, &uid, &patch).await.map_err(ScriptError)?)
	}

	#[rune::function]
	pub async fn delete_party(c: Ref<ScriptCtx>, id_or_code: String) -> R<()> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		inv.delete_party(&ctx, &party_uid(&id_or_code)?).await.map_err(ScriptError)
	}

	#[rune::function]
	pub async fn draft(c: Ref<ScriptCtx>, req: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let asked = asked_currency(&req)?;
		let cur = inv.currency(&ctx, asked.as_ref()).await.map_err(ScriptError)?.code;
		let req = new_draft_of(&req, &cur)?;
		let row = inv.draft(&ctx, &req).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn issue_now(c: Ref<ScriptCtx>, req: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let asked = asked_currency(&req)?;
		let cur = inv.currency(&ctx, asked.as_ref()).await.map_err(ScriptError)?.code;
		let req = new_draft_of(&req, &cur)?;
		let row = inv.issue_now(&ctx, &req).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn add_line(c: Ref<ScriptCtx>, uid: String, line: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let cur = inv.invoice_currency(&ctx, &uid).await.map_err(ScriptError)?.code;
		let line = line_of(&line, &cur)?;
		let row = inv.add_line(&ctx, &uid, line).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn edit_line(c: Ref<ScriptCtx>, uid: String, line_no: i64, patch: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let cur = inv.invoice_currency(&ctx, &uid).await.map_err(ScriptError)?.code;
		let patch = line_patch_of(&patch, &cur)?;
		let line_no = u32::try_from(line_no).map_err(|_| bad("lineNo is out of range"))?;
		let row = inv.edit_line(&ctx, &uid, line_no, patch).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn remove_line(c: Ref<ScriptCtx>, uid: String, line_no: i64) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let line_no = u32::try_from(line_no).map_err(|_| bad("lineNo is out of range"))?;
		let row = inv.remove_line(&ctx, &uid, line_no).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn patch(c: Ref<ScriptCtx>, uid: String, patch: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let (party, currency, patch) = invoice_patch_of(&patch)?;
		drop(c);
		let row = inv
			.patch_by_uid(&ctx, &uid, party.as_deref(), currency.as_ref(), &patch)
			.await
			.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn delete_draft(c: Ref<ScriptCtx>, uid: String) -> R<()> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		inv.delete_draft(&ctx, &uid).await.map_err(ScriptError)
	}

	#[rune::function]
	pub async fn issue(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let row = inv.issue(&ctx, &uid).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn storno(c: Ref<ScriptCtx>, uid: String, reason: String) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let row = inv.storno(&ctx, &uid, &reason).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn mark_paid(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let row = inv.mark_paid(&ctx, &uid).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn set_paid(
		c: Ref<ScriptCtx>,
		uid: String,
		amount: Value,
		paid_at: Value,
	) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let cur = inv.invoice_currency(&ctx, &uid).await.map_err(ScriptError)?.code;
		let amount = money_in(&amount, &cur)?;
		let paid_at = if is_unit(&paid_at) {
			None
		} else {
			let s: String = de(&paid_at, "paidAt")?;
			Some(
				Timestamp::parse_rfc3339(&s)
					.ok_or_else(|| bad("paidAt is not an ISO-8601 UTC timestamp"))?,
			)
		};
		let row = inv.set_paid(&ctx, &uid, amount, paid_at).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, true).await?)
	}

	#[rune::function]
	pub async fn invoice(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let row = inv.invoice(&ctx, &uid).await.map_err(ScriptError)?;
		out(&invoice_json(&inv, &ctx, row, false).await?)
	}

	#[rune::function]
	pub async fn full(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let full = inv.full(&ctx, &uid).await.map_err(ScriptError)?;
		out(&mintworks_invoice::routes::InvoiceView::of(full))
	}

	#[rune::function]
	pub async fn list_invoices(c: Ref<ScriptCtx>, cursor: Value, limit: i64) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let cursor = opt_cursor(&cursor)?;
		drop(c);
		// Clamped once, for the query *and* the cursor test: against the raw limit an over-max
		// request got a full page with no cursor.
		let limit = crate::objects::clamp(limit);
		// `list_invoices` pages on the internal id while the cursor is the last row's uid, so
		// the uid is resolved back here, so a script sees uids and never an internal id.
		let before_id = match &cursor {
			Some(uid) => Some(inv.invoice(&ctx, uid).await.map_err(ScriptError)?.id),
			None => None,
		};
		let rows = inv.list_invoices(&ctx, before_id, limit).await.map_err(ScriptError)?;
		let next = next_cursor(&rows, limit).map(|i| i.uid.as_str().to_string());
		let mut items = Vec::with_capacity(rows.len());
		for row in rows {
			items.push(invoice_json(&inv, &ctx, row, false).await?);
		}
		out(&page(items, next))
	}

	/// `filter` is `()` or `#{ status: "DRAFT,ISSUED", q: "Kovács" }`.
	#[rune::function]
	pub async fn list_full(
		c: Ref<ScriptCtx>,
		cursor: Value,
		limit: i64,
		filter: Value,
	) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let cursor = opt_cursor(&cursor)?;
		let filter = invoice_filter(&filter)?;
		drop(c);
		let limit = crate::objects::clamp(limit);
		let rows = inv
			.list_full(&ctx, &filter, cursor.as_deref(), limit)
			.await
			.map_err(ScriptError)?;
		let next = (i64::try_from(rows.len()).unwrap_or(i64::MAX) >= limit)
			.then(|| rows.last().map(|f| f.invoice.uid.as_str().to_string()))
			.flatten();
		let items = rows
			.into_iter()
			.map(|f| serde_json::to_value(mintworks_invoice::routes::InvoiceView::of(f)))
			.collect::<Result<Vec<_>, _>>()
			.map_err(|e| {
				ScriptError(error::runtime(format!("invoice is not representable: {e}")))
			})?;
		out(&page(items, next))
	}

	/// `months` is an integer or `()` for the default 12; the service clamps it.
	#[rune::function]
	pub async fn summary(c: Ref<ScriptCtx>, months: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let months: i64 = if is_unit(&months) { 12 } else { de(&months, "months")? };
		drop(c);
		let s = inv.summary(&ctx, months).await.map_err(ScriptError)?;
		out(&summary_json(&s))
	}

	/// `#{year, today, months: [#{month, invoiced, received}]}`: twelve HUF months of `year`,
	/// `()` for the current local one. `today` is here because a script has no clock of its own.
	#[rune::function]
	pub async fn revenue(c: Ref<ScriptCtx>, year: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let today = mintworks_invoice::date_of(Timestamp::now()).map_err(ScriptError)?;
		let year: i64 = if is_unit(&year) {
			today.get(..4).and_then(|y| y.parse().ok()).unwrap_or_default()
		} else {
			de(&year, "year")?
		};
		drop(c);
		let year = i32::try_from(year).map_err(|_| bad("year is out of range"))?;
		let months = inv.revenue(&ctx, year).await.map_err(ScriptError)?;
		out(&revenue_json(year, &today, &months))
	}

	/// `#{invoice, document}` — the `PathBuf` the Rust method also returns is dropped: serving
	/// the PDF is `mount("invoice.org_invoices")`'s job and a script has no `fs` by default.
	#[rune::function]
	pub async fn document(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let (invoice, doc, _path) = inv.document(&ctx, &uid).await.map_err(ScriptError)?;
		let invoice = invoice_json(&inv, &ctx, invoice, false).await?;
		let document = serde_json::to_value(&doc).map_err(|e| {
			ScriptError(error::runtime(format!("document is not representable: {e}")))
		})?;
		out(&json!({ "invoice": invoice, "document": document }))
	}

	#[rune::function]
	pub async fn list_currencies(c: Ref<ScriptCtx>, all: bool) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		let rows = inv.list_currencies(&ctx, all).await.map_err(ScriptError)?;
		out(&rows.iter().map(|(c, rate)| currency_json(c, *rate)).collect::<Vec<_>>())
	}

	#[rune::function]
	pub async fn currency(c: Ref<ScriptCtx>, code: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let asked: Option<CurrencyCode> =
			if is_unit(&code) { None } else { Some(de(&code, "currency")?) };
		drop(c);
		let row = inv.currency(&ctx, asked.as_ref()).await.map_err(ScriptError)?;
		out(&currency_json(&row, None))
	}

	#[rune::function]
	pub async fn invoice_currency(c: Ref<ScriptCtx>, uid: String) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		out(&currency_json(&inv.invoice_currency(&ctx, &uid).await.map_err(ScriptError)?, None))
	}

	#[rune::function]
	pub async fn base_currency(c: Ref<ScriptCtx>) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		out(&currency_json(&inv.base_currency(&ctx).await.map_err(ScriptError)?, None))
	}

	#[rune::function]
	pub async fn seller(c: Ref<ScriptCtx>) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		out(&inv.seller(&ctx).await.map_err(ScriptError)?)
	}

	#[rune::function]
	pub async fn seller_draft(c: Ref<ScriptCtx>) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		out(&inv.seller_draft(&ctx).await.map_err(ScriptError)?)
	}

	/// Mints or refreshes the acting org's `sellers` row — the operational half only; the
	/// statutory data is [`save_seller_draft`] + [`publish_seller`]. `System` only, per
	/// [`system_only`]; idempotent, so `on_init` may call it on every boot.
	#[rune::function]
	pub async fn put_seller(c: Ref<ScriptCtx>, def: Value) -> R<()> {
		let app = c.app()?.clone();
		let ctx = c.ctx().clone();
		let o = object(&def, "seller")?;
		let series_code = req_str(&o, "seriesCode")?;
		// Blank falls back to `settings['nav.base_url']`, which is where a deployment's NAV
		// endpoint belongs — not baked into the row.
		let nav_base_url = str_of(&o, "navBaseUrl")?.unwrap_or_default();
		let nav_login = str_of(&o, "navLogin")?;
		drop(o);
		drop(c);
		system_only(&ctx)?;
		let org_id = ctx.org().map_err(ScriptError)?;
		let store = invoice_store(&app)?;
		// `seller_for_org` climbs to an ancestor's row, and reusing that id would ask
		// `put_seller` to move a seller between orgs, which it refuses as a conflict.
		let own = store
			.seller_for_org(org_id)
			.await
			.map_err(ScriptError)?
			.filter(|s| s.org_id == org_id);
		store
			.put_seller(&mintworks_invoice::Seller {
				// `sellers.id` is not auto-assigned and there is no allocator; one seller per
				// org makes the org's own id the one choice that cannot collide.
				id: own.as_ref().map_or(org_id, |s| s.id),
				uid: own.as_ref().map_or_else(SellerId::generate, |s| s.uid.clone()),
				org_id,
				nav_base_url,
				nav_login,
				series_code,
				closed_at: None,
				payment_days: None,
				created_at: own.as_ref().map_or_else(Timestamp::now, |s| s.created_at),
			})
			.await
			.map_err(ScriptError)
	}

	#[rune::function]
	pub async fn save_seller_draft(c: Ref<ScriptCtx>, patch: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let patch: mintworks_invoice::store::SellerVersionPatch = de(&patch, "patch")?;
		drop(c);
		out(&inv.save_seller_draft(&ctx, &patch).await.map_err(ScriptError)?)
	}

	#[rune::function]
	pub async fn publish_seller(c: Ref<ScriptCtx>) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		drop(c);
		out(&inv.publish_seller(&ctx).await.map_err(ScriptError)?)
	}

	#[rune::function]
	pub async fn sync_seller(c: Ref<ScriptCtx>, patch: Value) -> R<Value> {
		let (inv, ctx) = invoices(&c)?;
		let patch: mintworks_invoice::store::SellerVersionPatch = de(&patch, "patch")?;
		drop(c);
		out(&inv.sync_seller(&ctx, &patch).await.map_err(ScriptError)?)
	}

	/// Both strings go to `InvoiceFilter::parse`, which `routes::list` also calls — a second
	/// comma-splitter here is where the two spellings would drift apart.
	fn invoice_filter(v: &Value) -> R<mintworks_invoice::store::InvoiceFilter> {
		if is_unit(v) {
			return Ok(mintworks_invoice::store::InvoiceFilter::default());
		}
		let o = object(v, "filter")?;
		let (status, q) = (str_of(&o, "status")?, str_of(&o, "q")?);
		mintworks_invoice::store::InvoiceFilter::parse(status.as_deref(), q.as_deref())
			.map_err(ScriptError)
	}

	fn opt_cursor(v: &Value) -> R<Option<String>> {
		if is_unit(v) { Ok(None) } else { Ok(Some(de(v, "cursor")?)) }
	}

	fn next_cursor(
		rows: &[mintworks_invoice::store::Invoice],
		limit: i64,
	) -> Option<&mintworks_invoice::store::Invoice> {
		(i64::try_from(rows.len()).unwrap_or(i64::MAX) >= limit)
			.then(|| rows.last())
			.flatten()
	}

	fn party_uid(id_or_code: &str) -> R<String> {
		// `billing_parties.code` is separate framework work; until it lands an
		// `idOrCode` naming a party resolves uids only.
		PartyId::parse(id_or_code).map_err(ScriptError)?;
		Ok(id_or_code.to_string())
	}

	async fn resolve_service(
		inv: &mintworks_invoice::service_api::Invoices,
		ctx: &Ctx,
		id_or_code: &str,
	) -> R<String> {
		if looks_like_uid(id_or_code) {
			ServiceId::parse(id_or_code).map_err(ScriptError)?;
			return Ok(id_or_code.to_string());
		}
		let svc = inv.service_by_code(ctx, id_or_code).await.map_err(ScriptError)?;
		Ok(svc.uid.as_str().to_string())
	}

	pub fn module() -> Result<Module, ContextError> {
		let mut m = Module::with_item(["invoices"])?;
		m.function_meta(list_services)?;
		m.function_meta(service)?;
		m.function_meta(create_service)?;
		m.function_meta(update_service)?;
		m.function_meta(sync_services)?;
		m.function_meta(list_parties)?;
		m.function_meta(party)?;
		m.function_meta(create_party)?;
		m.function_meta(update_party)?;
		m.function_meta(delete_party)?;
		m.function_meta(draft)?;
		m.function_meta(issue_now)?;
		m.function_meta(add_line)?;
		m.function_meta(edit_line)?;
		m.function_meta(remove_line)?;
		m.function_meta(patch)?;
		m.function_meta(delete_draft)?;
		m.function_meta(issue)?;
		m.function_meta(storno)?;
		m.function_meta(mark_paid)?;
		m.function_meta(set_paid)?;
		m.function_meta(invoice)?;
		m.function_meta(full)?;
		m.function_meta(list_invoices)?;
		m.function_meta(list_full)?;
		m.function_meta(summary)?;
		m.function_meta(revenue)?;
		m.function_meta(document)?;
		m.function_meta(list_currencies)?;
		m.function_meta(currency)?;
		m.function_meta(invoice_currency)?;
		m.function_meta(base_currency)?;
		m.function_meta(seller)?;
		m.function_meta(seller_draft)?;
		m.function_meta(put_seller)?;
		m.function_meta(save_seller_draft)?;
		m.function_meta(publish_seller)?;
		m.function_meta(sync_seller)?;
		Ok(m)
	}
}

/// Reads only. `submit`, `cancel_filing`, `resolve_filing`, `filing_archive` and `audit_export`
/// are deliberately unbound: filing is the framework's own job chain, and a script reaching past
/// it would be filing invoices by hand into a statutory system.
mod nav {
	use super::*;

	fn handle(c: &Ref<ScriptCtx>) -> R<(mintworks_nav::Nav, Ctx)> {
		Ok((mintworks_nav::Nav::new(c.app()?.clone()), c.ctx().clone()))
	}

	#[rune::function]
	pub async fn filing(c: Ref<ScriptCtx>, invoice_uid: String) -> R<Value> {
		let (nav, ctx) = handle(&c)?;
		drop(c);
		tx::outside_tx("nav::")?;
		let row = nav.filing(&ctx, &invoice_uid).await.map_err(ScriptError)?;
		match row {
			None => Ok(Value::from(())),
			Some(s) => out(&json!({
				"op": s.op.as_str(),
				"transactionId": s.transaction_id,
				"idx": s.idx,
				"verdict": s.verdict.map(mintworks_nav::NavVerdict::as_str),
				"errorCode": s.error_code,
				"errorMsg": s.error_msg,
				"createdAt": s.created_at,
				"doneAt": s.done_at,
				"batchUid": s.batch_uid,
				"resolvedAt": s.resolved_at,
			})),
		}
	}

	#[rune::function]
	pub async fn lookup_tax_number(c: Ref<ScriptCtx>, tax_number: String) -> R<Value> {
		let (nav, ctx) = handle(&c)?;
		drop(c);
		tx::outside_tx("nav::")?;
		let t = nav.lookup_tax_number(&ctx, &tax_number).await.map_err(ScriptError)?;
		out(&json!({ "valid": t.valid, "name": t.name, "taxNumber": t.tax_number }))
	}

	pub fn module() -> Result<Module, ContextError> {
		let mut m = Module::with_item(["nav"])?;
		m.function_meta(filing)?;
		m.function_meta(lookup_tax_number)?;
		Ok(m)
	}
}

/// One function. `mintworks-billing` is otherwise routes and jobs — `billing.org` serves paying,
/// listing and refunding — but `allocate::start` is the one thing a mounted route cannot do:
/// be called *from* a handler, so a checkout answers with the gateway's `redirectUrl` in the
/// same response instead of costing the payer a second click.
mod billing {
	use super::*;

	#[rune::function]
	pub async fn pay(c: Ref<ScriptCtx>, invoice_uid: String, req: Value) -> R<Value> {
		let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
		drop(c);
		tx::outside_tx("billing::")?;
		let req = object(&req, "request")?;
		// No default: `allocate::start` mints a key when none is sent, while one pinned to the
		// invoice uid stays spent, so a retry after CANCELED never reaches the gateway.
		let request_id = str_of(&req, "requestId")?;
		let start = mintworks_billing::allocate::StartRequest {
			provider: req_str(&req, "provider")?,
			request_id,
			return_url: req_str(&req, "returnUrl")?,
			locale: str_of(&req, "locale")?,
		};
		drop(req);
		let uid = mintworks_core::ids::InvoiceId::parse(&invoice_uid).map_err(ScriptError)?;
		let (payment, redirect_url) = mintworks_billing::allocate::start(&app, &ctx, &uid, start)
			.await
			.map_err(ScriptError)?;
		out(&json!({
			"paymentUid": payment.uid,
			"status": payment.status.as_str(),
			"redirectUrl": redirect_url,
		}))
	}

	/// Each payment on the invoice, re-asked at the gateway. `live` is membership of
	/// `mintworks_billing::allocate::LIVE` and `moved` is a non-zero allocation — the two halves of
	/// `examples/booking/app-rust/src/bookings.rs`'s discard refusal, kept here because `LIVE`
	/// is a Rust constant a script must not restate.
	#[rune::function]
	pub async fn refresh_invoice(c: Ref<ScriptCtx>, invoice_uid: String) -> R<Value> {
		let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
		drop(c);
		tx::outside_tx("billing::")?;
		let uid = mintworks_core::ids::InvoiceId::parse(&invoice_uid).map_err(ScriptError)?;
		let store = mintworks_billing::store::store(&app).map_err(ScriptError)?;
		let mut items = Vec::new();
		for p in mintworks_billing::allocate::refresh_invoice(&app, &ctx, &uid)
			.await
			.map_err(ScriptError)?
		{
			let moved = store
				.allocations(p.id)
				.await
				.map_err(ScriptError)?
				.iter()
				.any(|a| a.amount.0 != 0);
			items.push(json!({
				"uid": p.uid,
				"status": p.status.as_str(),
				"live": mintworks_billing::allocate::LIVE.contains(&p.status),
				"moved": moved,
			}));
		}
		out(&items)
	}

	pub fn module() -> Result<Module, ContextError> {
		let mut m = Module::with_item(["billing"])?;
		m.function_meta(pay)?;
		m.function_meta(refresh_invoice)?;
		Ok(m)
	}
}

/// Reads only, and nothing credential-shaped: login, refresh, step-up, password, TOTP, passkey,
/// QR and API-key methods are served by `mount("auth.public")` / `mount("auth.authenticated")`.
mod auth {
	use super::*;

	fn handle(c: &Ref<ScriptCtx>) -> R<(mintworks_auth::service_api::Auth, Ctx)> {
		Ok((mintworks_auth::service_api::Auth::new(c.app()?.clone()), c.ctx().clone()))
	}

	#[rune::function]
	pub async fn org(c: Ref<ScriptCtx>) -> R<Value> {
		let (auth, ctx) = handle(&c)?;
		drop(c);
		out(&auth.org(&ctx).await.map_err(ScriptError)?)
	}

	#[rune::function]
	pub async fn members(c: Ref<ScriptCtx>) -> R<Value> {
		let (auth, ctx) = handle(&c)?;
		drop(c);
		out(&auth.members(&ctx).await.map_err(ScriptError)?)
	}

	#[rune::function]
	pub async fn list_orgs(c: Ref<ScriptCtx>) -> R<Value> {
		let (auth, ctx) = handle(&c)?;
		drop(c);
		out(&auth.list_orgs(&ctx).await.map_err(ScriptError)?)
	}

	/// `auth::accounts` answers at most this many uids per call.
	const MAX_ACCOUNTS: usize = 500;

	/// `auth::me(ctx) -> #{uid, email, lang, displayName}` — the acting account. The `acc_` uid is
	/// the app's identity key: never the internal id, which a core-DB restore can renumber.
	#[rune::function]
	pub async fn me(c: Ref<ScriptCtx>) -> R<Value> {
		let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
		drop(c);
		let id = ctx.actor.account_id().ok_or_else(|| {
			ScriptError(error::runtime("auth::me needs an account actor (User, Operator or Key)"))
		})?;
		let store = mintworks_auth::routes::store(&app).map_err(ScriptError)?;
		let a = store
			.account_by_id(id)
			.await
			.map_err(ScriptError)?
			.ok_or(ScriptError(Error::NotFound))?;
		out(&json!({
			"uid": a.uid.as_str(), "email": a.email, "lang": a.locale, "displayName": a.name,
		}))
	}

	/// `auth::accounts(ctx, [uid…]) -> [#{uid, email?, displayName}]` — unknown, malformed and,
	/// outside Operator/System, non-member uids are dropped alike. `email` only for Operator,
	/// System and an org admin/owner, as `Auth::members` is admin-only.
	#[rune::function]
	pub async fn accounts(c: Ref<ScriptCtx>, uids: Vec<String>) -> R<Value> {
		let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
		drop(c);
		if uids.len() > MAX_ACCOUNTS {
			return Err(ScriptError(error::runtime(format!(
				"auth::accounts takes at most {MAX_ACCOUNTS} uids"
			))));
		}
		let uids: Vec<AccountId> = uids.iter().filter_map(|u| AccountId::parse(u).ok()).collect();
		let store = mintworks_auth::routes::store(&app).map_err(ScriptError)?;
		let mut rows = store.accounts_by_uid(&uids).await.map_err(ScriptError)?;
		let mut show_email = true;
		if !matches!(
			ctx.actor,
			mintworks_core::Actor::Operator { .. } | mintworks_core::Actor::System { .. }
		) {
			let org = ctx.org().map_err(ScriptError)?;
			show_email = match ctx.actor.account_id() {
				Some(me) => matches!(
					store.accepted_membership_role(org, me).await.map_err(ScriptError)?,
					Some(mintworks_core::store::Role::Admin | mintworks_core::store::Role::Owner)
				),
				None => false,
			};
			// One lookup per row, at most MAX_ACCOUNTS (500); batch it in the store if it shows up.
			let mut kept = Vec::with_capacity(rows.len());
			for a in rows {
				if store.accepted_membership_role(org, a.id).await.map_err(ScriptError)?.is_some() {
					kept.push(a);
				}
			}
			rows = kept;
		}
		let rows: Vec<Json> = rows
			.iter()
			.map(|a| {
				let mut row = json!({"uid": a.uid.as_str(), "displayName": a.name});
				if show_email {
					row["email"] = json!(a.email);
				}
				row
			})
			.collect();
		out(&rows)
	}

	pub fn module() -> Result<Module, ContextError> {
		let mut m = Module::with_item(["auth"])?;
		m.function_meta(me)?;
		m.function_meta(accounts)?;
		m.function_meta(org)?;
		m.function_meta(members)?;
		m.function_meta(list_orgs)?;
		Ok(m)
	}
}

/// The constructors only. `value::module()` already registers [`ScriptError`] itself, and
/// registering a type twice is a `ContextError`.
mod err {
	use super::*;

	/// `Error::Coded` needs a `&'static str`, so a code minted from script input would leak on
	/// every request — the same reason `Actor::System { source }` takes `&'static str`. Interned
	/// into a bounded set; past the cap is an internal error, never a silent truncation.
	const MAX_APP_CODES: usize = 64;

	fn interned() -> &'static Mutex<BTreeSet<&'static str>> {
		static CODES: OnceLock<Mutex<BTreeSet<&'static str>>> = OnceLock::new();
		CODES.get_or_init(|| Mutex::new(BTreeSet::new()))
	}

	fn intern(code: &str) -> Result<&'static str, ScriptError> {
		let mut set = interned()
			.lock()
			.map_err(|_| ScriptError(error::runtime("the app error code table is poisoned")))?;
		if let Some(found) = set.get(code) {
			return Ok(found);
		}
		if set.len() >= MAX_APP_CODES {
			return Err(ScriptError(error::runtime(format!(
				"more than {MAX_APP_CODES} distinct E-APP-* codes were minted"
			))));
		}
		let leaked: &'static str = String::leak(code.to_string());
		set.insert(leaked);
		Ok(leaked)
	}

	fn well_formed(code: &str) -> bool {
		code.starts_with("E-APP-")
			&& code.len() > "E-APP-".len()
			&& code[6..]
				.bytes()
				.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
	}

	#[rune::function]
	pub fn not_found() -> ScriptError {
		ScriptError(Error::NotFound)
	}

	#[rune::function]
	pub fn validation(msg: String) -> ScriptError {
		ScriptError(Error::validation(msg))
	}

	#[rune::function]
	pub fn conflict(msg: String) -> ScriptError {
		ScriptError(Error::conflict(msg))
	}

	#[rune::function]
	pub fn app(status: i64, code: String, msg: String) -> Result<ScriptError, ScriptError> {
		if !well_formed(&code) {
			return Err(bad(format!("'{code}' is not an E-APP-* error code")));
		}
		let status = u16::try_from(status)
			.ok()
			.and_then(|s| StatusCode::from_u16(s).ok())
			.ok_or_else(|| bad(format!("{status} is not an HTTP status code")))?;
		Ok(ScriptError(Error::coded(status, intern(&code)?, msg)))
	}

	pub fn module() -> Result<Module, ContextError> {
		let mut m = Module::with_item(["err"])?;
		m.function_meta(not_found)?;
		m.function_meta(validation)?;
		m.function_meta(conflict)?;
		m.function_meta(app)?;
		Ok(m)
	}
}

/// Every module of the v1 dispatch table, for installation into the compile `Context`.
///
/// # Errors
/// Whatever Rune raises registering a module, a type or a function.
pub fn modules() -> Result<Vec<Module>, ContextError> {
	Ok(vec![
		invoices::module()?,
		nav::module()?,
		billing::module()?,
		auth::module()?,
		err::module()?,
	])
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The decoders below are where a silent wrong answer becomes a wrong invoice; the
	/// service-shaped bindings above them are covered by `examples/*/app*/tests.rn`, which
	/// `cargo test` never runs.
	fn obj(pairs: &[(&str, Value)]) -> Object {
		let mut o = Object::new();
		for (k, v) in pairs {
			o.insert(rune::alloc::String::try_from(*k).unwrap(), v.clone()).unwrap();
		}
		o
	}

	fn unit() -> Value {
		Value::from(())
	}

	fn text(s: &str) -> Value {
		rune::to_value(s.to_owned()).unwrap()
	}

	#[test]
	fn patch_of_tells_absent_null_and_value_apart() {
		let o = obj(&[("cleared", unit()), ("set", text("x"))]);
		assert!(matches!(patch_of::<String>(&o, "missing").unwrap(), Patch::Undefined));
		assert!(matches!(patch_of::<String>(&o, "cleared").unwrap(), Patch::Null));
		let Patch::Value(v) = patch_of::<String>(&o, "set").unwrap() else {
			panic!("a present value did not decode as one");
		};
		assert_eq!(v, "x");
	}

	#[test]
	fn tri_tells_absent_null_and_value_apart() {
		let o = obj(&[("cleared", unit()), ("set", text("x"))]);
		assert_eq!(tri(&o, "missing", string_of).unwrap(), None);
		assert_eq!(tri(&o, "cleared", string_of).unwrap(), Some(None));
		assert_eq!(tri(&o, "set", string_of).unwrap(), Some(Some("x".to_owned())));
	}

	/// An explicit `()` reads as absent everywhere but the two three-state decoders above.
	#[test]
	fn str_of_reads_an_explicit_unit_as_absent() {
		let o = obj(&[("a", unit()), ("b", text("y"))]);
		assert_eq!(str_of(&o, "a").unwrap(), None);
		assert_eq!(str_of(&o, "missing").unwrap(), None);
		assert_eq!(str_of(&o, "b").unwrap(), Some("y".to_owned()));
	}

	#[test]
	fn a_missing_required_key_is_a_validation_error_not_a_vm_trap() {
		let o = obj(&[("present", text("v"))]);
		let ScriptError(err) = req_str(&o, "absent").unwrap_err();
		assert_eq!(err.parts(), (StatusCode::BAD_REQUEST, "E-CORE-VALIDATION"));
		// An explicit `()` is absent too, so clearing a required field is refused, not silently
		// decoded as an empty string.
		let ScriptError(err) = req_str(&obj(&[("present", unit())]), "present").unwrap_err();
		assert_eq!(err.parts().1, "E-CORE-VALIDATION");
		assert_eq!(req_str(&o, "present").unwrap(), "v");
	}

	#[test]
	fn a_non_object_argument_is_a_validation_error() {
		let ScriptError(err) = object(&text("not an object"), "request").unwrap_err();
		assert_eq!(err.parts(), (StatusCode::BAD_REQUEST, "E-CORE-VALIDATION"));
	}

	/// `money(…)` is the only door: a Rune float can never become an amount, and a currency that
	/// is not three ASCII letters never reaches a row.
	#[test]
	fn an_amount_is_never_a_float_and_never_an_unparsed_currency() {
		assert!(money_of(&rune::to_value(1.5f64).unwrap()).is_err());
		assert!(money_of(&text("12500.00")).is_err());
		assert!(Money::new("12500.00", "EURO").is_err());
		assert!(Money::new("12.500", "HUF").is_err());

		let ok = rune::to_value(Money::new("12500.00", "HUF").unwrap()).unwrap();
		let (amount, currency) = money_of(&ok).unwrap();
		assert_eq!(amount.0, 1_250_000);
		assert_eq!(currency.as_str(), "HUF");

		let eur = CurrencyCode::from_trusted("EUR".to_owned());
		assert!(money_in(&ok, &eur).is_err());
		assert_eq!(money_in(&ok, &currency).unwrap().0, 1_250_000);
	}

	#[test]
	fn a_qty_is_never_a_float_either() {
		assert!(qty_of(&rune::to_value(2.5f64).unwrap()).is_err());
		assert_eq!(
			qty_of(&rune::to_value(Qty::new("2.5").unwrap()).unwrap()).unwrap().0,
			2_500_000
		);
	}

	#[test]
	fn only_a_prefixed_ulid_looks_like_a_uid() {
		assert!(looks_like_uid("inv_01JCZ5X8K9N7QW3M6R2T4V8Y0A"));
		// An external id is not one by accident: lowercase, the wrong length and no prefix all
		// resolve as a `code` instead.
		assert!(!looks_like_uid("inv_01jcz5x8k9n7qw3m6r2t4v8y0a"));
		assert!(!looks_like_uid("inv_01JCZ5X8K9N7QW3M6R2T4V8Y0"));
		assert!(!looks_like_uid("01JCZ5X8K9N7QW3M6R2T4V8Y0A"));
		assert!(!looks_like_uid("ACME-2024"));
	}
}

// vim: ts=4
