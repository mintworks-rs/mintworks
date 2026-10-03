//! Barion's wire vocabulary, and the only place in the workspace it exists.
//!
//! Every struct below is Barion's shape, every string constant is Barion's spelling, and
//! [`client`](crate::client) converts to and from the neutral types in
//! `saas_billing::provider` — so no Barion field name, status or URL reaches a
//! `PaymentProvider` signature.

use saas_billing::{PaymentAddress, PaymentState, StartPayment};
use saas_core::{error::StatusCode, prelude::*};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Barion's timespan for how long a reservation is held before it lapses: `d.hh:mm:ss`.
const RESERVATION_PERIOD: &str = "1.00:00:00";

/// Seconds → Barion's `d.hh:mm:ss`. Barion's own default is 30 minutes and it is never the one
/// we mean, so `PaymentWindow` is always sent rather than left to the gateway.
fn timespan(secs: i64) -> String {
	let secs = secs.max(0);
	format!("{}.{:02}:{:02}:{:02}", secs / 86_400, secs / 3_600 % 24, secs / 60 % 60, secs % 60)
}

/// Never `coded_retry`: neither of the two faults below is an outage, so retrying gets the same
/// answer, and Barion's `Refund` carries no idempotency token to make a blind retry safe.
fn provider_fault(code: &'static str, what: impl Into<String>) -> Error {
	Error::coded(StatusCode::BAD_GATEWAY, code, what)
}

// ---------------------------------------------------------------------------------------
// Amounts
// ---------------------------------------------------------------------------------------

/// Barion's amounts are JSON **numbers**, and this codebase has no floats. `RawValue` puts
/// the digits [`Money`] renders on the wire verbatim, so nothing is ever parsed into an `f64`
/// on the way out or on the way back.
pub fn amount_out(amount: Money, currency: &CurrencyCode) -> ClResult<Box<RawValue>> {
	let digits = if currency.as_str() == "HUF" {
		// Barion rejects fractional forints, so a residual here is our own arithmetic being
		// wrong upstream — truncating it would post a different amount than the invoice says.
		if amount.0 % 100 != 0 {
			return Err(Error::coded(
				StatusCode::BAD_REQUEST,
				"E-PAY-ROUNDING",
				"Barion takes whole forints and this amount carries fillér",
			));
		}
		(amount.0 / 100).to_string()
	} else {
		amount.to_decimal_string()
	};
	RawValue::from_string(digits).map_err(|e| Error::internal(format!("barion amount: {e}")))
}

/// A quantity, by the same rule. `Qty` is scaled 1e6 and renders six decimals, which Barion
/// accepts on `Quantity`.
pub fn qty_out(qty: Qty) -> ClResult<Box<RawValue>> {
	RawValue::from_string(qty.to_decimal_string())
		.map_err(|e| Error::internal(format!("barion quantity: {e}")))
}

/// The inverse of [`amount_out`]. Barion writes whole forints for HUF and two decimals
/// elsewhere, and `Money::parse` reads both — `"100"` is `100.00`.
pub fn amount_in(raw: &RawValue) -> ClResult<Money> {
	Money::parse(raw.get().trim())
}

// ---------------------------------------------------------------------------------------
// Status vocabulary
// ---------------------------------------------------------------------------------------

/// Barion `PaymentStatus` → [`PaymentState`], exhaustively.
///
/// An unrecognised status is an error, never a default: mapping a new Barion state onto
/// `Pending` parks a settled payment forever, and mapping it onto `Failed` abandons money
/// that arrived. `Refunded` is deliberately unreachable — Barion has no payment-level refunded
/// status, and the caller derives it from `payments.refunded_amount`.
pub fn payment_state(status: &str) -> ClResult<PaymentState> {
	Ok(match status {
		"Prepared" => PaymentState::Pending,
		"Started" | "InProgress" | "Waiting" => PaymentState::AwaitingUser,
		"Reserved" => PaymentState::Reserved,
		"Authorized" => PaymentState::Authorized,
		// Barion has committed the funds and is only waiting to move them on; for us the
		// payer has paid.
		"Succeeded" | "PreparedForPayout" => PaymentState::Succeeded,
		"PartiallySucceeded" => PaymentState::PartiallySucceeded,
		"Canceled" => PaymentState::Canceled,
		"Expired" => PaymentState::Expired,
		"Failed" => PaymentState::Failed,
		// An out-of-date adapter, not a down gateway, so nothing retries into it.
		_ => {
			return Err(provider_fault(
				"E-PAY-PROVIDER-DOWN",
				format!("barion: unknown payment status '{status}'"),
			));
		}
	})
}

