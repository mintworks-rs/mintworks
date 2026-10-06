// SPDX-License-Identifier: MPL-2.0
//! The four Online Számla operations this crate calls, over the envelope and transport
//! `auth.rs` already owns: `manageInvoice`, `queryTransactionStatus`, `queryTransactionList`
//! and `queryTaxpayer`.
//!
//! Section numbers in this module are the NAV interface specification, HU v3.0 (2026-02-12):
//! <https://onlineszamla.nav.gov.hu/files/container/download/Online_Szamla_interfesz%20specifikacio_HU_v3.0.%20(2026.02.12).pdf>
//!
//! Request building and sending are deliberately separate calls: the caller archives the
//! request XML in `nav_submissions` *before* it leaves the process, and archives the reply
//! *before* anything here parses it. Nothing in this module writes to the database, and
//! nothing in it is reachable from the invoice issue path.

use std::fmt::Write as _;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use mintworks_core::{error::StatusCode, prelude::*};
use quick_xml::escape::escape;
use time::OffsetDateTime;

use crate::auth::{NavAuth, stamps};
use crate::reply::{InvoiceResult, Level, Reply};
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
	/// `invoiceStatus` outside `InvoiceStatusType` (`common.xsd:89`). Still polled — a new
	/// transient status must not park a filing — but recorded, so it stops being silent.
	Unknown {
		status: String,
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
	/// `ManageInvoiceRequest` carrying up to 100 invoices. Each entry is the document
	/// `xml::invoice_data` produced plus that invoice's own `pdf_sha256`; the XML is base64'd here,
	/// and the signature is taken over **that exact base64**, never a re-serialisation.
	///
	/// The `<index>` runs from 1, gapless and increasing, in slice order — interface
	/// specification §1.8.1, which NAV enforces; the same order is the signature's chunk
	/// order, so the two cannot be separated.
	///
	/// Obtains its own exchange token: §1.1 makes a token single-use and scoped to exactly one
	/// request — it covers every invoice in this batch and is useless for the next request — so
	/// one is fetched per request, never per invoice and never held across a window.
	/// `request_id` is supplied rather than minted, and the caller passes the filed invoice's
	/// `uid`.
	///
	/// NAV treats `requestId` as an idempotency key: an id it processed — or refused with
	/// `INVALID_REQUEST_SIGNATURE` or `FORBIDDEN` — can never be used again, so a retry under
	/// the same id is refused rather than filed twice (see [`FAULTS`]). That only
	/// protects an invoice if the id is **stable**, and `invoices.uid` is the only thing here
	/// that is: immutable on an `ISSUED` row, so it survives a restart, a `jobs` cleanup and a
	/// database rebuild. `jobs.id` and `nav_submissions.id` are bare rowid aliases with no
	/// `AUTOINCREMENT`, so a cleanup restarting either reissues ids NAV has already burned.
	///
	/// `inv_` plus a 26-character ULID is exactly 30 — `EntityIdType`'s maximum, with no
	/// headroom. A longer uid prefix would have to be shortened here.
	///
	/// `pdf_sha256` is filed as `electronicInvoiceHash`: NAV cannot validate it, it only
	/// archives it, which is what makes it the proof that a buyer's PDF is the issued one.
	/// `None` omits the element — the element asserts electronic issuance (Áfa tv. 175. §),
	/// which a paper-delivered deployment must be able to withhold; see the
	/// `nav.electronic_invoice` setting.
	/// The token is the caller's, not exchanged here: [`crate::job::report`] exchanges before it
	/// claims any row, so a `tokenExchange` outage burns no `nav_submissions` row.
	pub fn manage_invoice_request(
		&self,
		op: NavOp,
		request_id: &str,
		invoices: &[(String, Option<String>)],
		token: &crate::auth::ExchangeToken,
	) -> ClResult<String> {
		let encoded: Vec<String> =
			invoices.iter().map(|(doc, _)| B64.encode(doc.as_bytes())).collect();
		let (header_ts, sign_ts) = stamps(OffsetDateTime::now_utc());
		let chunks: Vec<(&str, &str)> = encoded.iter().map(|b| (op.as_str(), b.as_str())).collect();
		let signature = self.sign_invoices(request_id, &sign_ts, &chunks);

		let mut operations = String::new();
		for (i, ((_, pdf_sha256), b64)) in invoices.iter().zip(&encoded).enumerate() {
			let hash = pdf_sha256.as_deref().map_or_else(String::new, |h| {
				format!(
					"<electronicInvoiceHash cryptoType=\"SHA-256\">{}</electronicInvoiceHash>",
					escape(h.to_ascii_uppercase()),
				)
			});
			let _ = write!(
				operations,
				"<invoiceOperation>\
				 <index>{}</index>\
				 <invoiceOperation>{}</invoiceOperation>\
				 <invoiceData>{b64}</invoiceData>\
				 {hash}\
				 </invoiceOperation>",
				i + 1,
				op.as_str(),
			);
		}
		let body = format!(
			"<exchangeToken>{}</exchangeToken>\
			 <invoiceOperations>\
			 <compressedContent>false</compressedContent>\
			 {operations}\
			 </invoiceOperations>",
			escape(&token.token),
		);
		Ok(self.envelope("ManageInvoiceRequest", request_id, &header_ts, &signature, &body))
	}

	/// `QueryTransactionStatusRequest`. No exchange token — only the two `manage*`
	/// operations carry one.
	pub fn query_status_request(&self, transaction_id: &str) -> String {
		self.status_request(transaction_id, false)
	}

	/// The same question with `returnOriginalRequest=true`, which makes NAV echo every filed
	/// `invoiceData` back. Only [`crate::job::reconcile`] asks for it: on the poll path it
	/// would drag the whole batch's invoice data over the wire and into `response_xml` every
	/// round.
	pub fn query_status_original_request(&self, transaction_id: &str) -> String {
		self.status_request(transaction_id, true)
	}

	fn status_request(&self, transaction_id: &str, return_original: bool) -> String {
		let (request_id, header_ts, sign_ts) = ids();
		let signature = self.sign(&request_id, &sign_ts);
		let body = format!(
			"<transactionId>{}</transactionId>\
			 <returnOriginalRequest>{return_original}</returnOriginalRequest>",
			escape(transaction_id),
		);
		self.envelope("QueryTransactionStatusRequest", &request_id, &header_ts, &signature, &body)
	}

	/// `QueryTransactionListRequest` — every transaction this technical user submitted between
	/// `from` and `to`, one page at a time. `page` runs from 1 and the reply's `availablePage`
	/// says how many there are; both stamps are `common:timestamp` format, which
	/// [`crate::auth::stamps`] renders.
	///
	/// This is half of §1.9.2's lost-transaction recovery. `TransactionType` carries no
	/// `requestId` — the element does not exist in `invoiceApi.xsd` — so a transaction found
	/// here cannot be matched to the batch that submitted it; [`Self::query_status_original_request`]
	/// identifies it by the invoice numbers inside it.
	pub fn query_transaction_list_request(&self, page: i64, from: &str, to: &str) -> String {
		let (request_id, header_ts, sign_ts) = ids();
		let signature = self.sign(&request_id, &sign_ts);
		let body = format!(
			"<page>{page}</page>\
			 <insDate>\
			 <dateTimeFrom>{}</dateTimeFrom>\
			 <dateTimeTo>{}</dateTimeTo>\
			 </insDate>",
			escape(from),
			escape(to),
		);
		self.envelope("QueryTransactionListRequest", &request_id, &header_ts, &signature, &body)
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
		let (_, reply) = self
			.post("queryTaxpayer", &self.query_taxpayer_request(tax_number))
			.await?
			.body("queryTaxpayer")?;
		taxpayer(&Reply::parse(&reply))
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
pub fn accepted(status: StatusCode, reply: &Reply<'_>) -> ClResult<Accepted> {
	match reply.func_code.as_deref() {
		// Acceptance, pending a readable transactionId — fall through to the match below.
		Some("OK") => {}
		// NAV answered, and the answer is definitively not an acceptance: nothing was filed.
		Some(_) => {
			let (code, message) = reply.fault_pair();
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
	match reply.text("transactionId") {
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

/// One `outcomes` result per `processingResult`: NAV's `index`, its verdict, and its own subtree.
pub type OutcomeList<'a> = Vec<(i64, Outcome, &'a str)>;

/// Read a `queryTransactionStatus` reply into one terminal-or-not decision **per invoice**,
/// keyed on each `processingResult`'s own `<index>`.
///
/// `Err` is an envelope-level fault — a `funcCode` other than `OK` is a fault in the
/// *question* and says nothing about any invoice.
///
/// `processingResults` is `minOccurs="0"` (`xsd/invoiceApi.xsd:1662`), so an absent block
/// yields an empty `Vec`. An index the caller holds but the reply does not mention is pending;
/// nothing is settled by omission. `ProcessingResultListType` promises no ordering, which is
/// why position is never used as the key.
/// The third element is that result's own `processingResult` subtree, which `job::poll`
/// archives on the member's row — the whole reply on all N rows is O(N²).
pub fn outcomes<'a>(reply: &Reply<'a>) -> Result<OutcomeList<'a>, Outcome> {
	if !reply.ok() {
		let (code, message) = reply.fault_pair();
		return Err(Outcome::Unavailable { code, message });
	}
	let mut out = Vec::with_capacity(reply.results.len());
	for result in &reply.results {
		// A result with no readable index is unattributable, so every row stays pending rather
		// than one of them taking a verdict that may belong to another invoice.
		if let Some(idx) = result.index {
			out.push((idx, subtree_outcome(result), result.subtree));
		} else {
			tracing::warn!("NAV returned a processingResult with no readable index");
		}
	}
	Ok(out)
}

/// The verdict one `processingResult` carries, read from that result alone: scanning the whole
/// document attributed invoice #3's warning to invoice #1.
fn subtree_outcome(result: &InvoiceResult<'_>) -> Outcome {
	match result.status.as_deref() {
		Some("RECEIVED" | "PROCESSING" | "SAVED") | None => Outcome::Pending,
		// A business block on a DONE invoice is a warning, not a rejection — but an INFO-only
		// one is NAV remarking on an invoice it accepted without reservation, and recording
		// that as `WARN` mislabels a statutory archive.
		Some("DONE") => match Reply::worst(&result.messages) {
			None | Some(Level::Info) => Outcome::Done,
			Some(Level::Warn | Level::Error) => Outcome::Warn,
		},
		Some("ABORTED") => {
			let (code, message) = result.fault_pair();
			Outcome::Failed { code, message }
		}
		Some(other) => Outcome::Unknown { status: other.to_owned() },
	}
}

/// The reconciliation window is `RECONCILE_WINDOW_SECS` wide and NAV pages
/// `queryTransactionList` at 100 per page, so a real answer is one or two pages. The cap is on
/// NAV's number because `jobs.max_attempts.NAV_RECONCILE` is `0`: an unbounded page count is an
/// unbounded request loop against the tax authority.
pub const MAX_TRANSACTION_LIST_PAGES: i64 = 20;

/// One `queryTransactionList` page: the `transactionId`s it carries, and `availablePage`.
///
/// A missing `availablePage` reads as 1, so the caller stops rather than paging forever on a
/// reply it could not understand. More pages than [`MAX_TRANSACTION_LIST_PAGES`] is an `Err`,
/// not a clamp — see below.
pub fn transaction_list(reply: &Reply<'_>) -> ClResult<(Vec<String>, i64)> {
	if !reply.ok() {
		let (code, message) = reply.fault_pair();
		return Err(business(&code, &message));
	}
	// A truncated page is a short list plus `availablePage = 1`, which `reconcile` read as
	// "NAV never took this batch" and answered with a resend under a burned `requestId`.
	if reply.truncated {
		return Err(Error::coded_retry(
			StatusCode::BAD_GATEWAY,
			"E-NAV-UNAVAILABLE",
			"the queryTransactionList page was truncated; the window it covers is unknown",
		));
	}
	let available = reply.text("availablePage").and_then(|s| s.parse::<i64>().ok()).unwrap_or(1);
	// Clamping here made `reconcile` stop at page 20 and conclude "NAV never took this batch",
	// which resends under a burned `requestId`. A seller who genuinely exceeds the cap now
	// retries on a fresher window instead — an unresolved reconciliation beats a wrong one.
	if available > MAX_TRANSACTION_LIST_PAGES {
		return Err(Error::coded_retry(
			StatusCode::BAD_GATEWAY,
			"E-NAV-UNAVAILABLE",
			format!(
				"queryTransactionList reports {available} pages, over the \
				 {MAX_TRANSACTION_LIST_PAGES}-page ceiling; the window it covers is unknown"
			),
		));
	}
	Ok((reply.all_text("transactionId"), available.max(1)))
}

/// `(index, invoiceNumber)` for every result of a `returnOriginalRequest=true` reply, which is
/// how §1.9.2 matches a transaction NAV holds to the batch that submitted it.
///
/// The `Err` arm is [`outcomes`]'s: a fault in the *question*, which names no invoice. A result
/// whose `originalRequest` is absent, undecodable or carries no `invoiceNumber` is skipped —
/// it cannot be matched, and a batch only claims a transaction it matches in full.
pub fn original_invoice_numbers(reply: &Reply<'_>) -> Result<Vec<(i64, String)>, Outcome> {
	if !reply.ok() {
		let (code, message) = reply.fault_pair();
		return Err(Outcome::Unavailable { code, message });
	}
	// A truncated reply is a short `results` list, which `reconcile` reads as "not our
	// transaction" and answers with a resend under a burned `requestId`.
	if reply.truncated {
		return Err(Outcome::Unavailable {
			code: "E-NAV-UNAVAILABLE".to_owned(),
			message: "the queryTransactionStatus reply was truncated; \
			          the transaction it describes is unknown"
				.to_owned(),
		});
	}
	let mut out = Vec::new();
	for result in &reply.results {
		let (Some(idx), Some(encoded)) = (result.index, result.original_request.as_deref()) else {
			continue;
		};
		// Gzip under the base64, so no `invoiceNumber` is findable in it. Named rather than
		// silently skipped: an unmatchable member settles nothing and needs a person.
		if result.compressed {
			tracing::warn!(
				index = idx,
				"NAV echoed a compressed originalRequest; this crate never sends one"
			);
			continue;
		}
		let Ok(bytes) = B64.decode(encoded.as_bytes()) else {
			tracing::warn!(index = idx, "NAV returned an undecodable originalRequest");
			continue;
		};
		if let Some(number) =
			crate::reply::Reply::parse(&String::from_utf8_lossy(&bytes)).text("invoiceNumber")
		{
			out.push((idx, number));
		}
	}
	Ok(out)
}

pub fn taxpayer(reply: &Reply<'_>) -> ClResult<Taxpayer> {
	if !reply.ok() {
		let (code, message) = reply.fault_pair();
		return Err(business(&code, &message));
	}
	Ok(Taxpayer {
		valid: reply.text("taxpayerValidity").as_deref() == Some("true"),
		name: reply.text("taxpayerName"),
		tax_number: reply.text("taxpayerId"),
	})
}

/// What an operator would have to do about a NAV fault, which is also what the runner does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
	/// The cause clears on its own or by an operator changing configuration: an outage, clock
	/// skew, a credential. Retried forever, because terminating drops every invoice issued
	/// while the fault lasts.
	Retry,
	/// NAV spent this `requestId`. `report` re-sends `invoices.uid`, immutable on an ISSUED
	/// row, so every later attempt is refused identically: storno and re-issue.
	SpendsRequestId,
	/// NAV has already *processed* a request under this id, so the invoice may be on file.
	/// Query the transaction before anything else; never storno on the strength of it.
	PossiblyFiled,
	/// NAV answers the same way however often it is asked, and nothing here changes the
	/// answer. Parked for a person, no `requestId` spent and no storno implied.
	NeedsPerson,
}

/// NAV general error codes with a disposition this repository can justify. Everything absent
/// is [`Disposition::Retry`]: the catch-all is deliberate, and a code joins this table only
/// when its meaning is established, never on a guess.
///
/// The first three are the interface specification's §"requestId" uniqueness paragraph: *"Az
/// egyediségbe minden sikeresen feldolgozott kérés, valamint az INVALID_REQUEST_SIGNATURE és
/// FORBIDDEN hibakóddal elutasított kérés azonosítója számít bele. Ezen kérések azonosítói
/// (requestId) nem használhatók fel újra."* — a processed request, and one refused with either
/// of those two codes, burns its id. `REQUEST_ID_NOT_UNIQUE` is the refusal saying the id is
/// already burned, so it stops the loop too, but its remediation is the opposite one.
///
/// (The spec scopes uniqueness to "the timestamp tolerance window", so the burn may not be
/// eternal. Nothing here gambles on it expiring: `report` sends `invoices.uid`, which is
/// immutable on an `ISSUED` row, so every later attempt carries the same id.)
const FAULTS: &[(&str, Disposition)] = &[
	("INVALID_REQUEST_SIGNATURE", Disposition::SpendsRequestId),
	("FORBIDDEN", Disposition::SpendsRequestId),
	("REQUEST_ID_NOT_UNIQUE", Disposition::PossiblyFiled),
	// The number is already filed; no attempt under it can ever be accepted.
	("INVOICE_NUMBER_NOT_UNIQUE", Disposition::NeedsPerson),
	// `xml::invoice_data` is deterministic: the next attempt sends the same document and earns
	// the same refusal. Either the mapping or the invoice has to change.
	("SCHEMA_VIOLATION", Disposition::NeedsPerson),
	// An operator fixing `nav.tech_password` makes the next attempt succeed, so this stays
	// retryable — listed to record that the choice was made, not missed.
	("INVALID_SECURITY_USER", Disposition::Retry),
	("OPERATION_FAILED", Disposition::Retry),
];

#[must_use]
pub fn disposition(code: &str) -> Disposition {
	FAULTS.iter().find(|(c, _)| *c == code).map_or(Disposition::Retry, |(_, d)| *d)
}

/// A fault NAV answered with, as the error the job runner acts on.
///
/// `Retry::Backoff` for the catch-all: on `manageInvoice` a fault means nothing was filed, and
/// the causes a code table cannot name — an outage, clock skew, a credential — are exactly the
/// ones that clear on their own, so terminating on the first one would silently drop every
/// invoice issued while the fault lasted.
///
/// The three terminal dispositions differ only in remediation, and the `errCode` is what
/// carries that to the operator through `jobs.last_error`: a spent id needs a storno and
/// re-issue, a reused one needs a `queryTransactionStatus` first, and an unfilable invoice
/// needs correcting — under no new `requestId` at all.
pub fn business(code: &str, message: &str) -> Error {
	let detail = format!("{code}: {message}");
	match disposition(code) {
		Disposition::PossiblyFiled => Error::coded(
			StatusCode::BAD_GATEWAY,
			"E-NAV-REQUEST-ID-REUSED",
			format!(
				"{detail} — NAV has already processed a request under this invoice's requestId, \
				 so it may already be filed; query its transaction status before doing anything \
				 else, and do not storno it on the strength of this error"
			),
		),
		Disposition::SpendsRequestId => Error::coded(
			StatusCode::BAD_GATEWAY,
			"E-NAV-REQUEST-ID-SPENT",
			format!(
				"{detail} — NAV has spent this invoice's requestId; it can only be filed again \
				 under a new one, i.e. storno and re-issue"
			),
		),
		Disposition::NeedsPerson => Error::coded(
			StatusCode::BAD_GATEWAY,
			"E-NAV-UNFILABLE",
			format!(
				"{detail} — NAV refused this invoice for a reason no retry changes; nothing was \
				 filed and no request identifier was consumed, so the filing is parked for a \
				 person to correct the invoice and issue it again"
			),
		),
		Disposition::Retry => Error::coded_retry(StatusCode::BAD_GATEWAY, "E-NAV-BUSINESS", detail),
	}
}

// vim: ts=4
