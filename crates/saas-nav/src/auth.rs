//! `tokenExchange` and the request envelope every NAV operation shares.
//!
//! `claude-docs/nav-mapping.md` §2.1–2.5. The exchange token is **not** cached: `invoiceApi.xsd`
//! calls it "the decoded unique token issued for the current transaction", so one
//! `manageInvoice` consumes one token. NAV states the window itself in `tokenValidityFrom`
//! / `tokenValidityTo`; those are returned to the caller, never used to reuse a token.

use std::fmt::Write as _;
use std::time::Duration;

use quick_xml::events::{BytesText, Event};
use quick_xml::{Reader, Writer, escape::escape};
use saas_core::{App, error::StatusCode, http, prelude::*};
use saas_invoice::Seller;
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};
use ulid::Ulid;

use crate::crypto;

pub const API_NS: &str = "http://schemas.nav.gov.hu/OSA/3.0/api";
pub const COMMON_NS: &str = "http://schemas.nav.gov.hu/NTCA/1.0/common";
const REQUEST_VERSION: &str = "3.0";
const HEADER_VERSION: &str = "1.0";
const TIMEOUT: Duration = Duration::from_secs(30);

/// `software` block, in XSD element order. Every value is a setting (§2.5); `seller` has no
/// software columns.
///
/// The flag is whether `invoiceApi.xsd` requires the element. The six required ones are
/// `…NotBlankType` (or `SoftwareIdType`), so an empty string is schema-invalid; the last two
/// are `minOccurs="0"`, where *absent* is legal but blank is not — those are omitted rather
/// than written empty.
///
/// The last field is the element's XSD maximum length, which [`check_software_settings`]
/// enforces: `settings::REGISTRY` bounds none of these.
const SOFTWARE_FIELDS: [(&str, &str, bool, usize); 8] = [
	("softwareId", "nav.software_id", true, 18),
	("softwareName", "nav.software_name", true, 50),
	("softwareOperation", "nav.software_operation", true, 15),
	("softwareMainVersion", "nav.software_main_version", true, 15),
	("softwareDevName", "nav.software_dev_name", true, 512),
	("softwareDevContact", "nav.software_dev_contact", true, 200),
	("softwareDevCountryCode", "nav.software_dev_country", false, 2),
	("softwareDevTaxNumber", "nav.software_dev_tax_number", false, 50),
];

/// `invoiceApi.xsd`'s `SoftwareIdType`: `<xs:length value="18"/>` and `[0-9A-Z\-]{18}`.
fn valid_software_id(id: &str) -> bool {
	id.len() == 18 && id.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase() || b == b'-')
}

/// `common.xsd`'s `LoginType`: `[a-zA-Z0-9]{6,15}`.
fn valid_login(login: &str) -> bool {
	(6..=15).contains(&login.len()) && login.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `common.xsd`'s `BankAccountNumberType`: HU `8-8-8`, HU `8-8`, or an IBAN
/// `[A-Z]{2}[0-9]{2}[0-9A-Za-z]{11,30}`.
fn valid_bank_account(s: &str) -> bool {
	let digits8 = |p: &&str| p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit());
	let parts: Vec<&str> = s.split('-').collect();
	if matches!(parts.len(), 2 | 3) {
		return parts.iter().all(digits8);
	}
	let b = s.as_bytes();
	(15..=34).contains(&b.len())
		&& b[..2].iter().all(u8::is_ascii_uppercase)
		&& b[2..4].iter().all(u8::is_ascii_digit)
		&& b[4..].iter().all(u8::is_ascii_alphanumeric)
}

/// An operator misconfiguration, not the caller's fault. `Error::internal` logs the key and
/// answers `500 "internal error"`, so the setting name never goes on the wire.
fn unconfigured(key: &str) -> Error {
	Error::internal(format!("setting '{key}' must be set before invoices can be filed with NAV"))
}

