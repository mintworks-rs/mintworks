//! The three Online Számla operations this crate calls, over the envelope and transport
//! `auth.rs` already owns.
//!
//! Request building and sending are deliberately separate calls: the caller archives the
//! request XML in `nav_submissions` *before* it leaves the process, and archives the reply
//! *before* anything here parses it. Nothing in this module writes to the database, and
//! nothing in it is reachable from the invoice issue path.

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use quick_xml::escape::escape;
use saas_core::{error::StatusCode, prelude::*};
use time::OffsetDateTime;

use crate::auth::{NavAuth, element_text, has_element, stamps};
use crate::submission::NavOp;

/// What `manageInvoice` said.
///
/// On these two operations a `funcCode` other than `OK` is always a **technical** fault —
/// bad credentials, a bad signature, clock skew, a malformed envelope, a mistyped
/// `nav.base_url`. NAV's verdict on the invoice itself never arrives here: it comes later on
/// the poll path as `invoiceStatus = ABORTED`. So a fault means the invoice was *not* filed,
/// and the attempt must be retried rather than recorded as a rejection — recording it as one
/// silently drops every invoice issued while the fault lasts.
#[derive(Debug, Clone)]
pub enum Accepted {
	Ok {
		transaction_id: String,
	},
	/// A technical fault. Nothing was filed, so this is safe to retry.
	Fault {
		code: String,
		message: String,
	},
}

/// What `queryTransactionStatus` said about a `transactionId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
	/// `RECEIVED` / `PROCESSING` / `SAVED` — poll again.
	Pending,
	Done,
	/// Accepted, with business warnings. Still terminal, still reported.
	Warn,
	/// `ABORTED` — NAV's verdict on the invoice. Terminal, and never retried.
	Failed {
		code: String,
		message: String,
	},
	/// The *query* failed: a `funcCode` of `ERROR` on `queryTransactionStatus` is a fault in
	/// the question, not an answer about the invoice, which NAV may well have accepted and
	/// filed. Retried under the poll backoff; recording it as `Failed` would mark a filed
	/// invoice permanently rejected.
	Unavailable {
		code: String,
		message: String,
	},
}

/// `queryTaxpayer` — the "look up a buyer by tax number" helper the invoice API uses.
#[derive(Debug, Clone)]
pub struct Taxpayer {
	pub valid: bool,
	pub name: Option<String>,
	/// The 8-digit core tax number NAV holds, when it returned one.
	pub tax_number: Option<String>,
}

/// The three request builders. They live here rather than in `auth.rs` because they are the
/// operations; `NavAuth` is the envelope, the credentials and the transport they all share.
impl NavAuth {
	/// `ManageInvoiceRequest` carrying one invoice. `invoice_data_xml` is the document
	/// `xml::invoice_data` produced; it is base64'd here, and the signature is taken over
	/// **that exact base64**, never a re-serialisation (`nav-mapping.md` §2.3).
	///
	/// Obtains its own exchange token: the token is single-use, so one is fetched per request.
	/// `request_id` is supplied rather than minted, and the caller passes the filed invoice's
	/// `uid`.
	///
	/// NAV treats `requestId` as an idempotency key: an id it processed — or refused with
	/// `INVALID_REQUEST_SIGNATURE` or `FORBIDDEN` — can never be used again, so a retry under
	/// the same id is refused rather than filed twice (see [`BURNS_REQUEST_ID`]). That only
	/// protects an invoice if the id is **stable**, and `invoices.uid` is the only thing here
	/// that is: immutable on an `ISSUED` row, so it survives a restart, a `jobs` cleanup and a
	/// database rebuild. `jobs.id` and `nav_submissions.id` are bare rowid aliases with no
	/// `AUTOINCREMENT`, so a cleanup restarting either reissues ids NAV has already burned.
	///
	/// `inv_` plus a 26-character ULID is exactly 30 — `EntityIdType`'s maximum, with no
	/// headroom. A longer uid prefix would have to be shortened here.
	pub async fn manage_invoice_request(
		&self,
		op: NavOp,
		request_id: &str,
		invoice_data_xml: &str,
	) -> ClResult<String> {
		let token = self.token_exchange().await?;
		let b64 = B64.encode(invoice_data_xml.as_bytes());
		let (header_ts, sign_ts) = stamps(OffsetDateTime::now_utc());
		let signature = self.sign_invoices(request_id, &sign_ts, &[(op.as_str(), &b64)]);

		let body = format!(
			"<exchangeToken>{}</exchangeToken>\
			 <invoiceOperations>\
			 <compressedContent>false</compressedContent>\
			 <invoiceOperation>\
			 <index>1</index>\
			 <invoiceOperation>{}</invoiceOperation>\
			 <invoiceData>{b64}</invoiceData>\
			 </invoiceOperation>\
			 </invoiceOperations>",
			escape(&token.token),
			op.as_str(),
		);
		Ok(self.envelope("ManageInvoiceRequest", request_id, &header_ts, &signature, &body))
	}

