//! SMTP delivery over lettre.
//!
//! Configuration lives in the `settings` table under `email.*`; the password is the only
//! part held in the encrypted `secrets` table, under `smtp.password`
//! (`claude-docs/db-schema.md` §`secrets`).

use std::sync::LazyLock;
use std::time::Duration;

use lettre::{
	AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
	message::{Mailbox, Message as Mail, MultiPart, SinglePart},
	transport::smtp::{
		authentication::Credentials,
		client::{Tls, TlsParameters},
	},
};
use parking_lot::Mutex;
use saas_core::{App, ClResult, Error, error::StatusCode};

/// A rendered mail, ready to hand to SMTP.
#[derive(Clone, Debug)]
pub struct Message {
	pub to: String,
	pub subject: String,
	pub text: String,
	pub html: String,
}

/// `PartialEq` is the cache key: the transport is rebuilt only when the operator changes one
/// of these. No `Debug`/`Display` — `password` must never reach a tracing call.
#[derive(Clone, PartialEq, Eq)]
struct Smtp {
	host: String,
	port: u16,
	username: String,
	password: String,
	from_address: String,
	from_name: String,
	tls_mode: String,
	timeout: Duration,
}

/// A misconfiguration met on the send path, as a **retryable** error.
///
/// Never `Error::Setting`, which is `Retry::Never`: an operator fixing the setting must recover
/// the mail already queued, and nothing in the workspace re-drives a `SEND_EMAIL` row, so a
/// terminal failure here loses an activation link for good. `check_email_settings` is what
/// catches these at boot; this is the residue, because a setting can change after boot.
fn unconfigured(msg: impl Into<String>) -> Error {
	Error::coded_retry(StatusCode::SERVICE_UNAVAILABLE, "E-EMAIL-UNCONFIGURED", msg)
}

/// Re-classes an [`Error::Setting`] raised by the settings read *itself*.
///
/// `Settings::get` parses on every read, so a hand-edited row or a `SAAS_*` override that
/// appeared after `check_email_settings` ran makes a plain `settings.text(…)` return
/// `Retry::Never` — burning the queued mail before any of the checks below are reached.
pub(crate) fn retryable(e: Error) -> Error {
	match e {
		Error::Setting(msg) => unconfigured(msg),
		other => other,
	}
}

/// `None` when SMTP is not configured. There is no separate on/off flag: email is not
/// optional, so an empty host is the only "not configured" signal. What that means for the
/// job is `send`'s business, not this function's.
async fn load(app: &App) -> ClResult<Option<Smtp>> {
	let host = app.settings.text("email.smtp.host").await?;
	if host.is_empty() {
		return Ok(None);
	}
	let port = u16::try_from(app.settings.int("email.smtp.port").await?)
		.map_err(|_| unconfigured("email.smtp.port is out of range"))?;
	let timeout = u64::try_from(app.settings.int("email.smtp.timeout_seconds").await?)
		.map_err(|_| unconfigured("email.smtp.timeout_seconds is out of range"))?;
	let password = match app.secrets.get("smtp.password").await? {
		None => String::new(),
		Some(bytes) => String::from_utf8(bytes)
			.map_err(|_| unconfigured("secret 'smtp.password' is not UTF-8"))?,
	};
	let username = app.settings.text("email.smtp.username").await?;
	checked_credentials(&username, &password)?;
	Ok(Some(Smtp {
		host,
		port,
		username,
		password,
		from_address: app.settings.text("email.from").await?,
		from_name: app.settings.text("email.from.name").await?,
		tls_mode: app.settings.text("email.smtp.tls_mode").await?,
		timeout: Duration::from_secs(timeout),
	}))
}

/// An empty host is "not configured"; a username with no password is a *lost secret* — a
/// `MASTER_KEY` rotation or a never-seeded row — and `transport` would then authenticate with
/// `""`, which only earns a relay-side lockout.
fn checked_credentials(username: &str, password: &str) -> ClResult<()> {
	if !username.is_empty() && password.is_empty() {
		return Err(unconfigured("secret 'smtp.password' is unset"));
	}
	Ok(())
}

