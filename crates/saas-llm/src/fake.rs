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

/// A shared queue: clones push into and pop from the same FIFO.
#[derive(Clone, Debug, Default)]
pub struct FakeQueue(Arc<Mutex<VecDeque<Canned>>>);

impl FakeQueue {
	pub fn push(&self, canned: Canned) {
		self.0.lock().push_back(canned);
	}

	pub fn clear(&self) {
		self.0.lock().clear();
	}

	pub fn len(&self) -> usize {
		self.0.lock().len()
	}

	pub fn is_empty(&self) -> bool {
		self.0.lock().is_empty()
	}

	pub(crate) fn pop(&self) -> Option<Canned> {
		self.0.lock().pop_front()
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