// ---------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------

/// No `Debug`: `pos_key` is a live POS key, and one `tracing::debug!(?req)` in `post` would put
/// it in the log — a leaked POS key starts and refunds payments on the merchant account.
#[derive(Serialize)]
pub struct StartRequest<'a> {
	#[serde(rename = "POSKey")]
	pub pos_key: &'a str,
	#[serde(rename = "PaymentType")]
	pub payment_type: &'static str,
	#[serde(rename = "ReservationPeriod", skip_serializing_if = "Option::is_none")]
	pub reservation_period: Option<&'static str>,
	#[serde(rename = "PaymentWindow")]
	pub payment_window: String,
	#[serde(rename = "PaymentRequestId")]
	pub payment_request_id: &'a str,
	#[serde(rename = "Currency")]
	pub currency: &'a str,
	#[serde(rename = "Locale")]
	pub locale: &'a str,
	#[serde(rename = "GuestCheckOut")]
	pub guest_check_out: bool,
	#[serde(rename = "FundingSources")]
	pub funding_sources: [&'static str; 1],
	#[serde(rename = "RedirectUrl")]
	pub redirect_url: &'a str,
	#[serde(rename = "CallbackUrl")]
	pub callback_url: &'a str,
	#[serde(rename = "PayerHint", skip_serializing_if = "Option::is_none")]
	pub payer_hint: Option<&'a str>,
	#[serde(rename = "RecurrenceId", skip_serializing_if = "Option::is_none")]
	pub recurrence_id: Option<&'a str>,
	#[serde(rename = "RecurrenceType", skip_serializing_if = "Option::is_none")]
	pub recurrence_type: Option<&'static str>,
	/// Payer present, card stored under `RecurrenceId` for later merchant-initiated charges.
	#[serde(rename = "InitiateRecurrence", skip_serializing_if = "std::ops::Not::not")]
	pub initiate_recurrence: bool,
	#[serde(rename = "BillingAddress", skip_serializing_if = "Option::is_none")]
	pub billing_address: Option<BillingAddress<'a>>,
	#[serde(rename = "Transactions")]
	pub transactions: [PaymentTransaction<'a>; 1],
}

#[derive(Debug, Serialize)]
pub struct PaymentTransaction<'a> {
	#[serde(rename = "POSTransactionId")]
	pub pos_transaction_id: &'a str,
	#[serde(rename = "Payee")]
	pub payee: &'a str,
	#[serde(rename = "Total")]
	pub total: Box<RawValue>,
	#[serde(rename = "Items")]
	pub items: Vec<Item<'a>>,
}

/// `ItemTotal` is Barion's `Quantity × UnitPrice`, and the items must sum to the transaction
/// `Total` — both are the caller's to guarantee, and `saas_billing::allocate::start` does so by
/// sending one unit at the line's gross plus a balancing item.
#[derive(Debug, Serialize)]
pub struct Item<'a> {
	#[serde(rename = "Name")]
	pub name: &'a str,
	#[serde(rename = "Description")]
	pub description: &'a str,
	#[serde(rename = "Quantity")]
	pub quantity: Box<RawValue>,
	#[serde(rename = "Unit")]
	pub unit: &'a str,
	#[serde(rename = "UnitPrice")]
	pub unit_price: Box<RawValue>,
	#[serde(rename = "ItemTotal")]
	pub item_total: Box<RawValue>,
}

/// 3DS risk scoring reads this. Every field comes off [`StartPayment::billing`], which the
/// caller built from the invoice's frozen billing party — the adapter reads no table of ours.
#[derive(Debug, Serialize)]
pub struct BillingAddress<'a> {
	#[serde(rename = "Country")]
	pub country: &'a str,
	#[serde(rename = "City", skip_serializing_if = "Option::is_none")]
	pub city: Option<&'a str>,
	#[serde(rename = "Zip", skip_serializing_if = "Option::is_none")]
	pub zip: Option<&'a str>,
	#[serde(rename = "Street", skip_serializing_if = "Option::is_none")]
	pub street: Option<&'a str>,
	#[serde(rename = "FullName", skip_serializing_if = "Option::is_none")]
	pub full_name: Option<&'a str>,
}

