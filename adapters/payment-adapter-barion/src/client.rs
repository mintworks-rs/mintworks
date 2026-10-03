//! The Barion calls, behind `saas_billing::PaymentProvider`.
//!
//! `/v2/Payment/Start`, `/v2/Payment/GetPaymentState`, `/v2/Payment/Refund` and the
//! merchant-initiated recurring charge, over the one process-wide HTTPS client in
//! `saas_core::http`. Nothing here opens a connection of its own or reads a table of ours; the
//! credentials come from [`Credentials::resolve`] — settings and the encrypted `secrets` table
//! — either eagerly ([`BarionProvider::load`]) or on first use ([`BarionProvider::deferred`]),
//! and everything after that runs off plain fields, which keeps `tests/barion.rs` database-free.

use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::HeaderMap;
use saas_billing::{
	CallbackRef, PaymentProvider, PaymentState, ProviderCaps, RefundResult, StartPayment,
	StartedPayment,
};
use saas_core::{App, error::StatusCode, http, prelude::*};
use serde::{Serialize, de::DeserializeOwned};

use crate::map;

/// `payments.provider`, and the `{provider}` segment of the callback URL.
pub const PROVIDER_ID: &str = "barion";

pub const SANDBOX_BASE_URL: &str = "https://api.test.barion.com";
pub const PRODUCTION_BASE_URL: &str = "https://api.barion.com";

/// Which of the two [`BarionProvider::load`] picks. Not a `payment.barion.*` key of its own:
/// the deployment's one environment flag, so the gateway cannot end up in the sandbox while NAV
/// files into the statutory system.
const ENV_SETTING: &str = "deployment.env";
/// The shop's Barion account e-mail, which every transaction has to name as its payee.
const PAYEE_SETTING: &str = "payment.barion.payee";
/// Namespaced like [`PAYEE_SETTING`] beside it, so one `PAYMENT_BARION_*` family covers both
/// credentials. `payment.` is a settings *prefix* family, so this name also resolves as a
/// setting — harmless, `SecretStore` never reads `settings`, but a `PUT` there does nothing.
pub(crate) const POS_KEY_SECRET: &str = "payment.barion.pos_key";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// What a 429 carrying no `Retry-After` is waited out for — NAV sends 60 for the same
/// condition. [`MAX_THROTTLE_SECS`] bounds a stated one: the header is the gateway's, and a
/// wrong or hostile value there must not stop payments for a day.
const DEFAULT_THROTTLE_SECS: u64 = 60;
const MAX_THROTTLE_SECS: u64 = 900;

/// How much of a refused reply is logged. Enough to tell an edge's HTML page from a Barion
/// `Errors` document, which is the whole of the diagnosis a 429 permits.
const THROTTLE_BODY_BYTES: usize = 256;

/// [`ENV_SETTING`] as a base URL: this adapter's half of the deployment's one environment flag,
/// mapped onto Barion's vocabulary. `deployment.env` defaults to `production` because the
/// failure to guard against is a gateway pointed at the *sandbox*, where the payer pays nothing
/// and everything looks settled.
///
/// No blank arm: the setting is a validated `choice` with a default and cannot arrive empty.
///
/// # Errors
/// `Error::Internal` for anything but `test` and `production`.
pub fn base_url_for(env: &str) -> ClResult<&'static str> {
	match env.trim() {
		"test" => Ok(SANDBOX_BASE_URL),
		"production" => Ok(PRODUCTION_BASE_URL),
		other => Err(Error::internal(format!(
			"setting '{ENV_SETTING}' must be 'test' or 'production', not '{other}'"
		))),
	}
}

/// What every call needs, and the only thing read from the database.
#[derive(Clone)]
pub struct Credentials {
	pub base_url: String,
	pub pos_key: String,
	pub payee: String,
}

impl std::fmt::Debug for Credentials {
	/// Redacts `pos_key`, as `saas_core::config::Config` does `master_key`: a POS key in a log is
	/// a merchant-account compromise — it starts and refunds payments on the account.
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Credentials")
			.field("base_url", &self.base_url)
			.field("pos_key", &"<redacted>")
			.field("payee", &self.payee)
			.finish()
	}
}

