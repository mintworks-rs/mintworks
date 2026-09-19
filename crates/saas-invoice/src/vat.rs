//! VAT codes and the per-rate-group computation.
//!
//! **VAT is computed once per rate group on the summed net, never per line.** NAV's
//! `summaryByVatRate` has to add up exactly, and per-line rounding drifts from the group
//! total by a few fillér, which NAV's cross-field validation rejects. Line `vat` and
//! `gross` are display values, apportioned back out of the group figure.

use saas_core::prelude::{ClResult, Error, Money, bounded};

use crate::money::{Discount, DraftLine, apportion};
use serde::{Deserialize, Serialize};

/// Sum already-bounded amounts and re-establish the `MAX_MINOR` envelope on the result.
///
/// `Money`'s `Add` is unchecked by design (`saas_core::money`), so it would trap on the way
/// to a figure this then rejects: accumulate in `i128` first, then bound. Every point below
/// that adds *many* bounded values goes through here — a product is bounded at its source.
pub(crate) fn sum_bounded(amounts: impl IntoIterator<Item = Money>) -> ClResult<Money> {
	let total: i128 = amounts.into_iter().map(|a| i128::from(a.0)).sum();
	i64::try_from(total)
		.map_err(|_| Error::Validation("amount out of range".to_owned()))
		.and_then(bounded)
		.map(Money)
}

/// The eight VAT codes, as constrained by `invoice_lines.vat_code`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VatCode {
	Std27,
	Red18,
	Red05,
	Aam,
	Tam,
	Eufad37,
	Ho,
	Atk,
}

impl std::str::FromStr for VatCode {
	type Err = Error;

	/// The inverse of [`VatCode::as_str`]. The input is a column value, so an unknown tag is a
	/// corrupt database rather than bad input, and reports as `Error::Internal`.
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Self::ALL
			.into_iter()
			.find(|c| c.as_str() == s)
			.ok_or_else(|| Error::internal(format!("unknown VatCode in database: {s}")))
	}
}

/// How NAV wants a code reported. The exemption and out-of-scope forms carry the code
/// itself as NAV's `case`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VatClass {
	/// `vatPercentage`, as basis points.
	Percentage(i64),
	/// `vatExemption`, case = [`VatCode::as_str`].
	Exemption,
	/// `vatOutOfScope`, case = [`VatCode::as_str`].
	OutOfScope,
}

impl VatCode {
	pub const ALL: [Self; 8] = [
		Self::Std27,
		Self::Red18,
		Self::Red05,
		Self::Aam,
		Self::Tam,
		Self::Eufad37,
		Self::Ho,
		Self::Atk,
	];

	/// The value stored in `vat_code` and sent to NAV as the exemption or out-of-scope case.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Std27 => "STD27",
			Self::Red18 => "RED18",
			Self::Red05 => "RED05",
			Self::Aam => "AAM",
			Self::Tam => "TAM",
			Self::Eufad37 => "EUFAD37",
			Self::Ho => "HO",
			Self::Atk => "ATK",
		}
	}

	/// The rate in basis points. Everything that is not a percentage rate is zero-rated,
	/// but the codes stay distinct — see [`compute`].
	pub fn rate_bp(self) -> i64 {
		match self {
			Self::Std27 => 2700,
			Self::Red18 => 1800,
			Self::Red05 => 500,
			Self::Aam | Self::Tam | Self::Eufad37 | Self::Ho | Self::Atk => 0,
		}
	}

	pub fn nav_class(self) -> VatClass {
		match self {
			Self::Std27 | Self::Red18 | Self::Red05 => VatClass::Percentage(self.rate_bp()),
			Self::Aam | Self::Tam => VatClass::Exemption,
			Self::Eufad37 | Self::Ho | Self::Atk => VatClass::OutOfScope,
		}
	}

	/// The statutory text NAV requires in `vatExemption/reason` / `vatOutOfScope/reason`.
	/// `None` for the three percentage codes, which carry `vatPercentage` instead.
	///
	/// Keyed on the code and not on `invoices.vat_note`: that is the PDF's newline-joined
	/// list of i18n keys, while `reason` is one element per code. Kept byte-identical to
	/// `templates/invoice/strings.typ`.
	pub fn nav_reason(self) -> Option<&'static str> {
		match self {
			Self::Std27 | Self::Red18 | Self::Red05 => None,
			Self::Aam => {
				Some("Alanyi adómentes — a számla áfát nem tartalmaz (Áfa tv. 187–196. §).")
			}
			Self::Tam => Some("Tárgyi adómentes ügylet (Áfa tv. 85–86. §)."),
			Self::Eufad37 => {
				Some("Az áfa fizetésére a vevő kötelezett — fordított adózás (Áfa tv. 37. §).")
			}
			Self::Ho => {
				Some("Területi hatályon kívüli — az ügylet nem tartozik a magyar áfa hatálya alá.")
			}
			Self::Atk => Some("Az áfa tárgyi hatályán kívüli ügylet."),
		}
	}

	/// The i18n key of this code's legal note, for the PDF. Mirrors [`Self::nav_reason`].
	pub fn note_key(self) -> Option<&'static str> {
		match self {
			Self::Std27 | Self::Red18 | Self::Red05 => None,
			Self::Aam => Some("vat.aam"),
			Self::Tam => Some("vat.tam"),
			Self::Eufad37 => Some("vat.eufad37"),
			Self::Ho => Some("vat.ho"),
			Self::Atk => Some("vat.atk"),
		}
	}
}

