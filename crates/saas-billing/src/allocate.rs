//! The service half of payments: starting one, applying a state the gateway reported, and the
//! operator's manual entry and allocation.
//!
//! **No transition is decided by a read-then-write here.** Every one names the statuses it may
//! leave and hands them to the store, whose `WHERE` does the guarding inside its own
//! transaction — the callback endpoint is public and a gateway retries, so two copies of the
//! same ping may be in flight at once and only one of them may settle.

use std::sync::Arc;

use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::error::{Retry, StatusCode};
use saas_core::prelude::*;
use saas_core::store::Role;
use saas_core::{audit, ids};
use saas_invoice::Invoices;
use saas_invoice::store::{InvoiceStatus, PaymentMethod};

use crate::provider::{PaymentAddress, PaymentItem, PaymentState, StartPayment, providers};
use crate::store::{BillingStore, NewPayment, Payment, Settlement, store};

fn pay(status: StatusCode, code: &'static str, msg: &'static str) -> Error {
	Error::coded(status, code, msg)
}

/// Where a state sits on the one-way walk `Pending → AwaitingUser → Reserved → Authorized`.
/// `None` is terminal: the money either landed or it did not, and neither answer is revisited.
fn rank(s: PaymentState) -> Option<u8> {
	match s {
		PaymentState::Pending => Some(0),
		PaymentState::AwaitingUser => Some(1),
		PaymentState::Reserved => Some(2),
		PaymentState::Authorized => Some(3),
		_ => None,
	}
}

/// The statuses money may be allocated from: the gateway (or the operator's own entry) says it
/// arrived. Anything else would mark an invoice paid with money that never landed —
/// `Settlement { from: vec![payment.status], to: payment.status }` makes every status
/// self-legal, so the store's guard cannot catch this one.
const ALLOCATABLE: [PaymentState; 2] = [PaymentState::Succeeded, PaymentState::PartiallySucceeded];

/// A payment the gateway has not finished with. Public because a consumer deciding what a
/// still-open payment forbids needs the same list.
pub const LIVE: [PaymentState; 4] = [
	PaymentState::Pending,
	PaymentState::AwaitingUser,
	PaymentState::Reserved,
	PaymentState::Authorized,
];

/// The `from` guard for a move to `to`: every status it may legally be made from. A terminal
/// target is reachable from any live status; a live one only from an earlier live one, so a
/// callback that arrives out of order cannot walk a payment backwards.
fn from_states(to: PaymentState) -> Vec<PaymentState> {
	match rank(to) {
		Some(r) => LIVE.iter().copied().filter(|s| rank(*s).is_some_and(|x| x < r)).collect(),
		None => LIVE.to_vec(),
	}
}

/// Apply the status the gateway reported. It never arrives in the callback body — it is read
/// back with `fetch_state` — and this function is the only path by which a gateway moves a
/// payment.
pub async fn apply_state(app: &App, payment: &Payment, state: PaymentState) -> ClResult<()> {
	let bstore = store(app)?;
	if payment.status == state {
		return Ok(());
	}
	if state != PaymentState::Succeeded {
		// `PartiallySucceeded` lands here too and deliberately allocates nothing: `fetch_state`
		// answers with a status and no amount, so allocating `payments.amount` would overstate
		// it. The operator allocates the real figure through `POST .../payments/{uid}/allocations`.
		let from = from_states(state);
		// Nothing may move to PENDING, so there is no legal `from`: a gateway reporting it is a
		// replay.
		if from.is_empty() {
			return Ok(());
		}
		if !bstore.advance_status(payment.id, state, &from).await? {
			tracing::debug!(payment = %payment.uid.as_str(), %state, "state not applied");
			return Ok(());
		}
		// The money is never coming, so the lock this payment took has nothing behind it: the
		// invoice is an editable cart again and `Pay again` / `Discard draft` work. Only on the
		// move we just made — a replay must not unlock a draft somebody has since re-locked.
		if matches!(state, PaymentState::Failed | PaymentState::Canceled | PaymentState::Expired) {
			unlock_invoice(app, &bstore, payment).await?;
		}
		return Ok(());
	}
	settle_full(app, &bstore, payment).await
}

