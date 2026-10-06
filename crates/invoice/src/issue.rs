//! The issue transaction's argument, and everything that has to be settled before it.
//!
//! The transaction itself belongs to `InvoiceStore::issue`: it allocates the number from
//! `doc_series`, writes the VAT groups and flips `DRAFT` -> `ISSUED` in one write, so a
//! rollback consumes no number. This module decides the buyer's VAT treatment, resolves the
//! rates on the **fulfilment date** (Áfa tv. 80. §, not the issue date), re-prices, freezes
//! the buyer snapshot, and enqueues the two follow-up jobs once the row is committed.

use axum::http::StatusCode;
use mintworks_core::app::App;
use mintworks_core::job;
use mintworks_core::prelude::*;

use crate::currency::{self};
use crate::draft;
use crate::numbering;
use crate::store::{
	BillingParty, BuyerSnapshot, Invoice, InvoiceStatus, InvoiceStore, IssueInvoice, PartyKind,
	PaymentMethod, RateSource, Seller, SellerVersion,
};
use crate::taxrule::{BuyerProfile, BuyerZone, Verdict, determine};
use crate::vies;

pub const KIND_RENDER_PDF: &str = "RENDER_PDF";
pub const KIND_NAV_REPORT: &str = "NAV_REPORT";

/// Restates the `invoice.nav_report_delay_secs` registry default — a `SettingDef` default must be
/// a string literal, so the number cannot be shared with it. Same arrangement as
/// `job::DEFAULT_MAX_ATTEMPTS`.
pub const DEFAULT_NAV_REPORT_DELAY_SECS: i64 = 15;

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

/// `E-INV-SELLER-CLOSED` unless the seller is still open for new documents. A closed seller
/// only records payments; NAV filing of what it already issued is unaffected.
pub(crate) fn seller_open(seller: &Seller) -> ClResult<()> {
	match seller.closed_at {
		Some(_) => Err(coded("E-INV-SELLER-CLOSED", "this company is read-only")),
		None => Ok(()),
	}
}

/// The buyer's VAT profile. A VIES outage is propagated, never swallowed: falling through to
/// `has_eu_vat: false` here would be safe, but silently charging domestic VAT on an invoice
/// the customer expects under reverse charge is not the caller's expectation.
///
/// The whole [`vies::ViesResult`] comes back, not just `.valid`: its consultation number is
/// the evidence for the reverse charge and has to be frozen onto the invoice by [`snapshot`].
pub async fn profile(
	app: &App,
	seller: &SellerVersion,
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
/// `mintworks_nav::xml::Xml::tax_number` is *written*, never only at filing time: an invoice is
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
		// A malformed number fails NAV on a numbered, immutable invoice.
		// `mintworks_nav::xml::customer_info` splits it into `base:taxpayerId` (8) + `base:vatCode`
		// (1, `[1-5]`) + `base:countyCode` (2) — restated, not imported, since dependencies
		// point inward. Domestic buyers only: a foreign number is a `communityVatNumber`.
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
	// `mintworks_nav::xml::customer_info` requires `customerAddress` for every buyer that is not a
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
/// exempt supply unexplained on a numbered, immutable document, while `mintworks_nav::xml` filed a
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
/// `fulfilment_date` defaults to today, `due_date` to it plus the payment term
/// ([`default_due`]). A CARD or CASH invoice takes the issue date for both: `check_paid_dates`
/// refused any other date, and a CASH invoice is paid at issue. A settlement-period invoice
/// derives its fulfilment date per Áfa tv. 58. § ([`numbering::fulfilment_58`]) from the due
/// date, whose default then runs from the issue date instead. The HUF rate is resolved on the
/// fulfilment date — the issue date for a 58. § invoice (Áfa tv. 80. § (1) b)) — for every
/// non-HUF invoice regardless of the base currency, because a Hungarian seller invoicing in EUR
/// must report `exchangeRate` and HUF amounts to NAV.
#[allow(clippy::too_many_arguments)]
pub async fn plan(
	app: &App,
	store: &dyn InvoiceStore,
	seller: &Seller,
	version: &SellerVersion,
	invoice: &Invoice,
	party: &BillingParty,
	verdict: &Verdict,
	vies: Option<&vies::ViesResult>,
	lines: &[crate::money::DraftLine],
	issued_at: Timestamp,
) -> ClResult<IssueInvoice> {
	let issue_date = numbering::date_of(issued_at)?;
	let same_day = matches!(invoice.payment_method, PaymentMethod::Card | PaymentMethod::Cash);
	let stored_fulfilment = match &invoice.fulfilment_date {
		Some(date) if !same_day => date.clone(),
		_ => issue_date.clone(),
	};
	let (fulfilment_date, due_date) = match (&invoice.period_end, same_day) {
		(Some(end), true) => {
			(numbering::fulfilment_58(end, &issue_date, &issue_date)?, issue_date.clone())
		}
		(Some(end), false) => {
			let due = match &invoice.due_date {
				Some(date) => date.clone(),
				None => default_due(app, party, seller, &issue_date).await?,
			};
			(numbering::fulfilment_58(end, &issue_date, &due)?, due)
		}
		(None, true) => (issue_date.clone(), issue_date.clone()),
		(None, false) => {
			let due = match &invoice.due_date {
				Some(date) => date.clone(),
				None => default_due(app, party, seller, &stored_fulfilment).await?,
			};
			(stored_fulfilment, due)
		}
	};
	let due_date = Some(due_date);
	// Áfa tv. 80. § (1) b): a 58. § invoice converts at the rate valid when it is issued.
	let rate_date = if invoice.period_end.is_some() { issue_date } else { fulfilment_date.clone() };

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
				&rate_date,
				max_age,
			)
			.await?,
		),
		_ => None,
	};
	// `rate_e6` — the pricing rate — is **not** re-resolved here: the lines keep their stored
	// `unit_price`, so moving it under them left the row claiming a rate its prices were never
	// computed at. `huf_rate_e6` is the statutory one and does move to `rate_date`
	// (Áfa tv. 80. §, 172. §).

	// Read, never assumed: `price_round_step` is operator-writable, so a HUF row edited off 100
	// re-priced the invoice between draft and issue — the draft path passes the row's own
	// value. `currency_get`, not `currency::get`: the `None` above means HUF is the base
	// currency and may legitimately have no row, which `currency::get` raises
	// `E-INV-CURRENCY-DISABLED` for. 100 is only the no-row fallback.
	let vat_round_step = match &cur {
		Some(c) => c.price_round_step,
		None => store.currency_get("HUF").await?.map_or(100, |c| c.price_round_step),
	};

	// The invoice-level discount comes off the row, not off the request: re-pricing at ISSUE
	// with `None` here billed the customer more than the draft showed.
	let priced = draft::price(
		invoice.id,
		lines,
		crate::money::discount_of(invoice.discount_kind, invoice.discount_value)?,
		verdict,
		huf_rate_e6,
		true,
		vat_round_step,
	)?;
	let (series_code, series_year) = numbering::series_for(seller, issued_at)?;

	Ok(IssueInvoice {
		series_code,
		series_year,
		issued_at,
		fulfilment_date: fulfilment_date.clone(),
		due_date,
		rate_date: Some(rate_date),
		period_start: invoice.period_start.clone(),
		period_end: invoice.period_end.clone(),
		rate_source: Some(rate_source),
		huf_rate_e6,
		// `freeze` writes `COALESCE(?, rate_e6)`, so `None` keeps the draft's frozen value.
		rate_e6: None,
		net: priced.net,
		vat: priced.vat,
		gross: priced.gross,
		vat_note: vat_note(verdict, &priced.groups),
		buyer: snapshot(party, vies)?,
		seller_ver: version.seller_ver,
		lines: priced.lines,
		groups: priced.groups,
		paid: invoice.payment_method == PaymentMethod::Cash,
	})
}

