//! The Barion adapter against a mock gateway.
//!
//! No database and no `App`: the crate is a leaf, and `BarionProvider::new` takes the three
//! values `load` would have read, so every case here is one HTTP exchange.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use mintworks_billing::{PaymentAddress, PaymentItem, PaymentProvider, PaymentState, StartPayment};
use mintworks_core::error::Retry;
use mintworks_core::prelude::*;
use mintworks_payment_barion::{
	BarionProvider, PRODUCTION_BASE_URL, SANDBOX_BASE_URL, base_url_for,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FAST: Duration = Duration::from_millis(200);

fn provider(base_url: &str) -> BarionProvider {
	BarionProvider::new(base_url, "pos-key", "shop@example.com").with_timeout(FAST)
}

fn payment(amount: i64, currency: &str) -> StartPayment {
	StartPayment {
		request_id: "req-1".to_owned(),
		amount: Money(amount),
		currency: CurrencyCode::parse(currency).unwrap(),
		redirect_url: "https://shop.example.com/done".to_owned(),
		callback_url: "https://shop.example.com/api/webhook/barion".to_owned(),
		locale: "hu-HU".to_owned(),
		payer_email: Some("payer@example.com".to_owned()),
		reserve: false,
		window_secs: 600,
		items: vec![PaymentItem {
			name: "Foglalás".to_owned(),
			description: None,
			qty: Qty(1_000_000),
			unit: "db".to_owned(),
			unit_price: Money(amount),
			total: Money(amount),
		}],
		billing: Some(PaymentAddress {
			name: Some("Teszt Elek".to_owned()),
			country: Some("HU".to_owned()),
			postcode: Some("1051".to_owned()),
			city: Some("Budapest".to_owned()),
			street: Some("Fő utca 1.".to_owned()),
		}),
		recurrence: None,
	}
}

fn code_of(err: &Error) -> &str {
	match err {
		Error::Coded { code, .. } => code,
		other => panic!("expected a coded error, got {other:?}"),
	}
}

#[tokio::test]
async fn start_returns_the_gateway_url_and_the_mapped_state() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1",
			"Status": "Prepared",
			"GatewayUrl": "https://secure.test.barion.com/Pay?Id=bar-1",
			"Errors": [],
		})))
		.mount(&server)
		.await;

	let started = provider(&server.uri()).start(&payment(10_000, "HUF")).await.unwrap();
	assert_eq!(started.provider_ref, "bar-1");
	assert_eq!(
		started.redirect_url.as_deref(),
		Some("https://secure.test.barion.com/Pay?Id=bar-1")
	);
	assert_eq!(started.state, PaymentState::Pending);
}

/// The whole vocabulary, including the two Barion spellings that are not terminal and the
/// payout state that is. An unrecognised status is the last case: it must be an error, because
/// defaulting it to `Pending` parks a settled payment and defaulting it to `Failed` abandons
/// money that arrived.
#[tokio::test]
async fn every_barion_status_maps_or_fails_loudly() {
	let cases = [
		("Prepared", Some(PaymentState::Pending)),
		("Started", Some(PaymentState::AwaitingUser)),
		("InProgress", Some(PaymentState::AwaitingUser)),
		("Waiting", Some(PaymentState::AwaitingUser)),
		("Reserved", Some(PaymentState::Reserved)),
		("Authorized", Some(PaymentState::Authorized)),
		("Succeeded", Some(PaymentState::Succeeded)),
		("PreparedForPayout", Some(PaymentState::Succeeded)),
		("PartiallySucceeded", Some(PaymentState::PartiallySucceeded)),
		("Canceled", Some(PaymentState::Canceled)),
		("Expired", Some(PaymentState::Expired)),
		("Failed", Some(PaymentState::Failed)),
		("TeleportedToMars", None),
	];
	for (status, want) in cases {
		let server = MockServer::builder().start().await;
		Mock::given(method("GET"))
			.and(path("/v2/Payment/GetPaymentState"))
			.respond_with(ResponseTemplate::new(200).set_body_json(json!({
				"PaymentId": "bar-1",
				"Status": status,
				"Errors": [],
			})))
			.mount(&server)
			.await;

		let got = provider(&server.uri()).fetch_state("bar-1").await;
		match want {
			Some(state) => assert_eq!(got.unwrap(), state, "{status}"),
			None => assert_eq!(code_of(&got.unwrap_err()), "E-PAY-PROVIDER-DOWN", "{status}"),
		}
	}
}