/// `SUCCEEDED`: the whole payment settles the invoice it was opened for.
async fn settle_full(app: &App, bstore: &Arc<dyn BillingStore>, payment: &Payment) -> ClResult<()> {
	let mut from = from_states(PaymentState::Succeeded);
	// `PartiallySucceeded` on top of the live walk: it is terminal, so `from_states` cannot name
	// it, and without it a gateway that reported a partial and then the full capture settled
	// nothing at all — the same terminal-to-terminal exception `refund::REFUNDABLE` documents.
	from.push(PaymentState::PartiallySucceeded);
	// `Expired` too, and not as the gateway's word alone: it may be the sweep's own verdict on a
	// gateway that never resolved the payment, and a gateway that captured in the meantime is the
	// truth and still has to settle — without it the payer was charged and nothing recorded it.
	from.push(PaymentState::Expired);

	let links = bstore.allocations(payment.id).await?;
	let link = match links.len() {
		0 => {
			// No zero link row, so nothing knows which invoice this is. The money is real, so the
			// status still moves; `A-PAY-UNALLOCATED` is what surfaces it to an operator.
			tracing::warn!(payment = %payment.uid.as_str(), "SUCCEEDED with no invoice to settle");
			bstore.advance_status(payment.id, PaymentState::Succeeded, &from).await?;
			return Ok(());
		}
		1 => &links[0],
		n => {
			// `allocations` orders by `invoice_id`, never by insertion, so picking one settles
			// whichever sorts first — an operator's hand-allocated invoice rather than this
			// payment's own. The money is recorded; `A-PAY-UNALLOCATED` carries the rest to a human.
			tracing::warn!(
				payment = %payment.uid.as_str(),
				allocations = n,
				"SUCCEEDED against more than one invoice; the remainder is left to an operator"
			);
			bstore.advance_status(payment.id, PaymentState::Succeeded, &from).await?;
			return Ok(());
		}
	};
	issue_if_unissued(app, link.invoice_id).await?;

	let s = Settlement {
		payment_id: payment.id,
		from,
		to: PaymentState::Succeeded,
		invoice_id: link.invoice_id,
		// The link row already holds whatever an operator allocated by hand, and `settle` adds
		// to it rather than replacing it, so only the remainder may be sent.
		amount: payment.amount - link.amount,
		at: Timestamp::now(),
		allocated_by: None,
		// The same ceiling re-checked inside `settle`'s transaction: `link.amount` was read on a
		// reader connection, so a hand allocation racing this one could push the sum past what
		// the payment holds.
		ceiling: Some(Money(payment.amount.0 - payment.refunded_amount.0)),
	};
	if !bstore.settle(&s).await? {
		tracing::debug!(payment = %payment.uid.as_str(), "settle did not apply; replay or conflict");
	}
	Ok(())
}

/// A draft has no final `gross` and `BillingStore::settle` refuses it outright, so it is issued
/// before the settlement rather than by a job after it — the money has already arrived, and a
/// deferred issue would leave the callback with nothing to allocate against.
///
/// Through [`Invoices`] rather than `issue::run`, so the `ISSUE` audit row is written. The
/// `Ctx` is `System`, which `require_stepup` exempts: nobody is at a keyboard here.
///
/// [`Ctx::system`] and not [`Ctx::as_system`] deliberately, unlike [`start`]: this takes only
/// `&App`, and the one user-facing path that reaches it goes through `refresh`, which takes no
/// `Ctx` to escalate.
async fn issue_if_unissued(app: &App, invoice_id: i64) -> ClResult<()> {
	let istore = saas_invoice::service_api::store(app)?;
	let Some(invoice) = istore.invoice_by_id(invoice_id).await? else {
		return Ok(());
	};
	// `Pending` is this payment's own lock and issues from here — one transaction, no unlocked
	// window a concurrent `delete_draft` could reach. `payment_method` is already `CARD`:
	// [`start`] stamps it beside the lock through `begin_card_payment`.
	if !matches!(invoice.status, InvoiceStatus::Draft | InvoiceStatus::Pending) {
		return Ok(());
	}
	let sys = Ctx::system("payment").with_org(invoice.org_id);
	let invoices = Invoices::new(app.clone());
	// A dead payment the gateway captured late finds the draft unlocked, and `issue` refuses an
	// unlocked CARD draft (`E-INV-CARD-UNPAID`), so it is re-locked first. Only CARD: a TRANSFER
	// draft issues unlocked, and a lock left by a failed issue froze it for good.
	let locked = invoice.status == InvoiceStatus::Draft
		&& invoice.payment_method == PaymentMethod::Card
		&& invoices.lock(&sys, invoice.uid.as_str()).await?;
	if let Err(e) = invoices.issue(&sys, invoice.uid.as_str()).await {
		if locked && let Err(fault) = invoices.unlock(&sys, invoice.uid.as_str()).await {
			tracing::warn!(error = %fault, invoice = %invoice.uid.as_str(), "unlock after a failed issue failed");
		}
		return Err(e);
	}
	Ok(())
}

