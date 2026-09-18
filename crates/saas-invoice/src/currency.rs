//! Currency policy, conversion and the dated rate lookup.
//!
//! All service master data is priced in `settings['currency.base']`. A tenant may bill in
//! any enabled currency; the price converts at issue time and the rate is frozen on the
//! invoice row. Rates are integers scaled 1e6 (`_e6`), amounts are integer minor units —
//! no float appears on this path.

use axum::http::StatusCode;
use saas_core::app::App;
use saas_core::prelude::*;

use crate::store::InvoiceStore;

/// How a currency gets its rate against the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateMode {
	/// An admin-set rate, used verbatim.
	Fixed,
	/// The published rate for the date, marked up by `fee_bp`.
	Official,
}

/// A row of `currencies`.
#[derive(Debug, Clone)]
pub struct Currency {
	pub code: CurrencyCode,
	pub price_round_step: i64,
	pub mode: RateMode,
	pub fixed_rate_e6: Option<i64>,
	pub fee_bp: i64,
	pub enabled: bool,
}

/// `currency_rates.pair`: `"EURHUF"` is one EUR expressed in HUF.
#[must_use]
pub fn pair(quote: &CurrencyCode, base: &CurrencyCode) -> String {
	format!("{quote}{base}")
}

/// An enabled currency by ISO code.
///
/// # Errors
/// `E-INV-CURRENCY-DISABLED` (400) when the code is unknown or `enabled = 0` — the two are
/// the same fact to a caller: this deployment will not bill in it.
pub async fn get(store: &dyn InvoiceStore, code: &CurrencyCode) -> ClResult<Currency> {
	let row = store.currency_get(code.as_str()).await?;

	let disabled = || {
		Error::coded(
			StatusCode::BAD_REQUEST,
			"E-INV-CURRENCY-DISABLED",
			format!("currency '{code}' is not enabled"),
		)
	};
	let cur = row.ok_or_else(disabled)?;
	if !cur.enabled {
		return Err(disabled());
	}
	Ok(cur)
}

/// Every currency the deployment knows, `code` order — `GET /api/currencies`.
///
/// `all` includes the disabled ones, which are restricted to an operator: a disabled row is
/// policy the tenant cannot act on.
pub async fn list(store: &dyn InvoiceStore, all: bool) -> ClResult<Vec<Currency>> {
	store.currency_list(all).await
}

/// `settings['currency.base']`, resolved. Every `services.unit_price` is quoted in it, so
/// rendering one as a [`MoneyWire`] needs this currency's code.
pub async fn base(app: &App) -> ClResult<Currency> {
	let code = CurrencyCode::parse(&app.settings.text("currency.base").await?)?;
	get(crate::service_api::store(app)?.as_ref(), &code).await
}

/// The rate published for `pair` from `source` on `date`, or the last one published before it — the
/// weekend-and-holiday rule. `date` is `'YYYY-MM-DD'`.
///
/// **`WHERE date <= ?` is a string comparison**, so this is only a date lookup for a `date`
/// that is already a validated `YYYY-MM-DD`. `"tomorrow"` sorts above every published row and
/// silently returns the newest rate. [`crate::numbering::parse`] is the boundary that
/// guarantees the form; nothing here can re-check it cheaply.
///
/// `max_age_days` (`settings['currency.max_rate_age_days']`) bounds how far back "the last one
/// published before it" may reach. Unbounded, a fetch that silently stopped — `mnb::run`
/// parses an unrecognised document to zero rates and reports `DONE` — served the last row that
/// ever landed, forever: February's rate frozen onto a June invoice as its `exchangeRate` and
/// its HUF VAT amounts, with no error anywhere.
///
/// # Errors
/// `E-INV-NO-RATE` (409) when nothing was published for that pair and source within
/// `max_age_days` before `date`.
pub async fn rate_on(
	store: &dyn InvoiceStore,
	pair: &str,
	source: &str,
	date: &str,
	max_age_days: i64,
) -> ClResult<i64> {
	let row = store.currency_rate(pair, source, date).await?;
	let no_rate = |msg: String| Error::coded(StatusCode::CONFLICT, "E-INV-NO-RATE", msg);
	let (rate_e6, published) =
		row.ok_or_else(|| no_rate(format!("no {source} rate for {pair} on or before {date}")))?;
	// Rejected here, not only at ingest: MANUAL/BANK rows and data imports reach the table
	// without passing `mnb::run`, and `to_base` multiplies, so a 0 issued an invoice with
	// 0.00 HUF VAT rather than failing.
	if rate_e6 <= 0 {
		return Err(no_rate(format!(
			"the {source} rate for {pair} on {published} is not positive"
		)));
	}

	let fmt = time::macros::format_description!("[year]-[month]-[day]");
	let (Ok(asked), Ok(got)) = (time::Date::parse(date, fmt), time::Date::parse(&published, fmt))
	else {
		// `date` is guaranteed `YYYY-MM-DD` by `numbering::parse`, and the column is written
		// only from a parsed date, so this is unreachable — and if it ever is reached, the
		// age is unknowable, which is not an answer.
		return Err(no_rate(format!("unreadable rate date for {pair}: {published}")));
	};
	if (asked - got).whole_days() > max_age_days {
		return Err(no_rate(format!(
			"the newest {source} rate for {pair} on or before {date} was published {published}, \
			 more than {max_age_days} days earlier"
		)));
	}
	Ok(rate_e6)
}