/// A refund reads the payment first — that is where the transaction id and the currency come
/// from — and reports what the gateway says it gave back, not what was asked for.
#[tokio::test]
async fn a_refund_reports_what_the_gateway_gave_back() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1",
			"Status": "Succeeded",
			"Currency": "HUF",
			"Transactions": [
				{ "TransactionId": "txn-1", "POSTransactionId": "req-1", "TransactionType": "Shop" },
			],
			"Errors": [],
		})))
		.mount(&server)
		.await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Refund"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"RefundedTransactions": [{ "Total": 250, "Status": "Succeeded" }],
			"Errors": [],
		})))
		.mount(&server)
		.await;

	let res = provider(&server.uri()).refund("bar-1", Money(20_000), "pay_x:0").await.unwrap();
	// `250` whole forints off the wire is 25_000 minor units, and the payment's own state is
	// untouched: `REFUNDED` is the caller's, off `payments.refunded_amount`.
	assert_eq!(res.refunded, Money(25_000));
	assert_eq!(res.state, PaymentState::Succeeded);
}

/// Barion answers a business fault with `200` and a populated `Errors`, so the status line is
/// never the whole answer.
#[tokio::test]
async fn an_errors_array_is_a_failure_even_on_200() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"Errors": [{ "ErrorCode": "AuthenticationFailed", "Title": "Invalid POSKey" }],
		})))
		.mount(&server)
		.await;

	let err = provider(&server.uri()).start(&payment(10_000, "HUF")).await.unwrap_err();
	// A named Barion error is the merchant's configuration or this request, not an outage, so it
	// is `E-PAY-PROVIDER` and nothing retries into it.
	assert_eq!(code_of(&err), "E-PAY-PROVIDER");
	assert_eq!(err.retry(), Retry::Never);
}

/// The boundary Barion forces: HUF is whole forints there and minor units here, so a residual
/// is refused rather than truncated into a different amount than the invoice says.
#[tokio::test]
async fn huf_filler_never_reaches_the_gateway() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Errors": [] })))
		.mount(&server)
		.await;

	let err = provider(&server.uri()).start(&payment(10_050, "HUF")).await.unwrap_err();
	assert_eq!(code_of(&err), "E-PAY-ROUNDING");
	// EUR has two decimals at both ends, so the same amount goes through.
	let err = provider(&server.uri()).start(&payment(10_050, "EUR")).await.unwrap_err();
	assert_eq!(code_of(&err), "E-PAY-PROVIDER-DOWN", "no PaymentId in the reply, but it was sent");
}

#[tokio::test]
async fn an_unreachable_gateway_is_provider_down() {
	// A port nothing is listening on: bind one, learn its number, drop it.
	let port = {
		let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		l.local_addr().unwrap().port()
	};
	let err = provider(&format!("http://127.0.0.1:{port}"))
		.fetch_state("bar-1")
		.await
		.unwrap_err();
	assert_eq!(code_of(&err), "E-PAY-PROVIDER-DOWN");
	// The one case that never reached Barion at all, so the one that may be retried.
	assert_eq!(err.retry(), Retry::Backoff);
}

