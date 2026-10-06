//! One typed reading of a NAV reply document.
//!
//! Every operation used to re-derive what a reply meant from flat `element_text` lookups, so
//! the first `errorCode` anywhere in the document was the fault, N validation messages
//! collapsed to one, and `validationResultCode` was never read at all. [`Reply::parse`] is a
//! single `quick_xml` pass that keeps the structure the document actually has: what is inside
//! `<result>`, what is inside each `<processingResult>`, and which validation block each
//! message came out of.
//!
//! Section numbers are the NAV interface specification, HU v3.0 — see `client.rs`.

use quick_xml::{Reader, events::Event};

use crate::auth::element_text;

/// Which validation block a message came out of. The two carry different result-code
/// vocabularies (`common:TechnicalResultCodeType` is CRITICAL/ERROR, `BusinessResultCodeType`
/// is ERROR/WARN/INFO) and different remediations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
	Technical,
	Business,
}

/// `validationResultCode`, collapsed onto the three levels a caller acts on. CRITICAL folds
/// into `Error`; an unreadable or absent code reads as `Error`, never as `Info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
	Info,
	Warn,
	Error,
}

impl Level {
	fn parse(code: Option<&str>) -> Self {
		match code {
			Some("INFO") => Self::Info,
			Some("WARN") => Self::Warn,
			_ => Self::Error,
		}
	}
}

#[derive(Clone, Debug)]
pub struct Message {
	pub origin: Origin,
	pub level: Level,
	pub code: Option<String>,
	pub text: Option<String>,
}

/// One `<processingResult>`.
#[derive(Clone, Debug)]
pub struct InvoiceResult<'a> {
	pub index: Option<i64>,
	pub status: Option<String>,
	pub messages: Vec<Message>,
	pub original_request: Option<String>,
	/// `compressedContentIndicator`: `originalRequest` needs gunzipping before it is XML.
	pub compressed: bool,
	/// This result's own subtree, which `job::poll` archives on the member's row.
	pub subtree: &'a str,
}

impl InvoiceResult<'_> {
	/// `(code, message)` for this result's worst message, `("unknown", …)` when it has none.
	#[must_use]
	pub fn fault_pair(&self) -> (String, String) {
		pair(self.messages.iter().max_by_key(|m| m.level))
	}
}

#[derive(Clone, Debug)]
pub struct Reply<'a> {
	/// `<result><funcCode>` only — scoped, so an echoed request body cannot supply it.
	pub func_code: Option<String>,
	/// The envelope's own fault, read from `<result>`.
	pub fault: Option<Message>,
	pub results: Vec<InvoiceResult<'a>>,
	/// The XML ended where `quick_xml` stopped understanding it. Swallowed as EOF before.
	pub truncated: bool,
	src: &'a str,
}

/// `(code, message)` with the fallbacks every caller of a fault pair shares.
fn pair(message: Option<&Message>) -> (String, String) {
	(
		message.and_then(|m| m.code.clone()).unwrap_or_else(|| "unknown".to_owned()),
		message
			.and_then(|m| m.text.clone())
			.unwrap_or_else(|| "NAV gave no message".to_owned()),
	)
}

impl<'a> Reply<'a> {
	#[must_use]
	pub fn parse(xml: &'a str) -> Self {
		Parser::run(xml)
	}

	#[must_use]
	pub fn ok(&self) -> bool {
		self.func_code.as_deref() == Some("OK")
	}

	/// The flat lookups a reply still needs — `transactionId`, `availablePage`, `taxpayerName`,
	/// `encodedExchangeToken`. They name elements that are unique in the documents that carry
	/// them, so there is nothing for the structured parse to disambiguate.
	#[must_use]
	pub fn text(&self, name: &str) -> Option<String> {
		element_text(self.src, name)
	}

	/// Every non-empty `<…:name>` text in the document, in document order. [`Self::text`]
	/// returns the first, which a list reply's repeated element is not.
	#[must_use]
	pub fn all_text(&self, name: &str) -> Vec<String> {
		let mut out = Vec::new();
		let mut reader = Reader::from_str(self.src);
		// `<x/>` is an `Empty` event, never a `Start`, so an unexpanded element is invisible to
		// the `inside` flag below.
		reader.config_mut().expand_empty_elements = true;
		let mut inside = false;
		loop {
			match reader.read_event() {
				Ok(Event::Start(e)) => inside = e.local_name().as_ref() == name.as_bytes(),
				// Without this the whitespace `Text` node after the close tag still matches.
				Ok(Event::End(_)) => inside = false,
				Ok(Event::Text(t)) if inside => {
					if let Ok(text) = t.unescape()
						&& !text.trim().is_empty()
					{
						out.push(text.trim().to_owned());
					}
				}
				Ok(Event::Eof) | Err(_) => return out,
				_ => {}
			}
		}
	}

