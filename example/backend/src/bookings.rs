//! The example's one consumer feature: a ledger of dated, billable sessions, and the checkout
//! that turns the unbilled ones into a framework invoice.
//!
//! Shaped like a framework service handle — one struct over `App`, every method taking `&Ctx`
//! first, all validation and authorization here and none in a handler.

use std::sync::Arc;

use saas_billing::StartRequest;
use saas_billing::provider::providers;
use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::prelude::*;
use saas_invoice::Invoices;
use saas_invoice::draft::{Line, NewDraft, Party};
use saas_invoice::routes::Page;
use saas_invoice::service_api::MAX_PAGE_LIMIT;
use saas_invoice::store::{Invoice, InvoicePatch, PaymentMethod};

use crate::store::{Booking, BookingStore, NewBooking};

/// What `seed::services` puts in the catalogue. A booking may name nothing else: the draft's
/// `resolve` would raise `E-INV-SERVICE` at checkout, long after the customer typed the date.
const SERVICE_CODES: [&str; 2] = ["CONSULT", "SITEVISIT"];

/// The largest quantity one booking may carry: 24 units of a service sold by the hour. Both
/// services in `seed::services` are priced per unit, and a checkout turns a booking into a
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

/// How the customer said they want to pay. Two, deliberately: cash is not something a web
/// checkout can promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PayMethod {
	Card,
	Transfer,
}

/// What `POST /api/bookings/checkout` asks for. `provider` names which registered gateway to
/// open a `CARD` payment at; it is ignored for `TRANSFER`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutRequest {
	pub method: PayMethod,
	pub provider: Option<String>,
}

/// What a checkout produced: the invoice, and the gateway URL the browser must be sent to when
/// one was opened. A `TRANSFER` checkout has no redirect and comes back already `ISSUED` — a
/// transfer needs a number to quote as the payment reference.
#[derive(Debug)]
pub struct Checkout {
	pub invoice: Invoice,
	pub redirect_url: Option<String>,
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
	pub async fn checkout(&self, ctx: &Ctx, req: &CheckoutRequest) -> ClResult<Option<Checkout>> {
		let store = self.store()?;
		let tenant_id = ctx.tenant()?;
		// Resolved before the claim, not inside `start_payment`: past the claim the invoice is
		// committed and a gateway failure may only warn, so an unknown provider would silently
		// become "no redirect" instead of the 400 it is.
		let provider = match req.method {
			PayMethod::Card => Some(self.provider_id(req.provider.as_deref())?),
			PayMethod::Transfer => None,
		};
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
					payment_method: Some(match req.method {
						PayMethod::Card => PaymentMethod::Card,
						PayMethod::Transfer => PaymentMethod::Transfer,
					}),
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
		if req.method == PayMethod::Transfer {
			// Issued here, as the system: a transfer has nothing to redirect to and needs a
			// number to quote as its reference. `require_stepup` exempts `Actor::System`, which
			// is what keeps a password prompt off the customer's path.
			let invoice = self.issue_as_system(tenant_id, invoice.uid.as_str()).await?;
			return Ok(Some(Checkout { invoice, redirect_url: None }));
		}
		let redirect_url = self.start_payment(ctx, &invoice, &claim, provider).await;
		// Re-read: a started payment locks the draft to `PENDING`, so the copy above is a status
		// behind and the page it lands on would offer to edit a frozen invoice.
		let invoice = Invoices::new(self.app.clone()).invoice(ctx, invoice.uid.as_str()).await?;
		Ok(Some(Checkout { invoice, redirect_url }))
	}

	/// The registered gateway the request named. Naming none used to be `ids().first()`, which
	/// picks out of a `HashMap` and so chose a different gateway between two runs.
	fn provider_id(&self, asked: Option<&str>) -> ClResult<String> {
		let unknown = || {
			Error::coded(
				saas_core::error::StatusCode::BAD_REQUEST,
				"E-PAY-PROVIDER",
				"unknown payment provider",
			)
		};
		let asked = asked.ok_or_else(unknown)?;
		providers(&self.app)?.get(asked).ok_or_else(unknown)?;
		Ok(asked.to_owned())
	}

	/// Issue on the customer's behalf. Minting a numbered legal document is a consequence of
	/// their own method choice on their own draft, never something they are asked to confirm.
	async fn issue_as_system(&self, tenant_id: i64, uid: &str) -> ClResult<Invoice> {
		let sys = Ctx::system("checkout").with_tenant(tenant_id);
		Invoices::new(self.app.clone()).issue(&sys, uid).await
	}

