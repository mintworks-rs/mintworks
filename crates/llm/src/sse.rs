// SPDX-License-Identifier: MPL-2.0
//! A server-sent-events splitter: bytes in, `data:` payloads out, nothing after `[DONE]`.
//! Other fields (`event:`, `id:`, `retry:`) and `:` comments are ignored — the chat stream
//! carries everything in `data:`.

/// Feed it chunks with [`push`](Self::push), drain events with [`next_event`](Self::next_event).
#[derive(Debug, Default)]
pub struct SseParser {
	buf: Vec<u8>,
	data: Option<String>,
	done: bool,
}

impl SseParser {
	pub fn push(&mut self, chunk: &[u8]) {
		if !self.done {
			self.buf.extend_from_slice(chunk);
		}
	}

	/// The next complete event's data (multi-line `data:` joined with `\n`), or `None` until
	/// more bytes arrive — and forever once `[DONE]` has been seen.
	pub fn next_event(&mut self) -> Option<String> {
		while !self.done {
			let nl = self.buf.iter().position(|&b| b == b'\n')?;
			let raw: Vec<u8> = self.buf.drain(..=nl).collect();
			// Whole lines only, so a multi-byte character is never split here.
			let line = String::from_utf8_lossy(&raw);
			if let Some(data) = self.line(line.trim_end_matches(['\r', '\n'])) {
				return Some(data);
			}
		}
		None
	}

	/// End of body: the last event, when the server omitted the closing blank line.
	pub fn finish(&mut self) -> Option<String> {
		if !self.done && !self.buf.is_empty() {
			let raw = std::mem::take(&mut self.buf);
			let line = String::from_utf8_lossy(&raw).into_owned();
			if let Some(data) = self.line(line.trim_end_matches('\r')) {
				return Some(data);
			}
		}
		self.line("")
	}

	pub fn is_done(&self) -> bool {
		self.done
	}

	/// One line; a blank one dispatches the pending event.
	fn line(&mut self, line: &str) -> Option<String> {
		if line.is_empty() {
			let data = self.data.take()?;
			if data == "[DONE]" {
				self.done = true;
				self.buf.clear();
				return None;
			}
			return Some(data);
		}
		if let Some(v) = line.strip_prefix("data:") {
			let v = v.strip_prefix(' ').unwrap_or(v);
			match &mut self.data {
				Some(d) => {
					d.push('\n');
					d.push_str(v);
				}
				None => self.data = Some(v.to_owned()),
			}
		}
		None
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn all(p: &mut SseParser) -> Vec<String> {
		std::iter::from_fn(|| p.next_event()).collect()
	}

	#[test]
	fn events_split_across_chunks_and_crlf() {
		let mut p = SseParser::default();
		p.push(b"data: {\"a\"");
		assert!(p.next_event().is_none());
		p.push(b":1}\r\n\r\n: keepalive\n\nevent: x\ndata: b\n");
		assert_eq!(all(&mut p), vec!["{\"a\":1}"]);
		p.push(b"data: c\n\n");
		assert_eq!(all(&mut p), vec!["b\nc"]);
	}

	#[test]
	fn nothing_after_done() {
		let mut p = SseParser::default();
		p.push(b"data:x\n\ndata: [DONE]\n\ndata: y\n\n");
		assert_eq!(all(&mut p), vec!["x"]);
		assert!(p.is_done());
		p.push(b"data: z\n\n");
		assert!(p.next_event().is_none());
		assert!(p.finish().is_none());
	}

	#[test]
	fn finish_flushes_unterminated_event() {
		let mut p = SseParser::default();
		p.push(b"data: last");
		assert!(p.next_event().is_none());
		assert_eq!(p.finish().as_deref(), Some("last"));
	}
}

// vim: ts=4