/// The rate to use for `cur` against `base` on `date`: its fixed rate, or the published one.
///
/// `source` comes from `settings['currency.rate_source']` and is frozen on the invoice, because the
/// legally usable source depends on an election filed with NAV and that election can change.
///
/// `configured_base` is `settings['currency.base']`. It is separate from `base` because
/// callers ask for two different bases: `"HUF"` for the statutory VAT figure, and the
/// configured base for pricing. `currencies.fixed_rate_e6` is defined against the configured
/// base *only*, so a `FIXED` row cannot answer for any other one.
///
/// # Errors
/// `E-INV-NO-RATE` per [`rate_on`], for a `FIXED` currency asked about a base it has no rate
/// against, and for a non-positive `fixed_rate_e6`; `Error::Internal` for a `FIXED` row with no
/// rate, which the table's `CHECK` already forbids.
pub async fn effective_rate_e6(
	store: &dyn InvoiceStore,
	cur: &Currency,
	base: &CurrencyCode,
	configured_base: &CurrencyCode,
	source: &str,
	date: &str,
	max_age_days: i64,
) -> ClResult<i64> {
	if cur.code == *base {
		return Ok(1_000_000);
	}
	match cur.mode {
		RateMode::Fixed => {
			// Answering anyway freezes a fabricated figure onto the invoice: on a
			// `currency.base = 'EUR'` deployment the seeded HUF row reported 1 HUF = 1 EUR as
			// the HUF VAT amount Áfa tv. 172. § makes mandatory. The `base == configured_base`
			// half is covered at boot by [`check_currency_settings`].
			if base != configured_base {
				return Err(Error::coded(
					StatusCode::CONFLICT,
					"E-INV-NO-RATE",
					format!("no {base} rate for FIXED currency '{}'", cur.code),
				));
			}
			match cur.fixed_rate_e6 {
				Some(r) if r > 0 => Ok(r),
				Some(_) => Err(Error::coded(
					StatusCode::CONFLICT,
					"E-INV-NO-RATE",
					format!("currency '{}' has a non-positive fixed rate", cur.code),
				)),
				None => {
					Err(Error::internal(format!("currency '{}' is FIXED with no rate", cur.code)))
				}
			}
		}
		RateMode::Official => {
			rate_on(store, &pair(&cur.code, base), source, date, max_age_days).await
		}
	}
}

/// Refuse to start on a `currencies` table whose seeded HUF row contradicts `currency.base`.
/// An `on_init` check, like `saas_nav::auth::check_software_settings`.
///
/// `003_invoice.sql` seeds `HUF` as `FIXED` at 1.000000 — "base units per 1 HUF", true only
/// when the base *is* HUF. On any other base [`effective_rate_e6`] still answers with it,
/// because `base == configured_base` there, so a 10.00 EUR item bills as 10.00 HUF and
/// `A-RATE-MISSING` never fires: the lookup succeeded.
///
/// # Errors
/// `Error::Internal` — a refusal to boot — when the base is not HUF and the seeded row is
/// still in place.
pub async fn check_currency_settings(app: &App) -> ClResult<()> {
	let base = CurrencyCode::parse(app.settings.text("currency.base").await?.trim())?;
	if base == CurrencyCode::huf() {
		return Ok(());
	}
	let store = crate::service_api::store(app)?;
	let Some(huf) = store.currency_get("HUF").await? else {
		return Ok(());
	};
	if huf.mode == RateMode::Fixed && huf.fixed_rate_e6 == Some(1_000_000) {
		return Err(Error::internal(format!(
			"this deployment's currency.base is {base}, so the seeded HUF row's fixed rate of \
			 1.000000 means '1 HUF = 1 {base}' and is meaningless; re-seed it with the real \
			 fixed rate or switch it to OFFICIAL before any invoice is priced"
		)));
	}
	Ok(())
}

