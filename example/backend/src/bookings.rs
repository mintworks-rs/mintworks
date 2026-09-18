//! The example's one consumer feature: a ledger of dated, billable sessions, and the checkout
//! that turns the unbilled ones into a framework invoice.
//!
//! Shaped like a framework service handle — one struct over `App`, every method taking `&Ctx`
//! first, all validation and authorization here and none in a handler.

use std::sync::Arc;

use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::prelude::*;
use saas_invoice::Invoices;
use saas_invoice::draft::{Line, NewDraft, Party};
use saas_invoice::routes::Page;
use saas_invoice::service_api::MAX_PAGE_LIMIT;
use saas_invoice::store::Invoice;

use crate::store::{Booking, BookingStore, NewBooking};

/// What `seed::services` puts in the catalogue. A booking may name nothing else: the draft's
/// `resolve` would raise `E-INV-SERVICE` at checkout, long after the customer typed the date.
const SERVICE_CODES: [&str; 2] = ["CONSULT", "SITEVISIT"];

/// The largest quantity one booking may carry: 24 units of a service sold by the hour. Both
/// services in `seed::services` are priced per unit, and `confirm` turns a booking into a
/// numbered invoice filed under the operator's taxpayer id, so this is the face value ceiling
/// on what a customer can mint by themselves.
const MAX_QTY_E6: i64 = 24_000_000;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookRequest {
	pub service_code: String,
	pub occurred_on: String,
	pub qty_e6: i64,
	pub note: Option<String>,
}

pub struct Bookings {
	app: App,
}

impl Bookings {
	pub fn new(app: App) -> Self {
		Self { app }
	}

	fn store(&self) -> ClResult<Arc<dyn BookingStore>> {
		self.app
			.extensions
			.get::<Arc<dyn BookingStore>>()
			.cloned()
			.ok_or_else(|| Error::internal("Arc<dyn BookingStore> is not registered"))
	}

	pub async fn book(&self, ctx: &Ctx, req: &BookRequest) -> ClResult<Booking> {
		let tenant_id = ctx.tenant()?;
		if !SERVICE_CODES.contains(&req.service_code.as_str()) {
			return Err(Error::validation("unknown service code"));
		}
		if !is_iso_date(&req.occurred_on) {
			return Err(Error::validation("occurredOn must be YYYY-MM-DD"));
		}
		if req.qty_e6 <= 0 {
			return Err(Error::validation("qtyE6 must be positive"));
		}
		// The same rule `draft::price` applies to `Line::note`, over the string `line_for` will
		// actually build. Rejected here, not there: `checkout` commits the claim before it
		// drafts, so a note only `draft` refuses leaves a claim nothing but SQL can clear.
		saas_invoice::store::bounded_multiline_text(
			"note",
			&note_for(&req.occurred_on, req.note.as_deref()),
			saas_invoice::store::MAX_LINE_NOTE,
		)?;
		// `Qty(qty_e6).times(unit_price)` traps under `[profile.release] overflow-checks`; the
		// cap keeps that out of `draft`, past the committed claim. It also bounds the face value
		// of a legal document a customer can mint against the operator's own tax number.
		if req.qty_e6 > MAX_QTY_E6 {
			return Err(Error::validation("qtyE6 is out of range: at most 24 units per booking"));
		}
		self.store()?
			.create(&NewBooking {
				// `saas_core::ids::prefixed_id!` is private, so a consumer cannot mint its own
				// prefixed uid type and formats the prefix by hand.
				uid: format!("bkg_{}", ulid::Ulid::new()),
				tenant_id,
				service_code: req.service_code.clone(),
				occurred_on: req.occurred_on.clone(),
				qty_e6: req.qty_e6,
				note: req.note.clone(),
			})
			.await
	}

	/// The page, cursor and all: the clamp, the full-page test and the cursor derivation are
	/// this service's decisions, not a handler's.
	pub async fn list(
		&self,
		ctx: &Ctx,
		cursor: Option<&str>,
		limit: Option<i64>,
	) -> ClResult<Page<Booking>> {
		let limit = limit.unwrap_or(50).clamp(1, MAX_PAGE_LIMIT);
		let items = self.store()?.list_for_tenant(ctx.tenant()?, cursor, limit).await?;
		let full_page = i64::try_from(items.len()).unwrap_or(i64::MAX) == limit;
		// The uid, not the rowid: a cursor is opaque to a client but still a response field.
		let next_cursor = full_page.then(|| items.last().map(|b| b.uid.clone())).flatten();
		Ok(Page { items, next_cursor })
	}

	/// Drafts one invoice over every unbilled booking; `None` means there was nothing to bill,
	/// which is a different answer from an empty DRAFT the customer would have to delete.
	///
	/// `Invoices::draft` owns its own transaction, so `settle` is a second write and a crash
	/// between the two leaves a draft whose bookings still carry the claim. The claim is taken
	/// first and doubles as the idempotency key, so the re-run bills *that* set: keying on the
	/// booking uids instead meant a booking created in the crash window changed the key, and
	/// `draft` minted a second invoice over a superset of the first one's lines.
	pub async fn checkout(&self, ctx: &Ctx) -> ClResult<Option<Invoice>> {
		let store = self.store()?;
		let tenant_id = ctx.tenant()?;
		let Some(claim) = store.claim_unbilled(tenant_id).await? else {
			return Ok(None);
		};
		let booked = store.by_checkout(tenant_id, &claim).await?;

		let drafted = Invoices::new(self.app.clone())
			.draft(
				ctx,
				&NewDraft {
					request_id: Some(claim.clone()),
					billing_party: Party::TenantDefault,
					lines: booked.iter().map(line_for).collect(),
					..Default::default()
				},
			)
			.await;
		let invoice = match drafted {
			Ok(invoice) => invoice,
			// Released only on a *validation* failure: a transport or lock error must keep the
			// claim, which is what makes the retry bill the same set.
			Err(e) if e.parts().0.is_client_error() => {
				store.release(tenant_id, &claim).await?;
				return Err(e);
			}
			Err(e) => return Err(e),
		};

		// Logged, not failed — the invoice is committed and the customer has to see it. The
		// bookings are then back in the unbilled set and bill a second time, which is real
		// money, so `alerts` raises `A-BOOKING-ORPHANED` off the same condition.
		if !store.settle(tenant_id, &claim, &invoice.uid.to_string()).await? {
			tracing::error!(
				claim,
				invoice = %invoice.uid.as_str(),
				"the checkout claim was gone at settle"
			);
		}
		Ok(Some(invoice))
	}