/// `PENDING -> DRAFT` for the invoice a dead payment was opened against: the cart is editable
/// again, so `Pay again`, `Pay another way` and `Discard draft` all work.
async fn unlock_invoice(
	app: &App,
	bstore: &Arc<dyn BillingStore>,
	payment: &Payment,
) -> ClResult<()> {
	let Some(link) = bstore.allocations(payment.id).await?.into_iter().next() else {
		return Ok(());
	};
	let istore = saas_invoice::service_api::store(app)?;
	let Some(invoice) = istore.invoice_by_id(link.invoice_id).await? else {
		return Ok(());
	};
	// A resurrected `CANCELED` row is not the only payment for this invoice: a newer gateway
	// payment may hold the lock, and unlocking here would let the draft be edited while a card
	// charge for the old total is in flight. The guard only withholds an unlock, so the worst
	// case is a lock that stays one tick longer.
	if bstore
		.payments_by_invoice(invoice.org_id, invoice.id)
		.await?
		.iter()
		.any(|p| p.id != payment.id && LIVE.contains(&p.status))
	{
		return Ok(());
	}
	if invoice.status != InvoiceStatus::Pending {
		return Ok(());
	}
	let sys = Ctx::system("payment").with_org(invoice.org_id);
	Invoices::new(app.clone()).unlock(&sys, invoice.uid.as_str()).await?;
	Ok(())
}

/// What `POST /api/invoices/{uid}/pay` asks for, already parsed.
#[derive(Debug, Clone)]
pub struct StartRequest {
	pub provider: String,
	/// The idempotency key. Generated when the caller sends none.
	pub request_id: Option<String>,
	pub return_url: String,
	pub locale: Option<String>,
}

