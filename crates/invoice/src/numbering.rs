// SPDX-License-Identifier: MPL-2.0
//! Which series an invoice draws from, and the calendar arithmetic around it.
//!
//! The allocation itself is not here: `InvoiceStore::issue` does it inside its own write
//! transaction with `UPDATE doc_series … RETURNING`, and renders it through
//! [`crate::store::render_number`]. This module only answers "which series, which year",
//! and the two date questions issuing asks — because both are the same calendar problem.

use mintworks_core::prelude::*;
use time::{
	Date, Duration, Month, OffsetDateTime, UtcOffset, Weekday, format_description::FormatItem,
	macros::format_description,
};

use crate::store::{InvoicePatch, Seller};

/// The Europe/Budapest civil time of `ts`. Every date in this crate is a local calendar day —
/// `date_of`, `series_for`, `utc_span` — so anything reasoning about "today" or "what hour is
/// it" uses this, not `OffsetDateTime::now_utc`: an invoice issued at 00:30 local in summer is
/// 22:30 UTC the day before, and a Hungarian invoice must carry the local calendar date
/// (Áfa tv. 169. §) — and draw from the local year's series.
pub fn local(ts: Timestamp) -> ClResult<OffsetDateTime> {
	let utc = OffsetDateTime::from_unix_timestamp(ts.0)
		.map_err(|_| Error::internal("timestamp out of range"))?;
	Ok(utc.to_offset(budapest_offset(utc)))
}

/// Europe/Budapest is CET (UTC+1) / CEST (UTC+2) under the EU rule: summer time runs from
/// 01:00 UTC on the last Sunday of March to 01:00 UTC on the last Sunday of October.
//
// The EU rule hardcoded rather than a tz database: one jurisdiction, and `time` ships no zone
// data. If the seasonal change goes or a second zone is needed, swap in `time-tz`.
fn budapest_offset(utc: OffsetDateTime) -> UtcOffset {
	let summer = last_sunday_0100_utc(utc.year(), Month::March).is_some_and(|from| utc >= from)
		&& last_sunday_0100_utc(utc.year(), Month::October).is_some_and(|to| utc < to);
	// Both are valid whole-hour offsets, so neither `from_hms` can fail; CET is the safe
	// answer if one somehow did.
	let hours = if summer { 2 } else { 1 };
	UtcOffset::from_hms(hours, 0, 0).unwrap_or(UtcOffset::UTC)
}

/// 01:00 UTC on the last Sunday of `month` in `year` — both EU transition instants.
fn last_sunday_0100_utc(year: i32, month: Month) -> Option<OffsetDateTime> {
	let last = Date::from_calendar_date(year, month, month.length(year)).ok()?;
	let back = i64::from(last.weekday().number_days_from_sunday());
	let sunday = last - Duration::days(back);
	debug_assert_eq!(sunday.weekday(), Weekday::Sunday);
	Some(sunday.with_hms(1, 0, 0).ok()?.assume_utc())
}

/// `(series_code, series_year)` for an invoice issued at `issued_at`.
///
/// The year is the Europe/Budapest year of the issue instant, never of the fulfilment date:
/// the series counts documents as they are created.
///
/// `kind` never enters, but this is **not** the whole answer for a STORNO: that draws its
/// code from `invoices.series_code` on the invoice it cancels, so the two stay in one gapless
/// run even after `sellers.series_code` changes. [`crate::storno::run`] takes only the year
/// from here.
pub fn series_for(seller: &Seller, issued_at: Timestamp) -> ClResult<(String, i64)> {
	Ok((seller.series_code.clone(), i64::from(local(issued_at)?.year())))
}

