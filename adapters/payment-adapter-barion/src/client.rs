//! The Barion calls, behind `saas_billing::PaymentProvider`.
//!
//! `/v2/Payment/Start`, `/v2/Payment/GetPaymentState`, `/v2/Payment/Refund` and the
//! merchant-initiated recurring charge, over the one process-wide HTTPS client in
//! `saas_core::http`. Nothing here opens a connection of its own or reads a table of ours; the
//! credentials come from [`Credentials::resolve`] — settings and the encrypted `secrets` table
//! — either eagerly ([`BarionProvider::load`]) or on first use ([`BarionProvider::deferred`]),
//! and everything after that runs off plain fields, which keeps `tests/barion.rs` database-free.

use std::sync::OnceLock;
use std::time::Duration;

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
		}
	}

	/// A provider whose credentials are read on first use. Construct it before `build()`,
	/// register it, then hand it the `App` with [`Self::attach`] from `AppBuilder::on_init`.
	#[must_use]
	pub fn deferred() -> Self {
		Self { fixed: None, app: OnceLock::new(), cell: OnceLock::new(), timeout: DEFAULT_TIMEOUT }
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
	/// judged. `Errors` first: a Barion business fault is a 4xx carrying an `Errors` array, and
	/// its own message is the useful one. Without the status check a non-2xx body with no
	/// `Errors` deserialized cleanly and a refund the gateway never made was booked as a
	/// successful zero refund.
	fn judge(status: StatusCode, errors: &[map::BarionError]) -> ClResult<()> {
		map::check_errors(errors)?;
		if !status.is_success() {
			return Err(Self::fault(&format!("HTTP {status}")));
		}
		Ok(())
	}

	fn decode<T: DeserializeOwned>(status: StatusCode, body: &[u8]) -> ClResult<T> {
		serde_json::from_slice(body).map_err(|e| {
			tracing::warn!(%status, why = %e, "barion reply did not parse");
			Error::coded_retry(
				StatusCode::BAD_GATEWAY,
				"E-PAY-PROVIDER-DOWN",
				"barion: unreadable reply",
			)
		})
	}

	async fn post<T: DeserializeOwned>(
		&self,
		path: &str,
		body: &impl Serialize,
	) -> ClResult<(StatusCode, T)> {
		let uri = format!("{}{path}", self.creds().await?.base_url);
		let body = serde_json::to_vec(body)
			.map_err(|e| Error::internal(format!("barion request: {e}")))?;
		let (status, _, bytes) =
			http::post(&uri, &[("content-type", "application/json")], body, self.timeout)
				.await
				.map_err(|e| Self::down(&e))?;
		Ok((status, Self::decode(status, &bytes)?))
	}

	async fn get<T: DeserializeOwned>(&self, path: &str) -> ClResult<(StatusCode, T)> {
		let uri = format!("{}{path}", self.creds().await?.base_url);
		let (status, _, bytes) = http::get(&uri, &[("accept", "application/json")], self.timeout)
			.await
			.map_err(|e| Self::down(&e))?;
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

	async fn charge_recurring(&self, token: &str, req: &StartPayment) -> ClResult<PaymentState> {
		let res = self.start_payment(req, Some(token)).await?;
		let status = res.status.ok_or_else(|| Self::fault("no Status in the reply"))?;
		map::payment_state(&status)
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