fn build(msg: &Message, cfg: &Smtp) -> ClResult<Mail> {
	// RFC 5322: the display name is quoted, so its own quotes and backslashes escape.
	let name = cfg.from_name.replace('\\', "\\\\").replace('"', "\\\"");
	let from: Mailbox = format!("\"{name}\" <{}>", cfg.from_address)
		.parse()
		.map_err(|_| unconfigured(format!("email.from is not an address: {}", cfg.from_address)))?;
	let to: Mailbox = msg.to.parse().map_err(|_| Error::validation("invalid recipient address"))?;
	Mail::builder()
		.from(from)
		.to(to)
		.subject(&msg.subject)
		.multipart(
			MultiPart::alternative()
				.singlepart(SinglePart::plain(msg.text.clone()))
				.singlepart(SinglePart::html(msg.html.clone())),
		)
		.map_err(|e| Error::internal(format!("cannot build email: {e}")))
}

/// The transport for `email.smtp.tls_mode`. `starttls` **requires** the upgrade: lettre's
/// opportunistic mode continues in the clear when the server does not advertise STARTTLS, and
/// the credentials attached below would go with it. `none` is the explicit plaintext opt-out.
fn transport(cfg: &Smtp) -> ClResult<AsyncSmtpTransport<Tokio1Executor>> {
	let params = || {
		TlsParameters::builder(cfg.host.clone())
			.build()
			.map_err(|e| unconfigured(format!("TLS configuration error: {e}")))
	};
	let tls = match cfg.tls_mode.as_str() {
		"tls" => Tls::Wrapper(params()?),
		"starttls" => Tls::Required(params()?),
		"none" => Tls::None,
		other => {
			return Err(unconfigured(format!("email.smtp.tls_mode: unknown mode '{other}'")));
		}
	};
	// `builder_dangerous` only means "do not assume TLS" — the mode above decides.
	let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host)
		.port(cfg.port)
		.timeout(Some(cfg.timeout))
		.tls(tls);
	if !cfg.username.is_empty() {
		builder = builder.credentials(Credentials::new(cfg.username.clone(), cfg.password.clone()));
	}
	Ok(builder.build())
}

/// The last transport built, with the settings it was built from. `AsyncSmtpTransport` is
/// `Clone` and internally pooled, so a hit here reuses the connection pool instead of opening
/// a fresh TCP + TLS session per message. The config is operator-settable at runtime, hence
/// the compare rather than a plain `OnceLock`.
type Pooled = Option<(Smtp, AsyncSmtpTransport<Tokio1Executor>)>;

static TRANSPORT: LazyLock<Mutex<Pooled>> = LazyLock::new(|| Mutex::new(None));

/// The pooled transport for `cfg`, rebuilt only when the settings behind it changed.
fn pooled(cfg: &Smtp) -> ClResult<AsyncSmtpTransport<Tokio1Executor>> {
	// Cloned out of the lock: `send` awaits on it, and this mutex must never be held across
	// an await.
	let mut slot = TRANSPORT.lock();
	if let Some((have, t)) = slot.as_ref()
		&& have == cfg
	{
		return Ok(t.clone());
	}
	let t = transport(cfg)?;
	*slot = Some((cfg.clone(), t.clone()));
	Ok(t)
}

/// The retry class of a delivery failure. Split out of [`send`] because it is the whole of
/// what is worth testing there: there is no fake SMTP server in this workspace.
fn delivery_error(e: &lettre::transport::smtp::Error) -> Error {
	// 5xx is the relay's final answer; retrying it is eight more deliveries to an address it
	// has already refused, which is what earns a sender-reputation penalty. Anything that is
	// not a response at all — refused connection, DNS, TLS — stays retryable.
	if e.is_permanent() {
		Error::coded(
			StatusCode::BAD_GATEWAY,
			"E-EMAIL-REJECTED",
			format!("SMTP delivery permanently rejected: {e}"),
		)
	} else {
		Error::Unavailable(format!("SMTP delivery failed: {e}"))
	}
}

