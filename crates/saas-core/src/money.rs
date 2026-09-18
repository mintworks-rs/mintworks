//! Fixed-point money and quantity. No floats anywhere.
//!
//! [`Money`] is minor units of the row's own currency, **always two decimals** (HUF stores
//! fillér). A currency that displays no decimals is expressed by `currencies.price_round_step`,
//! not by a different scale — NAV's `MonetaryType` caps a filed amount at 2 decimals anyway.
//! [`Qty`] is scaled 1e6. VAT rates are integer basis points and exchange rates are scaled
//! 1e6 in an `_e6` column; both are plain integers and need no type here.
//!
//! On the wire an amount is a decimal **string** plus its currency code, because a JSON number is
//! an IEEE double and money is not.

use std::{
	fmt,
	ops::{Add, AddAssign, Neg, Sub, SubAssign},
	str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::error::{ClResult, Error};

/// `Qty` stores six decimals.
pub const QTY_DECIMALS: u32 = 6;

/// `Money` stores two decimals, for every currency.
pub const MONEY_DECIMALS: u32 = 2;

/// An amount in minor units of some currency. Which currency is a property of the row, not of
/// this value, so [`Money`] has no `Serialize` of its own: rendering it needs the code. Use
/// [`Money::to_wire`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Money(pub i64);

/// A quantity scaled 1e6, e.g. `2.5` is `Qty(2_500_000)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qty(pub i64);

/// An ISO-4217 alpha-3 code, uppercase by construction, so `==` is the only comparison
/// anybody needs. [`Self::parse`] is the single normalization door: every code from outside
/// the framework — `Deserialize`, a service handle argument, the `currency.base` setting —
/// comes through it, and the store adapter wraps what it reads back with
/// [`Self::from_trusted`] without transforming it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CurrencyCode(String);

impl CurrencyCode {
	/// Parse untrusted input: trim, uppercase, require exactly three ASCII letters.
	pub fn parse(s: &str) -> ClResult<Self> {
		let s = s.trim();
		if s.len() != 3 || !s.bytes().all(|b| b.is_ascii_alphabetic()) {
			return Err(Error::Validation(
				"expected a three-letter ISO-4217 currency code".to_owned(),
			));
		}
		Ok(Self(s.to_ascii_uppercase()))
	}

	/// The forint. Áfa tv. 172. § pins it into the VAT figures of every invoice this
	/// framework files, whatever the invoice's own currency is, so the literal has a home
	/// here rather than in each of those call sites.
	pub fn huf() -> Self {
		Self("HUF".to_owned())
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}

	pub fn into_string(self) -> String {
		self.0
	}

	/// Wrap a value already read from the database, skipping validation — the DB is a trusted
	/// source because every write path went through [`Self::parse`] first.
	pub fn from_trusted(s: String) -> Self {
		Self(s)
	}
}

impl fmt::Display for CurrencyCode {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.0)
	}
}

impl FromStr for CurrencyCode {
	type Err = Error;

	fn from_str(s: &str) -> ClResult<Self> {
		Self::parse(s)
	}
}

impl PartialEq<str> for CurrencyCode {
	fn eq(&self, other: &str) -> bool {
		self.0 == other
	}
}

impl PartialEq<&str> for CurrencyCode {
	fn eq(&self, other: &&str) -> bool {
		self.0 == *other
	}
}

impl Serialize for CurrencyCode {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_str(&self.0)
	}
}

impl<'de> Deserialize<'de> for CurrencyCode {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		let s = String::deserialize(deserializer)?;
		Self::parse(&s).map_err(D::Error::custom)
	}
}

/// The wire shape of an amount: `{"amount": "12500.00", "currency": "HUF"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoneyWire {
	pub amount: String,
	pub currency: CurrencyCode,
}

/// The largest magnitude [`Money::parse`] accepts, in minor units — 1e15, or ten trillion
/// forints. Anything above it is a typo or an attack, not an invoice line.
///
/// This bound is what makes the unchecked `Add`/`Sub`/`Sum` impls below safe: with it, ~9000
/// maximal amounts sum before `i64` overflows, which no invoice reaches. Without it the
/// release `overflow-checks` panic is the only guard, and an out-of-range amount deserves a
/// `400` — besides which `CatchPanicLayer` reaches neither `spawn_blocking` (the typst render)
/// nor a job-runner task. Switch the impls to `checked_*` if amounts above it are ever real.
pub const MAX_MINOR: i64 = 1_000_000_000_000_000;

/// Reject a computed amount that leaves the [`MAX_MINOR`] envelope the unchecked `Add`/`Sub`
/// impls depend on. `parse` establishes the invariant at the trust boundary; every *product*
/// has to re-establish it, because a product of two in-range values is not in range.
pub fn bounded(v: i64) -> ClResult<i64> {
	if v.unsigned_abs() > MAX_MINOR.unsigned_abs() {
		return Err(Error::Validation("amount out of range".to_owned()));
	}
	Ok(v)
}

