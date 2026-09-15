//! The issue transaction's argument, and everything that has to be settled before it.
//!
//! The transaction itself belongs to `InvoiceStore::issue`: it allocates the number from
//! `doc_series`, writes the VAT groups and flips `DRAFT` -> `ISSUED` in one write, so a
//! rollback consumes no number. This module decides the buyer's VAT treatment, resolves the
//! rates on the **fulfilment date** (Áfa tv. 80. §, not the issue date), re-prices, freezes
//! the buyer snapshot, and enqueues the two follow-up jobs once the row is committed.

use axum::http::StatusCode;
use saas_core::app::App;
use saas_core::job;
use saas_core::prelude::*;

use crate::currency::{self};
use crate::draft;
use crate::numbering;
use crate::store::{
	BillingParty, BuyerSnapshot, Invoice, InvoiceStatus, InvoiceStore, IssueInvoice, PartyKind,
	RateSource, Seller,
};
use crate::taxrule::{BuyerProfile, BuyerZone, Verdict, determine};
use crate::vies;

pub const KIND_RENDER_PDF: &str = "RENDER_PDF";
pub const KIND_NAV_REPORT: &str = "NAV_REPORT";

/// The payload every invoice-keyed job carries. `job_cancel`/`job_redrive` address rows by
/// `payload = ?` string equality, so this spelling is a contract with six other call sites
/// and with `InvoiceStore`'s `j.payload = '{"invoiceId":' || i.id || '}'` join.
#[must_use]
pub fn invoice_job_payload(invoice_id: i64) -> String {
	format!(r#"{{"invoiceId":{invoice_id}}}"#)
}

fn coded(code: &'static str, msg: &'static str) -> Error {
	Error::coded(StatusCode::CONFLICT, code, msg)
}

/// The buyer's VAT profile. A VIES outage is propagated, never swallowed: falling through to
/// `has_eu_vat: false` here would be safe, but silently charging domestic VAT on an invoice
/// the customer expects under reverse charge is not the caller's expectation.
///
/// The whole [`vies::ViesResult`] comes back, not just `.valid`: its consultation number is
/// the evidence for the reverse charge and has to be frozen onto the invoice by [`snapshot`].
pub async fn profile(
	app: &App,
	seller: &Seller,
	party: &BillingParty,
) -> ClResult<(BuyerProfile, Option<vies::ViesResult>)> {
	let zone = BuyerZone::of(&seller.country, &party.country);
	let is_company = party.kind == PartyKind::Company;
	let checked = match (&party.eu_vat_id, zone, is_company) {
		(Some(id), BuyerZone::Eu, true) => {
			Some(vies::check(app, id, seller.eu_vat_id.as_deref()).await?)
		}
		_ => None,
	};
	let has_eu_vat = checked.as_ref().is_some_and(|v| v.valid);
	Ok((BuyerProfile { zone, is_company, has_eu_vat }, checked))
}

/// The digits NAV reads out of a tax number. `base:TaxNumberType` splits the figure into
/// `base:taxpayerId` (the first 8), `base:vatCode` (1) and `base:countyCode` (2), so the
/// dashes a caller types are not part of it — counting characters is the wrong rule, here and
/// in [`crate::service_api`]'s `groupTaxNo` check, which is the other place this is asked.
///
/// The primitive only. A buyer's `tax_number` must additionally be exactly 8, 9 or 11 digits
/// ([`snapshot`], and only for a domestic buyer, since a foreign company's number is filed as
/// `communityVatNumber`); a group tax number is checked for a minimum. Both figures reach
/// `base:vatCode`, so both also carry [`vat_code_ok`].
pub fn tax_digits(s: &str) -> String {
	s.chars().filter(char::is_ascii_digit).collect()
}

/// `Ok` unless `digits`' 9th digit is one NAV's schema refuses.
///
/// `base:TaxNumberType` splits digit 9 out as `base:vatCode`, which `common.xsd`'s
/// `VatCodeType` restricts to `[1-5]{1}`. Checked wherever a value that reaches
/// `saas_nav::xml::Xml::tax_number` is *written*, never only at filing time: an invoice is
/// immutable once issued, so a bad code caught at filing can never be corrected.
#[must_use]
pub fn vat_code_ok(digits: &str) -> bool {
	digits.as_bytes().get(8).is_none_or(|c| (b'1'..=b'5').contains(c))
}

/// The buyer as frozen onto the invoice, or `E-INV-BUYER-INCOMPLETE` when the party is
/// missing something the snapshot needs. A company must carry a tax number.
///
/// `vies` is the verdict [`profile`] obtained, and only a *valid* one is frozen — an invalid
/// check justified nothing.
pub fn snapshot(party: &BillingParty, vies: Option<&vies::ViesResult>) -> ClResult<BuyerSnapshot> {
	let incomplete = || coded("E-INV-BUYER-INCOMPLETE", "the billing party is incomplete");
	if party.name.trim().is_empty() || party.country.trim().is_empty() {
		return Err(incomplete());
	}
	if party.kind == PartyKind::Company {
		let Some(tax_number) = party.tax_number.as_deref() else {
			return Err(incomplete());
		};
		// Presence is not enough for a foreign buyer either: a whitespace-only number is
		// filed as `thirdStateTaxId`, a `SimpleText50NotBlankType`, and rejected forever on
		// an invoice that already has a number. The digit rule below covers only `HU`.
		if tax_number.trim().is_empty() {
			return Err(coded("E-INV-BUYER-TAXNUMBER", "a company needs a tax number"));
		}
		// Presence was checked, shape was not, and a malformed number fails NAV on a numbered,
		// immutable invoice. `saas_nav::xml::customer_info` splits it into `base:taxpayerId` (8)
		// + `base:vatCode` (1, `[1-5]`) + `base:countyCode` (2) — restated, not imported, since
		// dependencies point inward. Domestic buyers only: a foreign number is a
		// `communityVatNumber`.
		if party.country.trim().eq_ignore_ascii_case("HU") {
			let digits = tax_digits(tax_number);
			if !matches!(digits.len(), 8 | 9 | 11) {
				return Err(coded(
					"E-INV-BUYER-TAXNUMBER",
					"a Hungarian tax number has 8, 9 or 11 digits",
				));
			}
			if !vat_code_ok(&digits) {
				return Err(coded(
					"E-INV-BUYER-TAXNUMBER",
					"the 9th digit of a Hungarian tax number must be 1-5",
				));
			}
		}
	}
	// `saas_nav::xml::customer_info` requires `customerAddress` for every buyer that is not a
	// `PRIVATE_PERSON`, so a company without one issues a legally binding invoice NAV will never
	// accept. Refused here, where the buyer is still correctable and no number is allocated.
	// Restated, not imported: dependencies point inward only.
	if party.kind != PartyKind::Person {
		let blank = |f: &Option<String>| f.as_ref().is_none_or(|v| v.trim().is_empty());
		if blank(&party.postcode) || blank(&party.city) || blank(&party.street) {
			return Err(coded(
				"E-INV-BUYER-ADDRESS",
				"a non-private buyer needs a postcode, city and street",
			));
		}
	}
	let valid = vies.filter(|v| v.valid);
	Ok(BuyerSnapshot {
		kind: party.kind,
		name: party.name.clone(),
		country: party.country.clone(),
		tax_number: party.tax_number.clone(),
		eu_vat_id: party.eu_vat_id.clone(),
		group_tax_no: party.group_tax_no.clone(),
		postcode: party.postcode.clone(),
		city: party.city.clone(),
		street: party.street.clone(),
		vies_request_id: valid.and_then(|v| v.request_id.clone()),
		vies_checked_at: valid.map(|v| v.checked_at),
	})
}

/// Every applicable Áfa tv. 169. § note key, newline-joined into `invoices.vat_note`.
///
/// **Every**, not the first. `Verdict::Product` has no note of its own, but a product-level
/// AAM/TAM/ATK line still needs its statutory note printed — and an invoice carrying one AAM
/// group and one TAM group owes a reference for each. Taking only the first left the second
/// exempt supply unexplained on a numbered, immutable document, while `saas_nav::xml` filed a
/// correct per-code `reason` for both.
///
/// The verdict's own key leads (it is invoice-level), the rest follow in group order,
/// deduplicated. `taxrule::vat_notes` is the inverse; nothing but the PDF and the wire view
/// reads the column.
fn vat_note(verdict: &Verdict, groups: &[crate::store::InvoiceVatGroup]) -> Option<String> {
	let mut notes: Vec<&'static str> = Vec::new();
	let group_keys = groups.iter().filter_map(|g| g.vat_code.note_key());
	for key in verdict.note_key().into_iter().chain(group_keys) {
		if !notes.contains(&key) {
			notes.push(key);
		}
	}
	(!notes.is_empty()).then(|| notes.join("\n"))
}

/// Assemble everything the issue transaction writes, for an invoice whose lines are already
/// priced as `lines`.
///
/// `fulfilment_date` defaults to today, `due_date` to it plus
/// `settings['invoice.default_payment_days']`. The HUF rate is resolved on the fulfilment
/// date for every non-HUF invoice regardless of the base currency, because a Hungarian
/// seller invoicing in EUR must report `exchangeRate` and HUF amounts to NAV.
#[allow(clippy::too_many_arguments)]
pub async fn plan(
	app: &App,
	store: &dyn InvoiceStore,
	seller: &Seller,
	invoice: &Invoice,
	party: &BillingParty,
	verdict: &Verdict,
	vies: Option<&vies::ViesResult>,
	lines: &[crate::money::DraftLine],
	issued_at: Timestamp,
) -> ClResult<IssueInvoice> {
	let fulfilment_date = match &invoice.fulfilment_date {
		Some(date) => date.clone(),
		None => numbering::date_of(issued_at)?,
	};
	let due_date = if let Some(date) = &invoice.due_date {
		Some(date.clone())
	} else {
		let days = app.settings.int("invoice.default_payment_days").await?;
		Some(numbering::add_days(&fulfilment_date, days)?)
	};

	let source = app.settings.text("currency.rate_source").await?;
	// Not a `_ => Bank` fallback: `MANUAL` is a real variant, and the fallback swallowed it —
	// along with any typo — onto an immutable row saying which rate the seller may legally use.
	// An unknown tag is `Error::internal`, so a misconfigured setting fails loudly at issue.
	let rate_source: RateSource = source.parse()?;
	// Through `effective_rate_e6`, not `rate_on`: `rate_on` demands a published row, so a
	// `mode = 'FIXED'` currency could be drafted and never issued. This `if` is also what
	// enforces Áfa tv. 172. §, which nothing in the database backs up — every non-HUF currency
	// takes the `else` branch and resolves a rate or fails loudly. A construction, not a check.
	let configured = CurrencyCode::parse(&app.settings.text("currency.base").await?)?;
	let max_age = app.settings.int("currency.max_rate_age_days").await?;
	let cur = if invoice.currency == "HUF" && invoice.currency == configured {
		None
	} else {
		Some(currency::get(store, &invoice.currency).await?)
	};
	let huf_rate_e6 = match &cur {
		Some(cur) if invoice.currency != "HUF" => Some(
			currency::effective_rate_e6(
				store,
				cur,
				&CurrencyCode::huf(),
				&configured,
				&source,
				&fulfilment_date,
				max_age,
			)
			.await?,
		),
		_ => None,
	};
	// `rate_e6` — the pricing rate — is **not** re-resolved here: the lines keep their stored
	// `unit_price`, so moving it under them left the row claiming a rate its prices were never
	// computed at. `huf_rate_e6` is the statutory one and does move to the fulfilment date
	// (Áfa tv. 172. §), which is what `rate_date` dates.

	// The invoice-level discount comes off the row, not off the request: re-pricing at ISSUE
	// with `None` here billed the customer more than the draft showed.
	let priced = draft::price(
		invoice.id,
		lines,
		crate::money::discount_of(invoice.discount_kind, invoice.discount_value)?,
		verdict,
		huf_rate_e6,
		true,
	)?;
	let (series_code, series_year) = numbering::series_for(seller, issued_at)?;

	Ok(IssueInvoice {
		series_code,
		series_year,
		issued_at,
		fulfilment_date: fulfilment_date.clone(),
		due_date,
		rate_date: Some(fulfilment_date),
		rate_source: Some(rate_source),
		huf_rate_e6,
		// `freeze` writes `COALESCE(?, rate_e6)`, so `None` keeps the draft's frozen value.
		rate_e6: None,
		net: priced.net,
		vat: priced.vat,
		gross: priced.gross,
		vat_note: vat_note(verdict, &priced.groups),
		buyer: snapshot(party, vies)?,
		lines: priced.lines,
		groups: priced.groups,
	})
}

/// Issue a draft. Already-`ISSUED` returns it unchanged; a `STORNOED` row is immutable.
///
/// The VAT treatment is re-decided from what is stored, because the buyer may have changed
/// since the draft was built and the buyer is what decides it. The `PricingHook` is **not**
/// re-run: the stored lines already carry its output, and a hook is under no obligation to be
/// idempotent, so applying it to its own result doubled an appended line onto a numbered,
/// immutable, NAV-filed invoice. See [`crate::pricing::PricingHook`].
pub async fn run(app: &App, store: &dyn InvoiceStore, invoice: Invoice) -> ClResult<Invoice> {
	match invoice.status {
		InvoiceStatus::Draft => {}
		InvoiceStatus::Issued | InvoiceStatus::Paid => return Ok(invoice),
		InvoiceStatus::Stornoed => {
			return Err(coded("E-INV-IMMUTABLE", "the invoice has been cancelled"));
		}
	}

	// Before the join: this is not a query, and `E-INV-NO-BUYER` must still come back ahead
	// of any row that is merely missing.
	let party_id = invoice
		.billing_party_id
		.ok_or_else(|| coded("E-INV-NO-BUYER", "the draft has no billing party"))?;

	let (seller, party, stored) = tokio::try_join!(
		store.seller_by_id(invoice.seller_id),
		store.party_by_id(party_id),
		store.invoice_lines(invoice.id),
	)?;
	let seller = seller.ok_or(Error::NotFound)?;
	let party = party.ok_or(Error::NotFound)?;

	if stored.is_empty() {
		return Err(coded("E-INV-EMPTY", "the draft has no lines"));
	}
	let lines: Vec<crate::money::DraftLine> =
		stored.iter().map(draft::to_draft).collect::<ClResult<_>>()?;

	let (buyer, vies) = profile(app, &seller, &party).await?;
	let verdict = determine(&seller, &buyer);
	let plan = plan(
		app,
		store,
		&seller,
		&invoice,
		&party,
		&verdict,
		vies.as_ref(),
		&lines,
		Timestamp::now(),
	)
	.await?;
	// `invoice.version` is the version the lines above were read at: the VIES lookup in
	// `profile` can hold this path open for up to 15 s, and an edit landing in that window
	// must not be overwritten by the snapshot taken before it.
	let issued = store.issue(invoice.id, &plan, invoice.version).await?;

	enqueue_jobs(app, &issued).await;
	Ok(issued)
}

/// Queued after the row is committed, not inside the issue transaction: `job::enqueue` takes
/// a pool, not a transaction, and `InvoiceStore::issue` owns the only handle to that one.
/// Both keys are derived from the invoice id, so a retry re-queues nothing.
///
/// At-most-once rather than exactly-once. A crash between COMMIT and enqueue
/// leaves an issued invoice with no PDF and no NAV report; the operator re-queues it. Move
/// both into the store transaction if that window ever matters.
pub async fn enqueue_jobs(app: &App, invoice: &Invoice) {
	let payload = invoice_job_payload(invoice.id);
	for (kind, key) in [
		(KIND_RENDER_PDF, format!("pdf:invoice:{}", invoice.id)),
		(KIND_NAV_REPORT, format!("nav:invoice:{}", invoice.id)),
	] {
		match job::enqueue(&app.store, kind, &payload, Some(&key), Timestamp::now()).await {
			Err(e) => {
				tracing::error!(error = %e, kind, invoice = %invoice.uid.as_str(), "enqueue failed");
			}
			// A `dedup_key` is never released, so one taken at *issue* time means something spent
			// it before the invoice was issuable. Either way this invoice has no live job, and
			// only the reconciliation sweep would notice — say so here, where it happens.
			Ok(None) => {
				tracing::error!(
					kind,
					key,
					invoice = %invoice.uid.as_str(),
					"dedup key was already spent; this invoice has no job of this kind"
				);
			}
			Ok(Some(_)) => {}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::vat::VatCode;

	fn group(code: VatCode) -> crate::store::InvoiceVatGroup {
		crate::store::InvoiceVatGroup {
			invoice_id: 0,
			vat_code: code,
			vat_rate_bp: 0,
			net: Money(0),
			vat: Money(0),
			gross: Money(0),
			net_huf: None,
			vat_huf: None,
			gross_huf: None,
		}
	}

	#[test]
	fn a_mixed_exempt_invoice_carries_every_statutory_note() {
		// One AAM group and one TAM group: both Áfa tv. 169. § references are owed, in group
		// order. `find_map` used to print only `vat.aam`.
		let groups = [group(VatCode::Aam), group(VatCode::Tam), group(VatCode::Std27)];
		assert_eq!(vat_note(&Verdict::Product, &groups).as_deref(), Some("vat.aam\nvat.tam"));

		// The verdict's own key leads and is not repeated by a group carrying the same code.
		let ho = Verdict::Override(VatCode::Ho, VatCode::Ho.note_key());
		assert_eq!(
			vat_note(&ho, &[group(VatCode::Ho), group(VatCode::Aam)]).as_deref(),
			Some("vat.ho\nvat.aam")
		);

		// A fully-taxed invoice stores nothing, which is what `taxrule::vat_notes` reads back
		// as an empty list.
		assert_eq!(vat_note(&Verdict::Product, &[group(VatCode::Std27)]), None);
		assert!(crate::taxrule::vat_notes(None).is_empty());
		assert_eq!(crate::taxrule::vat_notes(Some("vat.aam\nvat.tam")), ["vat.aam", "vat.tam"]);
	}
}

// vim: ts=4