fn billing_address(addr: &PaymentAddress) -> Option<BillingAddress<'_>> {
	// `Country` is Barion's only required member of the block, so an address without one is
	// sent as no address rather than as a rejected one.
	Some(BillingAddress {
		country: addr.country.as_deref()?,
		city: addr.city.as_deref(),
		zip: addr.postcode.as_deref(),
		street: addr.street.as_deref(),
		full_name: addr.name.as_deref(),
	})
}

/// One builder for the four flows: a plain payment, a reservation (`reserve`), a payment that
/// initiates a recurrence (`req.recurrence`), and the merchant-initiated charge of a stored
/// token (`recurrence`).
pub fn start_request<'a>(
	pos_key: &'a str,
	payee: &'a str,
	req: &'a StartPayment,
	recurrence: Option<&'a str>,
) -> ClResult<StartRequest<'a>> {
	let items = req
		.items
		.iter()
		.map(|it| {
			Ok(Item {
				name: &it.name,
				// Barion requires a non-empty description; the line's own name is the
				// honest fallback, and an empty string is a schema rejection.
				description: it.description.as_deref().unwrap_or(&it.name),
				quantity: qty_out(it.qty)?,
				unit: &it.unit,
				unit_price: amount_out(it.unit_price, &req.currency)?,
				item_total: amount_out(it.total, &req.currency)?,
			})
		})
		.collect::<ClResult<Vec<_>>>()?;

	Ok(StartRequest {
		pos_key,
		// A recurring charge is always immediate: Barion has no reserved token charge, and
		// `reserve` on one would be silently dropped by the gateway.
		payment_type: if req.reserve && recurrence.is_none() { "Reservation" } else { "Immediate" },
		reservation_period: (req.reserve && recurrence.is_none()).then_some(RESERVATION_PERIOD),
		payment_window: timespan(req.window_secs),
		payment_request_id: &req.request_id,
		currency: req.currency.as_str(),
		locale: &req.locale,
		guest_check_out: true,
		funding_sources: ["All"],
		redirect_url: &req.redirect_url,
		callback_url: &req.callback_url,
		payer_hint: req.payer_email.as_deref(),
		recurrence_id: recurrence.or(req.recurrence.as_deref()),
		// The registering payment needs it as well: Barion's `RecurringPayment` caps every later
		// charge at the first amount, which an upgrade or a reprice exceeds.
		recurrence_type: (recurrence.is_some() || req.recurrence.is_some())
			.then_some("MerchantInitiatedPayment"),
		initiate_recurrence: recurrence.is_none() && req.recurrence.is_some(),
		billing_address: req.billing.as_ref().and_then(billing_address),
		transactions: [PaymentTransaction {
			pos_transaction_id: &req.request_id,
			payee,
			total: amount_out(req.amount, &req.currency)?,
			items,
		}],
	})
}

/// No `Debug`, for the reason [`StartRequest`] gives.
#[derive(Serialize)]
pub struct RefundRequest<'a> {
	#[serde(rename = "POSKey")]
	pub pos_key: &'a str,
	#[serde(rename = "PaymentId")]
	pub payment_id: &'a str,
	#[serde(rename = "TransactionsToRefund")]
	pub transactions_to_refund: [RefundTarget<'a>; 1],
}

#[derive(Debug, Serialize)]
pub struct RefundTarget<'a> {
	#[serde(rename = "TransactionId")]
	pub transaction_id: &'a str,
	#[serde(rename = "POSTransactionId")]
	pub pos_transaction_id: &'a str,
	#[serde(rename = "AmountToRefund")]
	pub amount_to_refund: Box<RawValue>,
}

// ---------------------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------------------

/// Barion answers a rejected request with `200` and a populated `Errors`, so the status line
/// is never the whole answer. Every reply below carries it.
#[derive(Debug, Default, Deserialize)]
pub struct BarionError {
	#[serde(rename = "ErrorCode")]
	pub error_code: Option<String>,
	#[serde(rename = "Title")]
	pub title: Option<String>,
	#[serde(rename = "Description")]
	pub description: Option<String>,
}