/// `YYYY-MM-DD` — the form every date column in this schema stores.
const DATE: &[FormatItem<'_>] = format_description!("[year]-[month]-[day]");

fn render(d: Date) -> ClResult<String> {
	d.format(DATE)
		.map_err(|_| Error::internal(format!("date {d} is not renderable")))
}

/// `YYYY-MM-DD` in Europe/Budapest.
pub fn date_of(ts: Timestamp) -> ClResult<String> {
	render(local(ts)?.date())
}

/// The UTC instants bounding a Europe/Budapest date range: `[from 00:00 local, to+1 day
/// 00:00 local)`. The statutory export is asked for local dates but selects on `issued_at`,
/// which is a Unix timestamp — converting the two bounds once beats converting every row.
pub fn utc_span(from: &str, to: &str) -> ClResult<(Timestamp, Timestamp)> {
	Ok((local_midnight(parse(from)?), local_midnight(shift(parse(to)?, 1)?)))
}

/// 00:00 Europe/Budapest on `d`, as UTC. Local midnight is never inside a transition — both
/// happen at 02:00/03:00 local — so the offset is unambiguous.
fn local_midnight(d: Date) -> Timestamp {
	let naive = d.midnight().assume_utc();
	Timestamp(naive.unix_timestamp() - i64::from(budapest_offset(naive).whole_seconds()))
}

/// A calendar date in exactly `YYYY-MM-DD`, the one form every date column in this schema
/// stores. **This is the crate's date trust boundary** — every date string arriving from a
/// caller goes through here or through [`check_date`] before it is bound to a column.
///
/// Strictly 4-2-2 digits. `"2026-1-1"` used to be accepted, and the columns it reaches are
/// compared *lexically*: [`crate::currency::effective_rate_e6`] runs `WHERE date <= ?`, and
/// NAV's `invoiceDeliveryDate` is `xs:date`.
pub fn parse(date: &str) -> ClResult<Date> {
	let bad = || Error::validation(format!("not a YYYY-MM-DD date: {date}"));
	let b = date.as_bytes();
	if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
		return Err(bad());
	}
	let digits = |r: std::ops::Range<usize>| {
		date.get(r).filter(|s| s.bytes().all(|c| c.is_ascii_digit())).ok_or_else(bad)
	};
	let year: i32 = digits(0..4)?.parse().map_err(|_| bad())?;
	let month = digits(5..7)?
		.parse::<u8>()
		.ok()
		.and_then(|m| Month::try_from(m).ok())
		.ok_or_else(bad)?;
	let day: u8 = digits(8..10)?.parse().map_err(|_| bad())?;
	Date::from_calendar_date(year, month, day).map_err(|_| bad())
}

/// [`parse`] as a validator, for a caller that only needs the string checked.
pub fn check_date(date: &str) -> ClResult<()> {
	parse(date).map(|_| ())
}

/// `time`'s `impl Add<Duration> for Date` is a `checked_add(..).expect(..)`, and both callers
/// take their date from a request body: `add_days("9999-12-31", 8)` was a panic in the
/// handler task — a dropped connection, not a `400`.
///
/// The `Duration` is built by hand because `Duration::days` carries an `expect` of its own —
/// `days.checked_mul(86_400).expect(..)` — which fires before `checked_add` is ever reached.
/// `days` comes from `settings['invoice.default_payment_days']`, so it is bounded but not
/// trusted here.
fn shift(d: Date, days: i64) -> ClResult<Date> {
	days.checked_mul(86_400)
		.map(Duration::seconds)
		.and_then(|by| d.checked_add(by))
		.ok_or_else(|| Error::validation(format!("date {d} out of range")))
}

/// `date` shifted by `days`, as `YYYY-MM-DD`. Used for the due date, which is the
/// fulfilment date plus `settings['invoice.default_payment_days']`.
pub fn add_days(date: &str, days: i64) -> ClResult<String> {
	render(shift(parse(date)?, days)?)
}

/// The date rules the `Invoices` handle applies before it writes anything. They live here
/// rather than in `service_api.rs` because the window, the ordering and "is this a real
/// calendar date" are one calendar problem, not three.
///
/// The **service handle** is the trust boundary, not the router:
/// [`crate::service_api::Invoices::issue_now`] builds and issues an invoice with no HTTP
/// request in the loop. Unvalidated, a `fulfilment_date` of `"tomorrow"` froze onto an issued
/// invoice and became its `rate_date`, where `effective_rate_e6`'s **lexical** `WHERE date <=
/// ?` matched every published rate — so the invoice took the newest one — and NAV then
/// rejected it as an `xs:date`. `"9999-12-31"` was worse: a panic in `add_days`.
pub(crate) fn check_dates<'a>(dates: impl IntoIterator<Item = Option<&'a str>>) -> ClResult<()> {
	dates.into_iter().flatten().try_for_each(check_date)
}

