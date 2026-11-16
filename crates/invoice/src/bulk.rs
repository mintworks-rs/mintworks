// SPDX-License-Identifier: MPL-2.0
//! Bulk party and service upsert: `POST /api/billing-parties/bulk` and `POST /api/services/bulk`
//! (Rune `invoices::bulk_parties` / `invoices::bulk_services`). The client parses and maps the
//! CSV; these check and write the rows. Body `{rows: [{field: "text"…}], onDuplicate:
//! "skip"|"update", dryRun: bool, decimalDot?: "decimal"|"thousands"}` → `{results: [{row,
//! status: "create"|"update"|"skip"|"error", uid?, error?}]}`. A service price like `12.500`
//! without `decimalDot` is a 400 [`AMBIGUOUS_DECIMAL`]: the client asks which reading is meant
//! and retries with it.
//!
//! A row the service refuses carries that refusal's `errCode` as its `error`; a row that fails
//! the checks here carries a sentence. Rows are written one by one, not in one transaction, so a
//! bad row does not block the rest. Both are Admin on the acting org
//! ([`crate::service_api::Invoices::bulk_parties`]); the catalogue writes also check
//! `seller_admin`.

use std::collections::HashMap;

use axum::http::StatusCode;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use serde_json::{Map, Value, json};

use crate::service_api::{Invoices, MAX_PAGE_LIMIT};
use crate::store::{PartyFilter, PartyKind, PartyPatch, ServiceDef, ServicePatch};
use crate::vat::VatCode;

pub const MAX_ROWS: usize = 1000;
pub const AMBIGUOUS_DECIMAL: &str = "E-INV-AMBIGUOUS-DECIMAL";

const PARTY_KEYS: [&str; 13] = [
	"kind",
	"name",
	"country",
	"taxNumber",
	"euVatId",
	"groupTaxNo",
	"postcode",
	"city",
	"street",
	"email",
	"paymentDays",
	"paymentMethod",
	"docLang",
];
const SERVICE_KEYS: [&str; 7] =
	["code", "name", "description", "unit", "unitPrice", "currency", "vatCode"];

/// The request envelope, checked: a bad one is a 400 for the whole request.
struct Body<'a> {
	rows: &'a [Value],
	update: bool,
	dry: bool,
	dot: Option<&'a str>,
}

fn body(v: &Value) -> ClResult<Body<'_>> {
	let rows = v
		.get("rows")
		.ok_or_else(|| Error::validation("rows is required"))?
		.as_array()
		.ok_or_else(|| Error::validation("rows must be an array"))?;
	if rows.len() > MAX_ROWS {
		return Err(Error::validation(format!("a bulk upsert takes at most {MAX_ROWS} rows")));
	}
	let update = match v.get("onDuplicate") {
		None | Some(Value::Null) => false,
		Some(Value::String(s)) if s == "skip" => false,
		Some(Value::String(s)) if s == "update" => true,
		_ => return Err(Error::validation("onDuplicate must be skip or update")),
	};
	let dry = match v.get("dryRun") {
		None | Some(Value::Null) => false,
		Some(Value::Bool(b)) => *b,
		_ => return Err(Error::validation("dryRun must be true or false")),
	};
	let dot = match v.get("decimalDot") {
		None | Some(Value::Null) => None,
		Some(Value::String(s)) if s == "decimal" || s == "thousands" => Some(s.as_str()),
		_ => return Err(Error::validation("decimalDot must be decimal or thousands")),
	};
	Ok(Body { rows, update, dry, dot })
}

/// The row's `keys`, trimmed, blanks dropped. Every value is text, as a CSV cell is.
fn clean(raw: &Value, keys: &[&str]) -> Result<Map<String, Value>, String> {
	let raw = raw.as_object().ok_or("a row must be an object")?;
	let mut out = Map::new();
	for k in keys {
		match raw.get(*k) {
			None => {}
			Some(Value::String(s)) => {
				let t = s.trim();
				if !t.is_empty() {
					out.insert((*k).to_owned(), t.into());
				}
			}
			Some(_) => return Err(format!("{k} must be text")),
		}
	}
	Ok(out)
}