	pub async fn confirm(&self, ctx: &Ctx, uid: &str) -> ClResult<Invoice> {
		Invoices::new(self.app.clone()).issue(ctx, uid).await
	}

	/// The seam `saas-billing` and `payment-adapter-barion` will fill. There is no gateway
	/// yet, so the customer records the payment by hand and the invoice reaches PAID the same
	/// way it will once a `PaymentProvider` reports it.
	///
	/// **A demo stand-in, not a pattern to copy.** The framework gates `Invoices::mark_paid` on
	/// step-up and deliberately mounts *no* HTTP route for it: marking an invoice paid is the
	/// payment provider's report, not the payer's claim. The transition is one-way —
	/// `mark_status`' UPDATE carries `AND status = 'ISSUED'` — so a PAID invoice can never be
	/// stornoed, and exposing this to the invoice's own payer lets them permanently foreclose
	/// the cancellation of their own invoice. The example does it anyway because it seeds no
	/// operator account and the demo would otherwise have no payment step at all.
	///
	/// `expected` is what the caller believes it is paying, and a mismatch is refused: the
	/// transition is irreversible, so a customer clicking through a wrong-amount invoice must
	/// not leave both sides unable to storno it.
	pub async fn record_payment(
		&self,
		ctx: &Ctx,
		uid: &str,
		expected: &MoneyWire,
	) -> ClResult<Invoice> {
		let invoices = Invoices::new(self.app.clone());
		let invoice = invoices.invoice(ctx, uid).await?;
		if invoice.gross.to_wire(&invoice.currency) != *expected {
			return Err(Error::validation(
				"the amount does not match this invoice; reload it before recording a payment",
			));
		}
		invoices.mark_paid(ctx, uid).await
	}

	/// A STORNO does not un-bill the bookings: they stay attached to the cancelled invoice so
	/// the ledger still shows what was charged. Re-billing means booking them again.
	///
	/// The reason is required: it lands in the counter-invoice's `notes`, on a row that is
	/// numbered and immutable from creation, and `bounded_text` accepts the empty string.
	pub async fn cancel(&self, ctx: &Ctx, uid: &str, reason: &str) -> ClResult<Invoice> {
		if reason.trim().is_empty() {
			return Err(Error::validation("a cancellation reason is required"));
		}
		Invoices::new(self.app.clone()).storno(ctx, uid, reason).await
	}
}

/// `A-BOOKING-ORPHANED`: a checkout whose `settle` never landed. Those bookings are unbilled
/// again while their invoice stays issuable, so the next checkout bills them twice — real
/// money, and a `tracing::error!` at the moment it happened is the only other trace.
///
/// # Errors
/// Propagates the store read; `Error::Internal` when no `BookingStore` was registered.
pub async fn alerts(app: App) -> ClResult<Vec<saas_core::alert::Alert>> {
	let count = Bookings::new(app).store()?.orphaned_claims().await?;
	if count == 0 {
		return Ok(Vec::new());
	}
	Ok(vec![saas_core::alert::Alert {
		code: "A-BOOKING-ORPHANED",
		severity: saas_core::alert::Severity::Error,
		count,
		message: format!(
			"{count} booking(s) are still stamped with a checkout claim whose invoice exists: \
			 they will be billed a second time unless someone attaches them to that invoice"
		),
		since: None,
		link: None,
	}])
}

/// A catalogue line: `resolve` fills description, unit, price and VAT from the `services` row
/// and rejects a coded line that carries its own price. The booking's date and free text go in
/// `note`, the one field `resolve` never overwrites.
fn line_for(booking: &Booking) -> Line {
	let mut line = Line::code(booking.service_code.as_str(), Qty(booking.qty_e6));
	line.note = Some(note_for(&booking.occurred_on, booking.note.as_deref()));
	line
}

/// The line note a booking becomes: its date, then the customer's own text. One function, so
/// `book`'s length check and the line it later produces cannot disagree about the prefix.
fn note_for(occurred_on: &str, note: Option<&str>) -> String {
	match note {
		Some(n) if !n.is_empty() => format!("{occurred_on} — {n}"),
		_ => occurred_on.to_owned(),
	}
}

/// `YYYY-MM-DD`, with month and day in range — it reaches the invoice as free text and the SPA
/// sorts on it. Not a calendar: `2026-02-31` passes, which costs nothing here.
fn is_iso_date(s: &str) -> bool {
	let b = s.as_bytes();
	b.len() == 10
		&& b[4] == b'-'
		&& b[7] == b'-'
		&& b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
		&& matches!(s[5..7].parse::<u32>(), Ok(1..=12))
		&& matches!(s[8..10].parse::<u32>(), Ok(1..=31))
}

// vim: ts=4
