// SPDX-License-Identifier: MPL-2.0
//! The yearly revenue limits of the seller's Hungarian tax regimes (alanyi adómentesség, KATA,
//! átalányadó), and how much of each is used — `GET /api/invoices/tax-limits`.
//!
//! [`Invoices::revenue`](crate::Invoices::revenue) supplies the facts; the statute figures that
//! change every year live in [`limit_table`]. Every figure is whole forints and integer
//! arithmetic, dividing last so a result rounds down once.

use serde::Serialize;
use serde_json::{Value as Json, json};
use time::Date;

use crate::store::{RevenueMonth, SellerVersion};

/// The statutory figures of one year; `None` where not yet published.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitTable {
	/// Áfa tv. 188. §: the alanyi adómentesség ceiling.
	pub aam: Option<i64>,
	/// 2022. évi XIII. tv. 7. §: 18M/year, 1.5M per month of KATA status.
	pub kata: Option<i64>,
	/// The monthly minimum wage. Átalányadó's limit is 10× (retail 50×) its yearly sum, and its
	/// tax-free income ½× (Szja tv. 50–53. §).
	pub min_wage: Option<i64>,
	/// The átalány költséghányad of general activity (Szja tv. 53. § (1)).
	pub general_cost_pct: Option<i64>,
}

/// The figures of `year`. Add a year here when the statute publishes it.
#[must_use]
pub fn limit_table(year: i32) -> LimitTable {
	match year {
		2025 => LimitTable {
			aam: Some(18_000_000),
			kata: Some(18_000_000),
			min_wage: Some(290_800),
			general_cost_pct: Some(40),
		},
		2026 => LimitTable {
			aam: Some(20_000_000),
			kata: Some(18_000_000),
			min_wage: Some(322_800),
			general_cost_pct: Some(45),
		},
		2027 => LimitTable {
			aam: Some(22_000_000),
			kata: None,
			min_wage: None,
			general_cost_pct: Some(50),
		},
		_ => LimitTable::default(),
	}
}

/// The `GET /api/invoices/tax-limits` body: `{year, today, months: [{month, invoiced, received,
/// outstanding}], limits: [{kind, basis, limit, fullLimit, since, used, pending, crossedOn,
/// costPct?, taxFreeIncome?, startsOn?}]}`. `months` are running HUF totals; `seller`'s prior
/// revenue counts on both bases from January when its year is `year`.
#[must_use]
pub fn tax_limits(year: i32, today: &str, seller: &SellerVersion, rev: &[RevenueMonth]) -> Json {
	let table = limit_table(year);
	let prior = match (seller.prior_revenue_year, seller.prior_revenue_huf) {
		(Some(y), Some(huf)) if y == i64::from(year) => huf,
		_ => 0,
	};
	// Summed in fillér, so a month's rounding does not drift the running total.
	let (mut inv, mut rec, mut out) = (prior, prior, 0);
	let months: Vec<MonthRow> = rev
		.iter()
		.map(|m| {
			inv += m.invoiced_huf.0;
			rec += m.received_huf.0;
			out += m.outstanding_huf.0;
			MonthRow {
				month: m.month.clone(),
				invoiced: inv / 100,
				received: rec / 100,
				outstanding: out / 100,
			}
		})
		.collect();

	// A start inside `year` pro-rates; one before it is a full year, one after it no limit yet.
	// One start date for both the VAT and the income regime.
	let start = seller.regime_since.as_deref().and_then(ymd);
	let not_yet = start.is_some_and(|d| d.year() > year);
	let in_year = start.filter(|d| d.year() == year);
	let since = in_year.and(seller.regime_since.as_deref());
	let gate = |v: Option<i64>| if not_yet { None } else { v };

	let mut limits = Vec::new();
	if seller.vat_scheme == "ALANYI_MENTES" {
		let limit = gate(by_days(table.aam, in_year, year));
		limits.push(limit_row("AAM", "INVOICED", limit, table.aam, since, &months));
	}
	if seller.income_regime == "KATA" {
		let limit = match (table.kata, in_year) {
			(Some(k), Some(d)) => Some(k / 12 * (13 - i64::from(u8::from(d.month())))),
			(k, _) => k,
		};
		limits.push(limit_row("KATA", "RECEIVED", gate(limit), table.kata, since, &months));
	}
	if seller.income_regime == "ATALANY" {
		let cost_pct = seller.expense_ratio_pct.or(table.general_cost_pct);
		let known = match (table.min_wage, cost_pct) {
			(Some(w), Some(c)) if c < 100 => Some((w, c)),
			_ => None,
		};
		// Only retail gets the 90 % ratio, and retail alone gets the 50× limit (Szja tv. 50. §).
		let full = known.map(|(w, c)| if c == 90 { w * 12 * 50 } else { w * 12 * 10 });
		let mut entry = limit_row(
			"ATALANY",
			"RECEIVED",
			gate(by_days(full, in_year, year)),
			full,
			since,
			&months,
		);
		entry["costPct"] = json!(cost_pct);
		limits.push(entry);
		// The adómentes keret: revenue up to which no szja is due. A milestone, not a ceiling,
		// and never pro-rated.
		let income = known.map(|(w, _)| w * 12 / 2);
		let revenue = known.map(|(w, c)| w * 12 / 2 * 100 / (100 - c));
		let mut free =
			limit_row("ATALANY_TAXFREE", "RECEIVED", gate(revenue), revenue, None, &months);
		free["costPct"] = json!(cost_pct);
		free["taxFreeIncome"] = json!(income);
		limits.push(free);
	}
	if not_yet {
		for l in &mut limits {
			l["startsOn"] = json!(seller.regime_since);
		}
	}
	json!({ "year": year, "today": today, "months": months, "limits": limits })
}

#[derive(Serialize)]
struct MonthRow {
	month: String,
	invoiced: i64,
	received: i64,
	outstanding: i64,
}

/// One limit row: `crossedOn` is the first month whose running total passes `limit`. `pending` is
/// the year's issued-but-unpaid total, shown beside a `RECEIVED` basis only.
fn limit_row(
	kind: &str,
	basis: &str,
	limit: Option<i64>,
	full_limit: Option<i64>,
	since: Option<&str>,
	months: &[MonthRow],
) -> Json {
	let invoiced = basis == "INVOICED";
	let pick = |m: &MonthRow| if invoiced { (m.invoiced, 0) } else { (m.received, m.outstanding) };
	let (used, pending) = months.last().map_or((0, 0), pick);
	let crossed =
		limit.and_then(|l| months.iter().find(|m| pick(m).0 > l).map(|m| m.month.clone()));
	json!({
		"kind": kind,
		"basis": basis,
		"limit": limit,
		"fullLimit": full_limit,
		"since": since,
		"used": used,
		"pending": pending,
		"crossedOn": crossed,
	})
}

/// `full × days left from since to 31 Dec, inclusive ÷ days in the year` — NAV's "időarányos".
fn by_days(full: Option<i64>, since: Option<Date>, year: i32) -> Option<i64> {
	let (Some(full), Some(d)) = (full, since) else { return full };
	let total = i64::from(time::util::days_in_year(year));
	Some(full * (total - i64::from(d.ordinal()) + 1) / total)
}

/// A real `YYYY-MM-DD` calendar date, year 2000 or later.
fn ymd(s: &str) -> Option<Date> {
	crate::numbering::parse(s).ok().filter(|d| d.year() >= 2000)
}

// vim: ts=4