/// Refuse to start with a `software` block NAV would reject. Called from [`crate::job::seed`],
/// which the consumer runs from `AppBuilder::on_init`.
///
/// All eight keys default to `""` and the registry lets them stay that way — an unconfigured
/// deployment still has to be able to read them, and `settings::REGISTRY` has no "blank or
/// valid" bound to express the real rule. So this is the gate. Without it, a deployment that
/// configures NAV credentials and forgets one `nav.software_*` key has every `manageInvoice`
/// rejected on a schema error, and a faulted request is retryable — so it retries forever and
/// no invoice is ever filed, invisibly.
pub async fn check_software_settings(app: &App) -> ClResult<()> {
	// Ahead of the generic check, which cannot name the two endpoints: they are
	// indistinguishable from their answers and only one of them is statutory, so an operator
	// who has to guess is the failure this message exists to prevent.
	if app.settings.text("nav.base_url").await?.trim().is_empty() {
		return Err(Error::internal(
			"setting 'nav.base_url' must be set explicitly — production is \
			 https://api.onlineszamla.nav.gov.hu/invoiceService/v3, test is \
			 https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3",
		));
	}
	app.settings.check_required("nav.").await?;
	// Shape, which `required` cannot express — for all eight, not just `software_id`: the
	// registry bounds none of the others, so a 60-character `softwareName` or a lowercase
	// `"hu"` made every request schema-invalid, and `NAV_REPORT` retries that forever.
	for (el, key, _, max) in SOFTWARE_FIELDS {
		let raw = app.settings.text(key).await?;
		let value = raw.trim();
		// `check_required` above owns the blank-but-required case; a blank optional field is
		// simply left out of the block.
		if value.is_empty() {
			continue;
		}
		let bad = |rule: &str| {
			Error::internal(format!("setting '{key}' {rule}; NAV would reject every filing"))
		};
		saas_invoice::store::bounded_text(el, value, max).map_err(|e| bad(&e.to_string()))?;
		let rule = match el {
			"softwareId" if !valid_software_id(value) => {
				Some("must be exactly 18 characters of [0-9A-Z-]")
			}
			"softwareOperation" if !matches!(value, "LOCAL_SOFTWARE" | "ONLINE_SERVICE") => {
				Some("must be LOCAL_SOFTWARE or ONLINE_SERVICE")
			}
			// Required, not fixed up, for `check_seller`'s reason: nothing here may write back.
			"softwareDevCountryCode"
				if value.len() != 2 || !value.bytes().all(|b| b.is_ascii_uppercase()) =>
			{
				Some("must be an uppercase ISO-3166 alpha-2 code")
			}
			_ => None,
		};
		if let Some(rule) = rule {
			return Err(bad(rule));
		}
	}
	Ok(())
}

