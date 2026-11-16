// SPDX-License-Identifier: MPL-2.0
//! The pure half of the tax-regime limits: the statute table and the arithmetic of
//! `taxlimits::tax_limits`, with no database. The service read over a real store is in
//! `invoice_service.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_core::prelude::*;
use mintworks_invoice::{
	store::{RevenueMonth, SellerVersion, SellerVersionPatch},
	taxlimits::{LimitTable, limit_table, tax_limits},
};
use serde_json::{Value, json};

#[allow(clippy::needless_pass_by_value)]
fn seller(patch: SellerVersionPatch) -> SellerVersion {
	patch.merged(None)
}

/// One month of revenue, in whole forints.
fn month(m: &str, invoiced: i64, received: i64, outstanding: i64) -> RevenueMonth {
	RevenueMonth {
		month: m.into(),
		invoiced_huf: Money(invoiced * 100),
		received_huf: Money(received * 100),
		outstanding_huf: Money(outstanding * 100),
	}
}

fn row<'a>(body: &'a Value, kind: &str) -> &'a Value {
	body["limits"].as_array().unwrap().iter().find(|l| l["kind"] == kind).unwrap()
}

#[test]
fn limit_table_holds_the_published_figures_and_nothing_else() {
	let t = limit_table(2026);
	assert_eq!(
		(t.aam, t.kata, t.min_wage, t.general_cost_pct),
		(Some(20_000_000), Some(18_000_000), Some(322_800), Some(45))
	);
	let t = limit_table(2025);
	assert_eq!(
		(t.aam, t.min_wage, t.general_cost_pct),
		(Some(18_000_000), Some(290_800), Some(40))
	);
	let t = limit_table(2027);
	assert_eq!((t.aam, t.kata, t.min_wage), (Some(22_000_000), None, None));
	assert_eq!(limit_table(2030), LimitTable::default());
	assert_eq!(limit_table(2024), LimitTable::default());
}

/// A NORMAL seller with no income regime has no limit rows; months still sum.
#[test]
fn a_normal_seller_gets_months_and_no_limits() {
	let body = tax_limits(
		2026,
		"2026-03-01",
		&seller(SellerVersionPatch::default()),
		&[month("2026-01", 100, 50, 50), month("2026-02", 200, 100, 0)],
	);
	assert_eq!(body["year"], 2026);
	assert_eq!(body["today"], "2026-03-01");
	assert_eq!(body["limits"], json!([]));
	assert_eq!(
		body["months"],
		json!([
			{"month": "2026-01", "invoiced": 100, "received": 50, "outstanding": 50},
			{"month": "2026-02", "invoiced": 300, "received": 150, "outstanding": 50},
		])
	);
	let empty = tax_limits(2026, "2026-03-01", &seller(SellerVersionPatch::default()), &[]);
	assert_eq!(empty["months"], json!([]));
}

/// Prior revenue counts from January on both bases, only in its own year; `crossedOn` is the
/// first month whose running total passes the limit.
#[test]
fn aam_counts_prior_revenue_and_crosses_on_the_first_month_over() {
	let s = seller(SellerVersionPatch {
		vat_scheme: Some("ALANYI_MENTES".into()),
		prior_revenue_year: Patch::Value(2026),
		prior_revenue_huf: Patch::Value(19_000_000 * 100),
		..Default::default()
	});
	let rev = [month("2026-01", 500_000, 0, 500_000), month("2026-02", 1_000_000, 0, 0)];
	let body = tax_limits(2026, "2026-03-01", &s, &rev);
	assert_eq!(body["months"][0]["received"], 19_000_000);
	let aam = row(&body, "AAM");
	assert_eq!(aam["basis"], "INVOICED");
	assert_eq!(aam["limit"], 20_000_000);
	assert_eq!(aam["fullLimit"], 20_000_000);
	assert_eq!(aam["used"], 20_500_000);
	assert_eq!(aam["pending"], 0);
	assert_eq!(aam["crossedOn"], "2026-02");
	assert_eq!(aam["since"], Value::Null);
	assert!(aam.get("startsOn").is_none());

	// Another year's prior revenue counts nowhere.
	let body = tax_limits(2027, "2027-03-01", &s, &[month("2027-01", 500_000, 0, 0)]);
	let aam = row(&body, "AAM");
	assert_eq!((aam["limit"].as_i64(), aam["used"].as_i64()), (Some(22_000_000), Some(500_000)));
	assert_eq!(aam["crossedOn"], Value::Null);
}

