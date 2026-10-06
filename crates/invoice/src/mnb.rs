//! The `FETCH_RATES` job: MNB's published daily rates, over its SOAP endpoint.
//!
//! MNB rates are legally usable only with a prior election filed with NAV, so this job is a no-op
//! unless `settings['currency.rate_source']` is `MNB`. Rows land under `source = 'MNB'` and stay
//! there: a historical invoice must keep resolving against the source it actually used.
//!
//! The response is doubly encoded — a SOAP envelope whose `GetExchangeRatesResult` text is
//! the entity-escaped rate document — so it is parsed twice.

use std::time::Duration as StdDuration;

use mintworks_core::{
	App, http,
	job::{Job, Runner},
	prelude::*,
};
use quick_xml::{Reader, events::BytesStart, events::Event};
use time::{Duration, macros::format_description};

pub const KIND: &str = "FETCH_RATES";
/// The `currency_rates.source` value this job writes, and the `currency.rate_source`
/// setting that enables it.
pub const SOURCE: &str = "MNB";

const MNB_URL: &str = "https://www.mnb.hu/arfolyamok.asmx";
const SOAP_ACTION: &str = "http://www.mnb.hu/webservices/GetExchangeRates";
const TIMEOUT: StdDuration = StdDuration::from_secs(30);
/// How far back a first run reaches when the table is empty.
const BACKFILL_DAYS: i64 = 7;

/// `AppBuilder::jobs(mintworks_invoice::mnb::register)`. Seed the first occurrence with
/// `mintworks_core::job::seed_periodic(pool, mnb::KIND)`.
///
/// Registered hourly and gated on `currency.rate_fetch_hour` — a Europe/Budapest hour, like
/// every other clock reading in this crate — rather than registered daily,
/// which is the same shape `mintworks_core::alert::sweep` uses and for the same two reasons:
/// `register_periodic` fixes its period at registration, so gating in the handler is what
/// lets an operator move the hour without a restart; and a 24 h period runs from whenever
/// `seed_periodic` first ran and drifts forward on every completion, so a chain seeded before
/// MNB publishes the day's rates (around 11:00 CET) fetched ahead of publication every day.
/// The 23 ticks an hour that do not match return before any settings beyond the hour are read.
pub fn register(runner: &mut Runner, app: App) {
	runner.register_periodic(KIND, 3_600, move |_job: Job| {
		let app = app.clone();
		async move {
			let hour = app.settings.int("currency.rate_fetch_hour").await?;
			if i64::from(crate::numbering::local(Timestamp::now())?.hour()) != hour {
				return Ok(());
			}
			run(&app).await
		}
	});
}

/// Bring every enabled currency's `MNB` rates up to today.
///
/// # Errors
/// `Error::Unavailable` when MNB cannot be reached or its answer is unparseable — the job
/// runner retries with backoff.
pub async fn run(app: &App) -> ClResult<()> {
	if app.settings.text("currency.rate_source").await? != SOURCE {
		return Ok(());
	}
	let base = CurrencyCode::parse(&app.settings.text("currency.base").await?)?;
	if base != "HUF" {
		// MNB publishes against the forint only. A non-HUF deployment needs another source.
		tracing::warn!(base = %base, "{KIND}: MNB quotes HUF only, skipping");
		return Ok(());
	}

	let fmt = DATE;
	let today = crate::numbering::local(Timestamp::now())?.date();
	let today_s = today.format(fmt).map_err(|e| Error::internal(format!("date format: {e}")))?;

	let store = crate::service_api::store(app)?;
	let codes = store.mnb_currencies(base.as_str()).await?;

	// Per-currency failures are collected, not propagated: one malformed row used to abort the
	// whole tick, and the retry failed at the same row until `currency.max_rate_age_days`
	// expired. A total outage is still a job failure, so it still retries.
	let (mut tried, mut failed) = (0usize, 0usize);
	for code in codes {
		tried += 1;
		let code = CurrencyCode::from_trusted(code);
		if let Err(e) = one_currency(store.as_ref(), &code, &base, today, &today_s).await {
			failed += 1;
			tracing::error!(currency = %code, error = %e, "{KIND}: could not update rates");
		}
	}
	if failed > 0 && failed == tried {
		return Err(unavailable("every currency failed"));
	}
	Ok(())
}

