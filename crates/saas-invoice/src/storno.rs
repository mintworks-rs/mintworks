//! Cancellation. Corrections are storno + reissue only: there is no MODIFY and no helyesbítő chain
//! in v1, and `invoices.modification_index` is the column that awaits one.
//!
//! The counter-invoice negates every monetary figure, keeps the original's frozen buyer
//! snapshot, currency and rate, and draws its own number from the same series — so the run
//! of numbers stays gapless across both documents. `InvoiceStore::storno` does all of that
//! in one transaction and flips the original to `STORNOED`.

use axum::http::StatusCode;
use saas_core::app::App;
use saas_core::prelude::*;

use crate::issue;
use crate::numbering;
use crate::store::{
	BuyerSnapshot, DiscountKind, Invoice, InvoiceKind, InvoiceStatus, InvoiceStore,
	InvoiceVatGroup, IssueInvoice, NewInvoice, NewInvoiceLine, Seller,
};

/// `Qty` has no `Neg`; the counter-invoice needs one for exactly one field.
fn neg_qty(q: Qty) -> Qty {
	Qty(-q.0)
}

/// The original's frozen buyer, read back off the invoice row. The party table is never
/// consulted: editing or deleting the party after issue must not change what the storno says.
fn frozen_buyer(inv: &Invoice) -> ClResult<BuyerSnapshot> {
	let missing = || Error::internal("issued invoice has no frozen buyer snapshot");
	Ok(BuyerSnapshot {
		kind: inv.buyer_kind.ok_or_else(missing)?,
		name: inv.buyer_name.clone().ok_or_else(missing)?,
		country: inv.buyer_country.clone().ok_or_else(missing)?,
		tax_number: inv.buyer_tax_number.clone(),
		eu_vat_id: inv.buyer_eu_vat_id.clone(),
		group_tax_no: inv.buyer_group_tax_no.clone(),
		postcode: inv.buyer_postcode.clone(),
		city: inv.buyer_city.clone(),
		street: inv.buyer_street.clone(),
		vies_request_id: inv.buyer_vies_request_id.clone(),
		vies_checked_at: inv.buyer_vies_checked_at,
	})
}

