// SPDX-License-Identifier: MPL-2.0
//! `Money` and `Qty` as opaque Rune types.
//!
//! There is no conversion to Rune's float type in either direction — not a `to_f64`, not for
//! formatting. Every amount is parsed from and rendered to a decimal string over the integer
//! minor units, and every product goes through `round_half_up` + `bounded`, which is what the
//! unchecked `i64` arithmetic in `mintworks_core::money` depends on.
//!
//! VAT rates and exchange rates get no type here: they are integer basis points and `_e6`
//! scales, and they cross the bridge as `i64`.

use std::cmp::Ordering;

use mintworks_core::{
	error::Error,
	money::{self, CurrencyCode, MoneyWire},
};
use rune::{Any, ContextError, Module, runtime::VmResult};

use crate::value::ScriptError;

/// A script-side amount: the minor units, plus the currency they are in.
///
/// `mintworks_core::money::Money` carries no currency — which currency is a property of the row —
/// so adding two of them cannot notice a mismatch. The tag here is what does.
#[derive(Debug, Clone, Any)]
pub struct Money {
	amount: money::Money,
	currency: CurrencyCode,
}

/// A quantity, scaled 1e6.
#[derive(Debug, Clone, Copy, Any)]
pub struct Qty(money::Qty);

impl Money {
	/// # Errors
	/// `Error::Validation` for an unparseable amount, one past `MAX_MINOR`, or a code that is
	/// not three ASCII letters.
	pub fn new(amount: &str, currency: &str) -> Result<Self, Error> {
		Ok(Self { amount: money::Money::parse(amount)?, currency: CurrencyCode::parse(currency)? })
	}

	#[must_use]
	pub fn to_wire(&self) -> MoneyWire {
		self.amount.to_wire(&self.currency)
	}

	fn same_currency(&self, rhs: &Self) -> Result<(), ScriptError> {
		if self.currency == rhs.currency {
			return Ok(());
		}
		Err(ScriptError(Error::Validation(format!(
			"cannot combine {} with {}",
			self.currency, rhs.currency
		))))
	}

	/// Re-establish the `MAX_MINOR` envelope: a sum or product of two in-range amounts is not
	/// itself in range, and everything downstream is unchecked `i64`.
	fn rebound(&self, minor: i64) -> Result<Self, ScriptError> {
		let amount = money::Money(money::bounded(minor).map_err(ScriptError)?);
		Ok(Self { amount, currency: self.currency.clone() })
	}
}

impl Qty {
	/// # Errors
	/// `Error::Validation` for an unparseable quantity or one past `MAX_MINOR`.
	pub fn new(value: &str) -> Result<Self, Error> {
		money::Qty::parse(value).map(Self)
	}

	#[must_use]
	pub fn inner(&self) -> money::Qty {
		self.0
	}
}

/// `money("12500.00", "HUF")`.
#[rune::function]
pub fn money(amount: &str, currency: &str) -> Result<Money, ScriptError> {
	Money::new(amount, currency).map_err(ScriptError)
}

/// `qty("2.5")`.
#[rune::function]
pub fn qty(value: &str) -> Result<Qty, ScriptError> {
	Qty::new(value).map_err(ScriptError)
}

#[rune::function(keep, instance)]
fn add(this: &Money, rhs: &Money) -> Result<Money, ScriptError> {
	this.same_currency(rhs)?;
	this.rebound(this.amount.0 + rhs.amount.0)
}

#[rune::function(keep, instance)]
fn sub(this: &Money, rhs: &Money) -> Result<Money, ScriptError> {
	this.same_currency(rhs)?;
	this.rebound(this.amount.0 - rhs.amount.0)
}

/// The VAT and discount primitive: `net.mul_bp(2700)` is 27%, rounding halves away from zero.
#[rune::function(keep, instance)]
fn mul_bp(this: &Money, bp: i64) -> Result<Money, ScriptError> {
	let amount = this.amount.mul_bp(bp).map_err(ScriptError)?;
	Ok(Money { amount, currency: this.currency.clone() })
}

/// `(each, remainder)`, never a float: the remainder is what `each * n` leaves behind, so the
/// two always add back to the original and no minor unit is invented or lost.
#[rune::function(keep, instance)]
fn split(this: &Money, n: i64) -> Result<(Money, Money), ScriptError> {
	if n <= 0 {
		return Err(ScriptError(Error::Validation("split needs a positive divisor".to_owned())));
	}
	let each = this.amount.0 / n;
	Ok((this.rebound(each)?, this.rebound(this.amount.0 - each * n)?))
}

#[rune::function(instance)]
fn neg(this: &Money) -> Money {
	Money { amount: money::Money(-this.amount.0), currency: this.currency.clone() }
}

#[rune::function(instance)]
fn is_zero(this: &Money) -> bool {
	this.amount.0 == 0
}

#[rune::function(instance)]
fn to_str(this: &Money) -> String {
	this.amount.to_decimal_string()
}

#[rune::function(instance)]
fn currency(this: &Money) -> String {
	this.currency.as_str().to_owned()
}

#[rune::function(instance)]
fn minor(this: &Money) -> i64 {
	this.amount.0
}

/// The operators raise rather than return, so `a + b` stays an amount and a mismatch cannot be
/// read as one. The named `add`/`sub` above are the same arithmetic as a `Result`.
#[rune::function(instance, protocol = ADD)]
fn op_add(this: &Money, rhs: &Money) -> VmResult<Money> {
	match add(this, rhs) {
		Ok(money) => VmResult::Ok(money),
		Err(err) => VmResult::panic(err.0.to_string()),
	}
}