/// One currency's backfill: ask MNB for everything since the last stored day and upsert it.
async fn one_currency(
	store: &dyn crate::store::InvoiceStore,
	code: &CurrencyCode,
	base: &CurrencyCode,
	today: time::Date,
	today_s: &str,
) -> ClResult<()> {
	let fmt = DATE;
	let pair = crate::currency::pair(code, base);
	let last = store.mnb_max_date(&pair).await?;

	// Ask from the day after the last stored one. MNB simply omits days it has not
	// published, so no "rates appear after noon" branch is needed here.
	let start = last
		.and_then(|d| time::Date::parse(&d, fmt).ok())
		.and_then(time::Date::next_day)
		.unwrap_or_else(|| today.saturating_sub(Duration::days(BACKFILL_DAYS)));
	if start > today {
		return Ok(());
	}
	let start_s = start.format(fmt).map_err(|e| Error::internal(format!("date format: {e}")))?;

	let fetched = fetch(&start_s, today_s, code.as_str()).await?;
	// A parse that yields nothing is indistinguishable from a weekend and both are a `DONE` job,
	// so an MNB schema change stopped the rates silently. `currency.max_rate_age_days` is the
	// hard stop; this is the signal that something needs looking at.
	if fetched.is_empty() {
		tracing::warn!(
			pair = %pair,
			from = %start_s,
			to = %today_s,
			"MNB returned no rates for a non-empty range"
		);
	}
	store.mnb_upsert_rates(&pair, &fetched).await?;
	Ok(())
}

/// `(date, rate_e6)` for `currency` over `[start, end]`, both `'YYYY-MM-DD'`.
///
/// # Errors
/// `Error::Unavailable` on any transport or parse failure.
pub async fn fetch(start: &str, end: &str, currency: &str) -> ClResult<Vec<(String, i64)>> {
	// Validated, not escaped: `currencies.code` carries no charset CHECK, so a code holding
	// `<` or `&` built a malformed request that `parse_days` read as zero rates — a silent
	// stop that only surfaced as `E-INV-NO-RATE` at issue. `fetch` is `pub`, so the dates —
	// internally always `'YYYY-MM-DD'` — are escaped for the same reason.
	let currency = CurrencyCode::parse(currency)?;
	let (start, end) = (quick_xml::escape::escape(start), quick_xml::escape::escape(end));
	let body = format!(
		"<?xml version=\"1.0\" encoding=\"utf-8\"?>\
		 <soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\">\
		 <soap:Body><GetExchangeRates xmlns=\"http://www.mnb.hu/webservices/\">\
		 <startDate>{start}</startDate><endDate>{end}</endDate>\
		 <currencyNames>{}</currencyNames>\
		 </GetExchangeRates></soap:Body></soap:Envelope>",
		currency.as_str()
	);

	let (status, _, bytes) = http::post(
		MNB_URL,
		&[("content-type", "text/xml; charset=utf-8"), ("soapaction", SOAP_ACTION)],
		body.into_bytes(),
		TIMEOUT,
	)
	.await
	.map_err(
		|e| if e.parts().1 == http::E_REMOTE_IN_TX { e } else { unavailable("unreachable") },
	)?;
	if !status.is_success() {
		return Err(unavailable(&format!("status {status}")));
	}
	let soap = String::from_utf8_lossy(&bytes);

	parse_days(&inner_xml(&soap)?, currency.as_str())
}

fn unavailable(why: &str) -> Error {
	tracing::warn!(why, "MNB rate fetch failed");
	Error::Unavailable(format!("MNB exchange rate service: {why}"))
}

fn xml_err(e: &quick_xml::Error) -> Error {
	unavailable(&format!("malformed XML: {e}"))
}

/// Unwrap the envelope: `GetExchangeRatesResult`'s text is the escaped rate document.
fn inner_xml(soap: &str) -> ClResult<String> {
	let mut reader = Reader::from_str(soap);
	reader.config_mut().trim_text(true);
	let mut inside = false;
	let mut out = String::new();
	loop {
		match reader.read_event().map_err(|e| xml_err(&e))? {
			Event::Start(e) if e.local_name().as_ref() == b"GetExchangeRatesResult" => {
				inside = true;
			}
			Event::Text(e) if inside => {
				out.push_str(&e.unescape().map_err(|e| xml_err(&e))?);
			}
			Event::CData(e) if inside => {
				out.push_str(&String::from_utf8_lossy(&e.into_inner()));
			}
			Event::End(e) if e.local_name().as_ref() == b"GetExchangeRatesResult" => break,
			Event::Eof => break,
			_ => {}
		}
	}
	if out.trim().is_empty() {
		return Err(unavailable("no GetExchangeRatesResult in the response"));
	}
	Ok(out)
}