/// How far a newly written `fulfilment_date` may sit from today, in days, either way.
///
/// Deliberately generous rather than tight. What it closes is a `"2019-03-01"`
/// fulfilment date — a real calendar date, so `check_dates` passed it, and on an HUF invoice
/// no FX lookup fires either — freezing onto a numbered, immutable, NAV-filed invoice in a
/// VAT period that closed six years ago, correctable only by storno + reissue, which burns
/// two more numbers. It is **not** an accounting rule: a periodic-settlement invoice
/// (Áfa tv. 58. §) legitimately carries a future fulfilment date and a late-issued one a past
/// date, so a tight window would refuse real invoices. A constant rather than a setting: no
/// deployment has a reason to differ, and a setting would need its own `range(..)` ceiling
/// for a value that never changes.
pub(crate) const MAX_FULFILMENT_DRIFT_DAYS: i64 = 365;

/// The rule beyond "is this a real calendar date". `set` is the fulfilment date **this call
/// writes** — the window is checked only against that, because a stored date drifts past the
/// window with time and has already been accepted once.
///
/// No due-before-fulfilment rule: neither Áfa tv. nor NAV has one, so refusing it would block
/// real invoices.
pub(crate) fn check_range(set: Option<&str>) -> ClResult<()> {
	if let Some(date) = set {
		let drift = (parse(date)? - parse(resolved_today()?.as_str())?).whole_days();
		if drift.abs() > MAX_FULFILMENT_DRIFT_DAYS {
			return Err(Error::coded(
				mintworks_core::error::StatusCode::BAD_REQUEST,
				"E-INV-DATE-RANGE",
				format!("fulfilmentDate is more than {MAX_FULFILMENT_DRIFT_DAYS} days from today"),
			));
		}
	}
	Ok(())
}

/// The fulfilment date of a periodic settlement, Áfa tv. 58. § (1a) a): the issue date when both
/// it and the due date fall before the period's last day, otherwise the due date capped at 60 days past
/// the period end, and never before the period end. Dates are validated `YYYY-MM-DD`.
pub fn fulfilment_58(period_end: &str, issue_date: &str, due_date: &str) -> ClResult<String> {
	if issue_date < period_end && due_date < period_end {
		return Ok(issue_date.to_owned());
	}
	if due_date > period_end {
		let cap = add_days(period_end, 60)?;
		return Ok(due_date.min(cap.as_str()).to_owned());
	}
	Ok(period_end.to_owned())
}

/// The settlement-period rules over the **resolved** state — stored merged with the patch.
/// A period derives the fulfilment date ([`fulfilment_58`]), so a caller-supplied one conflicts.
/// Periods of 12 months or more are refused: the 58. § (3) split into yearly parts is deferred.
pub(crate) fn check_period(
	start: Option<&str>,
	end: Option<&str>,
	fulfilment: Option<&str>,
) -> ClResult<()> {
	let bad = |code, msg: &str| {
		Err(Error::coded(mintworks_core::error::StatusCode::BAD_REQUEST, code, msg.to_owned()))
	};
	let (start, end) = match (start, end) {
		(None, None) => return Ok(()),
		(Some(s), Some(e)) => (s, e),
		_ => return bad("E-INV-PERIOD-INCOMPLETE", "periodStart and periodEnd go together"),
	};
	check_dates([Some(start), Some(end)])?;
	if end < start {
		return Err(Error::validation("periodEnd is before periodStart"));
	}
	// Lexical on `YYYY-MM-DD`: one year on from a 02-29 start is the non-date "…-02-29",
	// which still orders between 02-28 and 03-01.
	if end >= format!("{:04}{}", parse(start)?.year() + 1, &start[4..]).as_str() {
		return bad("E-INV-PERIOD-TOO-LONG", "a settlement period must be under 12 months");
	}
	if fulfilment.is_some() {
		return bad("E-INV-PERIOD-FULFILMENT", "a period derives fulfilmentDate; do not send both");
	}
	Ok(())
}

pub(crate) fn resolved_today() -> ClResult<String> {
	date_of(Timestamp::now())
}

/// [`check_dates`] over the two date members of a patch, plus the range rule — so `PATCH`
/// cannot route around what [`crate::service_api::Invoices::draft`] enforces.
pub(crate) fn check_patch_dates(patch: &InvoicePatch) -> ClResult<()> {
	let set = patch.fulfilment_date.value().map(String::as_str);
	check_dates([set, patch.due_date.value().map(String::as_str)])?;
	check_range(set)
}

#[cfg(test)]
mod tests {
	use super::*;
	use mintworks_core::ids::SellerId;

	fn seller() -> Seller {
		Seller {
			id: 1,
			uid: SellerId::generate(),
			org_id: 1,
			nav_base_url: String::new(),
			nav_login: None,
			series_code: "A".into(),
			closed_at: None,
			payment_days: None,
			created_at: Timestamp(0),
		}
	}