impl Money {
	pub const ZERO: Self = Self(0);

	/// Render with exactly [`MONEY_DECIMALS`] decimals, as the wire format requires.
	pub fn to_decimal_string(self) -> String {
		format_scaled(self.0, MONEY_DECIMALS)
	}

	pub fn to_wire(self, currency: &CurrencyCode) -> MoneyWire {
		MoneyWire { amount: self.to_decimal_string(), currency: currency.clone() }
	}

	/// Parse a decimal string. Trailing decimals may be omitted (`"12500"` is accepted); more
	/// than [`MONEY_DECIMALS`] decimals is a validation failure, not a silent truncation.
	///
	/// Bounded by [`MAX_MINOR`]: this is the trust boundary, and the arithmetic downstream of
	/// it is unchecked `i64`.
	pub fn parse(s: &str) -> ClResult<Self> {
		let minor = parse_scaled(s, MONEY_DECIMALS)?;
		// `unsigned_abs`, not `abs`: `parse_scaled` can return `i64::MIN`, whose `abs`
		// overflows — and `overflow-checks` is on in release too.
		if minor.unsigned_abs() > MAX_MINOR.unsigned_abs() {
			return Err(Error::Validation("amount out of range".to_owned()));
		}
		Ok(Self(minor))
	}

	/// Multiply by a basis-point rate, rounding halves away from zero. This is the VAT and
	/// discount primitive: `net.mul_bp(2700)` is 27% VAT.
	pub fn mul_bp(self, bp: i64) -> ClResult<Self> {
		round_half_up(i128::from(self.0) * i128::from(bp), 10_000)
			.and_then(bounded)
			.map(Self)
	}
}

impl Qty {
	pub fn to_decimal_string(self) -> String {
		format_scaled(self.0, QTY_DECIMALS)
	}

	/// Bounded by [`MAX_MINOR`] exactly as [`Money::parse`] is, and for the same reason:
	/// [`Qty::times`] feeds the unchecked `Money` arithmetic, so an unbounded quantity is a
	/// way past the trust boundary. The sign is deliberately *not* checked here — a storno
	/// line carries a negative quantity; the HTTP bodies in `saas_invoice::routes` are where
	/// a positive quantity is required.
	pub fn parse(s: &str) -> ClResult<Self> {
		let minor = parse_scaled(s, QTY_DECIMALS)?;
		if minor.unsigned_abs() > MAX_MINOR.unsigned_abs() {
			return Err(Error::Validation("quantity out of range".to_owned()));
		}
		Ok(Self(minor))
	}

	/// Multiply a unit price by this quantity, rounding halves away from zero. The product is
	/// [`bounded`]: two in-range factors multiply to ~9000× [`MAX_MINOR`], which is past what
	/// the unchecked `Money` arithmetic downstream can absorb.
	pub fn times(self, unit_price: Money) -> ClResult<Money> {
		round_half_up(i128::from(self.0) * i128::from(unit_price.0), 1_000_000)
			.and_then(bounded)
			.map(Money)
	}
}

impl Serialize for Qty {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_str(&self.to_decimal_string())
	}
}

impl<'de> Deserialize<'de> for Qty {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		let s = String::deserialize(deserializer)?;
		Self::parse(&s).map_err(D::Error::custom)
	}
}

/// Divide `num` by `den`, rounding halves **away from zero** — the rule Hungarian VAT
/// arithmetic uses, and the same one on the way back out through a storno's negatives.
/// `den` must be positive.
pub fn round_half_up(num: i128, den: i128) -> ClResult<i64> {
	if den <= 0 {
		return Err(Error::Internal("round_half_up: non-positive denominator".to_owned()));
	}
	let range = || Error::Validation("amount out of range".to_owned());
	let negative = num < 0;
	// Checked: `rate_e6` is the one multiplicand with no `MAX_MINOR` envelope — operator-seeded
	// `MANUAL`/`BANK` rows carry only `CHECK (> 0)` — and release `overflow-checks` turned the
	// documented `E-CORE-VALIDATION` into a panic inside `change_currency`.
	let magnitude = num.checked_abs().ok_or_else(range)?;
	let quotient = magnitude
		.checked_mul(2)
		.and_then(|n| n.checked_add(den))
		.and_then(|n| den.checked_mul(2).map(|d| n / d))
		.ok_or_else(range)?;
	let signed = if negative { -quotient } else { quotient };
	i64::try_from(signed).map_err(|_| range())
}

/// `10^decimals`, clamped so the exponent can never overflow `u64`.
fn scale_of(decimals: u32) -> i64 {
	10i64.pow(decimals.min(18))
}