	/// `(code, message)` for the envelope fault, `("unknown", …)` when there is none.
	#[must_use]
	pub fn fault_pair(&self) -> (String, String) {
		pair(self.fault.as_ref())
	}

	/// The highest level among a result's messages, `None` when it has none.
	#[must_use]
	pub fn worst(messages: &[Message]) -> Option<Level> {
		messages.iter().map(|m| m.level).max()
	}
}

/// The one pass. Depth counters rather than a stack: the three contexts that matter never
/// nest inside themselves, and a `<message>` belongs to the innermost one that is open.
struct Parser<'a> {
	src: &'a str,
	/// Open `<result>` elements. `>0` is the envelope's own result block.
	in_result: u32,
	/// Where the open `<processingResult>`'s subtree starts, and what it has read so far.
	result_at: Option<(usize, InvoiceResult<'a>)>,
	/// The open validation-messages block: one block is one message (`TechnicalValidationResultType`).
	block: Option<Message>,
	/// Whether the open block has read any field of its own. `<x/>` is expanded, so a block
	/// that says nothing would otherwise emit a message defaulting to `Level::Error`.
	block_read: bool,
	/// The element whose text the next `Text` event carries.
	elem: Option<Vec<u8>>,
	out: Reply<'a>,
	/// `<result>`'s own `errorCode`/`message`, assembled into [`Reply::fault`] at the end.
	fault_code: Option<String>,
	fault_text: Option<String>,
}