	/// The whole point of the conversion: an invoice issued just after local midnight, and
	/// just after the local new year, must not be dated the day — or the year — before.
	#[test]
	fn dates_and_series_follow_the_budapest_wall_clock() {
		// 2025-12-31T23:30:00Z = 2026-01-01T00:30+01:00.
		let new_year = Timestamp(1_767_223_800);
		assert_eq!(date_of(new_year).unwrap(), "2026-01-01");
		assert_eq!(series_for(&seller(), new_year).unwrap().1, 2026);

		// 2026-06-30T22:30:00Z = 2026-07-01T00:30+02:00 — summer time, two hours out.
		let summer = Timestamp(1_782_858_600);
		assert_eq!(date_of(summer).unwrap(), "2026-07-01");
	}

	/// Summer time starts and ends at 01:00 UTC, and the offset must flip exactly there.
	/// `mnb` reads the clock through `local`, so a UTC answer put `rate_fetch_hour = 11` an
	/// hour or two off MNB's ~11:00 CET publication and made `rate_fetch_hour = 23` ask for
	/// the previous local day.
	#[test]
	fn the_offset_flips_at_both_eu_transitions() {
		let utc = |ts: i64| OffsetDateTime::from_unix_timestamp(ts).unwrap();
		// 2026-03-29T01:00:00Z, the last Sunday of March.
		let spring = 1_774_746_000;
		assert_eq!(budapest_offset(utc(spring - 1)), UtcOffset::from_hms(1, 0, 0).unwrap());
		assert_eq!(budapest_offset(utc(spring)), UtcOffset::from_hms(2, 0, 0).unwrap());
		// 2026-10-25T01:00:00Z, the last Sunday of October.
		let autumn = 1_792_890_000;
		assert_eq!(budapest_offset(utc(autumn - 1)), UtcOffset::from_hms(2, 0, 0).unwrap());
		assert_eq!(budapest_offset(utc(autumn)), UtcOffset::from_hms(1, 0, 0).unwrap());
	}

	/// The export range must cover the local day, not the UTC one.
	#[test]
	fn a_date_range_spans_local_days() {
		// 2026-01-01 CET starts at 2025-12-31T23:00:00Z; the range ends where 2026-01-03 does.
		let (from, to) = utc_span("2026-01-01", "2026-01-02").unwrap();
		assert_eq!(from.0, 1_767_222_000);
		assert_eq!(to.0 - from.0, 2 * 86_400);
		// An invoice issued at 2026-01-01T00:30+01:00 is inside it, one an hour earlier is not.
		assert!(from.0 <= 1_767_223_800 && 1_767_223_800 < to.0);
		assert!(1_767_220_200 < from.0);
		// Summer: 2026-07-01 CEST starts at 2026-06-30T22:00:00Z.
		assert_eq!(utc_span("2026-07-01", "2026-07-01").unwrap().0.0, 1_782_856_800);
	}

	/// `time`'s `Date + Duration` is a `checked_add(..).expect(..)`, and both callers take
	/// their date from a request body — so `{"fulfilmentDate":"9999-12-31"}` followed by an
	/// issue panicked in the handler task, which is a dropped connection rather than a `400`.
	#[test]
	fn a_date_at_the_end_of_the_calendar_is_an_error_not_a_panic() {
		// The ordinary shifts first, so the refusals below read as a boundary.
		assert_eq!(add_days("2026-03-06", 8).unwrap(), "2026-03-14");
		// Across a month and a leap day.
		assert_eq!(add_days("2026-12-28", 8).unwrap(), "2027-01-05");
		assert_eq!(add_days("2028-02-28", 1).unwrap(), "2028-02-29");

		assert!(add_days("9999-12-31", 8).is_err());
		assert!(utc_span("9999-12-31", "9999-12-31").is_err());
		// One day earlier still works, so this is a boundary and not a blanket refusal.
		assert_eq!(add_days("9999-12-30", 1).unwrap(), "9999-12-31");
		// The *days* side overflows first: `Duration::days` multiplies by 86_400 and
		// `expect`s, so this panicked before `checked_add` was reached.
		assert!(add_days("2026-01-01", i64::MAX).is_err());
		assert!(add_days("2026-01-01", i64::MIN).is_err());
	}