/// Refuse to start on a `sellers` row NAV would reject, for exactly the reason
/// [`check_software_settings`] exists — the same request, the other half of it.
///
/// [`InvoiceStore::put_seller`] has no service-handle wrapper, so nothing validates on the way
/// in — every **buyer** field is guarded in `saas_invoice::service_api`, no seller field was.
/// A postcode of `"1"` or a pasted `\n` in the name booted fine, invoices issued and got
/// numbers, and then every `NAV_REPORT` faulted on a schema error and retried forever
/// (`NAV_REPORT` is unbounded) against a refusal that can never change. The only exit was
/// storno and reissue of all of them.
///
/// The rules are the buyer side's own functions, called on the seller's fields, so the two
/// cannot drift. Canonical form is required rather than fixed up: nothing here may write to
/// the row, and NAV's `PostalCodeType` and `CountryCodeType` are both uppercase-only.
pub async fn check_seller(app: &App) -> ClResult<()> {
	use saas_invoice::store::{
		MAX_ADDRESS_TEXT, MAX_PARTY_NAME, MAX_SERIES_CODE, MAX_THIRD_STATE_TAX_ID,
		MIN_TAX_NUMBER_DIGITS, bounded_text, checked_postcode,
	};

	// A missing seller is the same "the seller is gone" the query path raises.
	let seller = saas_invoice::invoice_store(app)?
		.seller_by_id(saas_invoice::SELLER_ID)
		.await?
		.ok_or_else(|| Error::internal("saas-nav: the seller is gone"))?;

	let malformed = |field: &str, rule: String| {
		Error::internal(format!("sellers.{field}: {rule}; NAV would reject every filing"))
	};

	// `supplierName` is `SimpleText512NotBlankType`; `city` and `additionalAddressDetail`
	// (the street) are `SimpleText255NotBlankType`. Blank is its own failure — `NotBlank`.
	for (field, value, max) in [
		("name", &seller.name, MAX_PARTY_NAME),
		("city", &seller.city, MAX_ADDRESS_TEXT),
		("street", &seller.street, MAX_ADDRESS_TEXT),
		// `numbering::render_number` copies this verbatim into `invoiceNumber`, so a pasted
		// `\n` fails the XSD on an invoice that already has a number and is immutable.
		("series_code", &seller.series_code, MAX_SERIES_CODE),
	] {
		if value.trim().is_empty() {
			return Err(unconfigured(&format!("sellers.{field}")));
		}
		bounded_text(field, value, max).map_err(|e| malformed(field, e.to_string()))?;
	}

	if checked_postcode(&seller.postcode).map_err(|e| malformed("postcode", e.to_string()))?
		!= seller.postcode
	{
		return Err(malformed("postcode", "must already be uppercase".into()));
	}
	if saas_invoice::normalise_country(&seller.country)
		.map_err(|e| malformed("country", e.to_string()))?
		!= seller.country
	{
		return Err(malformed("country", "must already be an uppercase alpha-2 code".into()));
	}

	// `base:TaxNumberType` splits the first 8 **digits** off as `base:taxpayerId`, so a
	// length bound is the wrong rule — the same one `service_api`'s `groupTaxNo` check makes.
	for (field, value) in [
		("tax_number", Some(&seller.tax_number)),
		("group_member_tax_no", seller.group_member_tax_no.as_ref()),
	] {
		let Some(value) = value else { continue };
		if value.trim().is_empty() {
			return Err(unconfigured(&format!("sellers.{field}")));
		}
		bounded_text(field, value, MAX_THIRD_STATE_TAX_ID)
			.map_err(|e| malformed(field, e.to_string()))?;
		let digits = saas_invoice::tax_digits(value);
		if digits.len() < MIN_TAX_NUMBER_DIGITS {
			return Err(malformed(field, format!("has fewer than {MIN_TAX_NUMBER_DIGITS} digits")));
		}
		if !saas_invoice::vat_code_ok(&digits) {
			return Err(malformed(field, "9th digit must be 1-5".into()));
		}
	}

	// `communityVatNumber` is `[A-Z]{2}[0-9A-Z]{2,13}`, which is exactly what
	// `vies::normalise` produces — `"DE 811569869"` clears a length check and is then
	// rejected on every attempt.
	if let Some(eu) = &seller.eu_vat_id
		&& saas_invoice::vies::normalise(eu)
			.map_err(|e| malformed("eu_vat_id", e.to_string()))?
			.0 != *eu
	{
		return Err(malformed(
			"eu_vat_id",
			"must be the normalised form, e.g. 'DE811569869'".into(),
		));
	}

	// `xml::Xml` emits `supplierBankAccountNumber` on every filing when the column is set, so
	// the pattern — not just the 15-34 length — decides whether the filing is schema-valid.
	if let Some(acct) = &seller.bank_account {
		bounded_text("bank_account", acct, 34)
			.map_err(|e| malformed("bank_account", e.to_string()))?;
		// Untrimmed: `xml::supplier_info` writes the column's own bytes, so a value that only
		// validated after a trim went on the wire as stored and failed the schema.
		if !valid_bank_account(acct) {
			return Err(malformed("bank_account", "must be '8-8-8', '8-8' or an IBAN".into()));
		}
	}

	// `common.xsd`'s `LoginType` is `[a-zA-Z0-9]{6,15}` and `NavAuth::load` puts this straight
	// into every request. Checked untrimmed, like `postcode` and `country`: a pasted trailing
	// space passed the gate trimmed, went on the wire raw, and broke every `tokenExchange`.
	if let Some(login) = &seller.nav_login
		&& !valid_login(login)
	{
		return Err(malformed("nav_login", "must be 6-15 alphanumeric characters".into()));
	}
	Ok(())
}

