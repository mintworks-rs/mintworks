//! One provider call: one streamed completion.
//!
//! Errors are classified for the fallback in the service layer: `Error::retry() ==
//! Retry::Backoff` (429, 5xx, a transport failure or timeout, a mid-stream `error`) means try
//! the next profile entry; anything else (other 4xx) goes back to the caller.

use std::{
	collections::{BTreeMap, VecDeque},
	time::Duration,
};

use mintworks_core::{ClResult, Error, error::StatusCode, http};

use crate::{
	fake::{self, FakeQueue},
	sse::SseParser,
	wire::{ChatEvent, ChatRequest, Chunk, ToolCall},
};

/// A 429 or 5xx from the provider, or a broken stream: the next profile entry may answer.
pub const E_UPSTREAM: &str = "E-LLM-UPSTREAM";
/// A 4xx other than 429: the request itself is wrong, and every provider will say so.
pub const E_REJECTED: &str = "E-LLM-REJECTED";
/// A `fake` provider called with nothing queued.
pub const E_FAKE_EMPTY: &str = "E-LLM-FAKE-EMPTY";

// No `Debug`: `OpenAi` holds the API key.
#[derive(Clone)]
pub enum Provider {
	OpenAi(OpenAi),
	Fake(FakeQueue),
}

/// An OpenAI-compatible endpoint.
#[derive(Clone)]
pub struct OpenAi {
	/// Without `/v1`.
	pub base_url: String,
	pub api_key: Option<String>,
	/// Bounds the wait for the status line — the model's time to first token.
	pub deadline: Duration,
	/// Bounds each gap between chunks after it.
	pub idle: Duration,
	/// Let `base_url` be a loopback or private address (a self-hosted model, a test server).
	pub allow_internal: bool,
}

impl OpenAi {
	pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
		Self {
			base_url: base_url.into(),
			api_key,
			deadline: Duration::from_secs(120),
			idle: Duration::from_secs(60),
			allow_internal: false,
		}
	}
}

impl Provider {
	/// Start one streamed completion of `req` on `model`. A non-2xx status fails here, before
	/// any event, so the caller can still fall back.
	pub async fn stream(&self, model: &str, req: &ChatRequest) -> ClResult<ChatStream> {
		match self {
			Self::Fake(queue) => {
				let canned = queue.pop(req).ok_or_else(|| {
					Error::coded(
						StatusCode::INTERNAL_SERVER_ERROR,
						E_FAKE_EMPTY,
						"the fake LLM provider has no scripted completion left",
					)
				})?;
				Ok(ChatStream { http: None, pending: fake::events(req, canned).into() })
			}
			Self::OpenAi(p) => p.stream(model, req).await,
		}
	}
}

impl OpenAi {
	async fn stream(&self, model: &str, req: &ChatRequest) -> ClResult<ChatStream> {
		let uri = format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'));
		let auth = self.api_key.as_ref().map(|k| format!("Bearer {k}"));
		let mut headers =
			vec![("content-type", "application/json"), ("accept", "text/event-stream")];
		if let Some(auth) = &auth {
			headers.push(("authorization", auth.as_str()));
		}
		let body = serde_json::to_vec(&req.body(model))
			.map_err(|e| Error::internal(format!("llm request body: {e}")))?;
		let (status, retry_after, mut stream) = http::post_external_stream(
			&uri,
			&headers,
			body,
			self.deadline,
			self.idle,
			self.allow_internal,
		)
		.await?;
		if !status.is_success() {
			let detail = error_detail(&mut stream).await;
			tracing::warn!(%uri, %status, ?retry_after, %detail, "llm provider refused");
			let msg = format!("LLM provider answered {status}");
			return Err(if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
				Error::coded_retry(StatusCode::BAD_GATEWAY, E_UPSTREAM, msg)
			} else {
				Error::coded(StatusCode::BAD_GATEWAY, E_REJECTED, msg)
			});
		}
		Ok(ChatStream {
			http: Some(HttpStream {
				body: stream,
				sse: SseParser::default(),
				calls: BTreeMap::new(),
				finished: false,
				ended: false,
			}),
			pending: VecDeque::new(),
		})
	}
}