#[tokio::test]
async fn a_gateway_that_never_answers_is_provider_down() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(
			ResponseTemplate::new(200)
				.set_delay(FAST * 5)
				.set_body_json(json!({ "Status": "Succeeded", "Errors": [] })),
		)
		.mount(&server)
		.await;

	let err = provider(&server.uri()).fetch_state("bar-1").await.unwrap_err();
	assert_eq!(code_of(&err), "E-PAY-PROVIDER-DOWN");
	// The request may have landed, and Barion's `Refund` carries no idempotency token, so a
	// blind retry refunds twice. Only a refused connection is `Retry::Backoff`.
	assert_eq!(err.retry(), Retry::Never);
}

/// What an edge in front of Barion refuses with: HTML, no `Retry-After`, and nothing that
/// parses as JSON. The 429 that produced these tests carried a Cloudflare `server` header and
/// no `Retry-After` at all.
fn edge_refusal(status: u16) -> ResponseTemplate {
	ResponseTemplate::new(status).set_body_raw(
		format!("<html><body><h1>{status} Too Many Requests</h1></body></html>"),
		"text/html",
	)
}

/// A 429 with an HTML body is a throttle, not a parse failure: the status is judged before the
/// body, and only the status says who refused.
#[tokio::test]
async fn a_429_is_a_throttle_not_a_parse_failure() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(edge_refusal(429).insert_header("retry-after", "30"))
		.mount(&server)
		.await;

	let err = provider(&server.uri()).fetch_state("bar-1").await.unwrap_err();
	assert_eq!(code_of(&err), "E-PAY-PROVIDER-DOWN");
	assert_eq!(err.retry(), Retry::Backoff);
	assert!(err.to_string().contains("429"), "{err}");
}

/// The edge sends no `Retry-After`, and a refusal that names no window is still a refusal: the
/// default is NAV's own 60 seconds rather than no gate at all.
#[tokio::test]
async fn a_429_without_retry_after_waits_the_default_out() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(edge_refusal(429))
		.mount(&server)
		.await;

	let err = provider(&server.uri()).fetch_state("bar-1").await.unwrap_err();
	assert_eq!(err.retry(), Retry::Backoff);
	assert!(err.to_string().contains("60s"), "{err}");
}

/// The stated window is the gateway's and is not trusted: a `Retry-After` of a day would stop
/// payments for a day, so it is clamped to `MAX_THROTTLE_SECS`.
#[tokio::test]
async fn a_hostile_retry_after_is_clamped() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(edge_refusal(429).insert_header("retry-after", "86400"))
		.mount(&server)
		.await;

	let err = provider(&server.uri()).fetch_state("bar-1").await.unwrap_err();
	assert_eq!(err.retry(), Retry::Backoff);
	assert!(err.to_string().contains("900s"), "the stated window was not clamped: {err}");
}

/// A 429 whose body parses into an `Errors` document is still a throttle: the status is judged
/// first, so body shape cannot split one condition into two retry classes.
#[tokio::test]
async fn a_json_429_is_still_retryable() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(ResponseTemplate::new(429).set_body_json(json!({
			"Errors": [{ "ErrorCode": "TooManyRequests", "Title": "Slow down" }],
		})))
		.mount(&server)
		.await;

	let err = provider(&server.uri()).fetch_state("bar-1").await.unwrap_err();
	assert_eq!(err.retry(), Retry::Backoff);
}

/// The NAV rule, on the other gateway: a 503 that names its window is a maintenance pause, and
/// one that does not is an outage with the caller's own backoff — the difference is whether the
/// adapter is told how long to stay away.
#[tokio::test]
async fn a_5xx_with_retry_after_throttles_and_one_without_does_not() {
	for (header, throttled) in [(Some("45"), true), (None, false)] {
		let server = MockServer::builder().start().await;
		let mut reply = edge_refusal(503);
		if let Some(header) = header {
			reply = reply.insert_header("retry-after", header);
		}
		Mock::given(method("GET"))
			.and(path("/v2/Payment/GetPaymentState"))
			.respond_with(reply)
			.mount(&server)
			.await;

		let err = provider(&server.uri()).fetch_state("bar-1").await.unwrap_err();
		assert_eq!(err.retry(), Retry::Backoff, "{header:?}");
		assert_eq!(err.to_string().contains("throttled"), throttled, "{err}");
		assert!(err.to_string().contains("503"), "{err}");
	}
}