/// Open a gateway payment for an invoice's unpaid remainder.
///
/// Idempotent on `payments.request_id`, which is `UNIQUE (org_id, request_id)`: a retried
/// start finds the payment it already made instead of opening a second one at the gateway.
pub async fn start(
	app: &App,
	ctx: &Ctx,
	invoice_uid: &InvoiceId,
	req: StartRequest,
) -> ClResult<(Payment, Option<String>)> {
	let org_id = ctx.org()?;
	// Buyer-side: opening a payment is a member's act on their own org's invoice. Until now the
	// only gate was `Invoices::patch`'s seller-admin check, which refused a MEMBER wherever the
	// seller org *is* the buyer org, and which the system escalation below now bypasses.
	saas_core::auth_mw::require_role(app, ctx, Role::Member).await?;
	let bstore = store(app)?;
	let istore = saas_invoice::service_api::store(app)?;

	let invoice = istore.invoice_by_uid(Some(org_id), invoice_uid).await?.ok_or(Error::NotFound)?;
	if invoice.status == InvoiceStatus::Stornoed {
		return Err(pay(StatusCode::CONFLICT, "E-PAY-NOT-PAYABLE", "this invoice is stornoed"));
	}
	let remainder = invoice.gross - invoice.paid_amount;
	if remainder.0 <= 0 {
		return Err(pay(StatusCode::CONFLICT, "E-PAY-NOT-PAYABLE", "this invoice is already paid"));
	}
	// Gateways take whole forints and nothing rounds an invoice *total*: 4 990 Ft net at 27% is
	// a gross of 6 337.30 Ft, which no Hungarian customer could ever pay by card. Rounded **up**
	// to the currency's own step — down would leave the invoice short of `gross` forever — and
	// the surplus is allocated to the invoice as an overpayment, which the schema supports.
	let cur = istore
		.currency_get(invoice.currency.as_str())
		.await?
		.ok_or_else(|| Error::internal(format!("currency '{}' has no row", invoice.currency)))?;
	let charge = saas_invoice::currency::round_up_to_step(remainder, cur.price_round_step)?;

	let provider = providers(app)?.get(&req.provider).ok_or_else(|| {
		pay(StatusCode::BAD_REQUEST, "E-PAY-PROVIDER", "unknown payment provider")
	})?;
	// The gateway sends the payer wherever this points, so it may only point back at us. A bare
	// `starts_with` is not that test: it let `https://app.invalid.evil.tld/` through.
	let base = app.config.base_url.trim_end_matches('/');
	let under_base = req.return_url.strip_prefix(base).is_some_and(|rest| {
		rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#')
	});
	if !under_base {
		return Err(pay(
			StatusCode::BAD_REQUEST,
			"E-PAY-RETURN-URL",
			"returnUrl must be under the application's base URL",
		));
	}

	let request_id = req.request_id.unwrap_or_else(|| ids::PaymentId::generate().into_string());
	if let Some(existing) = bstore.payment_by_request_id(org_id, &request_id).await? {
		// Whatever is stored, terminal statuses included: the caller reads `payment.status` and
		// decides. A retry never opens a second gateway payment under a spent key — a fresh
		// attempt sends no `requestId` and the server mints one. Same org, same key, *other*
		// invoice is still a conflict: it would redirect the payer to pay invoice A off B's page.
		if !bstore
			.allocations(existing.id)
			.await?
			.iter()
			.any(|a| a.invoice_id == invoice.id)
		{
			return Err(pay(
				StatusCode::CONFLICT,
				"E-PAY-NOT-PAYABLE",
				"this request id belongs to another invoice",
			));
		}
		let url = existing.redirect_url.clone();
		return Ok((existing, url));
	}

	// Before the payment row and the gateway: refused after them, the charge stood against a
	// draft that could never issue.
	if invoice.status == InvoiceStatus::Draft {
		saas_invoice::service_api::check_card_dates(&invoice)?;
		let seller = istore.seller_by_id(invoice.seller_id).await?.ok_or(Error::NotFound)?;
		saas_invoice::service_api::check_seller_open(&seller)?;
	}
	let payment = bstore
		.create_payment(&NewPayment {
			org_id,
			kind: provider.id().to_ascii_uppercase(),
			provider: Some(provider.id().to_string()),
			provider_ref: None,
			request_id: Some(request_id.clone()),
			status: PaymentState::Pending,
			amount: charge,
			currency: invoice.currency.clone(),
			ext_ref: None,
			note: None,
			created_by: None,
			invoice_id: Some(invoice.id),
		})
		.await?;

	// Gateways run 3DS risk scoring on the item block, so it is built rather than sent empty, and
	// they cross-check it: `ItemTotal` must be `Quantity × UnitPrice` and the items must sum to the
	// total. Both are met by one unit at the line's *gross* with the quantity folded into the name
	// — a line's `unit_price` is net, so the invoice's own pair contradicts itself on the wire.
	let items = istore.invoice_lines(invoice.id).await?;
	let mut lines_gross: i128 = 0;
	let mut items: Vec<PaymentItem> = items
		.into_iter()
		.map(|l| {
			lines_gross += i128::from(l.gross.0);
			PaymentItem {
				name: format!("{} × {} {}", l.qty.to_decimal_string(), l.unit, l.description),
				description: l.note,
				qty: Qty(1_000_000),
				unit: "db".to_string(),
				unit_price: l.gross,
				total: l.gross,
			}
		})
		.collect();
	// A partially paid invoice charges less than its lines come to, and the HUF round-up above
	// charges a little more; either way the difference is one balancing item, because a gateway
	// rejects a `Start` whose items and total disagree.
	let balance = saas_core::money::bounded(
		i64::try_from(i128::from(charge.0) - lines_gross)
			.map_err(|_| Error::validation("amount out of range"))?,
	)?;
	if balance != 0 {
		items.push(PaymentItem {
			name: if balance < 0 { "Korábbi részfizetés" } else { "Kerekítés" }.to_string(),
			description: None,
			qty: Qty(1_000_000),
			unit: "db".to_string(),
			unit_price: Money(balance),
			total: Money(balance),
		});
	}
	let payer_email = match invoice.billing_party_id {
		Some(id) => istore.party_by_id(id).await?.and_then(|p| p.email),
		None => None,
	};
	// Read once, before the gateway is asked: the row's deadline is `now + window` from the
	// moment the gateway accepts, not a value read back from it.
	let window_secs = app.settings.int("payment.window_minutes").await? * 60;

	let started = provider
		.start(&StartPayment {
			request_id,
			amount: charge,
			currency: invoice.currency.clone(),
			redirect_url: req.return_url,
			callback_url: format!("{}/api/webhook/{}", app.config.base_url, provider.id()),
			locale: req.locale.unwrap_or_else(|| "hu-HU".to_string()),
			payer_email,
			reserve: false,
			window_secs,
			items,
			billing: Some(PaymentAddress {
				name: invoice.buyer_name.clone(),
				country: invoice.buyer_country.clone(),
				postcode: invoice.buyer_postcode.clone(),
				city: invoice.buyer_city.clone(),
				street: invoice.buyer_street.clone(),
			}),
		})
		.await;
	let started = match started {
		Ok(s) => s,
		Err(e) => {
			// The row already holds the UNIQUE `request_id`, so leaving it PENDING makes every
			// retry look like a payment in flight. FAILED is what lets the caller start over.
			let _ = bstore
				.advance_status(
					payment.id,
					PaymentState::Failed,
					&from_states(PaymentState::Failed),
				)
				.await;
			return Err(e);
		}
	};

	// One `now + window` for both the stored row and the view handed back.
	let expires_at = Timestamp(Timestamp::now().0 + window_secs);
	// `false` means the row already carried a reference: a gateway payment exists that nothing
	// in our database points at, so somebody has to go and find it.
	if !bstore
		.set_started(
			payment.id,
			&started.provider_ref,
			started.redirect_url.as_deref(),
			Some(expires_at),
		)
		.await?
	{
		tracing::warn!(
			payment = %payment.uid.as_str(),
			provider_ref = %started.provider_ref,
			"set_started: the payment already had a gateway reference"
		);
	}
	// Both writes record the same fact — the gateway has accepted a charge for this draft's
	// total — so they go together, and only now: a gateway that refused leaves nothing stamped
	// and nothing locked. `payment_method` is frozen at issue, so a card sale unstamped would
	// file `TRANSFER` at NAV forever. Only a DRAFT: an ISSUED invoice paid by card is
	// immutable, and correcting its method afterwards means a helyesbítő számla.
	//
	// ponytail: provider-backed ⇒ CARD, assumed rather than declared — a non-card gateway
	// would need a method on `PaymentProvider`, not worth it for one implementation.
	if invoice.status == InvoiceStatus::Draft {
		// Escalated: the method stamp and the lock are the framework marking a gateway charge it
		// started, not the caller's edit, and the gate is the *seller's* org.
		let sys = ctx.clone().as_system("payment");
		Invoices::new(app.clone())
			.begin_card_payment(&sys, invoice_uid.as_str())
			.await?;
	}
	let mut payment = payment;
	payment.provider_ref = Some(started.provider_ref);
	payment.redirect_url.clone_from(&started.redirect_url);
	payment.expires_at = Some(expires_at);
	// Through `apply_state`, not `advance_status`: a gateway that captures synchronously answers
	// `Start` with SUCCEEDED, and moving only the status left the payer charged against an
	// invoice still reading ISSUED with nothing but the zero link row.
	if started.state != PaymentState::Pending {
		apply_state(app, &payment, started.state).await?;
	}
	payment.status = started.state;
	Ok((payment, started.redirect_url))
}