/// The start of an error body, for the log only: it may echo the prompt.
async fn error_detail(stream: &mut http::BodyStream) -> String {
	match stream.next_chunk().await {
		Ok(Some(b)) => String::from_utf8_lossy(&b[..b.len().min(512)]).into_owned(),
		_ => String::new(),
	}
}

/// The events of one completion; see [`ChatEvent`] for their order.
pub struct ChatStream {
	http: Option<HttpStream>,
	pending: VecDeque<ChatEvent>,
}

struct HttpStream {
	body: http::BodyStream,
	sse: SseParser,
	/// Tool calls under assembly, by the provider's `index`.
	calls: BTreeMap<u32, ToolCall>,
	/// A `finish_reason` arrived: without it or `[DONE]`, EOF is a cut connection.
	finished: bool,
	ended: bool,
}

impl ChatStream {
	/// The next event, `None` once the completion is over. An error mid-stream is retryable
	/// only in the sense that another provider may answer: events already yielded stand.
	pub async fn next(&mut self) -> ClResult<Option<ChatEvent>> {
		loop {
			if let Some(ev) = self.pending.pop_front() {
				return Ok(Some(ev));
			}
			let Some(h) = self.http.as_mut() else { return Ok(None) };
			if h.ended {
				return Ok(None);
			}
			if let Some(data) = h.sse.next_event() {
				h.chunk(&data, &mut self.pending)?;
				continue;
			}
			if h.sse.is_done() {
				h.end(&mut self.pending);
				continue;
			}
			match h.body.next_chunk().await {
				Ok(Some(bytes)) => h.sse.push(&bytes),
				Ok(None) => {
					if let Some(data) = h.sse.finish() {
						h.chunk(&data, &mut self.pending)?;
					}
					if !h.finished {
						return Err(broken("stream ended without finish_reason or [DONE]"));
					}
					h.end(&mut self.pending);
				}
				Err(e) => return Err(broken(&e.to_string())),
			}
		}
	}
}

impl HttpStream {
	fn chunk(&mut self, data: &str, out: &mut VecDeque<ChatEvent>) -> ClResult<()> {
		let chunk: Chunk = serde_json::from_str(data)
			.map_err(|e| broken(&format!("unparseable stream chunk: {e}")))?;
		if let Some(err) = chunk.error {
			return Err(broken(&format!("provider error in stream: {err}")));
		}
		for choice in chunk.choices {
			if let Some(text) = choice.delta.content.filter(|t| !t.is_empty()) {
				out.push_back(ChatEvent::Text(text));
			}
			for d in choice.delta.tool_calls {
				let call = self.calls.entry(d.index).or_insert_with(|| ToolCall {
					id: String::new(),
					name: String::new(),
					arguments: String::new(),
				});
				if let Some(id) = d.id.filter(|s| !s.is_empty()) {
					call.id = id;
				}
				if let Some(name) = d.function.name {
					call.name.push_str(&name);
				}
				if let Some(args) = d.function.arguments {
					call.arguments.push_str(&args);
				}
			}
			if let Some(reason) = choice.finish_reason {
				self.finished = true;
				self.flush_calls(out);
				out.push_back(ChatEvent::Finish(reason));
			}
		}
		if let Some(usage) = chunk.usage {
			out.push_back(ChatEvent::Usage(usage));
		}
		Ok(())
	}

	/// A stream that ends at `[DONE]` without a `finish_reason` still hands over its tool calls.
	fn end(&mut self, out: &mut VecDeque<ChatEvent>) {
		self.flush_calls(out);
		self.ended = true;
	}

	fn flush_calls(&mut self, out: &mut VecDeque<ChatEvent>) {
		out.extend(std::mem::take(&mut self.calls).into_values().map(ChatEvent::ToolCall));
	}
}

fn broken(msg: &str) -> Error {
	tracing::warn!(%msg, "llm stream broken");
	Error::coded_retry(StatusCode::BAD_GATEWAY, E_UPSTREAM, "LLM provider stream broke off")
}

// vim: ts=4
