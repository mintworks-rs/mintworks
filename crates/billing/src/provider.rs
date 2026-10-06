//! The gateway boundary.
//!
//! [`PaymentProvider`] is shaped by the union of what payment gateways do, not by any one of
//! them: a method that could only be described in one gateway's vocabulary would make the
//! trait unimplementable by the next, which is the whole reason it exists. Every type below
//! is ours — no gateway's status string, field name or wire shape crosses into `crates/`.

use std::{collections::HashMap, fmt, str::FromStr, sync::Arc};

use async_trait::async_trait;
use axum::http::HeaderMap;
use mintworks_core::prelude::*;
use serde::Serialize;

/// Where a payment has got to, as the framework names it.
///
/// The `SCREAMING_SNAKE` rendering below **is** `payments.status`'s `CHECK` constraint, so a
/// variant added here needs a schema version with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaymentState {
	/// Created here; the gateway has not been asked yet.
	Pending,
	/// The payer has been sent to the gateway and has not come back.
	AwaitingUser,
	/// Funds held, not taken. Only gateways with [`ProviderCaps::reservation`] reach it.
	Reserved,
	/// Authorized but not captured — the card network's hold, as distinct from the
	/// gateway-level [`PaymentState::Reserved`].
	Authorized,
	Succeeded,
	/// Less than the full amount was taken. The gateway settled what it could; the
	/// difference is still owed, which `payment_allocations` records as a partial allocation.
	PartiallySucceeded,
	Failed,
	Canceled,
	/// The payer never finished and the gateway gave up waiting.
	Expired,
	/// Fully refunded. A partial refund leaves the state alone and moves
	/// `payments.refunded_amount`.
	Refunded,
}

impl PaymentState {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "PENDING",
			Self::AwaitingUser => "AWAITING_USER",
			Self::Reserved => "RESERVED",
			Self::Authorized => "AUTHORIZED",
			Self::Succeeded => "SUCCEEDED",
			Self::PartiallySucceeded => "PARTIALLY_SUCCEEDED",
			Self::Failed => "FAILED",
			Self::Canceled => "CANCELED",
			Self::Expired => "EXPIRED",
			Self::Refunded => "REFUNDED",
		}
	}
}

impl fmt::Display for PaymentState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

impl FromStr for PaymentState {
	type Err = Error;

	fn from_str(s: &str) -> ClResult<Self> {
		Ok(match s {
			"PENDING" => Self::Pending,
			"AWAITING_USER" => Self::AwaitingUser,
			"RESERVED" => Self::Reserved,
			"AUTHORIZED" => Self::Authorized,
			"SUCCEEDED" => Self::Succeeded,
			"PARTIALLY_SUCCEEDED" => Self::PartiallySucceeded,
			"FAILED" => Self::Failed,
			"CANCELED" => Self::Canceled,
			"EXPIRED" => Self::Expired,
			"REFUNDED" => Self::Refunded,
			// The column's CHECK admits exactly the ten above, so an unknown value is a
			// defect here, never client input.
			_ => return Err(Error::internal(format!("unknown payment status '{s}'"))),
		})
	}
}

/// What a gateway can do, so a caller can branch without naming it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCaps {
	/// Funds can be held and captured later ([`PaymentState::Reserved`]).
	pub reservation: bool,
	/// A stored token can be charged with the payer absent
	/// ([`PaymentProvider::charge_recurring`]).
	pub recurring: bool,
	/// A refund for less than the remaining amount is accepted.
	pub partial_refund: bool,
}

/// One line of what is being paid for.
///
/// Built from the invoice and its frozen billing party by the caller: gateways run 3DS risk
/// scoring on this, and the adapter must be able to fill it in without reading a table of ours.
#[derive(Debug, Clone)]
pub struct PaymentItem {
	pub name: String,
	pub description: Option<String>,
	pub qty: Qty,
	pub unit: String,
	pub unit_price: Money,
	pub total: Money,
}