fn error(i: usize, e: impl Into<String>) -> Value {
	json!({ "row": i, "status": "error", "error": e.into() })
}

/// `uid: None` is a row an earlier `create` of this same dry run stands for.
fn done(i: usize, status: &str, uid: Option<&str>) -> Value {
	json!({ "row": i, "status": status, "uid": uid })
}

/// [`crate::mail::valid_email`], and no `acc_` prefix: the mailer reads an `acc_…` recipient as
/// an account uid and mails that account's own address.
fn valid_email(s: &str) -> bool {
	crate::mail::valid_email(s) && !s.starts_with("acc_")
}

/// `CC:number`, or `""` when `tax` has nothing to key on. For HU the number is the first 8 digits:
/// the taxpayer id, whatever VAT code and county follow. Elsewhere it is the ASCII letters and
/// digits, uppercased: letters can tell two foreign numbers apart. A missing country is HU.
pub fn tax_key(tax: &str, country: Option<&str>) -> String {
	let cc = country
		.map(|c| c.trim().to_uppercase())
		.filter(|c| !c.is_empty())
		.unwrap_or_else(|| "HU".to_owned());
	let hu = cc == "HU";
	let d: String = tax
		.to_uppercase()
		.chars()
		.filter(|c| c.is_ascii_digit() || (!hu && c.is_ascii_uppercase()))
		.take(if hu { 8 } else { usize::MAX })
		.collect();
	if d.is_empty() { String::new() } else { format!("{cc}:{d}") }
}

// ------------------------------------------------------------ parties

/// A duplicate is the same country and normalised tax number ([`tax_key`]), else, for a row
/// without one, the same exact name among parties that have no tax number either: a bare name
/// must not overwrite a taxed party. A name two such parties share is an error row, as it cannot
/// say which one is meant. Absent `kind` is `C` with a tax number or EU VAT id, else `P`; absent
/// `country` is HU.
pub(crate) async fn parties(inv: &Invoices, ctx: &Ctx, v: &Value) -> ClResult<Value> {
	let b = body(v)?;
	// `Some("")` marks an ambiguous key; `None` a row this dry run creates.
	// Two concurrent imports can both create a party (no unique index: a tax number
	// may repeat); serialize bulk party imports per org if concurrent imports become real.
	let mut by_tax: HashMap<String, Option<String>> = HashMap::new();
	let mut by_name: HashMap<String, Option<String>> = HashMap::new();
	// Paged at `MAX_PAGE_LIMIT`; the default sort is name, then id: stable.
	let mut offset = 0_i64;
	loop {
		let filter = PartyFilter::parse(None, None, Some(offset), Some(MAX_PAGE_LIMIT))?;
		let (page, total) = inv.list_parties_page(ctx, &filter).await?;
		for p in page.iter().map(|lp| &lp.party) {
			let key = p
				.tax_number
				.as_deref()
				.map_or_else(String::new, |t| tax_key(t, Some(&p.country)));
			let uid = p.uid.as_str().to_owned();
			let (map, k) =
				if key.is_empty() { (&mut by_name, p.name.clone()) } else { (&mut by_tax, key) };
			map.entry(k).and_modify(|seen| *seen = Some(String::new())).or_insert(Some(uid));
		}
		offset += i64::try_from(page.len()).unwrap_or(MAX_PAGE_LIMIT);
		if page.is_empty() || u64::try_from(offset).unwrap_or(u64::MAX) >= total {
			break;
		}
	}
	let mut results = Vec::with_capacity(b.rows.len());
	for (i, raw) in b.rows.iter().enumerate() {
		results.push(party_row(inv, ctx, &b, &mut by_tax, &mut by_name, i, raw).await);
	}
	Ok(json!({ "results": results }))
}