/// Ask the gateway what a live payment's state really is, and apply it.
///
/// The callback is a signal only — the gateway's own `fetch_state` is what decides, and it is
/// to be asked when the payer returns. A gateway that cannot reach `callback_url` at all (any
/// deployment whose `BASE_URL` is not public) makes the return the only signal there is.
///
/// Two callers, one ask each: the return leg ([`refresh_invoice`], once per visit) and the
/// sweep. The SPA's 2 s poll is [`for_invoice`], which reads nothing but our own tables.
pub async fn refresh(app: &App, payment: &Payment) -> ClResult<Payment> {
	let (Some(id), Some(provider_ref)) = (&payment.provider, &payment.provider_ref) else {
		return Ok(payment.clone());
	};
	// Terminal: the answer has arrived and is not revisited. `CANCELED` included — the gateway
	// said it, and a gateway does not un-cancel.
	if rank(payment.status).is_none() {
		return Ok(payment.clone());
	}
	let Some(provider) = providers(app)?.get(id) else {
		tracing::debug!(payment = %payment.uid.as_str(), provider = %id, "provider not registered");
		return Ok(payment.clone());
	};
	let state = match provider.fetch_state(provider_ref).await {
		Ok(s) => s,
		Err(e) => {
			tracing::warn!(payment = %payment.uid.as_str(), error = %e, "fetch_state failed");
			// [`refresh_invoice`] renders the row it already has rather than a 500, but the sweep
			// cannot tell a stale row from a settled one, so an outage — and only an outage — is
			// handed on for it to stop on.
			if e.retry() == Retry::Backoff {
				return Err(e);
			}
			return Ok(payment.clone());
		}
	};
	apply_state(app, payment, state).await?;
	Ok(store(app)?.payment(payment.id).await?.unwrap_or_else(|| payment.clone()))
}

/// Every payment opened against one invoice, newest first — what the invoice page reads on a
/// cold visit, since the gateway's return URL carries no query string.
///
/// **Served from our own tables: this read does not touch the gateway.** The page polls it every
/// two seconds to notice a webhook land; the gateway's quota belongs to [`refresh_invoice`].
pub async fn for_invoice(app: &App, ctx: &Ctx, invoice_uid: &InvoiceId) -> ClResult<Vec<Payment>> {
	let org_id = ctx.org()?;
	let invoice = saas_invoice::service_api::store(app)?
		.invoice_by_uid(Some(org_id), invoice_uid)
		.await?
		.ok_or(Error::NotFound)?;
	store(app)?.payments_by_invoice(org_id, invoice.id).await
}

/// [`for_invoice`] with every live row re-asked of its gateway first: the return leg, and what a
/// caller about to move money reads to be sure nothing is still open at the gateway.
///
/// **A caller that reads the invoice too must re-read it after this call.** The two reads race,
/// and the invoice one answers from before the settlement this triggers — so a page that fires
/// both at once renders the invoice as it was, unnumbered and unpaid.
pub async fn refresh_invoice(
	app: &App,
	ctx: &Ctx,
	invoice_uid: &InvoiceId,
) -> ClResult<Vec<Payment>> {
	let rows = for_invoice(app, ctx, invoice_uid).await?;
	let mut out = Vec::with_capacity(rows.len());
	for p in &rows {
		// A gateway outage must not turn the invoice page into a 500: the row already read is
		// what renders, and the sweep is what asks again.
		match refresh(app, p).await {
			Ok(p) => out.push(p),
			Err(e) if e.retry() == Retry::Backoff => out.push(p.clone()),
			Err(e) => return Err(e),
		}
	}
	Ok(out)
}