/// `from` plus the payment term: the party's, else the seller's, else
/// `settings['invoice.default_payment_days']`.
async fn default_due(
	app: &App,
	party: &BillingParty,
	seller: &Seller,
	from: &str,
) -> ClResult<String> {
	let days = match party.payment_days.or(seller.payment_days) {
		Some(d) => d,
		None => app.settings.int("invoice.default_payment_days").await?,
	};
	numbering::add_days(from, days)
}

/// Issue a draft, locked (`PENDING`) or not. Already-`ISSUED` returns it unchanged; a
/// `STORNOED` row is immutable.
///
/// The VAT treatment is re-decided from what is stored, because the buyer may have changed
/// since the draft was built and the buyer is what decides it. The `PricingHook` is **not**
/// re-run: the stored lines already carry its output, and a hook is under no obligation to be
/// idempotent, so applying it to its own result doubled an appended line onto a numbered,
/// immutable, NAV-filed invoice. See [`crate::pricing::PricingHook`].
pub async fn run(app: &App, store: &dyn InvoiceStore, invoice: Invoice) -> ClResult<Invoice> {
	match invoice.status {
		// `Pending` issues like a draft: the lock froze the totals the gateway is charging, and
		// this is the transaction that allocates the number, exactly as it does from `Draft`.
		InvoiceStatus::Draft | InvoiceStatus::Pending => {}
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

	let (seller, version, party, stored) = tokio::try_join!(
		store.seller_by_id(invoice.seller_id),
		store.current_seller_version(invoice.seller_id),
		store.party_by_id(party_id),
		store.invoice_lines(invoice.id),
	)?;
	let seller = seller.ok_or(Error::NotFound)?;
	// The one choke point every issue path (service, `issue_now`, card payment, Rune) passes.
	seller_open(&seller)?;
	// Refused before a number is allocated, like `E-INV-BUYER-INCOMPLETE`. A DRAFT version
	// cannot leak in here: `current_seller_version` filters on status, so an invoice issued
	// mid-edit freezes the live version and never the half-typed one.
	let version = version
		.ok_or_else(|| coded("E-INV-SELLER-INCOMPLETE", "the seller has no published version"))?;
	let party = party.ok_or(Error::NotFound)?;

	if stored.is_empty() {
		return Err(coded("E-INV-EMPTY", "the draft has no lines"));
	}
	let lines: Vec<crate::money::DraftLine> =
		stored.iter().map(draft::to_draft).collect::<ClResult<_>>()?;

	let (buyer, vies) = profile(app, &version, &party).await?;
	let verdict = determine(&version, &buyer);
	let plan = plan(
		app,
		store,
		&seller,
		&version,
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
	// `NAV_REPORT` reads `invoice_documents` and answers `Unavailable` until `RENDER_PDF` has
	// landed, so the same `run_at` made every invoice pay a `2^attempts` backoff step for a
	// race it always loses. Typst takes seconds.
	let delay = app
		.settings
		.int("invoice.nav_report_delay_secs")
		.await
		.inspect_err(|e| tracing::error!(error = %e, "nav_report_delay_secs; using the default"))
		.unwrap_or(DEFAULT_NAV_REPORT_DELAY_SECS);

	let payload = invoice_job_payload(invoice.id);
	let now = Timestamp::now();
	for (kind, key, at) in [
		(KIND_RENDER_PDF, format!("pdf:invoice:{}", invoice.id), now),
		(KIND_NAV_REPORT, format!("nav:invoice:{}", invoice.id), Timestamp(now.0 + delay)),
	] {
		match job::enqueue(&app.store, kind, &payload, Some(&key), at).await {
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