async fn party_row(
	inv: &Invoices,
	ctx: &Ctx,
	b: &Body<'_>,
	by_tax: &mut HashMap<String, Option<String>>,
	by_name: &mut HashMap<String, Option<String>>,
	i: usize,
	raw: &Value,
) -> Value {
	let mut p = match party_fields(raw) {
		Ok(p) => p,
		Err(e) => return error(i, e),
	};
	let key = p
		.tax_number
		.value()
		.map_or_else(String::new, |t| tax_key(t, p.country.as_deref()));
	let name = p.name.clone().unwrap_or_default();
	let found = if key.is_empty() { by_name.get(&name) } else { by_tax.get(&key) }.cloned();
	match found {
		Some(Some(uid)) if uid.is_empty() && !key.is_empty() => {
			error(i, "several partners have this tax number")
		}
		Some(Some(uid)) if uid.is_empty() => {
			error(i, "several partners have this name; add a tax number")
		}
		Some(uid) if !b.update => done(i, "skip", uid.as_deref()),
		Some(uid) => {
			if b.dry {
				return done(i, "update", uid.as_deref());
			}
			// `None` is stored by dry runs only, which returned above.
			match inv.update_party(ctx, &uid.unwrap_or_default(), &p).await {
				Ok(u) => done(i, "update", Some(u.uid.as_str())),
				Err(e) => error(i, e.parts().1),
			}
		}
		None => {
			let mut uid = None;
			if !b.dry {
				if p.kind.is_none() {
					let company = !key.is_empty() || p.eu_vat_id.value().is_some();
					p.kind = Some(if company { PartyKind::Company } else { PartyKind::Person });
				}
				p.country.get_or_insert_with(|| "HU".to_owned());
				match inv.create_party(ctx, &p).await {
					Ok(c) => uid = Some(c.uid.as_str().to_owned()),
					Err(e) => return error(i, e.parts().1),
				}
			}
			// A later row of the same request is a duplicate of this one.
			if key.is_empty() {
				by_name.insert(name, uid.clone());
			} else {
				by_tax.insert(key, uid.clone());
			}
			done(i, "create", uid.as_deref())
		}
	}
}

/// A party row's fields, checked: `Err(text)` names what is wrong.
fn party_fields(raw: &Value) -> Result<PartyPatch, String> {
	let mut p = clean(raw, &PARTY_KEYS)?;
	let text = |k: &str| p.get(k).and_then(Value::as_str);
	if text("name").is_none() {
		return Err("name is required".into());
	}
	if text("kind").is_some_and(|k| k != "C" && k != "P") {
		return Err("kind must be C (company) or P (person)".into());
	}
	if text("email").is_some_and(|e| !valid_email(e)) {
		return Err("email is not a valid address".into());
	}
	if text("docLang").is_some_and(|l| !matches!(l, "hu" | "en" | "hu-en")) {
		return Err("docLang must be hu, en or hu-en".into());
	}
	if let Some(d) = text("paymentDays").map(str::to_owned) {
		match d.parse::<i64>() {
			Ok(n) if (0..=365).contains(&n) => {
				p.insert("paymentDays".into(), n.into());
			}
			_ => return Err("paymentDays must be a whole number between 0 and 365".into()),
		}
	}
	serde_json::from_value(Value::Object(p)).map_err(|e| e.to_string())
}

// ------------------------------------------------------------ services