/// One page of the org's payments with their allocations. `GET /api/payments`.
///
/// Both reads in one call so a page costs one read each rather than one per row; not re-asked
/// of the gateway, unlike [`refresh_invoice`] — a list view is not a return leg and would be as
/// many round trips as rows.
pub async fn list(
	app: &App,
	ctx: &Ctx,
	filter: &crate::store::PaymentFilter<'_>,
) -> ClResult<(Vec<Payment>, Vec<crate::store::PaymentAllocation>)> {
	let bstore = store(app)?;
	let rows = bstore.list_payments(ctx.org()?, filter).await?;
	let ids: Vec<i64> = rows.iter().map(|p| p.id).collect();
	let allocations = bstore.allocations_for(&ids).await?;
	Ok((rows, allocations))
}

/// The allocations of payments already read through [`payment`], [`for_invoice`] or [`start`],
/// in one read for all of them. `ctx` is not re-checked: the rows were scoped by the call that
/// produced them.
pub async fn allocations_of(
	app: &App,
	_ctx: &Ctx,
	payments: &[Payment],
) -> ClResult<Vec<crate::store::PaymentAllocation>> {
	let ids: Vec<i64> = payments.iter().map(|p| p.id).collect();
	store(app)?.allocations_for(&ids).await
}

/// One payment, re-asked of its gateway. `GET /api/payments/{uid}` — the return leg for a caller
/// that holds the payment's own id, and what the invoice page calls once per live row.
pub async fn payment(app: &App, ctx: &Ctx, uid: &PaymentId) -> ClResult<Payment> {
	let p = store(app)?
		.payment_by_uid(Some(ctx.org()?), uid)
		.await?
		.ok_or(Error::NotFound)?;
	match refresh(app, &p).await {
		Ok(fresh) => Ok(fresh),
		// A throttled gateway must not turn a read into a 502: the stored row is what renders,
		// and the sweep is what asks again. Same fallback as `refresh_invoice`.
		Err(e) if e.retry() == Retry::Backoff => Ok(p),
		Err(e) => Err(e),
	}
}

/// One line of a manual entry's `allocations`, or the body of the allocation route.
#[derive(Debug, Clone)]
pub struct Allocation {
	pub invoice_uid: InvoiceId,
	pub amount: Money,
	/// The currency the caller declared on the wire. Checked against the *payment*, not only
	/// the invoice: a refund or allocation posted as EUR against a HUF payment used to move
	/// HUF and answer 200, because the handler parsed the code and dropped it.
	pub currency: CurrencyCode,
}

/// What `POST /api/admin/payments` asks for, already parsed.
#[derive(Debug, Clone)]
pub struct ManualPayment {
	pub org_uid: OrgId,
	pub kind: String,
	pub amount: Money,
	pub currency: CurrencyCode,
	pub received_at: Timestamp,
	pub ext_ref: Option<String>,
	pub note: Option<String>,
	pub allocations: Vec<Allocation>,
}

/// Operator entry of money that arrived outside any gateway — a bank transfer, almost always.
///
/// The row is born `SUCCEEDED`: nothing is pending, the operator is looking at the statement.
///
/// **Operator-only and step-up**, gated here rather than in the route bundle for the reason
/// [`crate::routes::operator`] gives: the bundle is the part a consumer may leave unmounted.
/// The gate is also what makes reading `req.org_uid` off the wire legitimate — an operator
/// is not confined to `ctx.org_id`.
pub async fn manual(app: &App, ctx: &Ctx, req: ManualPayment) -> ClResult<Payment> {
	saas_core::auth_mw::require_operator(app, ctx).await?;
	saas_core::auth_mw::require_stepup(app, ctx).await?;
	if req.amount.0 <= 0 {
		return Err(pay(StatusCode::BAD_REQUEST, "E-PAY-AMOUNT", "amount must be positive"));
	}
	let bstore = store(app)?;
	let org_id = bstore.org_id_by_uid(&req.org_uid).await?.ok_or(Error::NotFound)?;
	// Every allocation checked before the payment row exists: the loop below commits one at a
	// time, so a failure on the second used to leave a half-applied entry on the screen.
	check_allocations(app, org_id, &req).await?;

	let payment = bstore
		.create_payment(&NewPayment {
			org_id,
			kind: req.kind,
			provider: None,
			provider_ref: None,
			request_id: None,
			status: PaymentState::Succeeded,
			amount: req.amount,
			currency: req.currency,
			ext_ref: req.ext_ref,
			note: req.note,
			created_by: ctx.actor.account_id(),
			invoice_id: None,
		})
		.await?;

	audit::log(&app.store, ctx, "payment", Some(payment.uid.as_str()), "PAYMENT_MANUAL", None)
		.await;

	// One `settle` per invoice, so a two-invoice transfer is two transactions rather than one:
	// the store's atomic unit is a payment against *an* invoice, and widening it to a list
	// would make every caller pay for a case almost none of them has.
	for a in req.allocations {
		allocate_to(app, ctx, &bstore, &payment, &a, req.received_at).await?;
	}
	bstore.payment(payment.id).await?.ok_or(Error::NotFound)
}