/// The non-secret seller fields, the three secrets in ready-to-send form, and the `software`
/// block. Built once per submission; holds secret material, so it is never logged or returned.
pub struct NavAuth {
	/// `sellers.nav_base_url` when it is set, else `settings['nav.base_url']`; no trailing
	/// slash (§2.1).
	pub base_url: String,
	login: String,
	/// `user/taxNumber`: the first 8 digits of the seller's tax number (§2.2).
	tax_number: String,
	password_hash: String,
	sign_key: String,
	exchange_key: Vec<u8>,
	software: String,
}

/// One `tokenExchange` result, with the validity window NAV itself stated.
#[derive(Debug, Clone)]
pub struct ExchangeToken {
	pub token: String,
	pub valid_from: String,
	pub valid_to: String,
}

/// `nav.base_url` is operator-settable, and every request to it carries tax data and a
/// `passwordHash` that is a replayable NAV credential. A typo'd `http://` would ship both in
/// the clear, so the scheme is checked once, where the client is built.
///
/// Loopback is the one exemption — a local stand-in or an inspection tunnel, never the
/// public path. It is also why `saas_core::http`'s connector stays `https_or_http`.
fn require_tls(base_url: &str) -> ClResult<()> {
	if base_url.starts_with("https://") {
		return Ok(());
	}
	let authority = base_url
		.strip_prefix("http://")
		.and_then(|rest| rest.split('/').next())
		.unwrap_or_default();
	// Userinfo, not a host: `http://localhost:80@attacker.example` put `localhost` in front of
	// `split(':')` and passed the loopback exemption while hyper dialled the attacker.
	if authority.contains('@') {
		return Err(creds("nav.base_url must not contain userinfo"));
	}
	match authority.split(':').next().unwrap_or_default() {
		"127.0.0.1" | "localhost" => Ok(()),
		_ => Err(creds("nav.base_url must be https")),
	}
}

impl NavAuth {
	/// Credentials from the `secret` store, the non-secret fields from `seller`, the base URL
	/// and the `software` block from `settings`.
	pub async fn load(app: &App, seller: &Seller) -> ClResult<Self> {
		let login = seller.nav_login.clone().ok_or_else(|| creds("seller has no nav_login"))?;
		let tax_number: String =
			seller.tax_number.chars().filter(char::is_ascii_digit).take(8).collect();
		if tax_number.len() != 8 {
			return Err(creds("seller tax_number has fewer than 8 digits"));
		}
		let mut software = String::from("<software>");
		for (el, key, required, _) in SOFTWARE_FIELDS {
			let value = app.settings.text(key).await?;
			let value = value.trim();
			if value.is_empty() {
				// A blank required field is schema-invalid and NAV rejects the whole
				// request; a blank optional one is legal only as an absent element.
				if required {
					return Err(unconfigured(key));
				}
				continue;
			}
			let _ = write!(software, "<{el}>{}</{el}>", escape(value));
		}
		software.push_str("</software>");

		// The seller row wins when set: `nav_base_url` sits beside `nav_login` and is the obvious
		// place to configure an endpoint, so ignoring it left an operator who configured the
		// seller for production still filing into NAV's *test* system.
		let base_url = match seller.nav_base_url.trim() {
			"" => app.settings.text("nav.base_url").await?,
			url => url.to_owned(),
		};
		let base_url = base_url.trim_end_matches('/').to_owned();
		require_tls(&base_url)?;
		// A deliberate test deployment stays possible, and becomes visible in the log.
		if base_url.contains("api-test") {
			tracing::warn!(
				%base_url,
				"NAV client is pointed at the TEST system; nothing filed here is statutory"
			);
		}

		Ok(Self {
			base_url,
			login,
			tax_number,
			password_hash: crypto::password_hash(&secret_text(app, "nav.tech_password").await?),
			sign_key: secret_text(app, "nav.sign_key").await?,
			exchange_key: secret(app, "nav.exchange_key").await?,
			software,
		})
	}