/// One line's resolved amounts. `vat` and `gross` are informational: the authoritative
/// figures are the [`VatGroup`] ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComputedLine {
	/// The resolved absolute discount, line-level plus this line's share of any
	/// invoice-level one.
	pub discount_amount: Money,
	pub net: Money,
	pub vat: Money,
	pub gross: Money,
}

/// The authoritative per-rate-group figures — one row of `invoice_vat_groups`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VatGroup {
	pub vat_code: VatCode,
	pub vat_rate_bp: i64,
	pub net: Money,
	pub vat: Money,
	pub gross: Money,
}

/// A fully priced invoice: the lines in input order, the groups in first-appearance order,
/// and the totals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputedInvoice {
	pub lines: Vec<ComputedLine>,
	pub groups: Vec<VatGroup>,
	pub net: Money,
	pub vat: Money,
	pub gross: Money,
}

/// Price a draft: line nets, an optional invoice-level discount apportioned across them,
/// the VAT groups, and the totals.
///
/// `invoice.net` is the sum of the *group* nets and `invoice.vat` the sum of the group
/// VATs, so the totals and `invoice_vat_groups` agree by construction.
///
/// `vat_round_step` is the currency's `price_round_step` — 100 (whole forints) on HUF, 1
/// elsewhere, where it is a no-op. It is the step the **group VAT** is rounded to, per group and
/// after the summed net, so `net + vat = gross` stays exact and NAV's `summaryByVatRate` still
/// cross-validates. The *net* passes through no step, so a HUF `gross` may still carry fillér.
pub fn compute(
	lines: &[DraftLine],
	invoice_discount: Option<Discount>,
	vat_round_step: i64,
) -> ClResult<ComputedInvoice> {
	// Guards before arithmetic: `compute`'s unchecked `full - discount_amount` turns an
	// out-of-range intermediate into a panic under release `overflow-checks`. This is the single
	// funnel every draft, issue and re-price path reaches, and the **only** guard for these
	// three — `routes`, `draft::price` and `money::discount_of` deliberately do not repeat them.
	for line in lines {
		// `E-INV-LINE`, the code registered for exactly these two. The discount envelope is
		// `bounded`'s own `E-CORE-VALIDATION`.
		let bad = |msg: &'static str| {
			Err(Error::coded(saas_core::error::StatusCode::BAD_REQUEST, "E-INV-LINE", msg))
		};
		if line.qty.0 <= 0 {
			return bad("a line quantity is a positive figure");
		}
		if line.unit_price.0 < 0 {
			return bad("a unit price is a non-negative figure");
		}
		if let Some(Discount::Amount(amount)) = line.discount {
			// `bounded` tests `unsigned_abs()`, so a negative amount passes it and `net = full -
			// discount_amount` *raises* the net, past `draft::price`'s `net < 0` check too. With
			// the sign check only at the HTTP boundary this filed a negative `discountValue` to
			// NAV. A storno is unaffected — it builds from the stored groups, not through here.
			if amount.0 < 0 {
				return bad("a line discount is a non-negative figure");
			}
			bounded(amount.0)?;
		}
	}

	let mut computed = Vec::with_capacity(lines.len());
	for line in lines {
		let full = line.qty.times(line.unit_price)?;
		let discount_amount = match line.discount {
			Some(discount) => discount.resolve(full)?,
			None => Money::ZERO,
		};
		computed.push(ComputedLine {
			discount_amount,
			net: full - discount_amount,
			vat: Money::ZERO,
			gross: Money::ZERO,
		});
	}

	// An invoice-level discount is apportioned across the lines pro rata by net, so every
	// rate group keeps its share of it rather than the first group absorbing the lot.
	if let Some(discount) = invoice_discount {
		if let Discount::Amount(amount) = discount {
			// Same reasoning as the per-line check above: negative is a raise, not a discount.
			if amount.0 < 0 {
				return Err(Error::coded(
					saas_core::error::StatusCode::BAD_REQUEST,
					"E-INV-LINE",
					"an invoice discount is a non-negative figure",
				));
			}
			bounded(amount.0)?;
		}
		let nets: Vec<Money> = computed.iter().map(|line| line.net).collect();
		let total = discount.resolve(sum_bounded(nets.iter().copied())?)?;
		for (line, share) in computed.iter_mut().zip(apportion(total, &nets)?) {
			line.discount_amount += share;
			line.net -= share;
		}
	}

	// Group by `vat_code`, not by rate: AAM and TAM are both 0 bp and NAV needs them as
	// separate rows.
	// Linear scan, but there are at most eight groups — a map earns nothing.
	let mut groups: Vec<VatGroup> = Vec::new();
	let mut group_of = Vec::with_capacity(lines.len());
	for (line, amounts) in lines.iter().zip(&computed) {
		let index = if let Some(index) = groups.iter().position(|g| g.vat_code == line.vat_code) {
			index
		} else {
			groups.push(VatGroup {
				vat_code: line.vat_code,
				vat_rate_bp: line.vat_code.rate_bp(),
				net: Money::ZERO,
				vat: Money::ZERO,
				gross: Money::ZERO,
			});
			groups.len() - 1
		};
		groups[index].net = sum_bounded([groups[index].net, amounts.net])?;
		group_of.push(index);
	}

	for group in &mut groups {
		// Rounded to the currency's display step, whole forints on HUF. This is practice, not
		// statute: the fillér is not a payable unit, so Hungarian invoicing expresses VAT in
		// whole forints, and no Áfa tv. section or NAV guidance prescribes where to round
		// (asked in Online-Invoice #738 and #1176, never answered). Per *group*, after the
		// summed net, so `net + vat = gross` stays exact and `summaryByVatRate` still
		// cross-validates — a per-line round is what forces the "kerekítési különbözet" line
		// this codebase does not have.
		//
		// **Open**: `vat_huf` on a foreign-currency invoice — the figure the buyer deducts —
		// gets no step at all (`currency::to_base`). If "no fillér on VAT" is the rationale it
		// arguably wants the same treatment; leaving it unrounded is the conservative choice,
		// because `gross_huf` is derived from `net_huf + vat_huf` and stepping one of three
		// independently breaks that sum.
		let exact = group.net.mul_bp(group.vat_rate_bp)?;
		group.vat = crate::currency::round_to_step(exact, vat_round_step)?;
		group.gross = sum_bounded([group.net, group.vat])?;
	}

	// Push the group VAT back onto the lines for display, pro rata by net, so the lines of
	// a group add up to the group exactly.
	for (index, group) in groups.iter().enumerate() {
		let members: Vec<usize> = group_of
			.iter()
			.enumerate()
			.filter_map(|(line, &g)| (g == index).then_some(line))
			.collect();
		let nets: Vec<Money> = members.iter().map(|&line| computed[line].net).collect();
		for (&line, vat) in members.iter().zip(apportion(group.vat, &nets)?) {
			computed[line].vat = vat;
			computed[line].gross = computed[line].net + vat;
		}
	}

	let net = sum_bounded(groups.iter().map(|g| g.net))?;
	let vat = sum_bounded(groups.iter().map(|g| g.vat))?;
	let gross = sum_bounded([net, vat])?;
	Ok(ComputedInvoice { lines: computed, groups, net, vat, gross })
}