#[rune::function(instance, protocol = SUB)]
fn op_sub(this: &Money, rhs: &Money) -> VmResult<Money> {
	match sub(this, rhs) {
		Ok(money) => VmResult::Ok(money),
		Err(err) => VmResult::panic(err.0.to_string()),
	}
}

#[rune::function(keep, instance, protocol = PARTIAL_EQ)]
fn op_partial_eq(this: &Money, rhs: &Money) -> VmResult<bool> {
	match this.same_currency(rhs) {
		Ok(()) => VmResult::Ok(this.amount == rhs.amount),
		Err(err) => VmResult::panic(err.0.to_string()),
	}
}

#[rune::function(instance, protocol = EQ)]
fn op_eq(this: &Money, rhs: &Money) -> VmResult<bool> {
	op_partial_eq(this, rhs)
}

#[rune::function(instance, protocol = PARTIAL_CMP)]
fn op_partial_cmp(this: &Money, rhs: &Money) -> VmResult<Option<Ordering>> {
	match this.same_currency(rhs) {
		Ok(()) => VmResult::Ok(this.amount.partial_cmp(&rhs.amount)),
		Err(err) => VmResult::panic(err.0.to_string()),
	}
}

#[rune::function(instance, protocol = CMP)]
fn op_cmp(this: &Money, rhs: &Money) -> VmResult<Ordering> {
	match this.same_currency(rhs) {
		Ok(()) => VmResult::Ok(this.amount.cmp(&rhs.amount)),
		Err(err) => VmResult::panic(err.0.to_string()),
	}
}

/// `qty.times(unit_price)` — the line-net primitive, bounded like every other product.
#[rune::function(keep, instance)]
fn times(this: &Qty, unit_price: &Money) -> Result<Money, ScriptError> {
	let amount = this.0.times(unit_price.amount).map_err(ScriptError)?;
	Ok(Money { amount, currency: unit_price.currency.clone() })
}

#[rune::function(instance)]
fn qty_to_str(this: &Qty) -> String {
	this.0.to_decimal_string()
}

/// The raw 1e6 scale, for a script that needs the integer rather than the rendering.
#[rune::function(instance)]
fn scaled(this: &Qty) -> i64 {
	this.0.0
}

#[rune::function(instance, protocol = PARTIAL_EQ)]
fn qty_partial_eq(this: &Qty, rhs: &Qty) -> bool {
	this.0 == rhs.0
}

#[rune::function(instance, protocol = EQ)]
fn qty_eq(this: &Qty, rhs: &Qty) -> bool {
	this.0 == rhs.0
}

#[rune::function(instance, protocol = PARTIAL_CMP)]
fn qty_partial_cmp(this: &Qty, rhs: &Qty) -> Option<Ordering> {
	this.0.partial_cmp(&rhs.0)
}

#[rune::function(instance, protocol = CMP)]
fn qty_cmp(this: &Qty, rhs: &Qty) -> Ordering {
	this.0.cmp(&rhs.0)
}

/// # Errors
/// Whatever Rune raises registering a type or a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::new();

	m.ty::<Money>()?;
	m.ty::<Qty>()?;

	m.function_meta(money)?;
	m.function_meta(qty)?;

	m.function_meta(add__meta)?;
	m.function_meta(sub__meta)?;
	m.function_meta(mul_bp__meta)?;
	m.function_meta(split__meta)?;
	m.function_meta(neg)?;
	m.function_meta(is_zero)?;
	m.function_meta(to_str)?;
	m.function_meta(currency)?;
	m.function_meta(minor)?;
	m.function_meta(op_add)?;
	m.function_meta(op_sub)?;
	m.function_meta(op_partial_eq__meta)?;
	m.function_meta(op_eq)?;
	m.function_meta(op_partial_cmp)?;
	m.function_meta(op_cmp)?;

	m.function_meta(times__meta)?;
	m.function_meta(qty_to_str)?;
	m.function_meta(scaled)?;
	m.function_meta(qty_partial_eq)?;
	m.function_meta(qty_eq)?;
	m.function_meta(qty_partial_cmp)?;
	m.function_meta(qty_cmp)?;

	Ok(m)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn huf(s: &str) -> Money {
		Money::new(s, "HUF").unwrap()
	}

	#[test]
	fn mixed_currencies_do_not_add() {
		let err = add(&huf("100"), &Money::new("100", "EUR").unwrap()).unwrap_err();
		assert!(matches!(err.0, Error::Validation(_)));
	}

	#[test]
	fn vat_is_basis_points_and_rounds_away_from_zero() {
		assert_eq!(mul_bp(&huf("100.01"), 2700).unwrap().to_wire().amount, "27.00");
	}

	#[test]
	fn a_split_loses_no_minor_unit() {
		let (each, rest) = split(&huf("100.00"), 3).unwrap();
		assert_eq!(each.amount.0 * 3 + rest.amount.0, huf("100.00").amount.0);
		assert_eq!((each.to_wire().amount, rest.to_wire().amount), ("33.33".into(), "0.01".into()));
	}

	#[test]
	fn split_by_zero_is_an_error() {
		assert!(split(&huf("100.00"), 0).is_err());
	}

	#[test]
	fn a_quantity_times_a_price_keeps_the_price_currency() {
		let net = times(&Qty::new("2.5").unwrap(), &huf("400.00")).unwrap();
		assert_eq!(net.to_wire(), huf("1000.00").to_wire());
	}
}

// vim: ts=4