	/// `header/requestId` for the operations with nothing to dedupe — `tokenExchange` and
	/// the `query*` calls. **`manageInvoice` must not use this**: it is the one operation NAV
	/// dedupes on, and it takes the filed invoice's `uid` instead. See
	/// [`NavAuth::manage_invoice_request`].
	///
	/// `common.xsd`'s `EntityIdType` is `[+a-zA-Z0-9_]{1,30}`, and a ULID is 26 alphanumeric
	/// characters, so it fits with room to spare.
	pub fn request_id() -> String {
		Ulid::new().to_string()
	}

	/// `requestSignature` for `tokenExchange` and the `query*` operations.
	pub fn sign(&self, request_id: &str, sign_ts: &str) -> String {
		crypto::request_signature(request_id, sign_ts, &self.sign_key)
	}

	/// `requestSignature` for `manageInvoice`. `invoices` holds
	/// `(invoiceOperation, base64 invoiceData)` — the exact base64 that goes on the wire.
	pub fn sign_invoices(
		&self,
		request_id: &str,
		sign_ts: &str,
		invoices: &[(&str, &str)],
	) -> String {
		crypto::request_signature_invoices(request_id, sign_ts, &self.sign_key, invoices)
	}

	/// The envelope every operation shares: `common:header`, `common:user`, `software`, then
	/// the operation's own `body` elements. The caller computes `signature`, because
	/// `manageInvoice` signs over the invoice chunks and everything else over the plain base.
	pub fn envelope(
		&self,
		root: &str,
		request_id: &str,
		header_ts: &str,
		signature: &str,
		body: &str,
	) -> String {
		format!(
			r#"<?xml version="1.0" encoding="UTF-8"?><{root} xmlns="{API_NS}" xmlns:common="{COMMON_NS}"><common:header><common:requestId>{request_id}</common:requestId><common:timestamp>{header_ts}</common:timestamp><common:requestVersion>{REQUEST_VERSION}</common:requestVersion><common:headerVersion>{HEADER_VERSION}</common:headerVersion></common:header><common:user><common:login>{login}</common:login><common:passwordHash cryptoType="SHA-512">{hash}</common:passwordHash><common:taxNumber>{tax}</common:taxNumber><common:requestSignature cryptoType="SHA3-512">{signature}</common:requestSignature></common:user>{software}{body}</{root}>"#,
			login = escape(&self.login),
			hash = self.password_hash,
			tax = self.tax_number,
			software = self.software,
		)
	}

	/// Exchange the technical user's credentials for a single-use token.
	pub async fn token_exchange(&self) -> ClResult<ExchangeToken> {
		let request_id = Self::request_id();
		let (header_ts, sign_ts) = stamps(OffsetDateTime::now_utc());
		let signature = self.sign(&request_id, &sign_ts);
		let xml = self.envelope("TokenExchangeRequest", &request_id, &header_ts, &signature, "");
		let (status, reply) = self.post("tokenExchange", &xml).await?;

		let field = |name: &str| element_text(&reply, name).ok_or_else(|| rejected(status, &reply));
		Ok(ExchangeToken {
			token: crypto::decrypt_exchange_token(
				&field("encodedExchangeToken")?,
				&self.exchange_key,
			)?,
			valid_from: field("tokenValidityFrom")?,
			valid_to: field("tokenValidityTo")?,
		})
	}