#[cfg(test)]
mod tests {
	use saas_core::prelude::Qty;

	use super::*;

	fn line(qty: i64, unit_price: i64, vat_code: VatCode, discount: Option<Discount>) -> DraftLine {
		DraftLine {
			service_id: None,
			description: "test".to_owned(),
			unit: "db".to_owned(),
			qty: Qty(qty),
			unit_price: Money(unit_price),
			vat_code,
			discount,
			discount_description: None,
			note: None,
		}
	}

	/// The property the whole design exists for: the groups and the totals agree exactly,
	/// and the lines of a group add up to that group.
	fn assert_consistent(invoice: &ComputedInvoice) {
		assert_eq!(invoice.groups.iter().map(|g| g.net).sum::<Money>(), invoice.net);
		assert_eq!(invoice.groups.iter().map(|g| g.vat).sum::<Money>(), invoice.vat);
		assert_eq!(invoice.net + invoice.vat, invoice.gross);
		for group in &invoice.groups {
			assert_eq!(group.net + group.vat, group.gross);
		}
		assert_eq!(invoice.lines.iter().map(|l| l.net).sum::<Money>(), invoice.net);
		assert_eq!(invoice.lines.iter().map(|l| l.vat).sum::<Money>(), invoice.vat);
	}

	#[test]
	fn rounds_at_the_boundary_and_never_per_line() {
		// 2.5 × 3.33 = 8.325 → 8.33, then 27% of 8.33 = 2.2491 → 2.25.
		let invoice = compute(&[line(2_500_000, 333, VatCode::Std27, None)], None, 1).unwrap();
		assert_eq!(invoice.net, Money(833));
		assert_eq!(invoice.vat, Money(225));
		assert_consistent(&invoice);

		// Two lines whose individual VATs each round up, but whose group is computed once:
		// 0.05 + 0.05 rounds to 0.06 on the summed net, not to 0.02 + 0.02.
		let invoice = compute(
			&[
				line(1_000_000, 9, VatCode::Std27, None),
				line(1_000_000, 9, VatCode::Std27, None),
			],
			None,
			1,
		)
		.unwrap();
		assert_eq!(invoice.net, Money(18));
		assert_eq!(invoice.vat, Money(5));
		assert_consistent(&invoice);
	}