/// `try_get_attribute` with this module's error classification: a malformed attribute is a bad
/// answer from MNB, which is `Unavailable` and retryable like every other parse failure here.
fn attr(e: &BytesStart<'_>, name: &[u8]) -> ClResult<Option<String>> {
	Ok(e.try_get_attribute(name)
		.map_err(|e| unavailable(&format!("malformed attribute: {e}")))?
		.map(|a| String::from_utf8_lossy(&a.value).into_owned()))
}

/// `YYYY-MM-DD`, the form `currency_rates.date` stores and `currency::rate_on` parses back.
const DATE: &[time::format_description::FormatItem<'_>] =
	format_description!("[year]-[month]-[day]");

/// `<MNBExchangeRates><Day date="…"><Rate unit="1" curr="EUR">400,50</Rate></Day></…>`
fn parse_days(xml: &str, currency: &str) -> ClResult<Vec<(String, i64)>> {
	let mut reader = Reader::from_str(xml);
	reader.config_mut().trim_text(true);
	let mut out = Vec::new();
	let mut date: Option<String> = None;
	let mut rate: Option<(String, i64)> = None;
	loop {
		match reader.read_event().map_err(|e| xml_err(&e))? {
			Event::Start(e) => match e.local_name().as_ref() {
				b"Day" => {
					// Parsed and discarded, not merely captured: `currency_rate` picks the max
					// row *lexically*, so one malformed value outranks every real date and then
					// fails `rate_on`'s parse, wedging the currency with no API path to delete
					// the row. Absent and malformed are one case.
					let raw = attr(&e, b"date")?
						.ok_or_else(|| unavailable("Day has no date attribute"))?;
					if time::Date::parse(&raw, DATE).is_err() {
						return Err(unavailable(&format!("unparseable rate date '{raw}'")));
					}
					date = Some(raw);
				}
				b"Rate" => {
					let curr = attr(&e, b"curr")?.unwrap_or_default();
					// MNB quotes JPY and KRW per 100, so a unit this could not read fell back to
					// 1 and stored a rate 100x too large, frozen onto an invoice and filed to
					// NAV. Absent still means 1. Non-positive is refused here so a bad answer
					// from MNB is a retryable `Unavailable`, not an internal defect.
					let unit = match attr(&e, b"unit")? {
						Some(u) => match u.trim().parse::<i64>() {
							Ok(n) if n > 0 => n,
							_ => {
								return Err(unavailable(&format!(
									"unusable rate unit '{}'",
									u.trim()
								)));
							}
						},
						None => 1,
					};
					rate = Some((curr, unit));
				}
				_ => {}
			},
			Event::Text(e) => {
				if let (Some(d), Some((curr, unit))) = (date.as_deref(), rate.take())
					&& curr.eq_ignore_ascii_case(currency)
				{
					let text = e.unescape().map_err(|e| xml_err(&e))?;
					let value = decimal_e6(&text)?;
					// `Qty::parse` deliberately does not check the sign, and
					// `currency::to_base` *multiplies* by the rate, so a 0 here zeroed the
					// Áfa tv. 172. § HUF figures on an issued invoice instead of failing.
					if value <= 0 {
						return Err(unavailable(&format!("non-positive rate '{}'", text.trim())));
					}
					out.push((d.to_owned(), round_half_up(value, i128::from(unit))?));
				}
			}
			Event::Eof => break,
			_ => {}
		}
	}
	Ok(out)
}

/// `"400,50"` -> `400_500_000`. MNB uses a comma; [`Qty`] is already the 1e6 fixed-point
/// scale a rate is stored at, and parses without constructing a float.
///
/// A seventh decimal is rejected rather than silently truncated; MNB publishes two.
fn decimal_e6(s: &str) -> ClResult<i128> {
	Qty::parse(&s.replace(',', "."))
		.map(|q| i128::from(q.0))
		.map_err(|_| unavailable(&format!("unparseable rate '{}'", s.trim())))
}

#[cfg(test)]
mod tests {
	use super::*;

	const INNER: &str = "<MNBExchangeRates><Day date=\"2026-09-02\">\
		<Rate unit=\"1\" curr=\"EUR\">395,25</Rate></Day>\
		<Day date=\"2026-09-03\"><Rate unit=\"1\" curr=\"EUR\">400,5</Rate>\
		<Rate unit=\"100\" curr=\"JPY\">271,04</Rate></Day></MNBExchangeRates>";

	fn envelope() -> String {
		let escaped = INNER.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
		format!(
			"<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
			 <GetExchangeRatesResponse xmlns=\"http://www.mnb.hu/webservices/\">\
			 <GetExchangeRatesResult>{escaped}</GetExchangeRatesResult>\
			 </GetExchangeRatesResponse></s:Body></s:Envelope>"
		)
	}

	#[test]
	fn decimals_scale_without_floats() {
		assert_eq!(decimal_e6("400,5").expect("comma"), 400_500_000);
		assert_eq!(decimal_e6(" 395.25 ").expect("dot"), 395_250_000);
		assert_eq!(decimal_e6("7").expect("integer"), 7_000_000);
		assert!(decimal_e6("4o0,5").is_err());
		assert!(decimal_e6("").is_err());
	}

	#[test]
	fn unwraps_the_double_encoding_and_reads_the_rates() {
		let inner = inner_xml(&envelope()).expect("inner document");
		assert_eq!(inner, INNER);
		let eur = parse_days(&inner, "EUR").expect("EUR");
		assert_eq!(
			eur,
			vec![("2026-09-02".to_owned(), 395_250_000), ("2026-09-03".to_owned(), 400_500_000)]
		);
		// unit="100" divides: 271,04 HUF per 100 JPY is 2,7104 HUF per JPY.
		let jpy = parse_days(&inner, "JPY").expect("JPY");
		assert_eq!(jpy, vec![("2026-09-03".to_owned(), 2_710_400)]);
	}

	/// A `unit` the parser could not read used to fall back to 1 — storing a JPY rate
	/// 100x too large — and an unparseable `date` was stored verbatim, where it outranks
	/// every real date lexically and wedges the currency. Both are an outage now, not a
	/// silently wrong rate.
	#[test]
	fn an_unreadable_unit_or_date_is_an_outage() {
		let day = |attrs: &str| {
			format!(
				"<MNBExchangeRates><Day date=\"2026-09-03\">\
				 <Rate {attrs} curr=\"JPY\">271,04</Rate></Day></MNBExchangeRates>"
			)
		};
		for attrs in ["unit=\"1 000\"", "unit=\"1.0\"", "unit=\"0\"", "unit=\"-1\""] {
			assert!(parse_days(&day(attrs), "JPY").is_err(), "{attrs} was accepted");
		}
		// Absent still means 1.
		assert_eq!(
			parse_days(&day(""), "JPY").expect("no unit"),
			vec![("2026-09-03".to_owned(), 271_040_000)]
		);
		let bad_date = "<MNBExchangeRates><Day date=\"2026-09-3\">\
			<Rate unit=\"1\" curr=\"EUR\">400,5</Rate></Day></MNBExchangeRates>";
		assert!(parse_days(bad_date, "EUR").is_err(), "a malformed date was stored");
		// An *absent* date used to drop every `Rate` under the day and answer `Ok(vec![])`.
		let no_date = "<MNBExchangeRates><Day>\
			<Rate unit=\"1\" curr=\"EUR\">400,5</Rate></Day></MNBExchangeRates>";
		assert!(parse_days(no_date, "EUR").is_err(), "a missing date was silently dropped");
	}

	/// `currency::to_base` multiplies by the rate, so a 0 froze 0.00 HUF VAT onto an issued
	/// invoice — `Qty::parse` checks neither sign nor zero.
	#[test]
	fn a_non_positive_rate_is_an_outage() {
		let day = |value: &str| {
			format!(
				"<MNBExchangeRates><Day date=\"2026-09-03\">\
				 <Rate unit=\"1\" curr=\"EUR\">{value}</Rate></Day></MNBExchangeRates>"
			)
		};
		for value in ["0,00", "0", "-395,25"] {
			assert!(parse_days(&day(value), "EUR").is_err(), "{value} was accepted");
		}
	}

	/// `currencies.code` has no charset CHECK, so an imported or operator-inserted code went
	/// raw into the envelope: `<` or `&` made the request malformed and `parse_days` read the
	/// answer as zero rates, which only surfaced as `E-INV-NO-RATE` at issue. `fetch` is `pub`,
	/// so a consumer reaches it with an arbitrary `&str` too.
	#[tokio::test]
	async fn a_code_the_currencies_table_does_not_constrain_is_refused_before_the_request() {
		for bad in ["EU<R", "EUR&amp;", "EURO", "", "eu r"] {
			let err = fetch("2026-09-01", "2026-09-02", bad).await.unwrap_err();
			assert_eq!(err.parts().1, "E-CORE-VALIDATION", "{bad:?} reached the network");
		}
	}

	#[test]
	fn a_soap_fault_is_an_error_not_an_empty_list() {
		assert!(inner_xml("<s:Envelope><s:Body><s:Fault/></s:Body></s:Envelope>").is_err());
	}
}

// vim: ts=4