	/// POST an XML body to `{base_url}/{operation}` and return the response body verbatim.
	/// A 4xx carries NAV's own `GeneralErrorResponse`, which the caller reads; only a
	/// transport failure or a 5xx is an outage.
	///
	/// **The kind of failure is preserved** in `jobs.err_code`, though both classes back off:
	/// [`Error::Timeout`] and a plain `500`/`504` are indeterminate — NAV may have taken the
	/// invoice. A connect failure and a `502`/`503` from NAV's load balancer demonstrably never
	/// reached the invoice service, so they are [`Error::Unavailable`].
	pub async fn post(&self, operation: &str, xml: &str) -> ClResult<(StatusCode, String)> {
		let uri = format!("{}/{operation}", self.base_url);
		let (status, bytes) = http::post(
			&uri,
			&[("content-type", "application/xml"), ("accept", "application/xml")],
			xml.as_bytes().to_vec(),
			TIMEOUT,
		)
		.await
		.map_err(|e| match e {
			Error::Timeout(_) => indeterminate(),
			_ => unavailable(),
		})?;
		if status.is_server_error() {
			tracing::warn!(%status, %uri, "NAV returned a server error");
			return Err(match status.as_u16() {
				502 | 503 => unavailable(),
				_ => indeterminate(),
			});
		}
		// The status is returned, not discarded: `client::accepted` needs it to tell a 200 it
		// could not read (NAV may hold the invoice — never refile) from a 4xx edge rejection
		// (the request never reached the invoice service — safe, and necessary, to retry).
		Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
	}
}

/// What [`redact`] leaves in place of a secret.
pub const REDACTED: &str = "[redacted]";

/// Strips the three elements that authenticate a request, for archiving.
///
/// `common:passwordHash` is not a derived secret an attacker has to crack — it *is* what NAV
/// authenticates on the wire, and it is an unsalted SHA-512. An archived envelope carrying it
/// is a complete, replayable credential for that taxpayer; so is `common:requestSignature`,
/// and so is the decrypted `exchangeToken`. The archive exists to settle a dispute about what
/// was sent, not to be replayable.
///
/// One `quick_xml` pass: every event is written back as it was read, except that a target
/// element's content is replaced by a single [`REDACTED`] text. Open tags keep their
/// attributes, so the archived document still parses and still shows the request's shape.
/// Matching is on the qualified name, so `exchangeToken` cannot catch `exchangeTokenValidity`.
///
/// **Fails closed.** The output stops at the first event that does not parse — `NavAuth::post`
/// builds its `reply` over a body `Limited` may have truncated at `MAX_RESPONSE_BYTES`, and a
/// truncated `requestSignature` used to put every later `passwordHash` into
/// `nav_submissions.response_xml` in the clear. Nothing between a target's open tag and its
/// close is ever copied, so a truncation inside one leaks nothing either.
pub fn redact(xml: &str) -> String {
	const TARGETS: [&[u8]; 3] =
		[b"common:passwordHash", b"common:requestSignature", b"exchangeToken"];
	let mut reader = Reader::from_str(xml);
	let mut writer = Writer::new(Vec::new());
	// `Some(n)` while inside a target element, `n` being how deep below its open tag we are.
	let mut skipping: Option<u32> = None;
	// An unparseable event ends the output where it is: better a truncated archive than
	// one that resumes copying after a tag it could not understand.
	while let Ok(event) = reader.read_event() {
		let written = match (event, skipping) {
			(Event::Eof, _) => break,
			(Event::Start(e), None) if TARGETS.contains(&e.name().as_ref()) => {
				skipping = Some(0);
				writer.write_event(Event::Start(e)).is_ok()
					&& writer.write_event(Event::Text(BytesText::new(REDACTED))).is_ok()
			}
			(Event::Start(_), Some(n)) => {
				skipping = Some(n + 1);
				true
			}
			(Event::End(e), Some(0)) => {
				skipping = None;
				writer.write_event(Event::End(e)).is_ok()
			}
			(Event::End(_), Some(n)) => {
				skipping = Some(n - 1);
				true
			}
			(_, Some(_)) => true,
			(other, None) => writer.write_event(other).is_ok(),
		};
		if !written {
			break;
		}
	}
	String::from_utf8_lossy(&writer.into_inner()).into_owned()
}