	/// Hungarian invoicing practice, not a statute: the fillér is not a payable unit, so a HUF
	/// group's VAT carries none. Per group and after the summed net, so `net + vat = gross`
	/// stays exact and NAV's `summaryByVatRate` still cross-validates.
	#[test]
	fn a_huf_group_rounds_its_vat_to_whole_forints() {
		// 4 990 Ft net at 27% is 1 347.30 Ft exactly — the gross no card gateway could take.
		let invoice =
			compute(&[line(1_000_000, 499_000, VatCode::Std27, None)], None, 100).unwrap();
		assert_eq!(invoice.net, Money(499_000));
		assert_eq!(invoice.vat, Money(134_700), "1 347.30 -> 1 347 Ft");
		assert_eq!(invoice.gross, Money(633_700));
		assert_eq!(invoice.gross.0 % 100, 0, "payable in whole forints");
		assert_consistent(&invoice);

		// Half-up, as the Hungarian rounding statute does: 1 347.50 goes to 1 348, not down
		// and not always up.
		let invoice =
			compute(&[line(1_000_000, 500_000, VatCode::Std27, None)], None, 100).unwrap();
		assert_eq!(invoice.vat, Money(135_000));
		assert_consistent(&invoice);
	}

	/// A one-minor-unit step is every non-HUF currency, and must leave the arithmetic alone.
	#[test]
	fn a_minor_unit_step_changes_nothing() {
		let stepped = compute(&[line(2_500_000, 333, VatCode::Std27, None)], None, 1).unwrap();
		assert_eq!(stepped.vat, Money(225));
	}