/// An unpublished figure is null, and nothing is crossed against it.
#[test]
fn an_unpublished_year_has_null_limits() {
	let s = seller(SellerVersionPatch { income_regime: Some("KATA".into()), ..Default::default() });
	let body = tax_limits(2027, "2027-03-01", &s, &[month("2027-01", 0, 30_000_000, 0)]);
	let kata = row(&body, "KATA");
	assert_eq!(kata["basis"], "RECEIVED");
	assert_eq!((kata["limit"].clone(), kata["fullLimit"].clone()), (Value::Null, Value::Null));
	assert_eq!(kata["used"], 30_000_000);
	assert_eq!(kata["crossedOn"], Value::Null);

	let s = seller(SellerVersionPatch {
		vat_scheme: Some("ALANYI_MENTES".into()),
		income_regime: Some("ATALANY".into()),
		..Default::default()
	});
	let body = tax_limits(2030, "2030-01-01", &s, &[]);
	for kind in ["AAM", "ATALANY", "ATALANY_TAXFREE"] {
		assert_eq!(row(&body, kind)["limit"], Value::Null, "{kind}");
		assert_eq!(row(&body, kind)["fullLimit"], Value::Null, "{kind}");
	}
}

/// A start inside the year pro-rates (AAM by days, KATA by months); a start in a later year
/// leaves the limits null and says when they start.
#[test]
fn regime_since_pro_rates_and_a_later_start_has_no_limit_yet() {
	let patch = |since: &str| SellerVersionPatch {
		vat_scheme: Some("ALANYI_MENTES".into()),
		income_regime: Some("KATA".into()),
		regime_since: Patch::Value(since.into()),
		..Default::default()
	};
	let body = tax_limits(2026, "2026-08-01", &seller(patch("2026-07-01")), &[]);
	let (aam, kata) = (row(&body, "AAM"), row(&body, "KATA"));
	// 20M × 184 days left of 365, rounded down once.
	assert_eq!(aam["limit"], 10_082_191);
	assert_eq!(aam["fullLimit"], 20_000_000);
	assert_eq!(aam["since"], "2026-07-01");
	assert_eq!(kata["limit"], 9_000_000);
	assert_eq!(kata["fullLimit"], 18_000_000);

	let body = tax_limits(2026, "2026-08-01", &seller(patch("2027-01-01")), &[]);
	for kind in ["AAM", "KATA"] {
		let l = row(&body, kind);
		assert_eq!(l["limit"], Value::Null, "{kind}");
		assert_eq!(l["startsOn"], "2027-01-01", "{kind}");
		assert_eq!(l["since"], Value::Null, "{kind}");
	}
	assert_eq!(row(&body, "AAM")["fullLimit"], 20_000_000);

	// A start before the year is a full year.
	let body = tax_limits(2026, "2026-08-01", &seller(patch("2025-03-01")), &[]);
	assert_eq!(row(&body, "AAM")["limit"], 20_000_000);
	assert_eq!(row(&body, "KATA")["limit"], 18_000_000);

	// 29 Feb of a common year is no date, so no pro-rating; 1 Mar is day 60 of 365.
	let body = tax_limits(2027, "2027-08-01", &seller(patch("2027-02-29")), &[]);
	assert_eq!(
		(row(&body, "AAM")["limit"].as_i64(), row(&body, "AAM")["since"].clone()),
		(Some(22_000_000), Value::Null)
	);
	let body = tax_limits(2027, "2027-08-01", &seller(patch("2027-03-01")), &[]);
	assert_eq!(row(&body, "AAM")["limit"], 22_000_000_i64 * 306 / 365);
}

/// Átalányadó: 10× the yearly minimum wage, and the tax-free revenue that ½× its yearly sum of
/// income takes at the cost ratio. `expense_ratio_pct` overrides the general ratio; 90 % is
/// retail and gets 50×.
#[test]
fn atalany_limits_follow_the_minimum_wage_and_cost_ratio() {
	let s =
		seller(SellerVersionPatch { income_regime: Some("ATALANY".into()), ..Default::default() });
	let body = tax_limits(2026, "2026-03-01", &s, &[month("2026-01", 0, 1_000, 2_000)]);
	let a = row(&body, "ATALANY");
	assert_eq!(a["limit"], 322_800 * 12 * 10);
	assert_eq!(a["costPct"], 45);
	assert_eq!((a["used"].as_i64(), a["pending"].as_i64()), (Some(1_000), Some(2_000)));
	let free = row(&body, "ATALANY_TAXFREE");
	assert_eq!(free["taxFreeIncome"], 1_936_800);
	assert_eq!(free["limit"], 3_521_454);

	let retail = seller(SellerVersionPatch {
		income_regime: Some("ATALANY".into()),
		expense_ratio_pct: Patch::Value(90),
		..Default::default()
	});
	let body = tax_limits(2026, "2026-03-01", &retail, &[]);
	assert_eq!(row(&body, "ATALANY")["limit"], 322_800 * 12 * 50);
	assert_eq!(row(&body, "ATALANY_TAXFREE")["limit"], 19_368_000);
}

// vim: ts=4