/// `common:timestamp` — ISO-8601 UTC with milliseconds (§2.2). `now` is always UTC, so the
/// `Z` is a literal.
const HEADER_TS: &[FormatItem<'_>] =
	format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
/// The same instant as `yyyyMMddHHmmss`, for `requestSignature` (§2.3).
const SIGN_TS: &[FormatItem<'_>] = format_description!("[year][month][day][hour][minute][second]");

/// `(header/timestamp, signature ts)`.
///
/// Only a year outside `0..=9999` can fail to render, which a system clock does not reach;
/// an empty stamp would be rejected by NAV rather than filed wrong.
pub fn stamps(now: OffsetDateTime) -> (String, String) {
	(now.format(HEADER_TS).unwrap_or_default(), now.format(SIGN_TS).unwrap_or_default())
}

/// The first **non-empty** text content of `<…:name>` anywhere in the document. NAV's replies
/// are small and flat, and the element names used here are unique within them.
///
/// Empty is `None`, not `Some("")`: `client::accepted` would read a `Some(_)` as "NAV gave us a
/// transactionId" and record the filing as sent under an empty id, then poll against it forever.
/// `Reader::from_str` does not trim, and NAV pretty-prints, so the inter-element whitespace has
/// to be discounted rather than returned.
pub(crate) fn element_text(xml: &str, name: &str) -> Option<String> {
	let mut reader = Reader::from_str(xml);
	let mut inside = false;
	loop {
		let found = match reader.read_event() {
			Ok(Event::Start(e)) => {
				inside = e.local_name().as_ref() == name.as_bytes();
				continue;
			}
			// Without this the whitespace `Text` node *after* the close tag still matches
			// `inside` and comes back as `""`.
			Ok(Event::End(_)) => {
				inside = false;
				continue;
			}
			Ok(Event::Text(t)) if inside => t.unescape().ok().map(|s| s.trim().to_owned()),
			// A CDATA-wrapped value is a `CData` event, never a `Text` one.
			Ok(Event::CData(c)) if inside => {
				String::from_utf8(c.to_vec()).ok().map(|s| s.trim().to_owned())
			}
			Ok(Event::Eof) | Err(_) => return None,
			_ => continue,
		};
		if let Some(s) = found
			&& !s.is_empty()
		{
			return Some(s);
		}
	}
}

/// Whether `<…:name>` appears anywhere in the document. The element may be empty or carry
/// children, which is why [`element_text`] cannot answer it.
pub(crate) fn has_element(xml: &str, name: &str) -> bool {
	let mut reader = Reader::from_str(xml);
	loop {
		match reader.read_event() {
			// `Empty` too: `<x/>` is never a `Start` event, and a self-closing block is still
			// the block.
			Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == name.as_bytes() => {
				return true;
			}
			Ok(Event::Eof) | Err(_) => return false,
			_ => {}
		}
	}
}

/// NAV answered, but not with a token.
///
/// Two different failures, and the difference decides whether the job retries. A readable
/// `errorCode` is NAV refusing the credentials — permanent, and worth surfacing. No `errorCode`
/// at all means the body was not a NAV reply: a WAF page, a proxy's HTML, a truncated response.
/// That is an outage, and raising the permanent `E-NAV-CREDENTIALS` for it terminates a filing
/// that would have succeeded once the outage passed.
fn rejected(status: StatusCode, reply: &str) -> Error {
	let msg = element_text(reply, "message").unwrap_or_else(|| "no message".to_owned());
	match element_text(reply, "errorCode") {
		Some(code) => creds(format!("tokenExchange rejected: {code}: {msg}")),
		None => Error::coded_retry(
			StatusCode::BAD_GATEWAY,
			"E-NAV-AUTH-UNREADABLE",
			format!("tokenExchange answered HTTP {status} with no readable errorCode or token"),
		),
	}
}

async fn secret(app: &App, key: &str) -> ClResult<Vec<u8>> {
	app.secrets
		.get(key)
		.await?
		.ok_or_else(|| creds(format!("secret '{key}' is not set")))
}

async fn secret_text(app: &App, key: &str) -> ClResult<String> {
	String::from_utf8(secret(app, key).await?)
		.map_err(|_| creds(format!("secret '{key}' is not UTF-8")))
}

/// `Retry::Never`: NAV refused the credentials, or a secret is missing or unreadable. Both
/// fail identically on the next attempt and both need a person, so the runner gives up at once.
fn creds(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::BAD_GATEWAY, "E-NAV-CREDENTIALS", msg)
}

/// `Retry::Backoff`: NAV was not there, so the same request is worth sending again.
fn unavailable() -> Error {
	Error::coded_retry(StatusCode::BAD_GATEWAY, "E-NAV-UNAVAILABLE", "NAV is unavailable")
}

/// NAV may or may not have processed the request. Retried like any other failure: the resend
/// carries the same `requestId`, which NAV refuses if it did take the invoice.
fn indeterminate() -> Error {
	Error::Timeout("NAV gave no answer".to_owned())
}

#[cfg(test)]
mod tests {
	use time::macros::datetime;

	use super::*;

	/// `http://localhost:80@attacker.example` put `localhost` in front of `split(':')`, cleared
	/// the loopback exemption, and shipped the `passwordHash` to the attacker in the clear.
	#[test]
	fn userinfo_never_passes_for_a_loopback_host() {
		require_tls("http://localhost:80@attacker.example/v3").unwrap_err();
		require_tls("http://user@127.0.0.1/").unwrap_err();
		require_tls("http://attacker.example/v3").unwrap_err();
		require_tls("http://localhost:8080/v3").unwrap();
		require_tls("https://api-test.onlineszamla.nav.gov.hu/").unwrap();
	}

	#[test]
	fn stamps_render_both_forms_of_the_same_instant() {
		let (header, sign) = stamps(datetime!(2026-02-03 04:05:06.078 UTC));
		assert_eq!(header, "2026-02-03T04:05:06.078Z");
		assert_eq!(sign, "20260203040506");
	}

	/// `NavAuth::post` redacts a reply body that `Limited` may have truncated mid-element, so
	/// an unmatched target tag has to end the output — copying the remainder verbatim archived
	/// every later credential in the clear.
	#[test]
	fn a_truncated_element_stops_the_output_rather_than_leaking_what_follows() {
		let xml = "<r><common:requestSignature>abc\
		           <common:passwordHash>secret</common:passwordHash></r>";
		let out = redact(xml);
		assert!(!out.contains("secret"), "{out}");
		assert!(out.contains(REDACTED), "{out}");

		// A self-closing tag takes the same path: there is no `</exchangeToken>` to find.
		let out =
			redact("<r><exchangeToken/><common:passwordHash>secret</common:passwordHash></r>");
		assert!(!out.contains("secret"), "{out}");

		// The ordinary case still round-trips the rest of the document.
		let out = redact("<r><common:passwordHash>secret</common:passwordHash><tail/></r>");
		assert!(!out.contains("secret"), "{out}");
		assert!(out.contains("<tail/>"), "{out}");
	}

	#[test]
	fn element_text_ignores_the_namespace_prefix_and_trims() {
		let xml =
			r"<a xmlns:ns='u'><ns:tokenValidityTo> 2026-02-03T04:10:06Z </ns:tokenValidityTo></a>";
		assert_eq!(element_text(xml, "tokenValidityTo").as_deref(), Some("2026-02-03T04:10:06Z"));
		assert_eq!(element_text(xml, "encodedExchangeToken"), None);
	}

	/// `outcome` decided `Warn` with `reply.contains(…)`, which a tag name appearing anywhere
	/// in the body — an echoed `invoiceData`, a NAV message — turned into a warning.
	#[test]
	fn has_element_ignores_the_namespace_prefix_and_does_not_match_text() {
		let xml = r"<a xmlns:ns='u'><ns:businessValidationMessages/></a>";
		assert!(has_element(xml, "businessValidationMessages"));
		let echoed = r"<a><message>no businessValidationMessages were raised</message></a>";
		assert!(!has_element(echoed, "businessValidationMessages"));
	}
}

// vim: ts=4