	#[test]
	fn groups_by_code_not_by_rate() {
		let invoice = compute(
			&[
				line(1_000_000, 10_000, VatCode::Std27, None),
				line(3_000_000, 333, VatCode::Red05, None),
				line(1_000_000, 5_000, VatCode::Aam, None),
				line(1_000_000, 2_000, VatCode::Tam, None),
			],
			None,
			1,
		)
		.unwrap();

		// AAM and TAM are both zero-rated but stay apart.
		assert_eq!(invoice.groups.len(), 4);
		assert_eq!(
			invoice.groups[0],
			VatGroup {
				vat_code: VatCode::Std27,
				vat_rate_bp: 2700,
				net: Money(10_000),
				vat: Money(2_700),
				gross: Money(12_700),
			}
		);
		// 3 × 3.33 = 9.99, 5% of which is 0.4995 → 0.50.
		assert_eq!(invoice.groups[1].net, Money(999));
		assert_eq!(invoice.groups[1].vat, Money(50));
		assert_eq!(invoice.groups[2].vat_code, VatCode::Aam);
		assert_eq!(invoice.groups[3].vat_code, VatCode::Tam);
		assert_consistent(&invoice);
	}

	#[test]
	fn line_discounts_of_both_kinds() {
		let invoice = compute(
			&[
				line(2_000_000, 10_000, VatCode::Std27, Some(Discount::Percent(1000))),
				line(1_000_000, 10_000, VatCode::Std27, Some(Discount::Amount(Money(2_500)))),
			],
			None,
			1,
		)
		.unwrap();
		assert_eq!(invoice.lines[0].discount_amount, Money(2_000));
		assert_eq!(invoice.lines[0].net, Money(18_000));
		assert_eq!(invoice.lines[1].discount_amount, Money(2_500));
		assert_eq!(invoice.lines[1].net, Money(7_500));
		assert_eq!(invoice.net, Money(25_500));
		assert_consistent(&invoice);
	}

	#[test]
	fn invoice_discount_leaves_each_group_its_share() {
		let invoice = compute(
			&[
				line(1_000_000, 10_000, VatCode::Std27, None),
				line(3_000_000, 333, VatCode::Red05, None),
			],
			Some(Discount::Percent(1000)),
			1,
		)
		.unwrap();

		// 10% of 109.99 is 10.999 → 11.00, split 10.00 / 1.00 pro rata by net.
		assert_eq!(invoice.lines[0].discount_amount, Money(1_000));
		assert_eq!(invoice.lines[1].discount_amount, Money(100));
		assert_eq!(invoice.groups[0].net, Money(9_000));
		assert_eq!(invoice.groups[0].vat, Money(2_430));
		assert_eq!(invoice.groups[1].net, Money(899));
		assert_eq!(invoice.groups[1].vat, Money(45));
		assert_eq!(invoice.net, Money(9_899));
		assert_eq!(invoice.vat, Money(2_475));
		assert_consistent(&invoice);
	}

	/// A line whose `qty × unit_price` lands near `i64::MAX` is a validation failure, not a
	/// panic: `overflow-checks` is on in release, and while `saas_core::app`'s `CatchPanicLayer`
	/// would turn one into a `500 E-CORE-INTERNAL`, an out-of-range amount is a `400` — and the
	/// layer does not reach a panic inside `spawn_blocking` or a job-runner task, which is where
	/// `compute` also runs.
	#[test]
	fn an_amount_past_the_envelope_is_rejected_not_panicked() {
		// 1e9 units at 8e7 each: both pass `Qty::parse`/`Money::parse`, the product is 8e18
		// and `group.net + group.vat` would be 1.016e19.
		let huge = line(1_000_000_000_000_000, 8_000_000_000, VatCode::Std27, None);
		assert!(compute(&[huge], None, 1).is_err());

		// Lines each inside the envelope (6e14) whose `+=` accumulation is not.
		let part = line(1_000_000_000_000_000, 600_000, VatCode::Std27, None);
		assert!(compute(std::slice::from_ref(&part), None, 1).is_ok());
		assert!(compute(&[part.clone(), part.clone(), part], None, 1).is_err());
	}