/// A duplicate is the same `code`. A new row needs `name`, `unit`, `unitPrice` and `vatCode`;
/// an update patches only the fields the row carries, of `name`, `description`, `unit`,
/// `unitPrice` and `vatCode`. `currency` defaults to the base currency and must be it.
pub(crate) async fn services(inv: &Invoices, ctx: &Ctx, v: &Value) -> ClResult<Value> {
	let b = body(v)?;
	if b.dot.is_none() {
		for (i, raw) in b.rows.iter().enumerate() {
			if let Some(a) = raw.get("unitPrice").and_then(Value::as_str).filter(|a| ambiguous(a)) {
				return Err(Error::coded(
					StatusCode::BAD_REQUEST,
					AMBIGUOUS_DECIMAL,
					format!(
						"row {}: \"{a}\" — say whether the separator before the last 3 digits is a decimal or a thousands separator",
						i + 1
					),
				));
			}
		}
	}
	let base = inv.base_currency(ctx).await?.code;
	let mut seen: HashMap<String, Option<String>> = HashMap::new();
	let mut results = Vec::with_capacity(b.rows.len());
	for (i, raw) in b.rows.iter().enumerate() {
		results.push(service_row(inv, ctx, &b, &base, &mut seen, i, raw).await);
	}
	Ok(json!({ "results": results }))
}

/// A catalogue row's fields, checked.
struct ServiceRow {
	code: String,
	name: Option<String>,
	description: Option<String>,
	unit: Option<String>,
	unit_price: Option<Money>,
	vat_code: Option<VatCode>,
}

async fn service_row(
	inv: &Invoices,
	ctx: &Ctx,
	b: &Body<'_>,
	base: &CurrencyCode,
	seen: &mut HashMap<String, Option<String>>,
	i: usize,
	raw: &Value,
) -> Value {
	let s = match service_fields(raw, b.dot, base) {
		Ok(s) => s,
		Err(e) => return error(i, e),
	};
	let found = match seen.get(&s.code) {
		Some(uid) => Some(uid.clone()),
		None => match inv.service_by_code(ctx, &s.code).await {
			Ok(svc) => Some(Some(svc.uid.as_str().to_owned())),
			Err(Error::NotFound) => None,
			Err(e) => return error(i, e.parts().1),
		},
	};
	match found {
		Some(uid) if !b.update => done(i, "skip", uid.as_deref()),
		Some(uid) => {
			if b.dry {
				return done(i, "update", uid.as_deref());
			}
			let patch = ServicePatch {
				name: s.name,
				description: s.description.map_or(Patch::Undefined, Patch::Value),
				unit: s.unit,
				unit_price: s.unit_price,
				vat_code: s.vat_code,
				..ServicePatch::default()
			};
			// `None` is stored by dry runs only, which returned above.
			match inv.update_service(ctx, &uid.unwrap_or_default(), &patch).await {
				Ok(u) => done(i, "update", Some(u.uid.as_str())),
				Err(e) => error(i, e.parts().1),
			}
		}
		None => {
			let (name, unit, unit_price, vat_code) =
				match (s.name, s.unit, s.unit_price, s.vat_code) {
					(Some(name), Some(unit), Some(price), Some(vat)) => (name, unit, price, vat),
					(name, unit, price, _) => {
						let missing = if name.is_none() {
							"name"
						} else if unit.is_none() {
							"unit"
						} else if price.is_none() {
							"unitPrice"
						} else {
							"vatCode"
						};
						return error(i, format!("{missing} is required for a new item"));
					}
				};
			let mut uid = None;
			if !b.dry {
				let def = ServiceDef {
					code: s.code.clone(),
					name,
					description: s.description,
					unit,
					unit_price,
					vat_code,
					name_en: None,
					product: Some(false),
					prices: None,
				};
				match inv.create_service(ctx, &def).await {
					Ok(c) => uid = Some(c.uid.as_str().to_owned()),
					Err(e) => return error(i, e.parts().1),
				}
			}
			seen.insert(s.code, uid.clone());
			done(i, "create", uid.as_deref())
		}
	}
}