/// Delivers `msg`.
///
/// A transient delivery failure and an unconfigured relay are both retryable; an SMTP 5xx is
/// `E-EMAIL-REJECTED`, which the runner terminates on attempt one.
pub async fn send(app: &App, msg: &Message) -> ClResult<()> {
	let Some(cfg) = load(app).await.map_err(retryable)? else {
		// Answering `Ok` is the one thing worse than failing here: `DONE` blanks the payload and
		// the activation link goes with it.
		return Err(unconfigured("setting 'email.smtp.host' is unset"));
	};
	let mail = build(msg, &cfg)?;
	pooled(&cfg)?.send(mail).await.map_err(|e| delivery_error(&e))?;
	// The address is personal data and `gdpr::ERASURE` cannot reach a log line; INFO carries
	// only that a mail went out.
	tracing::info!("email sent");
	tracing::debug!(to = %msg.to, subject = %msg.subject, "email sent");
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use saas_core::{Error, Retry, error::StatusCode};

	use super::{Message, Smtp, build, checked_credentials, transport, unconfigured};
	use crate::SendEmail;

	#[test]
	fn a_username_without_a_password_is_a_lost_secret() {
		checked_credentials("mailer@example.com", "").unwrap_err();
		// Both empty is the anonymous relay `transport` deliberately supports.
		checked_credentials("", "").unwrap();
		checked_credentials("mailer@example.com", "s3cret").unwrap();
	}

	/// lettre exposes no constructor for a response error, so `delivery_error`'s branch is not
	/// reachable from a test; what is pinned here is the half that decides the behaviour — the
	/// two classes it picks between must stay on opposite sides of `Error::retry`.
	#[test]
	fn a_permanent_rejection_is_never_retried_and_a_transient_one_is() {
		let rejected =
			Error::coded(StatusCode::BAD_GATEWAY, "E-EMAIL-REJECTED", "550 no such user");
		assert_eq!(rejected.retry(), Retry::Never);
		assert_eq!(Error::Unavailable("conn refused".to_owned()).retry(), Retry::Backoff);
		let unset_host = Error::coded_retry(
			StatusCode::SERVICE_UNAVAILABLE,
			"E-EMAIL-UNCONFIGURED",
			"unset host",
		);
		assert_eq!(unset_host.retry(), Retry::Backoff);

		// And its neighbours, which were `Error::Setting` — `Retry::Never`. Nothing in the
		// workspace re-drives a `SEND_EMAIL` row, so one operator mistake used to destroy every
		// queued activation link on attempt one instead of delaying it.
		let cfg = |tls: &str, from: &str| Smtp {
			host: "smtp.example.com".to_owned(),
			port: 587,
			username: String::new(),
			password: String::new(),
			from_address: from.to_owned(),
			from_name: String::new(),
			tls_mode: tls.to_owned(),
			timeout: Duration::from_secs(30),
		};
		let msg = Message {
			to: "rcpt@example.com".to_owned(),
			subject: "s".to_owned(),
			text: String::new(),
			html: String::new(),
		};
		for err in [
			unconfigured("a setting read that went wrong"),
			checked_credentials("mailer@example.com", "").unwrap_err(),
			build(&msg, &cfg("starttls", "not-an-address")).unwrap_err(),
			transport(&cfg("nonsense", "billing@example.com")).unwrap_err(),
		] {
			assert_eq!(err.retry(), Retry::Backoff, "{err}");
			assert_eq!(err.parts().1, "E-EMAIL-UNCONFIGURED", "{err}");
		}
	}

	/// The shape `saas_core::alert::sweep` hand-builds, because `saas-core` cannot name this
	/// type. Renaming a field here silently kills operator alerting; this is what says so.
	#[test]
	fn the_alert_sweeps_hand_built_payload_still_deserialises() {
		let raw = serde_json::json!({
			"to": "ops@example.com",
			"template": "alert",
			"lang": "",
			"vars": { "alerts": [] },
		});
		let req: SendEmail = serde_json::from_value(raw).unwrap();
		assert_eq!(req.template, "alert");
	}
}

// vim: ts=4