/// The payer's address, as far as 3DS wants it. A neutral shape, not `billing_parties`.
#[derive(Debug, Clone, Default)]
pub struct PaymentAddress {
	pub name: Option<String>,
	/// ISO-3166-1 alpha-2.
	pub country: Option<String>,
	pub postcode: Option<String>,
	pub city: Option<String>,
	pub street: Option<String>,
}

/// Everything a gateway needs to open a payment. No `Serialize`: [`Money`] has none, and the
/// gateway's own wire struct belongs in its adapter.
#[derive(Debug, Clone)]
pub struct StartPayment {
	/// Our idempotency key, stored as `payments.request_id`. Passed to the gateway so a
	/// retried start is the same payment there too, not a second one.
	pub request_id: String,
	pub amount: Money,
	pub currency: CurrencyCode,
	/// Where the gateway sends the payer when they are done.
	pub redirect_url: String,
	/// Where the gateway pings us. That ping is untrusted — see
	/// [`PaymentProvider::parse_callback`].
	pub callback_url: String,
	/// BCP-47 tag for the payer-facing pages, e.g. `hu-HU`.
	pub locale: String,
	pub payer_email: Option<String>,
	/// Hold rather than charge. Only meaningful with [`ProviderCaps::reservation`].
	pub reserve: bool,
	/// How long the gateway must keep this payment open before expiring it, from
	/// `payment.window_minutes`. An expiring gateway is what releases the invoice of a payer
	/// who closed the tab, so an adapter that cannot express this must say so rather than
	/// drop it.
	pub window_secs: i64,
	pub items: Vec<PaymentItem>,
	pub billing: Option<PaymentAddress>,
	/// Our recurrence id: `Some` asks the gateway to store this payer-present card for later
	/// [`PaymentProvider::charge_recurring`] calls under that id. Only with
	/// [`ProviderCaps::recurring`]; set from [`RecurrenceHook`].
	pub recurrence: Option<String>,
}

/// What [`PaymentProvider::start`] gives back.
#[derive(Debug, Clone)]
pub struct StartedPayment {
	/// The gateway's own id for the payment, stored as `payments.provider_ref`.
	pub provider_ref: String,
	/// Where to send the payer. `None` for a flow with no payer present, such as
	/// [`PaymentProvider::charge_recurring`].
	pub redirect_url: Option<String>,
	pub state: PaymentState,
}

#[derive(Debug, Clone)]
pub struct RefundResult {
	/// What the gateway actually gave back, which a gateway without
	/// [`ProviderCaps::partial_refund`] may round up to the whole payment.
	pub refunded: Money,
	pub state: PaymentState,
}

/// All a callback is allowed to tell us: which payment it is about. The truth about that
/// payment comes from [`PaymentProvider::fetch_state`].
#[derive(Debug, Clone)]
pub struct CallbackRef {
	pub provider_ref: String,
}

