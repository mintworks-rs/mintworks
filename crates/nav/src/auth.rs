// SPDX-License-Identifier: MPL-2.0
//! `tokenExchange` and the request envelope every NAV operation shares.
//!
//! The exchange token is **not** cached: `invoiceApi.xsd` calls it "the decoded unique token
//! issued for the current transaction", so one `manageInvoice` **request** consumes one token
//! however many invoices it carries, and no token ever spans two requests (interface
//! specification §1.1, cited in `client.rs`). NAV states the window itself in
//! `tokenValidityFrom` / `tokenValidityTo`; those are returned to the caller, never used to
//! reuse a token.

use std::fmt::Write as _;
use std::time::Duration;

use mintworks_core::{App, error::StatusCode, http, prelude::*};
use mintworks_invoice::{Seller, SellerVersion};
use quick_xml::events::{BytesText, Event};
use quick_xml::{Reader, Writer, escape::escape};
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};
use ulid::Ulid;

use crate::crypto;

pub const API_NS: &str = "http://schemas.nav.gov.hu/OSA/3.0/api";
pub const COMMON_NS: &str = "http://schemas.nav.gov.hu/NTCA/1.0/common";
const REQUEST_VERSION: &str = "3.0";
const HEADER_VERSION: &str = "1.0";
const TIMEOUT: Duration = Duration::from_secs(30);

pub const TEST_BASE_URL: &str = "https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3";
pub const PRODUCTION_BASE_URL: &str = "https://api.onlineszamla.nav.gov.hu/invoiceService/v3";

/// The NAV system `deployment.env` names, as a base URL. This crate's half of that one flag;
/// `mintworks_payment_barion::base_url_for` maps the same two names onto the gateway's.
///
/// # Errors
/// `Error::Internal` for anything but the two accepted names.
pub fn base_url_for(env: &str) -> ClResult<&'static str> {
	match env.trim() {
		"test" => Ok(TEST_BASE_URL),
		"production" => Ok(PRODUCTION_BASE_URL),
		other => Err(Error::internal(format!(
			"setting 'deployment.env' must be 'test' or 'production', not '{other}'"
		))),
	}
}

/// `software` block, in XSD element order. Every value is a setting (§2.5); `seller` has no
/// software columns.
///
/// The flag is whether `invoiceApi.xsd` requires the element. The six required ones are
/// `…NotBlankType` (or `SoftwareIdType`), so an empty string is schema-invalid; the last two
/// are `minOccurs="0"`, where *absent* is legal but blank is not — those are omitted rather
/// than written empty.
///
/// Each element's XSD length and charset live on the declaration in [`crate::SETTINGS`], so a
/// value NAV would reject is refused where the operator writes it.
const SOFTWARE_FIELDS: [(&str, &str, bool); 8] = [
	("softwareId", "nav.software_id", true),
	("softwareName", "nav.software_name", true),
	("softwareOperation", "nav.software_operation", true),
	("softwareMainVersion", "nav.software_main_version", true),
	("softwareDevName", "nav.software_dev_name", true),
	("softwareDevContact", "nav.software_dev_contact", true),
	("softwareDevCountryCode", "nav.software_dev_country", false),
	("softwareDevTaxNumber", "nav.software_dev_tax_number", false),
];

/// `SettingDef::check` for a `software` element: NAV's `SimpleText*` pattern is `.*[^\s].*`
/// and XSD's `.` excludes #x0A/#x0D, so a pasted line break made every request schema-invalid
/// and `NAV_REPORT` retried that forever. Length is the declaration's `range`.
///
/// # Errors
/// `E-CORE-SETTING` when the value carries a control character.
pub fn check_software_text(raw: &str) -> ClResult<()> {
	mintworks_invoice::store::bounded_text("value", raw, usize::MAX)
		.map_err(|e| Error::Setting(e.to_string()))
}