/// The gate: once the gateway has refused, the next caller does not touch the network — and it
/// is asked about another payment, because a quota belongs to the shop, not to one payment. This
/// is what stops the return leg and the sweep from spending a quota that is already gone.
#[tokio::test]
async fn a_throttle_stops_further_calls() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(edge_refusal(429))
		.mount(&server)
		.await;

	let p = provider(&server.uri());
	assert_eq!(p.fetch_state("bar-1").await.unwrap_err().retry(), Retry::Backoff);
	let second = p.fetch_state("bar-2").await.unwrap_err();
	assert_eq!(code_of(&second), "E-PAY-PROVIDER-DOWN");
	assert_eq!(second.retry(), Retry::Backoff);
	assert_eq!(
		server.received_requests().await.unwrap().len(),
		1,
		"a call went out while the gateway was throttling"
	);
}

/// A 500 that is not a throttle and carries no JSON still names its status, and stays retryable.
#[tokio::test]
async fn a_500_with_an_html_body_names_its_status() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(edge_refusal(500))
		.mount(&server)
		.await;

	let err = provider(&server.uri()).fetch_state("bar-1").await.unwrap_err();
	assert_eq!(code_of(&err), "E-PAY-PROVIDER-DOWN");
	assert_eq!(err.retry(), Retry::Backoff);
	assert!(err.to_string().contains("500"), "{err}");
}

/// The IPN is a ping naming a payment, in whichever of the two shapes Barion's documentation
/// is currently on. Nothing in it is trusted — `fetch_state` is the authority.
#[tokio::test]
async fn a_callback_yields_only_a_reference() {
	let p = provider("https://api.test.barion.com");
	let headers = axum::http::HeaderMap::new();
	assert_eq!(p.parse_callback(&headers, b"paymentId=bar-1").unwrap().provider_ref, "bar-1");
	assert_eq!(
		p.parse_callback(&headers, br#"{"PaymentId":"bar-2"}"#).unwrap().provider_ref,
		"bar-2"
	);
	assert_eq!(
		code_of(&p.parse_callback(&headers, b"nothing=here").unwrap_err()),
		"E-PAY-PROVIDER"
	);
}

/// Every `#[serde(rename)]` in `map.rs`, the HUF ÷100 in `amount_out` and `qty_out`'s 1e6
/// scaling, asserted on the wire: matching on method and path alone let a 100× money bug
/// through the whole suite.
#[tokio::test]
async fn the_start_body_carries_barions_own_spelling() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.and(body_partial_json(json!({
			"POSKey": "pos-key",
			"PaymentType": "Immediate",
			"PaymentRequestId": "req-1",
			"Currency": "HUF",
			"Locale": "hu-HU",
			"RedirectUrl": "https://shop.example.com/done",
			"CallbackUrl": "https://shop.example.com/api/webhook/barion",
			"PayerHint": "payer@example.com",
			"BillingAddress": { "Country": "HU", "City": "Budapest", "Zip": "1051" },
			"Transactions": [{ "Payee": "shop@example.com", "POSTransactionId": "req-1" }],
		})))
		// Whole forints and six-decimal quantity, as text: two `f64`s compare equal whatever the
		// rendering, and the rendering is the whole point.
		.and(body_string_contains(r#""Total":100"#))
		.and(body_string_contains(r#""Quantity":1.000000"#))
		.and(body_string_contains(r#""ItemTotal":100"#))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1", "Status": "Prepared", "Errors": [],
		})))
		.mount(&server)
		.await;

	provider(&server.uri()).start(&payment(10_000, "HUF")).await.unwrap();
}

/// EUR has two decimals at both ends, where HUF has none: the same `Money(10_000)` goes out as
/// `100.00` here and as `100` above.
#[tokio::test]
async fn a_two_decimal_currency_keeps_its_decimals() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.and(body_partial_json(json!({ "Currency": "EUR" })))
		.and(body_string_contains(r#""Total":100.00"#))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1", "Status": "Prepared", "Errors": [],
		})))
		.mount(&server)
		.await;

	provider(&server.uri()).start(&payment(10_000, "EUR")).await.unwrap();
}