/// Cancel an issued invoice, returning the STORNO counter-invoice.
///
/// `idx_invoice_storno_once` is what makes a second attempt `E-INV-ALREADY-STORNOED`; the
/// store maps that unique violation, so there is deliberately no read-then-write check here.
pub async fn run(
	app: &App,
	store: &dyn InvoiceStore,
	original: &Invoice,
	reason: &str,
) -> ClResult<Invoice> {
	let conflict =
		|code: &'static str, msg: &'static str| Error::coded(StatusCode::CONFLICT, code, msg);
	if original.kind == InvoiceKind::Storno {
		return Err(conflict("E-INV-STORNO-OF-STORNO", "a storno cannot itself be cancelled"));
	}
	match original.status {
		InvoiceStatus::Issued | InvoiceStatus::Paid => {}
		InvoiceStatus::Draft => {
			return Err(conflict("E-INV-NOT-ISSUED", "the invoice has not been issued"));
		}
		InvoiceStatus::Stornoed => {
			return Err(conflict("E-INV-ALREADY-STORNOED", "the invoice is already cancelled"));
		}
	}

	let seller: Seller = store.seller_by_id(original.seller_id).await?.ok_or(Error::NotFound)?;
	// The **original's** frozen version, not the seller's current one: a cancellation must
	// carry the same supplier data as the invoice it cancels, for the same reason `series_code`
	// below is the original's.
	let seller_ver = original
		.seller_ver
		.ok_or_else(|| Error::internal("saas-invoice: an issued invoice has no seller_ver"))?;
	let lines: Vec<NewInvoiceLine> = store
		.invoice_lines(original.id)
		.await?
		.iter()
		.map(|l| NewInvoiceLine {
			service_id: l.service_id,
			description: l.description.clone(),
			unit: l.unit.clone(),
			// The *quantity* is negated, not the unit price: NAV checks
			// `lineNetAmount == quantity × unitPrice` on every `invoiceLine`, and negating the
			// nets with the quantity left positive broke that identity on every storno line.
			qty: neg_qty(l.qty),
			unit_price: l.unit_price,
			discount_kind: l.discount_kind,
			// `AMOUNT` is money and follows `discount_amount` negative; `PERCENT` is basis points
			// and sign-neutral, so it is copied as-is. Copying both verbatim left `LineView`
			// serving a +5.00 discount beside a −5.00 resolved one.
			discount_value: match l.discount_kind {
				Some(DiscountKind::Amount) => l.discount_value.map(|v| -v),
				_ => l.discount_value,
			},
			// Negative, so `net = qty × unit_price / 1e6 − discount_amount` still holds with every
			// component negative — which is why `invoice_lines` carries no `>= 0` check here.
			discount_amount: -l.discount_amount,
			discount_description: l.discount_description.clone(),
			net: -l.net,
			vat_code: l.vat_code,
			vat_rate_bp: l.vat_rate_bp,
			vat: -l.vat,
			gross: -l.gross,
			note: l.note.clone(),
		})
		.collect();
	let groups: Vec<InvoiceVatGroup> = store
		.invoice_vat_groups(original.id)
		.await?
		.iter()
		.map(|g| InvoiceVatGroup {
			invoice_id: 0, // assigned by the store: the counter-invoice does not exist yet
			vat_code: g.vat_code,
			vat_rate_bp: g.vat_rate_bp,
			net: -g.net,
			vat: -g.vat,
			gross: -g.gross,
			net_huf: g.net_huf.map(|m| -m),
			vat_huf: g.vat_huf.map(|m| -m),
			gross_huf: g.gross_huf.map(|m| -m),
		})
		.collect();

	// Áfa tv. 172. §: the passed-on VAT must appear in HUF on a foreign-currency invoice. This is
	// the only path that cannot satisfy it by construction — everywhere else `Some(rate)` implies
	// `Some(vat_huf)`, but a storno copies the trio off the original and inherits what it has.
	if original.currency != "HUF" && groups.iter().any(|g| g.vat_huf.is_none()) {
		return Err(Error::internal(
			"saas-invoice: a foreign-currency invoice was issued without vat_huf",
		));
	}

	let issued_at = Timestamp::now();
	// The **code** is the original's, not `sellers.series_code`: reading the seller's current one
	// landed every later storno in a different series from its original and broke the gapless run
	// both share. The **year** stays the Europe/Budapest year of the storno's own issue instant,
	// so a January storno for a December invoice is a January document.
	let series_code = original
		.series_code
		.clone()
		.ok_or_else(|| Error::internal("saas-invoice: an issued invoice has no series code"))?;
	let series_year = numbering::series_for(&seller, issued_at)?.1;
	let new = NewInvoice {
		tenant_id: original.tenant_id,
		seller_id: original.seller_id,
		billing_party_id: original.billing_party_id,
		request_id: None,
		kind: InvoiceKind::Storno,
		original_invoice_id: Some(original.id),
		currency: original.currency.clone(),
		rate_e6: original.rate_e6,
		payment_method: original.payment_method,
		notes: Some(reason.to_owned()),
		// No invoice-level discount: the counter-invoice is not re-priced, and every line
		// already carries its share of the original's negated.
		discount_kind: None,
		discount_value: None,
	};
	let plan = IssueInvoice {
		series_code,
		series_year,
		issued_at,
		// The fulfilment date and every rate stay the original's: a cancellation reports the
		// same chargeability moment, so NAV sees one reversal and not a second transaction.
		fulfilment_date: original.fulfilment_date.clone().unwrap_or(numbering::date_of(issued_at)?),
		due_date: original.due_date.clone(),
		rate_date: original.rate_date.clone(),
		rate_source: original.rate_source,
		huf_rate_e6: original.huf_rate_e6,
		rate_e6: Some(original.rate_e6),
		net: -original.net,
		vat: -original.vat,
		gross: -original.gross,
		vat_note: original.vat_note.clone(),
		buyer: frozen_buyer(original)?,
		seller_ver,
		lines,
		groups,
	};

	let storno = store.storno(original.id, &new, &plan).await?;
	issue::enqueue_jobs(app, &storno).await;
	Ok(storno)
}

// vim: ts=4