	/// `QueryTransactionStatusRequest`. No exchange token — only the two `manage*`
	/// operations carry one.
	pub fn query_status_request(&self, transaction_id: &str) -> String {
		let (request_id, header_ts, sign_ts) = ids();
		let signature = self.sign(&request_id, &sign_ts);
		let body = format!(
			"<transactionId>{}</transactionId>\
			 <returnOriginalRequest>false</returnOriginalRequest>",
			escape(transaction_id),
		);
		self.envelope("QueryTransactionStatusRequest", &request_id, &header_ts, &signature, &body)
	}

	pub fn query_taxpayer_request(&self, tax_number: &str) -> String {
		let (request_id, header_ts, sign_ts) = ids();
		let signature = self.sign(&request_id, &sign_ts);
		let body = format!("<taxNumber>{}</taxNumber>", escape(tax_number));
		self.envelope("QueryTaxpayerRequest", &request_id, &header_ts, &signature, &body)
	}

	/// Validate a Hungarian tax number and read back the taxpayer's registered name.
	/// One round trip, nothing archived: it touches no invoice.
	pub async fn query_taxpayer(&self, tax_number: &str) -> ClResult<Taxpayer> {
		let (_, reply) =
			self.post("queryTaxpayer", &self.query_taxpayer_request(tax_number)).await?;
		taxpayer(&reply)
	}
}

/// A fresh request id and the two timestamps, for the operations with nothing to dedupe.
/// `manageInvoice` is not one of them — it takes its id from its job.
fn ids() -> (String, String, String) {
	let (header_ts, sign_ts) = stamps(OffsetDateTime::now_utc());
	(NavAuth::request_id(), header_ts, sign_ts)
}

/// Read a `manageInvoice` reply. `funcCode` of `OK` plus a `transactionId` is acceptance. A
/// readable non-`OK` `funcCode` is a technical fault — see [`Accepted`] for why there is no
/// business-rejection case on this operation. A reply with no `funcCode` at all is neither:
/// it is a reply we could not read, so the filing state is unknown.
pub fn accepted(status: StatusCode, reply: &str) -> ClResult<Accepted> {
	match element_text(reply, "funcCode").as_deref() {
		// Acceptance, pending a readable transactionId — fall through to the match below.
		Some("OK") => {}
		// NAV answered, and the answer is definitively not an acceptance: nothing was filed.
		Some(_) => {
			let (code, message) = error_pair(reply);
			return Ok(Accepted::Fault { code, message });
		}
		// No `funcCode` anywhere — a WAF page, a truncated body. On a **200**, calling that a
		// fault would mean "not filed", which we cannot know, so it is an `Err`: the runner
		// resends under the same `requestId`, which NAV refuses if it did file, making a
		// duplicate statutory filing impossible. On a **4xx** the edge rejected it outright.
		None if status.is_success() => {
			return Err(Error::coded_retry(
				StatusCode::BAD_GATEWAY,
				"E-NAV-UNREADABLE-REPLY",
				"the reply to manageInvoice carried no funcCode and could not be read",
			));
		}
		None => {
			return Ok(Accepted::Fault {
				code: "E-NAV-HTTP-STATUS".to_owned(),
				message: format!("manageInvoice answered HTTP {status} with no readable funcCode"),
			});
		}
	}
	match element_text(reply, "transactionId") {
		Some(id) => Ok(Accepted::Ok { transaction_id: id }),
		// `OK` with no readable id is acceptance we cannot act on: there is nothing to poll
		// with. Retried rather than recorded, for the reason above — the resend carries the
		// same `requestId`, so it either yields a usable id or NAV names the duplicate.
		None => Err(Error::coded_retry(
			StatusCode::BAD_GATEWAY,
			"E-NAV-NO-TRANSACTION-ID",
			"manageInvoice returned OK with no readable transactionId",
		)),
	}
}

/// Read a `queryTransactionStatus` reply into the terminal-or-not decision of
/// `nav-mapping.md` §2.6.
pub fn outcome(reply: &str) -> ClResult<Outcome> {
	if element_text(reply, "funcCode").as_deref() != Some("OK") {
		let (code, message) = error_pair(reply);
		return Ok(Outcome::Unavailable { code, message });
	}
	match element_text(reply, "invoiceStatus").as_deref() {
		Some("RECEIVED" | "PROCESSING" | "SAVED") | None => Ok(Outcome::Pending),
		Some("DONE") => {
			// A `businessValidationMessages` block on a DONE invoice is a warning, not a
			// rejection: NAV stored it and said so.
			if has_element(reply, "businessValidationMessages") {
				Ok(Outcome::Warn)
			} else {
				Ok(Outcome::Done)
			}
		}
		Some("ABORTED") => {
			let (code, message) = error_pair(reply);
			Ok(Outcome::Failed { code, message })
		}
		Some(other) => {
			// Pending rather than an interpretation we cannot justify: `poll` turns it into a
			// retryable `Err` on the job row, so an unknown status backs off like any other
			// non-verdict and `jobs.alert_after.NAV_POLL` raises it after a day.
			tracing::warn!(status = %other, "NAV returned an unknown invoiceStatus; treating as pending");
			Ok(Outcome::Pending)
		}
	}
}