/// `Err` when the gateway reported a business or authentication fault. The detail is logged
/// by `Error`'s 5xx path and never reaches a client.
///
/// `E-PAY-PROVIDER`, not `E-PAY-PROVIDER-DOWN`: a named Barion error is the merchant's
/// configuration or this request — a bad `POSKey`, a rejected payee — and no amount of retrying
/// fixes either. Every entry is reported, because the first one is often the least specific.
pub fn check_errors(errors: &[BarionError]) -> ClResult<()> {
	if errors.is_empty() {
		return Ok(());
	}
	let detail = errors
		.iter()
		.map(|e| {
			format!(
				"{} {}",
				e.error_code.as_deref().unwrap_or("?"),
				e.title.as_deref().or(e.description.as_deref()).unwrap_or("")
			)
		})
		.collect::<Vec<_>>()
		.join("; ");
	Err(provider_fault("E-PAY-PROVIDER", format!("barion: {detail}")))
}

#[derive(Debug, Deserialize)]
pub struct StartResponse {
	#[serde(rename = "PaymentId")]
	pub payment_id: Option<String>,
	#[serde(rename = "Status")]
	pub status: Option<String>,
	#[serde(rename = "GatewayUrl")]
	pub gateway_url: Option<String>,
	#[serde(rename = "Errors", default)]
	pub errors: Vec<BarionError>,
}

#[derive(Debug, Deserialize)]
pub struct StateResponse {
	#[serde(rename = "PaymentId")]
	pub payment_id: Option<String>,
	#[serde(rename = "Status")]
	pub status: Option<String>,
	#[serde(rename = "Currency")]
	pub currency: Option<String>,
	#[serde(rename = "Transactions", default)]
	pub transactions: Vec<StateTransaction>,
	#[serde(rename = "Errors", default)]
	pub errors: Vec<BarionError>,
}

#[derive(Debug, Deserialize)]
pub struct StateTransaction {
	#[serde(rename = "TransactionId")]
	pub transaction_id: Option<String>,
	#[serde(rename = "POSTransactionId")]
	pub pos_transaction_id: Option<String>,
	#[serde(rename = "TransactionType")]
	pub transaction_type: Option<String>,
}

impl StateResponse {
	/// The shop leg of the payment, which is the only one a refund may target. `Shop` is
	/// Barion's own label. `None` rather than a guess: the reply also carries fee, payout and
	/// earlier-refund legs, and refunding the wrong one moves money the caller never chose.
	pub fn shop_transaction(&self) -> Option<&StateTransaction> {
		self.transactions.iter().find(|t| t.transaction_type.as_deref() == Some("Shop"))
	}
}

#[derive(Debug, Deserialize)]
pub struct RefundResponse {
	#[serde(rename = "RefundedTransactions", default)]
	pub refunded_transactions: Vec<RefundedTransaction>,
	#[serde(rename = "Errors", default)]
	pub errors: Vec<BarionError>,
}

#[derive(Debug, Deserialize)]
pub struct RefundedTransaction {
	#[serde(rename = "Total")]
	pub total: Option<Box<RawValue>>,
	#[serde(rename = "Status")]
	pub status: Option<String>,
}

// ---------------------------------------------------------------------------------------
// Callback
// ---------------------------------------------------------------------------------------

/// Pull a payment id out of an **untrusted** IPN body.
///
/// Barion's documentation has carried the id as a form field and as a query parameter at
/// different times, and the casing with it; the trait hands us only the body, so a route that
/// sees it in the query string has to fold it in. Both the form and the JSON shape are
/// accepted here because neither costs anything, and nothing is trusted either way — the
/// caller answers with `GetPaymentState`.
pub fn callback_payment_id(body: &[u8]) -> Option<String> {
	let text = std::str::from_utf8(body).ok()?;
	let from_form = text.split('&').find_map(|pair| {
		let (k, v) = pair.split_once('=')?;
		k.trim().eq_ignore_ascii_case("paymentid").then(|| v.trim().to_owned())
	});
	from_form
		.filter(|v| !v.is_empty())
		.or_else(|| serde_json::from_slice::<StateResponse>(body).ok()?.payment_id)
}

// vim: ts=4