/// `invoiceApi.xsd`'s `SoftwareIdType`: `<xs:length value="18"/>` and `[0-9A-Z\-]{18}`.
/// Blank passes — `required` is what refuses an unconfigured deployment, at boot.
///
/// # Errors
/// `E-CORE-SETTING` when the value is neither blank nor 18 characters of `[0-9A-Z-]`.
pub fn check_software_id(raw: &str) -> ClResult<()> {
	let ok = raw.len() == 18
		&& raw.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase() || b == b'-');
	if raw.is_empty() || ok {
		Ok(())
	} else {
		Err(Error::Setting("must be exactly 18 characters of [0-9A-Z-]".to_owned()))
	}
}

/// `base:CountryCodeType`, uppercase ISO-3166 alpha-2. Required rather than fixed up, for
/// [`check_seller`]'s reason: nothing here may write back.
///
/// # Errors
/// `E-CORE-SETTING` when the value is not two uppercase ASCII letters.
pub fn check_country_code(raw: &str) -> ClResult<()> {
	if raw.len() == 2 && raw.bytes().all(|b| b.is_ascii_uppercase()) {
		Ok(())
	} else {
		Err(Error::Setting("must be an uppercase ISO-3166 alpha-2 code".to_owned()))
	}
}

/// `common.xsd`'s `LoginType`: `[a-zA-Z0-9]{6,15}`.
pub(crate) fn valid_login(login: &str) -> bool {
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

/// The deployment's own seller — the one the root org owns.
///
/// The three NAV paths with no acting org resolve here and nowhere else: the boot check
/// below, [`crate::job::sweep`] and [`crate::service_api::alerts`] all run as `Ctx::system`,
/// so `seller_for_org(ctx.org()?)` has nothing to read. A redriven filing does **not** come
/// through here — it resolves the seller from the invoice row it is filing, which is the only
/// answer that still holds days later.
///
/// `None` when the root org owns no seller: a multi-tenant deployment whose sellers are all its
/// tenants' boots and files without one.
pub(crate) async fn deployment_seller(app: &App) -> ClResult<Option<Seller>> {
	let org_id = app.store.root_org_id().await?;
	mintworks_invoice::invoice_store(app)?.seller_for_org(org_id).await
}

/// The org whose `secrets` rows hold `seller`'s NAV credentials: 0 — the global level, with
/// the environment in front of it — for the root org's seller, else the seller's own org and
/// **only** that org. A tenant with no credentials must never file as the deployment.
pub(crate) async fn credential_org(app: &App, seller: &Seller) -> ClResult<i64> {
	Ok(if seller.org_id == app.store.root_org_id().await? { 0 } else { seller.org_id })
}

/// Whether a tenant seller can file at all: `nav_login` set and all three secrets stored at its
/// org. The deployment's own seller is always `true` — a gap there is an operator fault the
/// filing must surface, not a state to wait out.
pub(crate) async fn connected(app: &App, seller: &Seller) -> ClResult<bool> {
	let org = credential_org(app, seller).await?;
	if org == 0 {
		return Ok(true);
	}
	if seller.nav_login.is_none() {
		return Ok(false);
	}
	for key in crate::SECRETS {
		if !app.secrets.status_at(org, key).await?.set {
			return Ok(false);
		}
	}
	Ok(true)
}

/// Refuse to start on a `sellers` row NAV would reject — the half of the request the
/// `nav.software_*` declarations in [`crate::SETTINGS`] cannot reach, because it is a row.
///
/// [`InvoiceStore::put_seller`](mintworks_invoice::InvoiceStore::put_seller) has no service-handle
/// wrapper, so nothing validates on the way in — every **buyer** field is guarded in
/// `mintworks_invoice::service_api`, no seller field was.
/// A postcode of `"1"` or a pasted `\n` in the name booted fine, invoices issued and got
/// numbers, and then every `NAV_REPORT` faulted on a schema error and retried forever
/// (`NAV_REPORT` is unbounded) against a refusal that can never change. The only exit was
/// storno and reissue of all of them.
///
/// The rules are the buyer side's own functions, called on the seller's fields, so the two
/// cannot drift. Canonical form is required rather than fixed up: nothing here may write to
/// the row, and NAV's `PostalCodeType` and `CountryCodeType` are both uppercase-only.
pub async fn check_seller(app: &App) -> ClResult<()> {
	use mintworks_invoice::store::{
		MAX_ADDRESS_TEXT, MAX_PARTY_NAME, MAX_SERIES_CODE, MAX_THIRD_STATE_TAX_ID,
		MIN_TAX_NUMBER_DIGITS, bounded_text, checked_postcode,
	};

	let store = mintworks_invoice::invoice_store(app)?;
	let Some(seller) = deployment_seller(app).await? else {
		return Ok(());
	};
	// The live version only. An archived one may be malformed by today's rules and cannot be
	// corrected — the invoices carrying it are immutable — so gating boot on it would be a
	// permanent outage over history.
	let current = store
		.current_seller_version(seller.id)
		.await?
		.ok_or_else(|| unconfigured("seller_versions"))?;

	let malformed = |field: &str, rule: String| {
		Error::internal(format!("sellers.{field}: {rule}; NAV would reject every filing"))
	};

	// `supplierName` is `SimpleText512NotBlankType`; `city` and `additionalAddressDetail`
	// (the street) are `SimpleText255NotBlankType`. Blank is its own failure — `NotBlank`.
	for (field, value, max) in [
		("name", &current.name, MAX_PARTY_NAME),
		("city", &current.city, MAX_ADDRESS_TEXT),
		("street", &current.street, MAX_ADDRESS_TEXT),
		// `numbering::render_number` copies this verbatim into `invoiceNumber`, so a pasted
		// `\n` fails the XSD on an invoice that already has a number and is immutable.
		("series_code", &seller.series_code, MAX_SERIES_CODE),
	] {
		if value.trim().is_empty() {
			return Err(unconfigured(&format!("sellers.{field}")));
		}
		bounded_text(field, value, max).map_err(|e| malformed(field, e.to_string()))?;
	}

	if checked_postcode(&current.postcode).map_err(|e| malformed("postcode", e.to_string()))?
		!= current.postcode
	{
		return Err(malformed("postcode", "must already be uppercase".into()));
	}
	if mintworks_invoice::normalise_country(&current.country)
		.map_err(|e| malformed("country", e.to_string()))?
		!= current.country
	{
		return Err(malformed("country", "must already be an uppercase alpha-2 code".into()));
	}

	// `base:TaxNumberType` splits the first 8 **digits** off as `base:taxpayerId`, so a
	// length bound is the wrong rule — the same one `service_api`'s `groupTaxNo` check makes.
	for (field, value) in [
		("tax_number", Some(&current.tax_number)),
		("group_member_tax_no", current.group_member_tax_no.as_ref()),
	] {
		let Some(value) = value else { continue };
		if value.trim().is_empty() {
			return Err(unconfigured(&format!("sellers.{field}")));
		}
		bounded_text(field, value, MAX_THIRD_STATE_TAX_ID)
			.map_err(|e| malformed(field, e.to_string()))?;
		let digits = mintworks_invoice::tax_digits(value);
		if digits.len() < MIN_TAX_NUMBER_DIGITS {
			return Err(malformed(field, format!("has fewer than {MIN_TAX_NUMBER_DIGITS} digits")));
		}
		if !mintworks_invoice::vat_code_ok(&digits) {
			return Err(malformed(field, "9th digit must be 1-5".into()));
		}
	}

	// `communityVatNumber` is `[A-Z]{2}[0-9A-Z]{2,13}`, which is exactly what
	// `vies::normalise` produces — `"DE 811569869"` clears a length check and is then
	// rejected on every attempt.
	if let Some(eu) = &current.eu_vat_id
		&& mintworks_invoice::vies::normalise(eu)
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
	if let Some(acct) = &current.bank_account {
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
	/// `sellers.nav_base_url` when it is set, else `settings['nav.base_url']`, else the system
	/// `settings['deployment.env']` names ([`base_url_for`]); no trailing slash (§2.1).
	pub base_url: String,
	login: String,
	/// `user/taxNumber`: the first 8 digits of the seller's tax number (§2.2).
	tax_number: String,
	password_hash: String,
	sign_key: String,
	exchange_key: Vec<u8>,
	software: String,
}

/// The three NAV secrets in the form [`NavAuth`] needs. No `Debug`: it is secret material.
pub(crate) struct Credentials {
	pub(crate) tech_password: String,
	pub(crate) sign_key: String,
	pub(crate) exchange_key: Vec<u8>,
}

/// `seller`'s credentials, resolved by [`credential_org`].
pub(crate) async fn credentials(app: &App, seller: &Seller) -> ClResult<Credentials> {
	let org = credential_org(app, seller).await?;
	let secret = |key: &'static str| async move {
		app.secrets.get_at(org, key).await?.ok_or_else(|| {
			if org == 0 {
				creds(format!("secret '{key}' is not set"))
			} else {
				creds(format!("NAV is not connected for this seller: '{key}' is not set"))
			}
		})
	};
	let text = |key: &'static str| async move {
		String::from_utf8(secret(key).await?)
			.map_err(|_| creds(format!("secret '{key}' is not UTF-8")))
	};
	Ok(Credentials {
		tech_password: text("nav.tech_password").await?,
		sign_key: text("nav.sign_key").await?,
		exchange_key: secret("nav.exchange_key").await?,
	})
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
/// public path. It is also why `mintworks_core::http`'s connector stays `https_or_http`.
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
	///
	/// `current` is the **live** seller version, never an invoice's frozen one: `user/taxNumber`
	/// identifies the technical user sending the request, so a filing redriven years later
	/// authenticates as today's taxpayer even though the invoice it carries does not.
	pub async fn load(app: &App, seller: &Seller, current: &SellerVersion) -> ClResult<Self> {
		let secrets = credentials(app, seller).await?;
		Self::with_credentials(app, seller, current, secrets).await
	}

	/// [`Self::load`] with the secrets supplied — so credentials can be verified against NAV
	/// before any of them is stored.
	pub(crate) async fn with_credentials(
		app: &App,
		seller: &Seller,
		current: &SellerVersion,
		secrets: Credentials,
	) -> ClResult<Self> {
		let login = seller.nav_login.clone().ok_or_else(|| creds("seller has no nav_login"))?;
		let tax_number: String =
			current.tax_number.chars().filter(char::is_ascii_digit).take(8).collect();
		if tax_number.len() != 8 {
			return Err(creds("seller tax_number has fewer than 8 digits"));
		}
		let mut software = String::from("<software>");
		for (el, key, required) in SOFTWARE_FIELDS {
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
		// place to configure an endpoint. Then the explicit setting, then the named system — so
		// an unconfigured deployment dials production rather than refusing to start, and
		// `deployment.env` is the one flag it has to get right.
		let base_url = match seller.nav_base_url.trim() {
			"" => match app.settings.text("nav.base_url").await?.trim() {
				"" => base_url_for(&app.settings.text("deployment.env").await?)?.to_owned(),
				url => url.to_owned(),
			},
			url => url.to_owned(),
		};
		let base_url = base_url.trim_end_matches('/').to_owned();
		require_tls(&base_url)?;
		// Once per base_url: `load` runs per filing *and per poll attempt*, and a retrying poll
		// chain buried the errors around it under one copy of this line per attempt — but the URL
		// is per seller, so a process-wide `Once` silenced every org after the first.
		if base_url.contains("api-test") {
			static WARNED: std::sync::LazyLock<
				std::sync::Mutex<std::collections::HashSet<String>>,
			> = std::sync::LazyLock::new(Default::default);
			let first = WARNED
				.lock()
				.unwrap_or_else(std::sync::PoisonError::into_inner)
				.insert(base_url.clone());
			if first {
				tracing::warn!(
					%base_url,
					"NAV client is pointed at the TEST system; nothing filed here is statutory"
				);
			}
		}

		Ok(Self {
			base_url,
			login,
			tax_number,
			password_hash: crypto::password_hash(&secrets.tech_password),
			sign_key: secrets.sign_key,
			exchange_key: secrets.exchange_key,
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
			// `escape` on all three: defence in depth, not a live bug — `request_id` is a uid or
			// a minted ULID, and `tax_number` is eleven validated digits.
			request_id = escape(request_id),
			login = escape(&self.login),
			hash = self.password_hash,
			tax = escape(&self.tax_number),
			software = self.software,
		)
	}

	/// Exchange the technical user's credentials for a single-use token.
	pub async fn token_exchange(&self) -> ClResult<ExchangeToken> {
		let request_id = Self::request_id();
		let (header_ts, sign_ts) = stamps(OffsetDateTime::now_utc());
		let signature = self.sign(&request_id, &sign_ts);
		let xml = self.envelope("TokenExchangeRequest", &request_id, &header_ts, &signature, "");
		let (status, body) = self.post("tokenExchange", &xml).await?.body("tokenExchange")?;
		let reply = crate::reply::Reply::parse(&body);

		let field = |name: &str| reply.text(name).ok_or_else(|| rejected(status, &reply));
		Ok(ExchangeToken {
			token: crypto::decrypt_exchange_token(
				&field("encodedExchangeToken")?,
				&self.exchange_key,
			)?,
			valid_from: field("tokenValidityFrom")?,
			valid_to: field("tokenValidityTo")?,
		})
	}

	/// POST an XML body to `{base_url}/{operation}` and classify what came back.
	///
	/// `Err` is reserved for a request this process built wrong; everything the network or NAV
	/// can do is an [`Answer`]. The classification is made **here**, once — the operation
	/// parsers used to re-derive it from a bare `StatusCode`, which is how a 429 became a
	/// business fault and a 408 a blind resend.
	pub async fn post(&self, operation: &str, xml: &str) -> ClResult<Answer> {
		let uri = format!("{}/{operation}", self.base_url);
		let (status, retry_after, bytes) = match http::post(
			&uri,
			&[("content-type", "application/xml"), ("accept", "application/xml")],
			xml.as_bytes().to_vec(),
			TIMEOUT,
		)
		.await
		{
			Ok(answer) => answer,
			// The request was on the wire, so NAV may hold the batch: §1.9.2, never a resend.
			Err(Error::Timeout(_)) => return Ok(Answer::Indeterminate),
			// A connect failure demonstrably never reached the invoice service.
			Err(Error::Unavailable(_)) => return Ok(Answer::Unavailable),
			Err(e) => return Err(e),
		};
		let throttled =
			|| Answer::Throttled { retry_after: retry_after.unwrap_or(DEFAULT_THROTTLE_SECS) };
		let answer = match status.as_u16() {
			429 => throttled(),
			// A 503 carrying `Retry-After` is a maintenance window, not a blind backoff.
			502 | 503 => retry_after
				.map_or(Answer::Unavailable, |retry_after| Answer::Throttled { retry_after }),
			// 408 joins the indeterminate 5xx: NAV received something, whether it processed it
			// is unknowable, and for a statutory filing the safe direction is §1.9.2 rather
			// than a blind resend.
			408 | 500..=599 => Answer::Indeterminate,
			// Every 4xx goes to the operation parser: one carrying NAV's own
			// `GeneralErrorResponse` keeps NAV's code, and one that does not — a WAF page, a CDN
			// error — is the edge refusing the request, so nothing was filed and
			// `client::accepted` says so.
			_ => {
				return Ok(Answer::Reply {
					status,
					xml: String::from_utf8_lossy(&bytes).into_owned(),
				});
			}
		};
		tracing::warn!(%status, %uri, ?answer, "NAV did not answer with a reply");
		Ok(answer)
	}
}

/// What NAV sends when it throttles without saying for how long, and what a `Retry-After` in
/// the HTTP-date form falls back to — `mintworks_core::http::post` reads only delta-seconds.
const DEFAULT_THROTTLE_SECS: u64 = 60;

/// What one NAV round trip came back as. The classification is made in [`NavAuth::post`],
/// once — not re-derived from a `StatusCode` at each operation parser.
#[derive(Debug)]
pub enum Answer {
	/// A body for the operation parser, including every 4xx that carries NAV's own
	/// `GeneralErrorResponse` and every 4xx that does not (a WAF page, a CDN error): the edge
	/// refused the request, so nothing was filed and `client::accepted` says so.
	Reply { status: StatusCode, xml: String },
	/// Demonstrably never reached the invoice service: a refused connection, a 502/503.
	/// Safe to resend under the same `requestId`.
	Unavailable,
	/// The fate of the submission is unknown — NAV may hold the batch. The only trigger for
	/// `NAV_RECONCILE` (§1.9.2).
	Indeterminate,
	/// NAV asked for a pause and said how long. Nothing was filed.
	Throttled { retry_after: u64 },
}

impl Answer {
	/// The body, or the `Error` the job runner should see. For every caller but
	/// [`crate::job::report`], which must enqueue `NAV_RECONCILE` before it propagates.
	///
	/// # Errors
	/// Whatever the round trip was, when it was not a body.
	pub fn body(self, operation: &str) -> ClResult<(StatusCode, String)> {
		match self {
			Self::Reply { status, xml } => Ok((status, xml)),
			Self::Unavailable => Err(unavailable()),
			Self::Indeterminate => Err(indeterminate()),
			// `Error::RateLimit` carries the seconds, which `Runner::fail` waits out: an
			// `E-NAV-THROTTLED` with its own field would duplicate a number the variant has.
			Self::Throttled { retry_after } => {
				tracing::warn!(operation, retry_after, "NAV asked for a pause");
				Err(Error::RateLimit(retry_after))
			}
		}
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
/// `nav_submission_xml.response_xml` in the clear. Nothing between a target's open tag and its
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
	let mut text = String::new();
	loop {
		match reader.read_event() {
			Ok(Event::Start(e)) => {
				inside = e.local_name().as_ref() == name.as_bytes();
				text.clear();
			}
			Ok(Event::End(_)) if inside => {
				inside = false;
				if !text.trim().is_empty() {
					return Some(text.trim().to_owned());
				}
			}
			Ok(Event::Eof) | Err(_) => return None,
			Ok(e) if inside => push_text(&mut text, &e),
			_ => {}
		}
	}
}

/// Appends one piece of an element's text, unescaped; a piece that does not decode is dropped.
/// Since quick-xml 0.38 an entity reference is its own `GeneralRef` event, so one element's
/// text arrives as several events, and a CDATA-wrapped value is a `CData` one.
pub(crate) fn push_text(out: &mut String, event: &Event<'_>) {
	match event {
		Event::Text(t) => out.push_str(&t.xml10_content().unwrap_or_default()),
		Event::CData(c) => out.push_str(&c.decode().unwrap_or_default()),
		Event::GeneralRef(r) => {
			if let Ok(Some(c)) = r.resolve_char_ref() {
				out.push(c);
			} else {
				let name = r.decode().unwrap_or_default();
				out.push_str(quick_xml::escape::resolve_predefined_entity(&name).unwrap_or(""));
			}
		}
		_ => {}
	}
}

/// NAV answered, but not with a token.
///
/// Two different failures, and the difference decides whether the job retries. A readable
/// `errorCode` is NAV refusing the credentials — permanent, and worth surfacing. No `errorCode`
/// at all means the body was not a NAV reply: a WAF page, a proxy's HTML, a truncated response.
/// That is an outage, and raising the permanent `E-NAV-CREDENTIALS` for it terminates a filing
/// that would have succeeded once the outage passed.
fn rejected(status: StatusCode, reply: &crate::reply::Reply<'_>) -> Error {
	match reply.fault.as_ref().and_then(|f| f.code.clone()) {
		Some(code) => {
			let (_, msg) = reply.fault_pair();
			creds(format!("tokenExchange rejected: {code}: {msg}"))
		}
		None => Error::coded_retry(
			StatusCode::BAD_GATEWAY,
			"E-NAV-AUTH-UNREADABLE",
			format!("tokenExchange answered HTTP {status} with no readable errorCode or token"),
		),
	}
}

/// `Retry::Never`: NAV refused the credentials, or a secret is missing or unreadable. Both
/// fail identically on the next attempt and both need a person, so the runner gives up at once.
fn creds(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::BAD_GATEWAY, "E-NAV-CREDENTIALS", msg)
}

/// `Retry::Backoff`: NAV was not there, so the same request is worth sending again.
pub(crate) fn unavailable() -> Error {
	Error::coded_retry(StatusCode::BAD_GATEWAY, "E-NAV-UNAVAILABLE", "NAV is unavailable")
}

/// NAV may or may not have processed the request. Retried like any other failure: the resend
/// carries the same `requestId`, which NAV refuses if it did take the invoice.
pub(crate) fn indeterminate() -> Error {
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
}

// vim: ts=4