pub fn taxpayer(reply: &str) -> ClResult<Taxpayer> {
	if element_text(reply, "funcCode").as_deref() != Some("OK") {
		let (code, message) = error_pair(reply);
		return Err(business(&code, &message));
	}
	Ok(Taxpayer {
		valid: element_text(reply, "taxpayerValidity").as_deref() == Some("true"),
		name: element_text(reply, "taxpayerName"),
		tax_number: element_text(reply, "taxpayerId"),
	})
}

/// NAV states a fault in one of three shapes: the envelope's own `errorCode`/`message`, a
/// `technicalValidationMessages` block, or a `businessValidationMessages` block. All three
/// use the same two local names, so the first hit is the fault.
fn error_pair(reply: &str) -> (String, String) {
	let code = element_text(reply, "errorCode")
		.or_else(|| element_text(reply, "validationErrorCode"))
		.unwrap_or_else(|| "unknown".to_owned());
	let message =
		element_text(reply, "message").unwrap_or_else(|| "NAV gave no message".to_owned());
	(code, message)
}

/// The faults that spend the `requestId` they arrived on. Online Számla 3.0 interface
/// specification §"requestId", the uniqueness paragraph: *"Az egyediségbe minden sikeresen
/// feldolgozott kérés, valamint az INVALID_REQUEST_SIGNATURE és FORBIDDEN hibakóddal
/// elutasított kérés azonosítója számít bele. Ezen kérések azonosítói (requestId) nem
/// használhatók fel újra."* — a successfully processed request, and one refused with either of
/// those two codes, burns its id. `REQUEST_ID_NOT_UNIQUE` is the refusal that says the id is
/// already burned, so it stops the loop for the same reason even though it burns nothing
/// itself — but its *remediation* is the opposite one, and [`business`] splits the message.
///
/// (The spec scopes uniqueness to "the timestamp tolerance window", so the burn may not be
/// eternal. Nothing here gambles on it expiring: `report` sends `invoices.uid`, which is
/// immutable on an `ISSUED` row, so every later attempt — an operator re-drive included —
/// carries the same id.)
const BURNS_REQUEST_ID: [&str; 3] =
	["INVALID_REQUEST_SIGNATURE", "FORBIDDEN", "REQUEST_ID_NOT_UNIQUE"];

/// A fault NAV answered with.
///
/// `Retry::Backoff` for the majority: on `manageInvoice` a fault means nothing was filed, and
/// the causes — bad credentials, clock skew, a NAV outage — are exactly the ones that clear on
/// their own, so terminating on the first one would silently drop every invoice issued while
/// the fault lasted. An invoice NAV will *never* accept therefore retries forever;
/// `Nav::cancel_filing` is how a person stops it.
///
/// [`BURNS_REQUEST_ID`] is the exception, and it has to be: `report` always re-sends
/// `invoice.uid`, `NAV_REPORT` is unbounded, and `may_send` keeps saying yes because no
/// verdict was ever recorded — so one clock-skew signature fault would burn the only stable id
/// that invoice has and then retry it hourly forever against a refusal that cannot change.
/// `Retry::Never` stops the loop and leaves the reason where `A-JOB-FAILED` finds it.
pub fn business(code: &str, message: &str) -> Error {
	let detail = format!("{code}: {message}");
	// Same `Retry::Never`, opposite remediation: on `manageInvoice` this means NAV already
	// *processed* a request under this id, so the invoice may well be filed and the operator
	// must query its transaction status rather than storno it.
	if code == "REQUEST_ID_NOT_UNIQUE" {
		return Error::coded(
			StatusCode::BAD_GATEWAY,
			"E-NAV-REQUEST-ID-REUSED",
			format!(
				"{detail} — NAV has already processed a request under this invoice's requestId, \
				 so it may already be filed; query its transaction status before doing anything \
				 else, and do not storno it on the strength of this error"
			),
		);
	}
	if BURNS_REQUEST_ID.contains(&code) {
		return Error::coded(
			StatusCode::BAD_GATEWAY,
			"E-NAV-REQUEST-ID-SPENT",
			format!(
				"{detail} — NAV has spent this invoice's requestId; it can only be filed again \
				 under a new one, i.e. storno and re-issue"
			),
		);
	}
	Error::coded_retry(StatusCode::BAD_GATEWAY, "E-NAV-BUSINESS", detail)
}

// vim: ts=4