/// The same refusals [`allocate_to`] makes, made against every line of a manual entry before
/// any of it is written. Every one of them, because past `create_payment` a refusal is a
/// committed `SUCCEEDED` payment nobody allocated — money only `A-PAY-UNALLOCATED` will
/// surface, 48 hours later.
async fn check_allocations(app: &App, org_id: i64, req: &ManualPayment) -> ClResult<()> {
	/// One transfer settling more invoices than this is a data-entry mistake, not a payment.
	/// It is also what keeps `total` inside `i64`: `MAX_MINOR` is 1e15 and release builds trap
	/// on overflow.
	const MAX_ALLOCATIONS: usize = 200;

	if req.allocations.len() > MAX_ALLOCATIONS {
		return Err(Error::validation("too many allocations"));
	}
	let istore = saas_invoice::service_api::store(app)?;
	let mut seen: Vec<&str> = Vec::with_capacity(req.allocations.len());
	let mut total: i64 = 0;
	for a in &req.allocations {
		// Positive, not merely non-zero: a manual entry records money arriving. Reversing an
		// allocation is `POST /api/admin/payments/{uid}/allocations`, where negatives stay legal.
		if a.amount.0 <= 0 {
			return Err(pay(StatusCode::BAD_REQUEST, "E-PAY-AMOUNT", "amount must be positive"));
		}
		if seen.contains(&a.invoice_uid.as_str()) {
			return Err(pay(
				StatusCode::CONFLICT,
				"E-PAY-ALREADY-ALLOCATED",
				"this entry allocates to the same invoice twice",
			));
		}
		seen.push(a.invoice_uid.as_str());
		total += a.amount.0;
		let invoice = istore
			.invoice_by_uid(Some(org_id), &a.invoice_uid)
			.await?
			.ok_or(Error::NotFound)?;
		if invoice.currency != req.currency || a.currency != req.currency {
			return Err(pay(
				StatusCode::BAD_REQUEST,
				"E-PAY-CURRENCY",
				"the payment and the invoice are in different currencies",
			));
		}
		// `settle` matches only `ISSUED`/`PAID`, so a stornoed invoice came back from
		// `allocate_to` as `E-PAY-ALLOC-EXCEEDS` — past the commit, and about the wrong thing.
		if invoice.status == InvoiceStatus::Stornoed {
			return Err(pay(StatusCode::CONFLICT, "E-PAY-NOT-PAYABLE", "this invoice is stornoed"));
		}
		outstanding_allows(app, &invoice, a.amount).await?;
	}
	if total > req.amount.0 {
		return Err(pay(
			StatusCode::CONFLICT,
			"E-PAY-ALLOC-EXCEEDS",
			"allocations would exceed the payment",
		));
	}
	Ok(())
}

/// `POST /api/admin/payments/{uid}/allocations`. A negative `amount` reverses an earlier
/// allocation — there is no delete route, because removing the row would destroy the trail.
///
/// **Operator-only and step-up**, as [`manual`] is.
pub async fn allocate(
	app: &App,
	ctx: &Ctx,
	payment_uid: &PaymentId,
	req: &Allocation,
) -> ClResult<()> {
	saas_core::auth_mw::require_operator(app, ctx).await?;
	saas_core::auth_mw::require_stepup(app, ctx).await?;
	let bstore = store(app)?;
	// Not `ctx.org()`: the gate above is the authorization, and `allocate_to` scopes the
	// invoice by `payment.org_id`, which is the org that owns the money.
	let payment = bstore.payment_by_uid(None, payment_uid).await?.ok_or(Error::NotFound)?;
	allocate_to(app, ctx, &bstore, &payment, req, Timestamp::now()).await
}