/// Render a fixed-point integer with exactly `decimals` decimals. Public for the values that
/// are *not* [`Money`] and so must not borrow its formatter: basis points at 4, `_e6` rates
/// at 6.
pub fn format_scaled(value: i64, decimals: u32) -> String {
	if decimals == 0 {
		return value.to_string();
	}
	let sign = if value < 0 { "-" } else { "" };
	let magnitude = i128::from(value).abs();
	let scale = i128::from(scale_of(decimals));
	let width = usize::try_from(decimals.min(18)).unwrap_or(0);
	format!("{}{}.{:0width$}", sign, magnitude / scale, magnitude % scale, width = width)
}

fn parse_scaled(s: &str, decimals: u32) -> ClResult<i64> {
	let s = s.trim();
	let malformed = || Error::Validation(format!("malformed decimal amount '{}'", s));

	let (negative, unsigned) = match s.strip_prefix('-') {
		Some(rest) => (true, rest),
		None => (false, s.strip_prefix('+').unwrap_or(s)),
	};
	let (int_part, frac_part) = unsigned.split_once('.').unwrap_or((unsigned, ""));
	if int_part.is_empty() && frac_part.is_empty() {
		return Err(malformed());
	}
	if !int_part.bytes().chain(frac_part.bytes()).all(|b| b.is_ascii_digit()) {
		return Err(malformed());
	}
	// An `i64` holds at most 19 digits, so a longer input is out of range by definition.
	// Rejecting it here is also what keeps the `i128` arithmetic below away from its own
	// limit, where a wrap could land back inside `i64` range and pass for a valid amount.
	if int_part.len() > 19 || frac_part.len() > 19 {
		return Err(Error::Validation("amount out of range".to_owned()));
	}
	let places = usize::try_from(decimals.min(18)).unwrap_or(0);
	if frac_part.len() > places {
		return Err(Error::Validation(format!("at most {} decimals allowed", places)));
	}

	let int: i128 =
		if int_part.is_empty() { 0 } else { int_part.parse().map_err(|_| malformed())? };
	let frac: i128 =
		if frac_part.is_empty() { 0 } else { frac_part.parse().map_err(|_| malformed())? };
	// Right-pad the fraction to the full number of places: "1.5" with 2 decimals is 150.
	let pad_places = u32::try_from(places - frac_part.len()).unwrap_or(0);
	let padding = i128::from(scale_of(pad_places));
	let out_of_range = || Error::Validation("amount out of range".to_owned());
	// Checked, so that a future change to the digit bound above cannot quietly reopen the wrap.
	let total = int
		.checked_mul(i128::from(scale_of(decimals)))
		.and_then(|v| v.checked_add(frac.checked_mul(padding)?))
		.ok_or_else(out_of_range)?;

	i64::try_from(if negative { -total } else { total }).map_err(|_| out_of_range())
}

impl Add for Money {
	type Output = Self;

	fn add(self, rhs: Self) -> Self {
		Self(self.0 + rhs.0)
	}
}

impl Sub for Money {
	type Output = Self;

	fn sub(self, rhs: Self) -> Self {
		Self(self.0 - rhs.0)
	}
}

impl Neg for Money {
	type Output = Self;

	fn neg(self) -> Self {
		Self(-self.0)
	}
}

impl AddAssign for Money {
	fn add_assign(&mut self, rhs: Self) {
		self.0 += rhs.0;
	}
}

impl SubAssign for Money {
	fn sub_assign(&mut self, rhs: Self) {
		self.0 -= rhs.0;
	}
}

impl std::iter::Sum for Money {
	fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
		iter.fold(Self::ZERO, Add::add)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The newtype's whole point: one door, and it uppercases. Everything downstream compares
	/// with `==`, so a `huf` row slipping past here takes the wrong branch in the NAV writer.
	#[test]
	fn a_currency_code_is_uppercase_by_construction() {
		assert_eq!(CurrencyCode::parse(" huf ").unwrap().as_str(), "HUF");
		assert_eq!(CurrencyCode::parse("Eur").unwrap().as_str(), "EUR");
		assert!(CurrencyCode::parse("HU").is_err());
		assert!(CurrencyCode::parse("HUFF").is_err());
		assert!(CurrencyCode::parse("HU1").is_err());
		assert!(CurrencyCode::parse("").is_err());
	}

	/// A long digit string used to reach the `i128` multiplication and could wrap back into
	/// `i64` range; the only guard was the final `try_from`, which cannot see a wrap.
	#[test]
	fn an_absurdly_long_amount_is_rejected_not_wrapped() {
		assert!(Money::parse(&"9".repeat(25)).is_err());
		assert!(Money::parse(&"9".repeat(40)).is_err());
		assert!(Money::parse(&format!("-{}", "9".repeat(40))).is_err());
		assert!(Money::parse(&format!("1.{}", "9".repeat(40))).is_err());
		// `Qty` has no cap, so it still shows the raw `i64` boundary: 19 digits in, 20 out.
		assert!(parse_scaled("9223372036854775807", 0).is_ok());
		assert!(parse_scaled("92233720368547758070", 0).is_err());
	}

