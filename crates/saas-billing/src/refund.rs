//! Refunds: money going back out through the gateway that brought it in.
//!
//! **A refund never alters the invoice.** An `ISSUED` invoice is immutable, so cancelling the
//! *sale* is storno + reissue in `saas-invoice` and is deliberately not reachable from here.
//! What a refund moves is the *payment*: `payments.refunded_amount` and a negative
//! `payment_allocations` row, which drops the invoice's `paid_amount` cache back without
//! touching a figure on the document.

use saas_core::app::App;
use saas_core::audit;
use saas_core::ctx::Ctx;
use saas_core::error::StatusCode;
use saas_core::event::{self, Event};
use saas_core::prelude::*;

use crate::provider::{PaymentState, providers};
use crate::store::{Payment, RefundRecord, store};

fn pay(status: StatusCode, code: &'static str, msg: &'static str) -> Error {
	Error::coded(status, code, msg)
}

/// The statuses a refund may be made from, and the `from` guard it hands the store. All
/// terminal: `crate::allocate`'s `from_states` walks the *live* statuses and cannot express a
/// terminal-to-terminal move. `Refunded` is in the list so a second partial refund is legal.
const REFUNDABLE: [PaymentState; 3] = [
	PaymentState::Succeeded,
	PaymentState::PartiallySucceeded,
	PaymentState::Refunded,
];