impl Credentials {
	/// The one credential path: setting `deployment.env`, setting `payment.barion.payee`
	/// and secret `payment.barion.pos_key` — the last encrypted at rest under `MASTER_KEY`,
	/// which a process environment variable is not.
	///
	/// # Errors
	/// `Error::Internal` when the environment is unrecognised ([`base_url_for`]), the secret is
	/// unset or not text, or the payee is blank.
	pub async fn resolve(app: &App) -> ClResult<Self> {
		let base_url = base_url_for(&app.settings.text(ENV_SETTING).await?)?;
		let pos_key = app
			.secrets
			.get(POS_KEY_SECRET)
			.await?
			.ok_or_else(|| Error::internal(format!("secret '{POS_KEY_SECRET}' is not set")))?;
		let pos_key = String::from_utf8(pos_key)
			.map_err(|_| Error::internal(format!("secret '{POS_KEY_SECRET}' is not text")))?;
		let payee = app.settings.text(PAYEE_SETTING).await?;
		if payee.trim().is_empty() {
			return Err(Error::internal(format!("setting '{PAYEE_SETTING}' is not set")));
		}
		Ok(Self { base_url: base_url.to_owned(), pos_key, payee })
	}
}

pub struct BarionProvider {
	/// `Some` when the credentials were supplied outright — [`BarionProvider::new`] and
	/// [`BarionProvider::load`]. `None` defers them to [`BarionProvider::deferred`]'s cell.
	fixed: Option<Credentials>,
	/// Set once by [`BarionProvider::attach`]. `AppBuilder::build` freezes the extension map
	/// into an `Arc<AppState>`, so no `App` exists when the provider is constructed — which is
	/// what pushed the POS key out into the process environment before this.
	app: OnceLock<App>,
	cell: OnceLock<Credentials>,
	timeout: Duration,
	/// Set by [`Self::throttle`], read by [`Self::gate`]. Provider-wide: a quota belongs to the
	/// shop and its POS key, so one refused payment means they all are. Process-local and
	/// accepted: the truth is the gateway's, and a restart re-learns it in one call.
	throttled_until: Mutex<Option<Instant>>,
}

impl BarionProvider {
	pub fn new(
		base_url: impl Into<String>,
		pos_key: impl Into<String>,
		payee: impl Into<String>,
	) -> Self {
		Self {
			fixed: Some(Credentials {
				base_url: base_url.into().trim_end_matches('/').to_owned(),
				pos_key: pos_key.into(),
				payee: payee.into(),
			}),
			app: OnceLock::new(),
			cell: OnceLock::new(),
			timeout: DEFAULT_TIMEOUT,
			throttled_until: Mutex::new(None),
		}
	}

	/// A provider whose credentials are read on first use. Construct it before `build()`,
	/// register it, then hand it the `App` with [`Self::attach`] from `AppBuilder::on_init`.
	#[must_use]
	pub fn deferred() -> Self {
		Self {
			fixed: None,
			app: OnceLock::new(),
			cell: OnceLock::new(),
			timeout: DEFAULT_TIMEOUT,
			throttled_until: Mutex::new(None),
		}
	}

	/// Hands a [`Self::deferred`] provider the `App` its credentials live in. Second and later
	/// calls are ignored: the first `App` is the one this process runs on.
	pub fn attach(&self, app: &App) {
		let _ = self.app.set(app.clone());
	}

	/// The credentials, resolved at most once. An unattached deferred provider is an
	/// `Error::internal` rather than a silent sandbox.
	async fn creds(&self) -> ClResult<&Credentials> {
		if let Some(fixed) = &self.fixed {
			return Ok(fixed);
		}
		if let Some(c) = self.cell.get() {
			return Ok(c);
		}
		let app = self
			.app
			.get()
			.ok_or_else(|| Error::internal("barion: the provider was never attached to an App"))?;
		// `OnceLock`, not an async cell, so this crate needs no runtime dependency: two racing
		// first calls both resolve and one wins the set, which costs a second settings read and
		// nothing else.
		let resolved = Credentials::resolve(app).await?;
		Ok(self.cell.get_or_init(|| resolved))
	}

	/// Only the tests use this: a mock server answering instantly does not exercise the
	/// deadline, and a real one is 30 seconds long.
	#[must_use]
	pub fn with_timeout(mut self, timeout: Duration) -> Self {
		self.timeout = timeout;
		self
	}