	/// `Add`/`Sub`/`Sum` are unchecked `i64` and `overflow-checks` makes a wrap a panic, so
	/// what keeps that panic unreachable is the parse bound — not the arithmetic.
	#[test]
	fn parse_is_bounded_so_the_unchecked_arithmetic_cannot_reach_its_trap() {
		// `MAX_MINOR` minor units is 1e13 whole units at two decimals.
		assert_eq!(Money::parse("10000000000000.00").unwrap(), Money(MAX_MINOR));
		assert!(Money::parse("10000000000000.01").is_err());
		assert!(Money::parse("-10000000000000.01").is_err());

		// Two maximal amounts summed are nowhere near the `i64` ceiling; so are ~9000.
		let sum = Money(MAX_MINOR) + Money(MAX_MINOR);
		assert_eq!(sum, Money(2 * MAX_MINOR));
		const { assert!(i64::MAX / MAX_MINOR >= 9000) };
	}

	/// `Qty` had neither bound. `Qty::times` feeds the unchecked `Money` arithmetic, so an
	/// unbounded quantity was a way round the trust boundary `Money::parse` is.
	#[test]
	fn qty_parse_is_bounded_the_same_way_money_is() {
		// `Qty` carries six decimals, so `MAX_MINOR` is a billion whole units.
		assert_eq!(Qty::parse("1000000000").unwrap(), Qty(MAX_MINOR));
		assert!(Qty::parse("1000000000.000001").is_err());
		assert!(Qty::parse("-1000000000.000001").is_err());
		// The sign stays legal: a storno line carries a negative quantity.
		assert_eq!(Qty::parse("-1").unwrap(), Qty(-1_000_000));
	}

	#[test]
	fn decimal_string_round_trip() {
		assert_eq!(Money(1_250_000).to_decimal_string(), "12500.00");
		assert_eq!(Money(-5).to_decimal_string(), "-0.05");
		assert_eq!(Qty(2_500_000).to_decimal_string(), "2.500000");
		// The non-money scales borrow the free function, not `Money`'s formatter.
		assert_eq!(format_scaled(7, 0), "7");
		assert_eq!(format_scaled(2700, 4), "0.2700");
		assert_eq!(format_scaled(1_000_000, 6), "1.000000");

		assert_eq!(Money::parse("12500").unwrap(), Money(1_250_000));
		assert_eq!(Money::parse("12500.5").unwrap(), Money(1_250_050));
		assert_eq!(Money::parse("-0.05").unwrap(), Money(-5));
		assert_eq!(Qty::parse("2.5").unwrap(), Qty(2_500_000));

		// `parse_scaled` can land exactly on `i64::MIN`, whose `abs` overflows — and
		// `overflow-checks` is on in release too, so `abs` panicked the handler task.
		assert!(Money::parse("-92233720368547758.08").is_err());
		assert!(Qty::parse("-9223372036854.775808").is_err());

		// Excess precision is rejected rather than silently truncated.
		assert!(Money::parse("1.005").is_err());
		assert!(Money::parse("1.2.3").is_err());
		assert!(Money::parse("").is_err());
	}

	#[test]
	fn rounding_is_half_away_from_zero() {
		assert_eq!(round_half_up(5, 10).unwrap(), 1);
		assert_eq!(round_half_up(4, 10).unwrap(), 0);
		assert_eq!(round_half_up(-5, 10).unwrap(), -1);
		assert_eq!(round_half_up(-4, 10).unwrap(), 0);
		assert!(round_half_up(1, 0).is_err());

		// `E-CORE-VALIDATION`, not a panic: `magnitude * 2` was unchecked `i128`, and
		// `currency::to_base_unfeed` feeds it an operator-seeded `rate_e6` with no `MAX_MINOR`
		// envelope — under release `overflow-checks` that panicked inside `change_currency`.
		assert!(round_half_up(i128::MAX, 1).is_err());
		assert!(round_half_up(i128::MIN, 1).is_err());
		// The largest case that is still a legal `Money`: 1e15 minor units at rate 1.0.
		assert_eq!(
			round_half_up(1_000_000_000_000_000 * 1_000_000, 1_000_000).unwrap(),
			10i64.pow(15)
		);

		// 27% VAT on 12 500.00, and the storno of the same line.
		assert_eq!(Money(1_250_000).mul_bp(2700).unwrap(), Money(337_500));
		assert_eq!(Money(-1_250_000).mul_bp(2700).unwrap(), Money(-337_500));
		assert_eq!(Qty(2_500_000).times(Money(333)).unwrap(), Money(833));
	}
}

// vim: ts=4