/// `POST /api/admin/payments/{uid}/refund`. `amount` `None` gives back the whole remaining
/// `amount - refunded_amount`.
///
/// **Operator-only and step-up**, as [`crate::allocate::manual`] is.
pub async fn refund(
	app: &App,
	ctx: &Ctx,
	payment_uid: &PaymentId,
	amount: Option<(Money, CurrencyCode)>,
	reason: Option<String>,
) -> ClResult<Payment> {
	saas_core::auth_mw::require_operator(app, ctx).await?;
	saas_core::auth_mw::require_stepup(app, ctx).await?;
	let bstore = store(app)?;
	// Not `ctx.org()`: `require_operator` above is the gate, and an operator is not confined
	// to a selected org — scoping here refused the payment it had just created.
	let payment = bstore.payment_by_uid(None, payment_uid).await?.ok_or(Error::NotFound)?;
	if !REFUNDABLE.contains(&payment.status) {
		return Err(pay(StatusCode::CONFLICT, "E-PAY-STATE", "this payment cannot be refunded"));
	}

	// The declared currency is checked, not dropped: `{"amount":"10.00","currency":"EUR"}`
	// against a HUF payment refunded 10.00 HUF and answered 200.
	if let Some((_, currency)) = &amount
		&& *currency != payment.currency
	{
		return Err(pay(
			StatusCode::BAD_REQUEST,
			"E-PAY-CURRENCY",
			"the refund and the payment are in different currencies",
		));
	}
	// Read before the payout, never after: `provider.refund` is irreversible, so a refusal or a
	// driver error below it gave back money and recorded nothing — not even the
	// `PAYMENT_REFUND_UNRECORDED` row `A-PAY-REFUND-UNRECORDED` is built from, and every retry
	// re-ran the same refusal.
	//
	// Which allocation the negative row reverses: the one that actually moved money, never the
	// zero link row — reversing against an invoice that received nothing drives its
	// `paid_amount` negative. `None` touches no invoice at all, which `record_refund` honours.
	// Two *settled* invoices have no field on the wire to choose between them, so the operator
	// reverses those by hand.
	let allocations = bstore.allocations(payment.id).await?;
	if allocations.iter().filter(|a| a.amount.0 != 0).count() > 1 {
		return Err(pay(
			StatusCode::CONFLICT,
			"E-PAY-STATE",
			"this payment settles more than one invoice; reverse the allocations first",
		));
	}
	let allocated: i64 = allocations.iter().map(|a| a.amount.0).sum();

	// What the gateway actually took, which on a partial is *not* what the payment was opened
	// for: `fetch_state` answers with a status and no amount, so the only record of a partial
	// capture is what an operator allocated by hand. Without this the ceiling was the full
	// amount and a fully-refunded partial never reached `Refunded`.
	let arrived = if payment.status == PaymentState::PartiallySucceeded {
		allocated
	} else {
		payment.amount.0
	};
	let remaining = arrived - payment.refunded_amount.0;
	let asked = amount.map_or(Money(remaining), |(m, _)| m);
	if asked.0 <= 0 || asked.0 > remaining {
		return Err(pay(
			StatusCode::BAD_REQUEST,
			"E-PAY-AMOUNT",
			"the amount must be positive and at most the unrefunded remainder; \
			 a partial capture has to be recorded as an allocation first",
		));
	}

	let given = match payment.provider.as_deref() {
		// A transfer or a manual entry has no gateway to ask: the money went back by hand and
		// the operator is recording that it did.
		None => asked,
		Some(id) => {
			let provider = providers(app)?.get(id).ok_or_else(|| {
				pay(StatusCode::BAD_REQUEST, "E-PAY-PROVIDER", "unknown payment provider")
			})?;
			if asked.0 < remaining && !provider.capabilities().partial_refund {
				return Err(pay(
					StatusCode::BAD_REQUEST,
					"E-PAY-CAPABILITY",
					"this provider cannot refund part of a payment",
				));
			}
			let reference = payment.provider_ref.as_deref().ok_or_else(|| {
				pay(StatusCode::CONFLICT, "E-PAY-STATE", "this payment has no gateway reference")
			})?;
			// Stable across retries of the *same* logical refund and different across distinct
			// ones: a retry after a failure that recorded nothing reuses the key, while a genuine
			// second refund has a different `refunded_amount` behind it.
			let request_id = format!("{}:{}", payment.uid.as_str(), payment.refunded_amount.0);
			// What the gateway says it gave back, not what was asked for: a provider without
			// `partial_refund` may round a partial up to the whole payment. Clamped because
			// `refunded_amount <= amount` is a table CHECK and would otherwise be a driver
			// error rather than `E-PAY-AMOUNT`.
			Money(
				provider
					.refund(reference, asked, &request_id)
					.await?
					.refunded
					.0
					.clamp(0, remaining),
			)
		}
	};

	// The refund comes out of the unallocated part first: refunding a 600 overpayment on a
	// payment that settled 400 of an invoice must leave that invoice settled.
	// `refunded_amount` too: an earlier refund that came out of the unallocated part left the
	// allocation sum alone, so without it the same money looks unallocated twice.
	let unallocated = (arrived - payment.refunded_amount.0 - allocated).max(0);
	let reverse = (given.0 - unallocated).max(0);
	let invoice_id = allocations
		.iter()
		.find(|a| a.amount.0 != 0)
		.map(|a| a.invoice_id)
		.filter(|_| reverse > 0);

	// `Refunded` is derived here and nowhere else: no gateway status maps to it, and
	// `PaymentProvider::refund` answers with the *pre-refund* state.
	let to = if payment.refunded_amount.0 + given.0 >= arrived {
		PaymentState::Refunded
	} else {
		payment.status
	};

	if !bstore
		.record_refund(&RefundRecord {
			payment_id: payment.id,
			from: REFUNDABLE.to_vec(),
			to,
			amount: given,
			expect_refunded: payment.refunded_amount,
			reverse: Money(reverse),
			invoice_id,
			at: Timestamp::now(),
			by: ctx.actor.account_id(),
		})
		.await?
	{
		// Both arms below need a gateway behind them, which is why `provider.is_some()` gates
		// them: with no gateway there is no idempotency key, so two concurrent refunds of one
		// transfer both pass validation and the loser answered 200 for a payout nobody made —
		// and no payout means no discrepancy to alert on either.
		if payment.provider.is_some() {
			// A concurrent request that recorded this same gateway refund is not a discrepancy:
			// both derived one idempotency key from one `refunded_amount`, so the gateway paid
			// out once and the other request booked it. Alerting would be a false
			// `A-PAY-REFUND-UNRECORDED`, an ERROR nobody can clear.
			let current = bstore.payment(payment.id).await?;
			if let Some(p) = current
				&& p.refunded_amount.0 >= payment.refunded_amount.0 + given.0
			{
				return Ok(p);
			}
			// The gateway has already paid out. Nothing in our tables records it, so the money
			// is gone and the operator's retry would send it twice — `A-PAY-REFUND-UNRECORDED`
			// is what carries that to a human, since a log line reaches nobody.
			audit::detached(
				&app.store,
				ctx,
				"payment",
				Some(payment.uid.as_str()),
				"PAYMENT_REFUND_UNRECORDED",
				// `providerRef` is the only handle on the gateway payout an operator reconciles
				// against; `null` where the payment carries none is the honest value.
				Some(serde_json::json!({
					"amount": given.0,
					"reason": reason,
					"providerRef": payment.provider_ref.as_deref(),
				})),
			)
			.await;
		}
		return Err(pay(
			StatusCode::CONFLICT,
			"E-PAY-STATE",
			"the payment moved, or the refund would exceed it",
		));
	}

	audit::log(
		&app.store,
		ctx,
		"payment",
		Some(payment.uid.as_str()),
		"PAYMENT_REFUND",
		Some(serde_json::json!({ "amount": given.0, "reason": reason })),
	)
	.await;

	let invoice = match invoice_id {
		Some(id) => {
			match async { saas_invoice::service_api::store(app)?.invoice_by_id(id).await }.await {
				Ok(i) => i.map(|i| i.uid),
				Err(e) => {
					tracing::warn!(error = %e, invoice_id = id, "cannot re-read the refunded invoice");
					None
				}
			}
		}
		None => None,
	};
	event::emit(
		app,
		Event::PaymentRefunded {
			payment: payment.uid.clone(),
			invoice,
			amount: given,
			currency: payment.currency.clone(),
		},
	);

	bstore.payment(payment.id).await?.ok_or(Error::NotFound)
}

// vim: ts=4