	/// [`Credentials::resolve`] eagerly, for a caller that already holds an `App`.
	///
	/// # Errors
	/// Whatever [`Credentials::resolve`] raises: an unset secret, a blank payee, or a
	/// `deployment.env` that is neither `test` nor `production`.
	pub async fn load(app: &App) -> ClResult<Self> {
		let c = Credentials::resolve(app).await?;
		Ok(Self::new(c.base_url, c.pos_key, c.payee))
	}

	/// The refusal this adapter returns while, and after, the gateway has told it to wait. A
	/// throttled gateway *is* a gateway not serving us, so the code does not change; the retry
	/// class does, because the next attempt is worth making.
	fn throttled(why: &str) -> Error {
		Error::coded_retry(
			StatusCode::BAD_GATEWAY,
			"E-PAY-PROVIDER-DOWN",
			format!("barion: throttled for {why}"),
		)
	}

	/// A poisoned lock is a window, not a corrupt value: whoever panicked held it for the length
	/// of one assignment.
	fn lock_gate(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
		self.throttled_until.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Refuses the call without touching the network while the window is open. This is what
	/// stops a poll from spending a quota that is already gone: the invoice page's read is cheap,
	/// but the return leg behind it and the five-minute sweep are not, and they race the window.
	fn gate(&self) -> ClResult<()> {
		let Some(until) = *self.lock_gate() else { return Ok(()) };
		let now = Instant::now();
		if until <= now {
			return Ok(());
		}
		let wait = (until - now).as_secs();
		// `debug`, not `warn`: the window logged once when it opened, and every refusal after
		// that is the gate working.
		tracing::debug!(retry_after = wait, "barion is throttled; not calling");
		Err(Self::throttled(&format!("{wait}s")))
	}

	/// Records a throttle and answers the error the caller returns, warning once per window: the
	/// requests it causes stop at [`Self::gate`] and never reach the gateway. `body` is lossy
	/// UTF-8 — an edge refuses with HTML and Barion with JSON, and the first line says which.
	fn throttle(
		&self,
		status: StatusCode,
		retry_after: Option<u64>,
		asked: u64,
		body: &[u8],
	) -> Error {
		let wait = asked.min(MAX_THROTTLE_SECS);
		let now = Instant::now();
		let mut gate = self.lock_gate();
		if gate.is_none_or(|until| until <= now) {
			tracing::warn!(
				%status,
				retry_after,
				body = %String::from_utf8_lossy(&body[..body.len().min(THROTTLE_BODY_BYTES)]),
				"barion refused the call"
			);
		}
		*gate = Some(now + Duration::from_secs(wait));
		Self::throttled(&format!("{wait}s after HTTP {status}"))
	}

	/// The status, judged **before** the body is parsed, as `saas_nav::auth::post` does. A body
	/// first would split one condition into three classes — an HTML page unreadable, a JSON body a
	/// business fault — and name the status in none of them.
	fn throttle_delay(status: StatusCode, retry_after: Option<u64>) -> Option<u64> {
		match status {
			StatusCode::TOO_MANY_REQUESTS => Some(retry_after.unwrap_or(DEFAULT_THROTTLE_SECS)),
			// A 503 carrying `Retry-After` is a maintenance window, not a blind backoff.
			StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE => retry_after,
			_ => None,
		}
	}

	/// A transport failure is the gateway's, not the caller's: `E-PAY-PROVIDER-DOWN`, 502, with
	/// the path already logged by `saas_core::http` (the query string is dropped there — this
	/// adapter puts its POS key in one).
	///
	/// Retryable **only** for [`Error::Unavailable`], a connect failure that never landed.
	/// `Error::Timeout` may have landed, and Barion's `Refund` carries no idempotency token, so
	/// a blind retry refunds twice — the same distinction `saas_core::http` keeps because
	/// classifying it the other way resent a NAV filing NAV already held.
	fn down(e: &Error) -> Error {
		match e {
			Error::Unavailable(_) => Error::coded_retry(
				StatusCode::BAD_GATEWAY,
				"E-PAY-PROVIDER-DOWN",
				format!("barion: {e}"),
			),
			_ => {
				Error::coded(StatusCode::BAD_GATEWAY, "E-PAY-PROVIDER-DOWN", format!("barion: {e}"))
			}
		}
	}

	/// A reply that parsed but says something impossible. Not an outage: the gateway answered,
	/// and a retry gets the same answer.
	fn fault(what: &str) -> Error {
		Error::coded(StatusCode::BAD_GATEWAY, "E-PAY-PROVIDER-DOWN", format!("barion: {what}"))
	}

	/// `saas_core::http` returns the status rather than judging it, so this is where it is
	/// judged — for a reply [`Self::throttle_delay`] did not claim. `Errors` first: a Barion
	/// business fault is a 4xx carrying an `Errors` array, and its own message is the useful one.
	/// Without the status check a non-2xx body with no `Errors` deserialized cleanly and a refund
	/// the gateway never made was booked as a successful zero refund.
	fn judge(status: StatusCode, errors: &[map::BarionError]) -> ClResult<()> {
		map::check_errors(errors)?;
		if !status.is_success() {
			return Err(Self::fault(&format!("HTTP {status}")));
		}
		Ok(())
	}

	/// A body that does not parse is a retryable outage either way, and the status says which: a
	/// 2xx holds a reply we cannot read, a non-2xx an edge refusing the request. A non-2xx that
	/// *does* parse goes to [`Self::judge`], which is where a fault Barion named itself is.
	fn decode<T: DeserializeOwned>(status: StatusCode, body: &[u8]) -> ClResult<T> {
		serde_json::from_slice(body).map_err(|e| {
			tracing::warn!(%status, why = %e, "barion reply did not parse");
			Error::coded_retry(
				StatusCode::BAD_GATEWAY,
				"E-PAY-PROVIDER-DOWN",
				if status.is_success() {
					"barion: unreadable reply".to_owned()
				} else {
					format!("barion: HTTP {status} with no readable error document")
				},
			)
		})
	}

	async fn post<T: DeserializeOwned>(
		&self,
		path: &str,
		body: &impl Serialize,
	) -> ClResult<(StatusCode, T)> {
		self.gate()?;
		let uri = format!("{}{path}", self.creds().await?.base_url);
		let body = serde_json::to_vec(body)
			.map_err(|e| Error::internal(format!("barion request: {e}")))?;
		let (status, retry_after, bytes) =
			http::post(&uri, &[("content-type", "application/json")], body, self.timeout)
				.await
				.map_err(|e| Self::down(&e))?;
		if let Some(delay) = Self::throttle_delay(status, retry_after) {
			return Err(self.throttle(status, retry_after, delay, &bytes));
		}
		Ok((status, Self::decode(status, &bytes)?))
	}

	async fn get<T: DeserializeOwned>(&self, path: &str) -> ClResult<(StatusCode, T)> {
		self.gate()?;
		let uri = format!("{}{path}", self.creds().await?.base_url);
		let (status, retry_after, bytes) =
			http::get(&uri, &[("accept", "application/json")], self.timeout)
				.await
				.map_err(|e| Self::down(&e))?;
		if let Some(delay) = Self::throttle_delay(status, retry_after) {
			return Err(self.throttle(status, retry_after, delay, &bytes));
		}
		Ok((status, Self::decode(status, &bytes)?))
	}

	/// `Start`, shared by the payer-present flow and the recurring charge — Barion has one
	/// endpoint for both and distinguishes them by `RecurrenceId`.
	async fn start_payment(
		&self,
		req: &StartPayment,
		recurrence: Option<&str>,
	) -> ClResult<map::StartResponse> {
		let c = self.creds().await?;
		let body = map::start_request(&c.pos_key, &c.payee, req, recurrence)?;
		let (status, res): (_, map::StartResponse) = self.post("/v2/Payment/Start", &body).await?;
		Self::judge(status, &res.errors)?;
		Ok(res)
	}

	async fn state(&self, provider_ref: &str) -> ClResult<map::StateResponse> {
		let path = format!(
			"/v2/Payment/GetPaymentState?POSKey={}&PaymentId={provider_ref}",
			self.creds().await?.pos_key
		);
		let (status, res): (_, map::StateResponse) = self.get(&path).await?;
		Self::judge(status, &res.errors)?;
		Ok(res)
	}
}

#[async_trait]
impl PaymentProvider for BarionProvider {
	fn id(&self) -> &str {
		PROVIDER_ID
	}