	/// The schema says `YYYY-MM-DD`, and every consumer of these columns compares them as
	/// strings: `effective_rate_e6`'s `WHERE date <= ?`, and NAV's `xs:date`.
	#[test]
	fn only_the_padded_form_parses() {
		assert!(parse("2026-01-01").is_ok());
		for off in [
			"2026-1-1",
			"2026-01-1",
			"26-01-01",
			"2026-01-01T00:00:00Z",
			"2026-01-0a",
			"",
			"2026-13-01",
		] {
			assert!(parse(off).is_err(), "{off}");
		}
	}

	fn today() -> String {
		resolved_today().unwrap()
	}

	fn shifted(days: i64) -> String {
		add_days(&today(), days).unwrap()
	}

	/// `check_dates` asserted only that the string was a real `YYYY-MM-DD`, so
	/// `{"fulfilmentDate":"2019-03-01"}` on an HUF invoice — no FX lookup, so the rate
	/// staleness guard never fires either — produced a numbered, immutable, NAV-filed invoice
	/// in a VAT period that closed six years ago. Storno + reissue is the only correction, and
	/// it burns two more numbers.
	#[test]
	fn a_fulfilment_date_far_from_today_is_refused() {
		for far in ["2019-03-01", &shifted(MAX_FULFILMENT_DRIFT_DAYS + 1), &shifted(-400)] {
			assert_eq!(
				check_range(Some(far)).unwrap_err().parts().1,
				"E-INV-DATE-RANGE",
				"{far} was accepted"
			);
		}
		// The window is wide on purpose: a periodic-settlement invoice (Áfa tv. 58. §) carries
		// a future fulfilment date and a late-issued one a past date, and both are real.
		for near in [today(), shifted(MAX_FULFILMENT_DRIFT_DAYS), shifted(-30), shifted(60)] {
			check_range(Some(&near)).unwrap();
		}
	}

	#[test]
	fn a_patch_is_range_checked_like_a_draft() {
		let patch = InvoicePatch {
			fulfilment_date: Patch::Value("2019-03-01".to_owned()),
			..Default::default()
		};
		assert_eq!(check_patch_dates(&patch).unwrap_err().parts().1, "E-INV-DATE-RANGE");
	}

	#[test]
	fn fulfilment_58_is_the_issue_date_when_issue_and_due_fall_before_the_period_end() {
		assert_eq!(fulfilment_58("2026-09-30", "2026-09-20", "2026-09-29").unwrap(), "2026-09-20");
	}

	#[test]
	fn fulfilment_58_due_on_the_last_day_is_the_period_end() {
		assert_eq!(fulfilment_58("2026-09-30", "2026-09-20", "2026-09-30").unwrap(), "2026-09-30");
	}

	#[test]
	fn fulfilment_58_is_the_due_date_capped_at_60_days_after_the_period() {
		assert_eq!(fulfilment_58("2026-09-30", "2026-10-02", "2026-10-15").unwrap(), "2026-10-15");
		assert_eq!(fulfilment_58("2026-09-30", "2026-10-02", "2027-01-15").unwrap(), "2026-11-29");
	}

	#[test]
	fn fulfilment_58_is_the_period_end_when_issued_after_it_but_due_within_it() {
		assert_eq!(fulfilment_58("2026-09-30", "2026-10-02", "2026-09-25").unwrap(), "2026-09-30");
	}

	#[test]
	fn a_period_is_complete_ordered_and_under_a_year() {
		let code = |s, e, f| check_period(s, e, f).unwrap_err().parts().1;
		assert_eq!(code(Some("2026-01-01"), None, None), "E-INV-PERIOD-INCOMPLETE");
		assert_eq!(code(None, Some("2026-01-31"), None), "E-INV-PERIOD-INCOMPLETE");
		assert_eq!(code(Some("2026-02-01"), Some("2026-01-31"), None), "E-CORE-VALIDATION");
		assert_eq!(code(Some("2026-01-01"), Some("2027-01-01"), None), "E-INV-PERIOD-TOO-LONG");
		assert_eq!(code(Some("2028-02-29"), Some("2029-03-01"), None), "E-INV-PERIOD-TOO-LONG");
		assert_eq!(
			code(Some("2026-01-01"), Some("2026-01-31"), Some("2026-01-31")),
			"E-INV-PERIOD-FULFILMENT"
		);
		check_period(None, None, Some("2026-01-31")).unwrap();
		check_period(Some("2026-01-01"), Some("2026-12-31"), None).unwrap();
		check_period(Some("2028-02-29"), Some("2029-02-28"), None).unwrap();
	}
}

// vim: ts=4