#[async_trait]
pub trait PaymentProvider: Send + Sync + 'static {
	/// Stable across restarts and stored in `payments.provider`: it is how a row found years
	/// later is matched back to the code that can refund it, and it is the `{provider}`
	/// segment of the callback URL.
	fn id(&self) -> &str;

	fn capabilities(&self) -> ProviderCaps;

	/// Opens a payment at the gateway. Called once per `payments` row; `req.request_id` is
	/// what makes a retry of this call idempotent on the far side.
	async fn start(&self, req: &StartPayment) -> ClResult<StartedPayment>;

	/// Asks the gateway what a payment's state really is. **This is the only thing that may
	/// move a payment forward** — the callback body never is.
	async fn fetch_state(&self, provider_ref: &str) -> ClResult<PaymentState>;

	/// Gives money back. `amount` may be less than the payment only when
	/// [`ProviderCaps::partial_refund`]; the ceiling is `amount - refunded_amount` and is the
	/// caller's to enforce, because only the caller has the row.
	///
	/// `request_id` is the idempotency key, as [`StartPayment::request_id`] is for a start: the
	/// payout happens before anything records it, so a retry after a failed `record_refund`
	/// must be the same refund at the gateway rather than a second one.
	async fn refund(
		&self,
		provider_ref: &str,
		amount: Money,
		request_id: &str,
	) -> ClResult<RefundResult>;

	/// Charges a stored token with the payer absent. Only for [`ProviderCaps::recurring`];
	/// `token` is the [`StartPayment::recurrence`] the first payment initiated. The result's
	/// `provider_ref` is what later `fetch_state`/`refund` calls name; `redirect_url` is `None`.
	async fn charge_recurring(&self, token: &str, req: &StartPayment) -> ClResult<StartedPayment>;

	/// Reads a reference out of an **untrusted** ping. The endpoint is public and
	/// unauthenticated, so this must not be where trust is established: it parses, and
	/// [`Self::fetch_state`] decides. A signature header the gateway provides is still worth
	/// checking here — it is a cheap filter, not the authority.
	fn parse_callback(&self, headers: &HeaderMap, body: &[u8]) -> ClResult<CallbackRef>;
}

/// Decides whether a payer-present payment should initiate a card recurrence, and under which
/// id. Registered as `AppBuilder::extension(Arc::new(hook) as Arc<dyn RecurrenceHook>)`; with
/// none registered, no payment initiates one. Asked only when the gateway has
/// [`ProviderCaps::recurring`].
#[async_trait]
pub trait RecurrenceHook: Send + Sync + 'static {
	async fn recurrence_for(
		&self,
		app: &mintworks_core::App,
		invoice: &InvoiceId,
	) -> ClResult<Option<String>>;
}

/// The gateways an application has wired up, keyed by [`PaymentProvider::id`].
///
/// A consumer builds one and parks it with `AppBuilder::extension(Arc::new(providers))`, the
/// same reflex as registering a store trait; handlers read it back with
/// [`providers`]. That is what keeps `adapters/payment-barion` a leaf: nothing under
/// `crates/` constructs it.
#[derive(Default)]
pub struct PaymentProviders(HashMap<String, Arc<dyn PaymentProvider>>);

impl PaymentProviders {
	pub fn new() -> Self {
		Self::default()
	}

	/// Last registration under an id wins, so a consumer can swap a gateway out without
	/// rebuilding the map.
	#[must_use]
	pub fn with(mut self, provider: Arc<dyn PaymentProvider>) -> Self {
		self.0.insert(provider.id().to_owned(), provider);
		self
	}

	/// `None` for an unregistered id. The caller picks the error: the `{provider}` segment of
	/// a public callback URL and an operator's configuration mistake deserve different ones.
	pub fn get(&self, id: &str) -> Option<Arc<dyn PaymentProvider>> {
		self.0.get(id).cloned()
	}

	pub fn ids(&self) -> Vec<&str> {
		self.0.keys().map(String::as_str).collect()
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

/// The registered [`PaymentProviders`], or an internal error naming what the application
/// forgot.
pub fn providers(app: &mintworks_core::App) -> ClResult<Arc<PaymentProviders>> {
	app.extensions.get::<Arc<PaymentProviders>>().cloned().ok_or_else(|| {
		Error::internal("mintworks-billing: no PaymentProviders were registered on the app")
	})
}

/// Every registered gateway and what it can do. `GET /api/payment-providers`.
///
/// Sorted: [`PaymentProviders`] is a `HashMap`, so the SPA's `items[0]` picked a different
/// gateway between requests — the nondeterminism `Bookings::provider_id` was fixed for.
pub fn list_providers(app: &mintworks_core::App) -> ClResult<Vec<(String, ProviderCaps)>> {
	let registry = providers(app)?;
	let mut ids = registry.ids();
	ids.sort_unstable();
	Ok(ids
		.into_iter()
		.filter_map(|id| registry.get(id).map(|p| (id.to_string(), p.capabilities())))
		.collect())
}

// vim: ts=4