/// Barion's own `PaymentWindow` default is 30 minutes and it is never the one the operator
/// chose, so an ordinary payment states it — a plain payment carries `ReservationPeriod`
/// nowhere. `d.hh:mm:ss`, Barion's `TimeSpan`, the same shape as `RESERVATION_PERIOD`.
#[tokio::test]
async fn a_start_names_its_payment_window() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.and(body_partial_json(json!({
			"PaymentType": "Immediate",
			"PaymentWindow": "0.00:10:00",
		})))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1", "Status": "Prepared", "Errors": [],
		})))
		.mount(&server)
		.await;

	let started = provider(&server.uri()).start(&payment(10_000, "HUF")).await.unwrap();
	assert_eq!(started.state, PaymentState::Pending);
}

/// A reservation is a different `PaymentType` and carries the period Barion holds the funds
/// for; a plain payment sends neither.
#[tokio::test]
async fn a_reservation_names_its_period() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.and(body_partial_json(json!({
			"PaymentType": "Reservation",
			"ReservationPeriod": "1.00:00:00",
		})))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1", "Status": "Reserved", "Errors": [],
		})))
		.mount(&server)
		.await;

	let started = provider(&server.uri())
		.start(&StartPayment { reserve: true, ..payment(10_000, "HUF") })
		.await
		.unwrap();
	assert_eq!(started.state, PaymentState::Reserved);
}

/// Barion has one `Start` endpoint for both flows and tells them apart by `RecurrenceId`, so a
/// recurring charge that lost the field would silently become a payer-present payment.
#[tokio::test]
async fn a_recurring_charge_names_its_token() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.and(body_partial_json(json!({
			"RecurrenceId": "tok-1",
			"RecurrenceType": "MerchantInitiatedPayment",
			// A stored-token charge is never a reservation, whatever the caller asked for.
			"PaymentType": "Immediate",
		})))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1", "Status": "Succeeded", "Errors": [],
		})))
		.mount(&server)
		.await;

	let started = provider(&server.uri())
		.charge_recurring("tok-1", &StartPayment { reserve: true, ..payment(10_000, "HUF") })
		.await
		.unwrap();
	assert_eq!(started.state, PaymentState::Succeeded);
	assert_eq!(started.provider_ref, "bar-1");
}

#[tokio::test]
async fn an_initial_recurring_payment_names_its_type() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.and(body_partial_json(json!({
			"RecurrenceId": "rec-1",
			"RecurrenceType": "MerchantInitiatedPayment",
			"InitiateRecurrence": true,
		})))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1", "Status": "Prepared", "Errors": [],
		})))
		.mount(&server)
		.await;

	provider(&server.uri())
		.start(&StartPayment { recurrence: Some("rec-1".into()), ..payment(10_000, "HUF") })
		.await
		.unwrap();
}

/// The refund body's own ÷100: `Money(20_000)` is 200 forints there.
#[tokio::test]
async fn the_refund_body_carries_whole_forints() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1",
			"Status": "Succeeded",
			"Currency": "HUF",
			"Transactions": [
				{ "TransactionId": "txn-1", "POSTransactionId": "req-1", "TransactionType": "Shop" },
			],
			"Errors": [],
		})))
		.mount(&server)
		.await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Refund"))
		.and(body_partial_json(json!({
			"POSKey": "pos-key",
			"PaymentId": "bar-1",
			// The caller's idempotency key, not the original transaction's: Barion dedupes on
			// `POSTransactionId`, and a refund pays out before anything records it.
			"TransactionsToRefund": [{ "TransactionId": "txn-1", "POSTransactionId": "pay_x:0" }],
		})))
		.and(body_string_contains(r#""AmountToRefund":200"#))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"RefundedTransactions": [{ "Total": 200, "Status": "Succeeded" }],
			"Errors": [],
		})))
		.mount(&server)
		.await;

	let res = provider(&server.uri()).refund("bar-1", Money(20_000), "pay_x:0").await.unwrap();
	assert_eq!(res.refunded, Money(20_000));
}