/// A positive allocation may not drive an invoice past its own `gross`.
///
/// The tolerance is one `price_round_step`, and it is load-bearing rather than slack: a
/// gateway that takes whole forints is charged `round_up_to_step(remainder, step)` in
/// [`start`], so a HUF invoice is deliberately overpaid by up to 99 fillér and that surplus
/// still has to be allocatable. On every invoice, not only old ones: `vat::compute` steps the
/// group *VAT* and never the net, so a discounted or fractional-quantity HUF line leaves fillér
/// in the gross.
async fn outstanding_allows(
	app: &App,
	invoice: &saas_invoice::store::Invoice,
	amount: Money,
) -> ClResult<()> {
	if amount.0 <= 0 {
		return Ok(());
	}
	let istore = saas_invoice::service_api::store(app)?;
	let tolerance = istore
		.currency_get(invoice.currency.as_str())
		.await?
		.map_or(1, |c| c.price_round_step);
	if invoice.paid_amount.0 + amount.0 > invoice.gross.0 + tolerance {
		return Err(Error::coded(
			StatusCode::CONFLICT,
			"E-PAY-ALLOC-EXCEEDS",
			format!(
				"this invoice has {} outstanding of {}",
				(invoice.gross - invoice.paid_amount).to_decimal_string(),
				invoice.gross.to_decimal_string()
			),
		));
	}
	Ok(())
}

async fn allocate_to(
	app: &App,
	ctx: &Ctx,
	bstore: &Arc<dyn BillingStore>,
	payment: &Payment,
	req: &Allocation,
	at: Timestamp,
) -> ClResult<()> {
	let istore = saas_invoice::service_api::store(app)?;
	let invoice = istore
		.invoice_by_uid(Some(payment.org_id), &req.invoice_uid)
		.await?
		.ok_or(Error::NotFound)?;
	if invoice.currency != payment.currency || req.currency != payment.currency {
		return Err(pay(
			StatusCode::BAD_REQUEST,
			"E-PAY-CURRENCY",
			"the payment and the invoice are in different currencies",
		));
	}
	outstanding_allows(app, &invoice, req.amount).await?;

	if !ALLOCATABLE.contains(&payment.status) {
		return Err(pay(
			StatusCode::CONFLICT,
			"E-PAY-STATE",
			"this payment has not arrived; only a settled one may be allocated",
		));
	}

	let existing = bstore.allocations(payment.id).await?;
	let allocated: i64 = existing.iter().map(|a| a.amount.0).sum();
	// Minus what went back: a partial refund lowers the allocation sum but not the payment.
	if allocated + req.amount.0 > payment.amount.0 - payment.refunded_amount.0 {
		return Err(pay(
			StatusCode::CONFLICT,
			"E-PAY-ALLOC-EXCEEDS",
			"allocations would exceed the payment",
		));
	}
	// A zero row is the link `create_payment` wrote, not an allocation, so `settled == 0` is the
	// "not allocated yet" test rather than the row's absence.
	let settled: i64 =
		existing.iter().filter(|a| a.invoice_id == invoice.id).map(|a| a.amount.0).sum();
	if req.amount.0 == 0 {
		return Err(pay(StatusCode::BAD_REQUEST, "E-PAY-AMOUNT", "amount must not be zero"));
	}
	if req.amount.0 > 0 && settled != 0 {
		return Err(pay(
			StatusCode::CONFLICT,
			"E-PAY-ALREADY-ALLOCATED",
			"this payment already settles this invoice; reverse it with a negative amount",
		));
	}
	if req.amount.0 < 0 && settled + req.amount.0 < 0 {
		return Err(pay(
			StatusCode::CONFLICT,
			"E-PAY-ALLOC-EXCEEDS",
			"the reversal is larger than what this payment allocated to the invoice",
		));
	}

	issue_if_unissued(app, invoice.id).await?;
	let s = Settlement {
		payment_id: payment.id,
		from: vec![payment.status],
		to: payment.status,
		invoice_id: invoice.id,
		amount: req.amount,
		at,
		allocated_by: ctx.actor.account_id(),
		// The Rust check above is a read-then-write on a reader connection: two concurrent
		// allocations of the whole payment both saw zero allocated. `settle`'s own transaction
		// re-checks this same ceiling, which is what actually serialises them.
		ceiling: Some(Money(payment.amount.0 - payment.refunded_amount.0)),
	};
	if !bstore.settle(&s).await? {
		// The status guard passed on the way in, so a `false` here is either the ceiling this
		// call brought or a racing writer that took the headroom first.
		return Err(pay(
			StatusCode::CONFLICT,
			"E-PAY-ALLOC-EXCEEDS",
			"the payment moved, the invoice is not payable, or the allocations would exceed it",
		));
	}
	audit::log(&app.store, ctx, "payment", Some(payment.uid.as_str()), "PAYMENT_ALLOCATE", None)
		.await;
	Ok(())
}

// vim: ts=4