	/// The gateway leg of a checkout, and `None` whenever there is not one: with no
	/// `PaymentProvider` registered the demo settles through operator manual entry instead.
	///
	/// Never fails the checkout. The invoice is already committed and the customer has to see
	/// it, so a gateway that is down or misconfigured is a warning and no redirect.
	async fn start_payment(
		&self,
		ctx: &Ctx,
		invoice: &Invoice,
		claim: &str,
		provider: Option<String>,
	) -> Option<String> {
		let provider = provider?;
		let started = saas_billing::allocate::start(
			&self.app,
			ctx,
			&invoice.uid,
			StartRequest {
				provider,
				// The checkout claim again: `payments.request_id` is UNIQUE, so a retried
				// checkout reuses its payment rather than opening a second one at the gateway.
				request_id: Some(claim.to_owned()),
				return_url: format!("{}/invoices/{}", self.app.config.base_url, invoice.uid),
				locale: None,
			},
		)
		.await;
		match started {
			Ok((_, redirect_url)) => redirect_url,
			Err(e) => {
				tracing::warn!(
					error = %e,
					invoice = %invoice.uid.as_str(),
					"the gateway payment could not be started"
				);
				None
			}
		}
	}

	/// "Pay another way": restamp a draft as `TRANSFER` and issue it. The patch is tenant-scoped
	/// and draft-only by construction (`payment_method` is not one of the fields an issued
	/// invoice takes), so this is also the escape hatch for a draft whose gateway payment is
	/// stranded.
	///
	/// A live payment is abandoned first, which unlocks the invoice: the payer left the gateway
	/// page, but the gateway keeps reporting the payment live until it expires, and `patch`
	/// refuses a locked invoice with `E-INV-LOCKED` until then.
	pub async fn pay_by_transfer(&self, ctx: &Ctx, uid: &str) -> ClResult<Invoice> {
		let invoice_uid = InvoiceId::parse(uid)?;
		// `for_invoice` rather than the store, as `discard` does: it re-asks the gateway on the
		// way out, so a payment that has actually succeeded is already terminal here and is
		// never cancelled out from under the money.
		for p in saas_billing::allocate::for_invoice(&self.app, ctx, &invoice_uid).await? {
			if saas_billing::allocate::LIVE.contains(&p.status) {
				saas_billing::allocate::abandon(&self.app, &p).await?;
			}
		}
		let invoices = Invoices::new(self.app.clone());
		invoices
			.patch(
				ctx,
				uid,
				&InvoicePatch {
					payment_method: Some(PaymentMethod::Transfer),
					..Default::default()
				},
			)
			.await?;
		self.issue_as_system(ctx.tenant()?, uid).await
	}

	/// Throw an unpaid draft away and put its bookings back in the unbilled set.
	///
	/// `delete_draft` refuses anything but a `DRAFT`, so a numbered invoice cannot be reached
	/// from here — cancelling one of those is a storno, which is an operator's job.
	///
	/// An open gateway payment is refused outright: `delete_draft` drops the zero link row with
	/// the invoice, so a payment that then succeeds lands in `allocate::settle_full`'s "no
	/// invoice to settle" branch — charged, unallocated, and the released bookings billed again
	/// by the next checkout.
	pub async fn discard(&self, ctx: &Ctx, uid: &str) -> ClResult<()> {
		let tenant_id = ctx.tenant()?;
		let invoice_uid = InvoiceId::parse(uid)?;
		let bstore = saas_billing::store::store(&self.app)?;
		// `for_invoice` rather than the store: it re-asks the gateway on the way out, so a payment
		// that settled while nobody was looking is already terminal here.
		for p in saas_billing::allocate::for_invoice(&self.app, ctx, &invoice_uid).await? {
			let moved = bstore.allocations(p.id).await?.iter().any(|a| a.amount.0 != 0);
			if saas_billing::allocate::LIVE.contains(&p.status) || moved {
				return Err(Error::coded(
					saas_core::error::StatusCode::CONFLICT,
					"E-BOOK-PAYMENT-OPEN",
					"a payment is open on this draft",
				));
			}
		}
		Invoices::new(self.app.clone()).delete_draft(ctx, uid).await?;
		self.store()?.release(tenant_id, uid).await?;
		Ok(())
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
			"{count} booking(s) carry an invoice reference that does not bill them: either a \
			 checkout claim whose invoice exists — they will be billed a second time — or an \
			 invoice that no longer exists, which makes them permanently unbillable"
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
