//! In-process domain events: [`AppBuilder::on_event`](crate::AppBuilder::on_event) registers a
//! handler, [`emit`] fans an event out to every handler.
//!
//! Not an outbox: no table, no retry, no delivery guarantee. An emitter calls [`emit`] **after**
//! its transaction commits, never inside one, and a handler's error is logged, never returned to
//! the emitter. A durable signed-webhook outbox was rejected (`claude-docs/todo.md` §7).

use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::App;
use crate::error::ClResult;
use crate::ids::{AccountId, InvoiceId, OrgId, PaymentId, RefId, SubscriptionId};
use crate::money::{CurrencyCode, Money};

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
	InvoiceIssued {
		invoice: InvoiceId,
	},
	PaymentSettled {
		payment: PaymentId,
		invoice: InvoiceId,
	},
	/// `invoice` is `None` when the refund came out of the payment's unallocated part.
	PaymentRefunded {
		payment: PaymentId,
		invoice: Option<InvoiceId>,
		amount: Money,
		currency: CurrencyCode,
	},
	/// `org` is the account's personal org; `ref_uid` the ref it was admitted with (wire `ref`),
	/// whose use is `(ref, account)`.
	AccountActivated {
		account: AccountId,
		org: OrgId,
		ref_uid: Option<RefId>,
	},
	MembershipAccepted {
		org: OrgId,
		account: AccountId,
	},
	/// `from`/`to` are `subscriptions.status` values (`TRIALING`, `ACTIVE`, …).
	SubscriptionChanged {
		subscription: SubscriptionId,
		from: String,
		to: String,
	},
	/// An upgrade applied now: the sub's offer and/or seats moved up mid-period.
	SubscriptionTierChanged {
		subscription: SubscriptionId,
	},
	/// An operator rewrote the sub's price; it is billed from the next renewal.
	SubscriptionRepriced {
		subscription: SubscriptionId,
		price: Money,
		currency: CurrencyCode,
	},
}

impl Event {
	/// The name a Rune `app.on_event(kind, …)` matches: the variant name.
	pub fn kind(&self) -> &'static str {
		match self {
			Self::InvoiceIssued { .. } => "InvoiceIssued",
			Self::PaymentSettled { .. } => "PaymentSettled",
			Self::PaymentRefunded { .. } => "PaymentRefunded",
			Self::AccountActivated { .. } => "AccountActivated",
			Self::MembershipAccepted { .. } => "MembershipAccepted",
			Self::SubscriptionChanged { .. } => "SubscriptionChanged",
			Self::SubscriptionTierChanged { .. } => "SubscriptionTierChanged",
			Self::SubscriptionRepriced { .. } => "SubscriptionRepriced",
		}
	}

	/// Every kind [`Self::kind`] can return, so a declaration can be checked at boot.
	pub const KINDS: &'static [&'static str] = &[
		"InvoiceIssued",
		"PaymentSettled",
		"PaymentRefunded",
		"AccountActivated",
		"MembershipAccepted",
		"SubscriptionChanged",
		"SubscriptionTierChanged",
		"SubscriptionRepriced",
	];

	/// The wire shape: `kind` plus the fields in camelCase, uids as strings, money as
	/// `{amount, currency}`.
	pub fn to_json(&self) -> Value {
		let mut v = match self {
			Self::InvoiceIssued { invoice } => json!({ "invoice": invoice }),
			Self::PaymentSettled { payment, invoice } => {
				json!({ "payment": payment, "invoice": invoice })
			}
			Self::PaymentRefunded { payment, invoice, amount, currency } => json!({
				"payment": payment,
				"invoice": invoice,
				"amount": amount.to_wire(currency),
			}),
			Self::AccountActivated { account, org, ref_uid } => {
				json!({ "account": account, "org": org, "ref": ref_uid })
			}
			Self::MembershipAccepted { org, account } => json!({ "org": org, "account": account }),
			Self::SubscriptionChanged { subscription, from, to } => {
				json!({ "subscription": subscription, "from": from, "to": to })
			}
			Self::SubscriptionTierChanged { subscription } => {
				json!({ "subscription": subscription })
			}
			Self::SubscriptionRepriced { subscription, price, currency } => {
				json!({ "subscription": subscription, "price": price.to_wire(currency) })
			}
		};
		v["kind"] = Value::from(self.kind());
		v
	}
}

pub type EventHandler =
	Arc<dyn Fn(App, Event) -> Pin<Box<dyn Future<Output = ClResult<()>> + Send>> + Send + Sync>;

/// Returns at once; one spawned task runs the handlers in registration order, each to completion
/// before the next, so a later handler sees an earlier one's writes. A failing or panicking
/// handler is logged and the rest still run.
#[allow(clippy::needless_pass_by_value)] // by value: every emit site builds the event inline
pub fn emit(app: &App, ev: Event) {
	let app = app.clone();
	tokio::spawn(async move {
		let kind = ev.kind();
		for handler in &app.event_handlers {
			match tokio::spawn(handler(app.clone(), ev.clone())).await {
				Ok(Ok(())) => {}
				Ok(Err(e)) => tracing::error!(kind, error = %e, "event handler failed"),
				Err(e) => tracing::error!(kind, error = %e, "event handler panicked"),
			}
		}
	});
}

// vim: ts=4
