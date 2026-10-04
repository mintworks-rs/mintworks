//! The `fake` provider kind: canned completions served in FIFO order, for `saas-run test`.

use std::{collections::VecDeque, sync::Arc};

use parking_lot::Mutex;

use crate::wire::{ChatEvent, ChatRequest, Message, ToolCall, Usage};

/// One scripted completion: optional text, then tool calls.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Canned {
	pub text: String,
	pub tool_calls: Vec<ToolCall>,
}

impl Canned {
	pub fn text(text: impl Into<String>) -> Self {
		Self { text: text.into(), tool_calls: Vec::new() }
	}

	pub fn tool_calls(tool_calls: Vec<ToolCall>) -> Self {
		Self { text: String::new(), tool_calls }
	}
}

/// A shared queue: clones push into and pop from the same FIFO. It also logs every request it
/// serves, so a test can see what the model was sent.
#[derive(Clone, Debug, Default)]
pub struct FakeQueue(Arc<Mutex<Inner>>);

#[derive(Debug, Default)]
struct Inner {
	queue: VecDeque<Canned>,
	requests: Vec<ChatRequest>,
}

impl FakeQueue {
	pub fn push(&self, canned: Canned) {
		self.0.lock().queue.push_back(canned);
	}

	/// Drops the scripted completions and the request log.
	pub fn clear(&self) {
		let mut g = self.0.lock();
		g.queue.clear();
		g.requests.clear();
	}

	pub fn len(&self) -> usize {
		self.0.lock().queue.len()
	}

	pub fn is_empty(&self) -> bool {
		self.0.lock().queue.is_empty()
	}

	/// Every request served since the last take, oldest first; empties the log.
	pub fn take_requests(&self) -> Vec<ChatRequest> {
		std::mem::take(&mut self.0.lock().requests)
	}

	/// The next completion for `req`; `req` is logged only when one was left to serve it.
	pub(crate) fn pop(&self, req: &ChatRequest) -> Option<Canned> {
		let mut g = self.0.lock();
		let canned = g.queue.pop_front()?;
		g.requests.push(req.clone());
		Some(canned)
	}
}

/// The events a real stream of `canned` would yield. Usage is estimated at 4 characters per
/// token, rounded up, so the ledger sees non-zero rows under test.
pub(crate) fn events(req: &ChatRequest, canned: Canned) -> Vec<ChatEvent> {
	let input: usize = req
		.messages
		.iter()
		.map(|m| match m {
			Message::System { content }
			| Message::User { content }
			| Message::Tool { content, .. } => content.len(),
			Message::Assistant { content, tool_calls } => {
				content.as_deref().map_or(0, str::len)
					+ tool_calls.iter().map(|c| c.arguments.len()).sum::<usize>()
			}
		})
		.sum();
	let output =
		canned.text.len() + canned.tool_calls.iter().map(|c| c.arguments.len()).sum::<usize>();
	let finish = if canned.tool_calls.is_empty() { "stop" } else { "tool_calls" };
	let mut out = Vec::new();
	if !canned.text.is_empty() {
		out.push(ChatEvent::Text(canned.text));
	}
	out.extend(canned.tool_calls.into_iter().map(ChatEvent::ToolCall));
	out.push(ChatEvent::Finish(finish.into()));
	out.push(ChatEvent::Usage(Usage {
		input: input.div_ceil(4) as u64,
		output: output.div_ceil(4) as u64,
		cached: 0,
	}));
	out
}

// vim: ts=4