/// `mintworks_core::http` hands the status back rather than judging it, so the adapter has to. A
/// non-2xx body with no `Errors` array deserializes cleanly, and `refund` used to answer
/// `Ok(RefundResult { refunded: ZERO })` — a refund the gateway never made, booked as done.
#[tokio::test]
async fn a_reply_that_refunds_nothing_is_never_ok() {
	// 502 on the state read: `fetch_state` and the refund's own first leg both fail.
	let down = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(ResponseTemplate::new(502).set_body_json(json!({})))
		.mount(&down)
		.await;
	assert_eq!(
		code_of(&provider(&down.uri()).fetch_state("bar-1").await.unwrap_err()),
		"E-PAY-PROVIDER-DOWN"
	);
	assert_eq!(
		code_of(
			&provider(&down.uri())
				.refund("bar-1", Money(20_000), "pay_x:0")
				.await
				.unwrap_err()
		),
		"E-PAY-PROVIDER-DOWN"
	);

	// The state read is fine; the Refund POST is the one that fails, first with a 502 and then
	// with a 200 naming no refunded amount at all.
	for refund_reply in [
		ResponseTemplate::new(502).set_body_json(json!({})),
		ResponseTemplate::new(200).set_body_json(json!({ "RefundedTransactions": [] })),
	] {
		let server = MockServer::builder().start().await;
		Mock::given(method("GET"))
			.and(path("/v2/Payment/GetPaymentState"))
			.respond_with(ResponseTemplate::new(200).set_body_json(json!({
				"PaymentId": "bar-1",
				"Status": "Succeeded",
				"Currency": "HUF",
				"Transactions": [{ "TransactionId": "txn-1", "TransactionType": "Shop" }],
				"Errors": [],
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(path("/v2/Payment/Refund"))
			.respond_with(refund_reply)
			.mount(&server)
			.await;

		let err = provider(&server.uri())
			.refund("bar-1", Money(20_000), "pay_x:0")
			.await
			.unwrap_err();
		assert_eq!(code_of(&err), "E-PAY-PROVIDER-DOWN");
	}
}

/// Barion cross-checks the item block: `ItemTotal` must be `Quantity × UnitPrice` and the items
/// must sum to the transaction `Total`. The old fixture had one line whose `unit_price` equalled
/// its `total` and equalled the whole transaction, so it could not see either rule break.
#[tokio::test]
async fn the_items_are_consistent_and_sum_to_the_total() {
	let server = MockServer::builder().start().await;
	Mock::given(method("POST"))
		.and(path("/v2/Payment/Start"))
		.and(body_string_contains(r#""Total":150"#))
		.and(body_string_contains(r#""ItemTotal":100"#))
		.and(body_string_contains(r#""ItemTotal":50"#))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1", "Status": "Prepared", "Errors": [],
		})))
		.mount(&server)
		.await;

	let item = |name: &str, minor: i64| PaymentItem {
		name: name.to_owned(),
		description: None,
		qty: Qty(1_000_000),
		unit: "db".to_owned(),
		unit_price: Money(minor),
		total: Money(minor),
	};
	let req = StartPayment {
		amount: Money(15_000),
		items: vec![item("2.000000 × ora Tanacsadas", 10_000), item("Kerekítés", 5_000)],
		..payment(15_000, "HUF")
	};
	// The caller's guarantee, asserted here because it is what the gateway rejects.
	assert_eq!(
		req.items.iter().map(|i| i.total.0).sum::<i64>(),
		req.amount.0,
		"the items must sum to the transaction total"
	);
	for i in &req.items {
		assert_eq!(i.unit_price, i.total, "one unit, so ItemTotal == Quantity × UnitPrice");
	}

	provider(&server.uri()).start(&req).await.unwrap();
}

/// The credential path, without a database: the branch that matters is the environment name,
/// and an unrecognised one must be an error rather than silently the sandbox — a gateway
/// quietly pointed at the sandbox takes no money and reports everything settled.
#[test]
fn an_unrecognised_environment_is_never_silently_the_sandbox() {
	assert_eq!(base_url_for("test").unwrap(), SANDBOX_BASE_URL);
	assert_eq!(base_url_for("production").unwrap(), PRODUCTION_BASE_URL);
	// Whitespace is an operator's `.env` and not a third environment.
	assert_eq!(base_url_for("  production \n").unwrap(), PRODUCTION_BASE_URL);
	// `sandbox`/`prod` are gone with `payment.barion.env`: `deployment.env` admits two names.
	for bad in ["", "   ", "sandbox", "Test", "prod"] {
		assert!(matches!(base_url_for(bad), Err(Error::Internal(_))), "{bad:?} was accepted");
	}
}

/// A deferred provider that nobody attached must fail loudly on first use rather than reach a
/// gateway with empty credentials.
#[tokio::test]
async fn an_unattached_provider_refuses_to_call_out() {
	let err = BarionProvider::deferred().fetch_state("bar-1").await.unwrap_err();
	assert!(matches!(err, Error::Internal(_)), "{err:?}");
}

/// A POS key in a log is a merchant-account compromise: it starts and refunds payments on the
/// account. `Credentials` is public and re-exported, so one `tracing::debug!` in a consumer's
/// own wiring was all it took.
#[test]
fn the_pos_key_is_never_in_a_debug_rendering() {
	let creds = mintworks_payment_barion::Credentials {
		base_url: SANDBOX_BASE_URL.to_owned(),
		pos_key: "live-pos-key-4f2a".to_owned(),
		payee: "shop@example.com".to_owned(),
	};
	let rendered = format!("{creds:?}");
	assert!(!rendered.contains("live-pos-key-4f2a"), "{rendered}");
	assert!(rendered.contains("<redacted>"), "{rendered}");
}

/// Barion refunds a *transaction*, not a payment, so guessing the leg when none is typed `Shop`
/// could refund a fee or payout leg. An error is safe: the caller retries under the same
/// `POSTransactionId`, which Barion dedupes.
#[tokio::test]
async fn a_reply_with_no_shop_transaction_is_a_fault() {
	let server = MockServer::builder().start().await;
	Mock::given(method("GET"))
		.and(path("/v2/Payment/GetPaymentState"))
		.respond_with(ResponseTemplate::new(200).set_body_json(json!({
			"PaymentId": "bar-1",
			"Status": "Succeeded",
			"Currency": "HUF",
			"Transactions": [
				{ "TransactionId": "txn-fee", "TransactionType": "Fee" },
				{ "TransactionId": "txn-out", "TransactionType": "Payout" },
			],
			"Errors": [],
		})))
		.mount(&server)
		.await;
	// No Refund mock at all: a request that reached it would be an unmatched call, and wiremock
	// answers those 404 — so `verify` below is what proves none was sent.
	let err = provider(&server.uri())
		.refund("bar-1", Money(20_000), "pay_x:0")
		.await
		.unwrap_err();
	assert_eq!(code_of(&err), "E-PAY-PROVIDER-DOWN");
	assert!(
		!server
			.received_requests()
			.await
			.unwrap()
			.iter()
			.any(|r| r.url.path().contains("Refund")),
		"nothing may be refunded when the leg is a guess"
	);
}

// vim: ts=4