	fn capabilities(&self) -> ProviderCaps {
		ProviderCaps { reservation: true, recurring: true, partial_refund: true }
	}

	async fn start(&self, req: &StartPayment) -> ClResult<StartedPayment> {
		let res = self.start_payment(req, None).await?;
		let provider_ref =
			res.payment_id.ok_or_else(|| Self::fault("Start returned no PaymentId"))?;
		let status = res.status.as_deref().unwrap_or("Prepared");
		Ok(StartedPayment {
			provider_ref,
			redirect_url: res.gateway_url,
			state: map::payment_state(status)?,
		})
	}

	async fn fetch_state(&self, provider_ref: &str) -> ClResult<PaymentState> {
		let res = self.state(provider_ref).await?;
		let status = res.status.ok_or_else(|| Self::fault("no Status in the reply"))?;
		map::payment_state(&status)
	}

	async fn refund(
		&self,
		provider_ref: &str,
		amount: Money,
		request_id: &str,
	) -> ClResult<RefundResult> {
		// Barion refunds a *transaction*, not a payment, and the reply carries no payment
		// status — so the state read here is both where the transaction id comes from and
		// the state that comes back. It is unchanged by a refund: `REFUNDED` is the caller's
		// call, off `payments.refunded_amount`.
		let state = self.state(provider_ref).await?;
		let currency = CurrencyCode::parse(state.currency.as_deref().unwrap_or("HUF"))?;
		let before = map::payment_state(
			state.status.as_deref().ok_or_else(|| Self::fault("no Status in the reply"))?,
		)?;
		let txn = state
			.shop_transaction()
			.ok_or_else(|| Self::fault("no Shop transaction in the reply"))?;
		let transaction_id = txn
			.transaction_id
			.as_deref()
			.ok_or_else(|| Self::fault("transaction has no id"))?;

		let body = map::RefundRequest {
			pos_key: &self.creds().await?.pos_key,
			payment_id: provider_ref,
			transactions_to_refund: [map::RefundTarget {
				transaction_id,
				// The caller's idempotency key, not the original transaction's: Barion dedupes on
				// `POSTransactionId`, and the payout happens before anything records it, so a
				// retry has to be the same refund here rather than a second one.
				pos_transaction_id: request_id,
				amount_to_refund: map::amount_out(amount, &currency)?,
			}],
		};
		let (status, res): (_, map::RefundResponse) =
			self.post("/v2/Payment/Refund", &body).await?;
		Self::judge(status, &res.errors)?;

		// What the gateway says it gave back, not what we asked for: Barion may round a
		// refund up to the whole transaction, and the caller books `refunded_amount` off this.
		let mut refunded = Money::ZERO;
		for t in &res.refunded_transactions {
			if let Some(total) = t.total.as_deref() {
				refunded += map::amount_in(total)?;
			}
		}
		// A reply naming no amount is not a refund. Answering `Ok(ZERO)` recorded a successful
		// zero refund, which left `refunded_amount` at 0 and the payment refundable again.
		if refunded == Money::ZERO {
			return Err(Self::fault("the refund reply names no refunded amount"));
		}
		Ok(RefundResult { refunded, state: before })
	}

	async fn charge_recurring(&self, token: &str, req: &StartPayment) -> ClResult<StartedPayment> {
		let res = self.start_payment(req, Some(token)).await?;
		let provider_ref =
			res.payment_id.ok_or_else(|| Self::fault("Start returned no PaymentId"))?;
		let status = res.status.ok_or_else(|| Self::fault("no Status in the reply"))?;
		Ok(StartedPayment { provider_ref, redirect_url: None, state: map::payment_state(&status)? })
	}

	fn parse_callback(&self, _headers: &HeaderMap, body: &[u8]) -> ClResult<CallbackRef> {
		// Barion signs nothing on the IPN, so there is no cheap filter to apply here and the
		// body is parsed for one field and otherwise ignored. `fetch_state` is the authority.
		map::callback_payment_id(body)
			.map(|provider_ref| CallbackRef { provider_ref })
			.ok_or_else(|| {
				Error::coded(
					StatusCode::BAD_REQUEST,
					"E-PAY-PROVIDER",
					"barion: callback names no payment",
				)
			})
	}
}

// vim: ts=4