fn service_fields(
	raw: &Value,
	dot: Option<&str>,
	base: &CurrencyCode,
) -> Result<ServiceRow, String> {
	let mut s = clean(raw, &SERVICE_KEYS)?;
	let mut take = |k: &str| match s.remove(k) {
		Some(Value::String(t)) => Some(t),
		_ => None,
	};
	let code = take("code").ok_or("code is required")?;
	let currency = take("currency");
	let unit_price = match take("unitPrice") {
		None => None,
		Some(a) => {
			let cur = currency.as_deref().map_or_else(|| Ok(base.clone()), CurrencyCode::parse);
			let (Ok(cur), Ok(m)) = (cur, Money::parse(&amount(&a, dot))) else {
				return Err("unitPrice must be a decimal amount in a known currency".into());
			};
			if cur != *base {
				return Err(format!("unitPrice must be in {}", base.as_str()));
			}
			Some(m)
		}
	};
	let vat_code = match take("vatCode") {
		None => None,
		Some(c) => Some(
			serde_json::from_value(Value::String(c))
				.map_err(|_| "vatCode is not a known VAT code".to_owned())?,
		),
	};
	Ok(ServiceRow {
		code,
		name: take("name"),
		description: take("description"),
		unit: take("unit"),
		unit_price,
		vat_code,
	})
}

/// A Hungarian- or English-style amount (`12 500,00`, `1,234.56`) in the dot form
/// [`Money::parse`] takes: spaces (also U+00A0 and U+202F) dropped. With both separators the
/// last one is the decimal point and the other must group by 3 (else returned as is, for
/// `Money::parse` to refuse). With one: grouped by 3 and repeated (`1.234.567`) or
/// `dot == "thousands"` (`1,000`) drops it, else it is the decimal point.
pub fn amount(a: &str, dot: Option<&str>) -> String {
	let a = nospace(a);
	let sep = if a.contains(',') { ',' } else { '.' };
	let mut a = if a.contains(',') && a.contains('.') {
		match mixed(&a, ',', '.').or_else(|| mixed(&a, '.', ',')) {
			Some(m) => m,
			None => return a,
		}
	} else if grouped(&a, sep) && (a.matches(sep).count() > 1 || dot == Some("thousands")) {
		a.replace(sep, "")
	} else {
		a.replace(',', ".")
	};
	// Trailing fraction zeros carry nothing (`12.500` → `12.5`) and would exceed 2 places.
	// One dot only: `1.000.00` is left whole for `Money::parse` to refuse.
	while a.matches('.').count() == 1 && (a.ends_with('0') || a.ends_with('.')) {
		a.pop();
	}
	a
}

fn nospace(a: &str) -> String {
	a.chars().filter(|c| !matches!(c, ' ' | '\u{a0}' | '\u{202f}')).collect()
}

/// `12.500`, `1,000`: one separator, once, with exactly 3 digits after it — a decimal or a
/// thousands separator, nothing tells which.
pub fn ambiguous(a: &str) -> bool {
	let a = nospace(a);
	let sep = if a.contains(',') { ',' } else { '.' };
	!(a.contains(',') && a.contains('.')) && a.matches(sep).count() == 1 && grouped(&a, sep)
}

/// `1.234.567`, `1,000`: something, then 1+ `sep` each followed by exactly 3 digits.
fn grouped(a: &str, sep: char) -> bool {
	let mut parts = a.split(sep);
	let first = parts.next().unwrap_or_default();
	let rest: Vec<&str> = parts.collect();
	!first.is_empty()
		&& !rest.is_empty()
		&& rest.iter().all(|p| p.len() == 3 && p.bytes().all(|c| c.is_ascii_digit()))
}

/// `1.234,56` with `dec` ',' and `grp` '.' → `1234.56`; `None` if not that shape.
fn mixed(a: &str, dec: char, grp: char) -> Option<String> {
	let (int, frac) = a.split_once(dec)?;
	(!frac.contains(dec) && !frac.contains(grp) && grouped(int, grp))
		.then(|| format!("{}.{frac}", int.replace(grp, "")))
}

// vim: ts=4