impl<'a> Parser<'a> {
	fn run(src: &'a str) -> Reply<'a> {
		let mut p = Self {
			src,
			in_result: 0,
			result_at: None,
			block: None,
			block_read: false,
			elem: None,
			out: Reply { func_code: None, fault: None, results: Vec::new(), truncated: false, src },
			fault_code: None,
			fault_text: None,
		};
		let mut reader = Reader::from_str(src);
		// `<x/>` is an `Empty` event, never a `Start`: an unexpanded `<invoiceStatus/>` read as
		// `Outcome::Pending` and `NAV_POLL` is unbounded, so the poll never stopped.
		reader.config_mut().expand_empty_elements = true;
		loop {
			// Read before the event, so it is the offset of the tag about to be parsed: a
			// subtree ends where `</processingResult>` begins.
			let pos = usize::try_from(reader.buffer_position()).unwrap_or(usize::MAX);
			match reader.read_event() {
				Ok(Event::Start(e)) => {
					let here = usize::try_from(reader.buffer_position()).unwrap_or(usize::MAX);
					p.start(e.local_name().as_ref(), here);
				}
				Ok(Event::End(e)) => p.end(e.local_name().as_ref(), pos),
				Ok(Event::Text(t)) => {
					if let Ok(text) = t.unescape() {
						p.text(text.trim());
					}
				}
				// A CDATA-wrapped value is a `CData` event, never a `Text` one.
				Ok(Event::CData(c)) => {
					if let Ok(text) = String::from_utf8(c.to_vec()) {
						p.text(text.trim());
					}
				}
				Ok(Event::Eof) => break,
				Err(_) => {
					p.out.truncated = true;
					break;
				}
				_ => {}
			}
		}
		p.finish()
	}

	fn start(&mut self, name: &[u8], after_tag: usize) {
		match name {
			b"result" => self.in_result += 1,
			b"processingResult" => {
				self.result_at = Some((
					after_tag,
					InvoiceResult {
						index: None,
						status: None,
						messages: Vec::new(),
						original_request: None,
						compressed: false,
						subtree: "",
					},
				));
			}
			b"technicalValidationMessages" | b"businessValidationMessages" => {
				let origin = if name == b"technicalValidationMessages" {
					Origin::Technical
				} else {
					Origin::Business
				};
				self.block = Some(Message { origin, level: Level::Error, code: None, text: None });
				self.block_read = false;
			}
			_ => {}
		}
		self.elem = Some(name.to_vec());
	}

	fn end(&mut self, name: &[u8], before_tag: usize) {
		match name {
			b"result" => self.in_result = self.in_result.saturating_sub(1),
			b"processingResult" => {
				if let Some((from, mut result)) = self.result_at.take() {
					result.subtree = self.src.get(from..before_tag).unwrap_or_default();
					self.out.results.push(result);
				}
			}
			b"technicalValidationMessages" | b"businessValidationMessages" => {
				// An empty block says nothing, and a phantom message defaults to `Level::Error`,
				// which turns a `DONE` NAV was happy with into `Outcome::Warn`.
				let read = std::mem::take(&mut self.block_read);
				if let Some(message) = self.block.take().filter(|_| read) {
					match &mut self.result_at {
						Some((_, result)) => result.messages.push(message),
						// Outside every `processingResult`: `GeneralErrorResponse` states its
						// fault this way, beside `<result>` rather than inside it.
						None => {
							if self.fault_code.is_none() {
								self.fault_code.clone_from(&message.code);
								self.fault_text.clone_from(&message.text);
							}
						}
					}
				}
			}
			_ => {}
		}
		// Without this the whitespace `Text` node *after* a close tag is still attributed to
		// the element that just ended.
		self.elem = None;
	}

	fn text(&mut self, text: &str) {
		if text.is_empty() {
			return;
		}
		let Some(elem) = self.elem.clone() else { return };
		// A message block is the innermost context: `<message>` means its message, not the
		// enclosing result's.
		if let Some(message) = &mut self.block {
			match elem.as_slice() {
				b"validationResultCode" => message.level = Level::parse(Some(text)),
				b"validationErrorCode" => message.code = Some(text.to_owned()),
				b"message" => message.text = Some(text.to_owned()),
				_ => return,
			}
			self.block_read = true;
			return;
		}
		if let Some((_, result)) = &mut self.result_at {
			match elem.as_slice() {
				b"index" => result.index = text.parse().ok(),
				b"invoiceStatus" => result.status = Some(text.to_owned()),
				b"originalRequest" => result.original_request = Some(text.to_owned()),
				b"compressedContentIndicator" => result.compressed = text == "true",
				_ => {}
			}
			return;
		}
		if self.in_result > 0 {
			match elem.as_slice() {
				b"funcCode" => self.out.func_code = Some(text.to_owned()),
				b"errorCode" => self.fault_code = Some(text.to_owned()),
				b"message" => self.fault_text = Some(text.to_owned()),
				_ => {}
			}
		}
	}

	fn finish(mut self) -> Reply<'a> {
		if self.fault_code.is_some() || self.fault_text.is_some() {
			self.out.fault = Some(Message {
				origin: Origin::Technical,
				level: Level::Error,
				code: self.fault_code,
				text: self.fault_text,
			});
		}
		self.out
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// One block is one message, and both vocabularies survive: `error_pair` kept the first
	/// code and the first message of the whole document, so N−1 messages were lost and a
	/// technical block was indistinguishable from a business one.
	#[test]
	fn every_validation_message_survives_the_parse() {
		let xml = "<R><result><funcCode>OK</funcCode></result><processingResults>\
			<processingResult><index>1</index><invoiceStatus>DONE</invoiceStatus>\
			<technicalValidationMessages><validationResultCode>CRITICAL</validationResultCode>\
			<validationErrorCode>T1</validationErrorCode><message>tech</message>\
			</technicalValidationMessages>\
			<businessValidationMessages><validationResultCode>INFO</validationResultCode>\
			<validationErrorCode>B1</validationErrorCode><message>info</message>\
			</businessValidationMessages>\
			<businessValidationMessages><validationResultCode>WARN</validationResultCode>\
			<validationErrorCode>B2</validationErrorCode><message>warn</message>\
			</businessValidationMessages>\
			</processingResult></processingResults></R>";
		let reply = Reply::parse(xml);
		let result = &reply.results[0];
		assert_eq!(result.messages.len(), 3);
		assert_eq!(result.messages[0].origin, Origin::Technical);
		assert_eq!(result.messages[0].level, Level::Error, "CRITICAL folds into Error");
		assert_eq!(result.messages[1].origin, Origin::Business);
		assert_eq!(result.messages[1].level, Level::Info);
		assert_eq!(result.messages[2].level, Level::Warn);
		assert_eq!(
			result.messages.iter().filter_map(|m| m.code.as_deref()).collect::<Vec<_>>(),
			vec!["T1", "B1", "B2"]
		);
		assert_eq!(Reply::worst(&result.messages), Some(Level::Error));
		// The worst message is the one an operator is shown, not the first.
		assert_eq!(result.fault_pair().0, "T1");
	}

	/// quick-xml never raises `Start` for `<x/>`, so an unexpanded `<invoiceStatus/>` left
	/// `status = None`, which `client::subtree_outcome` reads as `Pending` — and
	/// `jobs.max_attempts.NAV_POLL` is `0`, so the poll never stopped.
	#[test]
	fn a_self_closing_element_is_still_an_element() {
		let reply = Reply::parse(
			"<R><result><funcCode>OK</funcCode></result><processingResults>\
			 <processingResult><index>1</index><invoiceStatus/>\
			 <technicalValidationMessages/></processingResult></processingResults></R>",
		);
		assert_eq!(reply.results.len(), 1);
		assert_eq!(reply.results[0].index, Some(1));
		assert!(reply.results[0].messages.is_empty(), "an empty block carries no message");
	}
}

// vim: ts=4