/// Round to the currency's display step, e.g. whole forints on a `price_round_step` of 100.
///
/// # Errors
/// `Error::Internal` when `step <= 0`; `E-CORE-VALIDATION` on `i64` overflow.
pub fn round_to_step(amount: Money, step: i64) -> ClResult<Money> {
	let steps = round_half_up(i128::from(amount.0), i128::from(step))?;
	steps
		.checked_mul(step)
		.map(Money)
		.ok_or_else(|| Error::validation("amount out of range"))
}

/// `E-INV-LINE` unless a **caller-supplied** unit price is already on `price_round_step`.
///
/// Checked, not rounded. A catalogue price is derived, so [`price_in`] rounds it and a
/// catalogue and an ad-hoc line at the same price still agree; a caller-supplied price is
/// asserted, and silently storing `2.00 HUF` for the `1.50` the client sent — a 33%
/// overcharge answered with `201` — is the wrong shape whatever the tax answer turns out
/// to be.
///
/// A negative amount passes through **untouched**: it is invalid, and `draft::price`'s
/// `unit_price < 0` guard is what must see it, not a modulo complaint.
///
/// OPEN TAX QUESTION — whether the step should apply to ad-hoc lines at all is for
/// the accountant. Group VAT and totals stay unstepped either way: rounding them breaks
/// `net + vat = gross` and NAV's `summaryByVatRate` cross-validation.
pub fn ensure_price_on_step(amount: Money, step: i64) -> ClResult<Money> {
	if amount.0 >= 0 && step > 0 && amount.0 % step != 0 {
		return Err(Error::coded(
			StatusCode::BAD_REQUEST,
			"E-INV-LINE",
			format!("unit price must be a multiple of {step} minor units in this currency"),
		));
	}
	Ok(amount)
}

/// A base-currency amount expressed in `cur`, marked up by `fee_bp` and rounded to
/// `price_round_step`: `price = round_to_step(base × rate × (1 + fee_bp/10000), step)`, with
/// `rate` the base-per-quote figure so the division is the one that converts.
///
/// # Errors
/// `Error::Internal` when `rate_e6` or `price_round_step` is not positive; `E-CORE-VALIDATION`
/// on overflow, or past the `MAX_MINOR` envelope — a `rate_e6` under 1e6 converts an in-range
/// base amount out of range, and the adapter's `read_money` then refuses the row it was
/// stored in. Bounded *after* the step, which can round a value up past the edge.
pub fn price_in(base_amount: Money, cur: &Currency, rate_e6: i64) -> ClResult<Money> {
	let num = i128::from(base_amount.0) * 1_000_000 * (10_000 + i128::from(cur.fee_bp));
	let den = i128::from(rate_e6) * 10_000;
	// One rounding, not two. Rounding to minor units and *then* to the step put the price a
	// full step out whenever the exact quotient landed within half a minor unit below a step
	// midpoint: 299 at rate 2.0, step 100, is 149.5 — which went 150 then 200, not 100.
	let steps = round_half_up(num, den * i128::from(cur.price_round_step))?;
	let priced = steps
		.checked_mul(cur.price_round_step)
		.ok_or_else(|| Error::validation("amount out of range"))?;
	bounded(priced).map(Money)
}

/// [`price_in`] without the markup: the base amount expressed in `cur`, rounded to the
/// currency's `price_round_step` but with **no** `fee_bp` applied.
///
/// An ad-hoc line's unit price is the caller's own figure, stored verbatim, where a catalogue
/// price goes through [`price_in`] and picks the fee up. Re-pricing an ad-hoc line through
/// [`price_in`] would add a markup it never had. Paired with [`to_base`] this is the fee-free
/// round trip — see `an_adhoc_price_round_trips_fee_free`.
///
/// # Errors
/// `Error::Internal` when `rate_e6` or `price_round_step` is not positive; `E-CORE-VALIDATION`
/// on overflow, or past the `MAX_MINOR` envelope, exactly as [`price_in`].
pub fn price_in_nofee(base_amount: Money, cur: &Currency, rate_e6: i64) -> ClResult<Money> {
	let num = i128::from(base_amount.0) * 1_000_000;
	// One rounding, for [`price_in`]'s reason.
	let steps = round_half_up(num, i128::from(rate_e6) * i128::from(cur.price_round_step))?;
	let priced = steps
		.checked_mul(cur.price_round_step)
		.ok_or_else(|| Error::validation("amount out of range"))?;
	bounded(priced).map(Money)
}

