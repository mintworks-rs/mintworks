//! What a line costs: discounts, line net, and exact pro-rata apportionment.
//!
//! The arithmetic primitives themselves live in `mintworks_core::money` ([`Qty::times`],
//! [`Money::mul_bp`], [`round_half_up`]); this module only composes them.

use mintworks_core::error::StatusCode;
use mintworks_core::prelude::{ClResult, Error, Money, Qty, round_half_up};

use crate::store::DiscountKind;
use crate::vat::VatCode;

/// A line or invoice level discount. `Percent` is basis points, so `1000` is 10%.
///
/// The original form is kept rather than only its resolved amount because NAV's
/// `lineDiscountData` wants both `discountRate` and `discountValue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discount {
	Amount(Money),
	Percent(u32),
}

impl Discount {
	/// Resolve to an absolute amount against `base`, rounding halves away from zero.
	pub fn resolve(self, base: Money) -> ClResult<Money> {
		match self {
			Self::Amount(amount) => Ok(amount),
			Self::Percent(bp) => base.mul_bp(i64::from(bp)),
		}
	}
}

/// One line of a draft invoice, before pricing. Mutable in place so `PricingHook` can
/// rewrite the lines it is handed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftLine {
	/// The backing `services.id`, or `None` for an ad-hoc line no master row backs.
	///
	/// The internal id rather than the `ServiceId` uid, and carried **on the line** rather than
	/// in a slice beside it: `PricingHook` may add or remove lines, and a parallel
	/// `Vec<Option<i64>>` silently desynchronises — the `zip` in [`crate::draft::price`] truncates
	/// to the shorter of the two, shifting stored totals onto the wrong line's service.
	/// `crate::store::Service` exposes this id, so a hook can match on it.
	pub service_id: Option<i64>,
	pub description: String,
	/// NAV's `unitOfMeasure`. `invoice_lines.unit` is `NOT NULL` with no default, so an
	/// ad-hoc line has to carry one too.
	pub unit: String,
	pub qty: Qty,
	pub unit_price: Money,
	pub vat_code: VatCode,
	pub discount: Option<Discount>,
	pub discount_description: Option<String>,
	/// Caller free text; see [`crate::store::InvoiceLine::note`].
	pub note: Option<String>,
}

/// Split `total` across `weights` pro rata, **exactly**: the parts always sum back to
/// `total`, with no remainder left over and none counted twice.
///
/// Each part is the difference between two cumulative roundings rather than a rounding of
/// its own share, which is what makes the sum exact. Negative weights work too, so a
/// storno's negated lines apportion the same way.
pub fn apportion(total: Money, weights: &[Money]) -> ClResult<Vec<Money>> {
	let mut parts = vec![Money::ZERO; weights.len()];
	let sum: i128 = weights.iter().map(|w| i128::from(w.0)).sum();
	if sum == 0 {
		// Nothing to weigh by — an all-zero-net invoice. Put it all on the first line
		// rather than lose it, so the parts still sum to `total`.
		if let Some(first) = parts.first_mut() {
			*first = total;
		}
		return Ok(parts);
	}

	// `round_half_up` needs a positive denominator; flip both sides when the weights are
	// negative, which leaves the ratio unchanged.
	let (sign, den) = if sum < 0 { (-1i128, -sum) } else { (1i128, sum) };
	let (mut cumulative, mut allocated) = (0i128, 0i64);
	for (part, weight) in parts.iter_mut().zip(weights) {
		cumulative += i128::from(weight.0);
		let up_to_here = round_half_up(sign * i128::from(total.0) * cumulative, den)?;
		*part = Money(up_to_here - allocated);
		allocated = up_to_here;
	}
	Ok(parts)
}

/// A discount as the wire carries it, mirroring the two `discount_kind`/`discount_value`
/// columns that keep what the caller asked for: `discountValue` is basis points for
/// `PERCENT` and minor units for `AMOUNT`, exactly as the column stores it.
///
/// The one place a stored pair — on the invoice or on a line — becomes a [`Discount`].
///
/// A conversion, not a guard. An `AMOUNT` discount's sign and magnitude are checked in
/// [`crate::vat::compute`] alone, the single funnel every draft, issue and re-price path
/// reaches — and a `STORNO` line's legitimately negative stored discount has to survive the
/// round trip here.
///
/// # Errors
/// `E-INV-DISCOUNT` on a negative percentage, or one column given without the other.
pub fn discount_of(kind: Option<DiscountKind>, value: Option<i64>) -> ClResult<Option<Discount>> {
	let bad = |msg: &'static str| Error::coded(StatusCode::BAD_REQUEST, "E-INV-DISCOUNT", msg);
	match (kind, value) {
		(None, None) => Ok(None),
		(Some(DiscountKind::Amount), Some(v)) => Ok(Some(Discount::Amount(Money(v)))),
		(Some(DiscountKind::Percent), Some(v)) => u32::try_from(v)
			.map(|bp| Some(Discount::Percent(bp)))
			.map_err(|_| bad("a percent discount is a non-negative basis-point figure")),
		_ => Err(bad("discountKind and discountValue are given together or not at all")),
	}
}

/// The inverse of [`discount_of`]: the column pair to store for a [`Discount`].
#[must_use]
pub fn discount_parts(d: Option<Discount>) -> (Option<DiscountKind>, Option<i64>) {
	match d {
		None => (None, None),
		Some(Discount::Amount(m)) => (Some(DiscountKind::Amount), Some(m.0)),
		Some(Discount::Percent(bp)) => (Some(DiscountKind::Percent), Some(i64::from(bp))),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn apportionment_is_exact() {
		// 100 over three equal weights cannot be split evenly; nothing may be lost.
		let parts = apportion(Money(100), &[Money(1), Money(1), Money(1)]).unwrap();
		assert_eq!(parts, vec![Money(33), Money(34), Money(33)]);
		assert_eq!(parts.iter().copied().sum::<Money>(), Money(100));

		// The same on the way back out through a storno.
		let parts = apportion(Money(-100), &[Money(-1), Money(-1), Money(-1)]).unwrap();
		assert_eq!(parts.iter().copied().sum::<Money>(), Money(-100));

		// Weights that sum to zero still account for the whole total.
		let parts = apportion(Money(500), &[Money::ZERO, Money::ZERO]).unwrap();
		assert_eq!(parts, vec![Money(500), Money::ZERO]);

		assert!(apportion(Money(500), &[]).unwrap().is_empty());
	}
}

// vim: ts=4