	/// `nav_reason` is the mandatory `vatExemption/reason`, so a code that is not a
	/// percentage must have one — otherwise `saas_nav::xml` cannot emit the invoice and it
	/// sits in `unfiled_invoices` forever. This keeps the two matches in step when a code
	/// is added.
	#[test]
	fn every_zero_rated_code_has_a_reason_and_a_note_key() {
		// The three classes a code can land in, and the tag NAV reads it back by.
		assert_eq!(VatCode::Std27.nav_class(), VatClass::Percentage(2700));
		assert_eq!(VatCode::Aam.nav_class(), VatClass::Exemption);
		assert_eq!(VatCode::Eufad37.nav_class(), VatClass::OutOfScope);
		assert_eq!(VatCode::Eufad37.as_str(), "EUFAD37");

		for code in VatCode::ALL {
			let percentage = matches!(code.nav_class(), VatClass::Percentage(_));
			assert_eq!(code.nav_reason().is_some(), !percentage, "{}", code.as_str());
			assert_eq!(code.note_key().is_some(), !percentage, "{}", code.as_str());
		}
		assert!(VatCode::Aam.nav_reason().unwrap().contains("Alanyi adómentes"));
		assert_eq!(VatCode::Aam.note_key(), Some("vat.aam"));
	}

	/// The guards run *before* the arithmetic, so an out-of-envelope input is a `400` and
	/// never the release-profile panic `overflow-checks = true` deliberately produces.
	#[test]
	fn hostile_lines_are_rejected_not_computed() {
		let code = |r: ClResult<ComputedInvoice>| r.unwrap_err().parts().1;

		let huge = line(1_000_000, 100, VatCode::Std27, Some(Discount::Amount(Money(i64::MAX))));
		assert_eq!(code(compute(&[huge], None, 1)), "E-CORE-VALIDATION");

		let negative_price = line(1_000_000, -1, VatCode::Std27, None);
		assert_eq!(code(compute(&[negative_price], None, 1)), "E-INV-LINE");

		let zero_qty = line(0, 100, VatCode::Std27, None);
		assert_eq!(code(compute(&[zero_qty], None, 1)), "E-INV-LINE");

		let ok = line(1_000_000, 100, VatCode::Std27, None);
		let invoice_discount = Some(Discount::Amount(Money(i64::MAX)));
		assert_eq!(
			code(compute(std::slice::from_ref(&ok), invoice_discount, 1)),
			"E-CORE-VALIDATION"
		);

		// A *negative* amount discount is a raise, which `bounded`'s `unsigned_abs` waves through
		// and the downstream `net < 0` check passes. `issue_now` and any `PricingHook` reach
		// here with no router in the loop.
		let raised = line(1_000_000, 10_000, VatCode::Std27, Some(Discount::Amount(Money(-5000))));
		assert_eq!(code(compute(&[raised], None, 1)), "E-INV-LINE");
		assert_eq!(code(compute(&[ok], Some(Discount::Amount(Money(-5000))), 1)), "E-INV-LINE");
	}

	/// The group *net* passes through no step and `gross = net + vat`, so a discounted HUF line
	/// leaves fillér in the gross. `allocate::start` charges the step-rounded remainder and
	/// `outstanding_allows` tolerates one step, which is what makes that payable — the tolerance
	/// is permanent, not legacy.
	#[test]
	fn a_discounted_huf_group_keeps_filler_in_the_gross() {
		let c = compute(
			&[line(1_000_000, 1_000, VatCode::Std27, Some(Discount::Percent(1500)))],
			None,
			100,
		)
		.unwrap();
		assert_eq!(c.net, Money(850));
		assert_eq!(c.vat, Money(200), "230 stepped to whole forints");
		assert_eq!(c.gross, Money(1_050));
		assert_ne!(c.gross.0 % 100, 0, "a HUF gross is not whole-forint");
	}
}

// vim: ts=4