/// An amount in `pair`'s quote currency expressed in its base — the direction NAV needs, since a
/// Hungarian seller invoicing in EUR must report the HUF VAT amount. No fee and no step rounding:
/// this is a report of a legal figure, not a price.
///
/// # Errors
/// `E-CORE-VALIDATION` on `i64` overflow, or past the `MAX_MINOR` envelope — a product of two
/// in-range values is not in range, and the adapter's `read_money` re-applies `bounded` on the
/// way back out. See `a_conversion_past_the_money_envelope_is_rejected_not_stored`.
pub fn to_base(amount: Money, rate_e6: i64) -> ClResult<Money> {
	round_half_up(i128::from(amount.0) * i128::from(rate_e6), 1_000_000)
		.and_then(bounded)
		.map(Money)
}

/// The inverse of [`price_in`]'s markup: recover the base amount a priced figure was
/// derived from, so a re-price under a different currency does not stack one fee on another.
///
/// [`to_base`] alone divides out the rate but leaves `old.fee_bp` in, and [`price_in`] then
/// multiplies by `(10_000 + fee_bp)` again — so every currency change re-applied a markup on
/// top of one already baked into the stored unit price, compounding per change.
///
/// Not bit-exact in a round trip: [`price_in`] also applies `price_round_step`, so a
/// currency with a coarse step loses up to half a step on the way out and back. That is a
/// rounding artefact of re-pricing, not a compounding error — it does not grow with the
/// number of changes.
///
/// # Errors
/// `E-CORE-VALIDATION` on overflow or past the `MAX_MINOR` envelope.
pub fn to_base_unfeed(amount: Money, old: &Currency, old_rate_e6: i64) -> ClResult<Money> {
	let num = i128::from(amount.0) * i128::from(old_rate_e6) * 10_000;
	let den = 1_000_000 * (10_000 + i128::from(old.fee_bp));
	round_half_up(num, den).and_then(bounded).map(Money)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn eur() -> Currency {
		Currency {
			code: CurrencyCode::parse("EUR").unwrap(),
			price_round_step: 1,
			mode: RateMode::Official,
			fixed_rate_e6: None,
			fee_bp: 0,
			enabled: true,
		}
	}

	/// `to_base` bounded only by `i64`, but the adapter's `read_money` bounds by `MAX_MINOR` —
	/// so a large enough FX invoice wrote a `net_huf` that no later read could decode, and
	/// hydration, the PDF and the NAV filing all became a permanent 500 on that invoice.
	#[test]
	fn a_conversion_past_the_money_envelope_is_rejected_not_stored() {
		use saas_core::money::MAX_MINOR;
		let rate = 400_000_000; // 1 EUR = 400 HUF
		let at_the_edge = Money(MAX_MINOR / 400);
		assert_eq!(to_base(at_the_edge, rate).expect("at the edge"), Money(MAX_MINOR));
		assert!(to_base(Money(at_the_edge.0 + 1), rate).is_err(), "one unit past it");
	}

	/// The other direction, which ended at `round_to_step` and so had no envelope at all: a
	/// `rate_e6` under 1e6 converted an in-range base amount out of range, `draft::resolve`
	/// stored it verbatim, and `read_money` then made every hydration, render and filing of
	/// that invoice a permanent 500.
	#[test]
	fn a_price_past_the_money_envelope_is_rejected_not_stored() {
		use saas_core::money::MAX_MINOR;
		let mut c = eur();
		c.price_round_step = 100;
		let rate = 500_000; // 1 EUR = 0.5 HUF, so the price is twice the base amount
		let at_the_edge = Money(MAX_MINOR / 2 / 100 * 100);
		assert!(price_in_nofee(at_the_edge, &c, rate).is_ok(), "at the edge");
		assert!(price_in_nofee(Money(at_the_edge.0 + 100), &c, rate).is_err());

		let err = price_in(Money(at_the_edge.0 + 100), &c, rate).expect_err("past the envelope");
		assert_eq!(err.parts().1, "E-CORE-VALIDATION");
	}

	#[test]
	fn fee_marks_the_price_up_and_the_step_applies() {
		let rate = 400_000_000; // 1 EUR = 400 HUF
		// No fee, no step: 40 000 HUF (4 000 000 fillér) -> 100.00 EUR = 10 000 cents,
		assert_eq!(price_in(Money(4_000_000), &eur(), rate).expect("to EUR"), Money(10_000));
		// and back.
		assert_eq!(to_base(Money(10_000), rate).expect("to HUF"), Money(4_000_000));

		let mut c = eur();
		c.fee_bp = 250; // 2.5%
		c.price_round_step = 5;
		// 10 000 cents +2.5% = 10 250, already on the step.
		assert_eq!(price_in(Money(4_000_000), &c, rate).expect("fee"), Money(10_250));
		c.fee_bp = 0;
		// 10 003 cents rounds to the nearest 5.
		assert_eq!(price_in(Money(4_001_200), &c, rate).expect("step"), Money(10_005));
	}

	/// An ad-hoc price never had the fee applied, so unfeeding it on a currency change
	/// divided out a markup that was never there — a 4.76% undercharge at `fee_bp = 500` that
	/// survived `issue` onto a numbered, immutable invoice and was filed to NAV.
	#[test]
	fn an_adhoc_price_round_trips_fee_free() {
		let eur = Currency { fee_bp: 500, ..eur() };
		let huf = Currency {
			code: CurrencyCode::parse("HUF").unwrap(),
			price_round_step: 1,
			mode: RateMode::Fixed,
			fixed_rate_e6: Some(1_000_000),
			fee_bp: 300,
			enabled: true,
		};
		let (huf_rate, eur_rate) = (1_000_000, 400_000_000);

		// 100.00 EUR posted verbatim by the caller -> 40 000.00 HUF, not 38 095.24.
		let in_huf = price_in_nofee(to_base(Money(10_000), eur_rate).unwrap(), &huf, huf_rate);
		assert_eq!(in_huf.unwrap(), Money(4_000_000));
		// And back, with neither currency's fee stuck to it.
		let back = price_in_nofee(to_base(Money(4_000_000), huf_rate).unwrap(), &eur, eur_rate);
		assert_eq!(back.unwrap(), Money(10_000));
	}

	#[test]
	fn a_bad_step_is_an_error_not_a_panic() {
		// `price_in` no longer goes through `round_to_step`, so its own guard is the
		// non-positive denominator `round_half_up` refuses.
		assert!(
			price_in(Money(100), &Currency { price_round_step: 0, ..eur() }, 1_000_000).is_err()
		);
	}

	/// `price_in` rounded to minor units and *then* to the step, which is a full step out
	/// whenever the exact quotient lands within half a minor unit below a step midpoint.
	/// 299 base at rate 2.0 with whole-forint steps is exactly 149.5: nearest step is 100,
	/// but 149.5 -> 150 -> 200 doubled the stored catalogue price.
	#[test]
	fn a_price_is_rounded_to_the_step_once_not_twice() {
		let huf = Currency {
			code: CurrencyCode::parse("HUF").unwrap(),
			price_round_step: 100,
			mode: RateMode::Fixed,
			fixed_rate_e6: Some(2_000_000),
			fee_bp: 0,
			enabled: true,
		};
		assert_eq!(price_in(Money(299), &huf, 2_000_000).unwrap(), Money(100));
		assert_eq!(price_in_nofee(Money(299), &huf, 2_000_000).unwrap(), Money(100));
		// Half a step still rounds up, as `round_half_up` says.
		assert_eq!(price_in(Money(300), &huf, 2_000_000).unwrap(), Money(200));
	}

	/// HUF -> EUR -> HUF on marked-up currencies must come back where it started, and must
	/// not drift further on each successive change. `to_base` alone left the old fee in, so
	/// every hop multiplied the price by `(1 + fee)` again.
	#[test]
	fn a_currency_round_trip_does_not_stack_fees() {
		let huf = Currency {
			code: CurrencyCode::parse("HUF").unwrap(),
			price_round_step: 1,
			mode: RateMode::Fixed,
			fixed_rate_e6: Some(1_000_000),
			fee_bp: 300,
			enabled: true,
		};
		let eur = Currency { fee_bp: 500, ..eur() };
		let (huf_rate, eur_rate) = (1_000_000, 400_000_000);

		// The stored HUF unit price already carries HUF's own 3% markup.
		let start = price_in(Money(1_000_000), &huf, huf_rate).unwrap();

		let hop = |m: Money, from: &Currency, from_rate: i64, to: &Currency, to_rate: i64| {
			price_in(to_base_unfeed(m, from, from_rate).unwrap(), to, to_rate).unwrap()
		};

		let mut here = start;
		for round in 1..=5 {
			let there = hop(here, &huf, huf_rate, &eur, eur_rate);
			here = hop(there, &eur, eur_rate, &huf, huf_rate);
			// Within one `price_round_step` of where it started, however many times round.
			assert!(
				(here.0 - start.0).abs() <= huf.price_round_step,
				"round {round}: {here:?} drifted from {start:?}"
			);
		}
	}
}

// vim: ts=4
