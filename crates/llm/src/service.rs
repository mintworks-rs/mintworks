// SPDX-License-Identifier: MPL-2.0
//! The `Llm` service handle: a role resolves to its `llm.profile.<role>` fallback list, and each
//! entry is tried under its provider's concurrency slot until one starts streaming.

use std::{collections::HashMap, sync::Arc, time::Duration};

use mintworks_core::{App, ClResult, Ctx, Error, Retry, error::StatusCode, prelude::Timestamp};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
	config::{Price, Target, parse_profile},
	fake::FakeQueue,
	ledger,
	provider::{ChatStream, OpenAi, Provider},
	store::{UsageKind, UsageRow},
	structured,
	wire::{ChatEvent, ChatRequest, Message, ToolCall, Usage},
};

/// A role without a profile, a provider without a base URL, or `llm` not enabled.
pub const E_CONFIG: &str = "E-LLM-CONFIG";
/// Every profile entry's provider stayed at `max_concurrent` for `llm.slot_wait_secs`.
pub const E_BUSY: &str = "E-LLM-BUSY";

/// The state every [`Llm`] handle shares, registered with
/// `AppBuilder::extension(LlmState::default())`. Without it, every call fails with
/// `E-LLM-CONFIG` — that is the `llm` feature gate.
#[derive(Clone, Default)]
pub struct LlmState(Arc<Shared>);

#[derive(Default)]
struct Shared {
	/// One queue for every `fake`-kind provider, so a test scripts completions without
	/// knowing which provider the profile picks.
	fake: FakeQueue,
	/// Per provider: the `max_concurrent` it was sized for, and the semaphore.
	slots: Mutex<HashMap<String, (usize, Arc<Semaphore>)>>,
}

impl LlmState {
	pub fn fake_queue(&self) -> &FakeQueue {
		&self.0.fake
	}
}

/// One call. `role` picks the profile; `subject`, `step` and `run` label the ledger row.
#[derive(Clone, Debug, Default)]
pub struct CallSpec {
	pub role: String,
	pub request: ChatRequest,
	/// A JSON schema the output must satisfy; [`Llm::complete`] validates and re-asks.
	pub schema: Option<serde_json::Value>,
	/// Opaque budget key, e.g. `project:prj_…`.
	pub subject: Option<String>,
	pub step: String,
	pub run: Option<String>,
	/// `provider:model` to try first, if it is still in the profile.
	pub prefer: Option<String>,
}

/// A started completion and the profile entry that answered. Its ledger row is written when
/// the stream ends, fails, or is dropped unfinished.
pub struct LlmStream {
	pub target: Target,
	stream: ChatStream,
	_permit: OwnedSemaphorePermit,
	pending: Option<Pending>,
	usage: Option<Usage>,
	/// The request's estimated input tokens, and the streamed text and tool-argument chars:
	/// the estimate when no usage arrives.
	in_est: u64,
	out_chars: u64,
}

impl LlmStream {
	pub async fn next(&mut self) -> ClResult<Option<ChatEvent>> {
		let ev = self.stream.next().await;
		match &ev {
			Ok(Some(ChatEvent::Usage(u))) => self.usage = Some(*u),
			Ok(Some(ChatEvent::Text(t))) => {
				self.out_chars = self.out_chars.saturating_add(chars(t));
			}
			Ok(Some(ChatEvent::ToolCall(c))) => {
				self.out_chars = self.out_chars.saturating_add(chars(&c.arguments));
			}
			Ok(Some(_)) => {}
			Ok(None) | Err(_) => {
				if let Some(p) = self.pending.take() {
					let usage = self.used();
					p.record(usage).await;
				}
			}
		}
		ev
	}

	/// The provider's reported usage, else an estimate at 4 chars a token: a stream cut before
	/// its usage chunk, or from a provider that never sends one, was still billed.
	#[must_use]
	pub fn used(&self) -> Usage {
		self.usage.unwrap_or(Usage {
			input: self.in_est,
			output: self.out_chars.div_ceil(4),
			..Usage::default()
		})
	}
}

fn chars(s: &str) -> u64 {
	u64::try_from(s.chars().count()).unwrap_or(u64::MAX)
}

/// The request's message and tool-argument chars / 4, rounded up.
fn input_estimate(req: &ChatRequest) -> u64 {
	let n: u64 = req
		.messages
		.iter()
		.map(|m| match m {
			Message::System { content }
			| Message::User { content }
			| Message::Tool { content, .. } => chars(content),
			Message::Assistant { content, tool_calls } => {
				tool_calls.iter().fold(content.as_deref().map_or(0, chars), |n, c| {
					n.saturating_add(chars(&c.arguments))
				})
			}
		})
		.fold(0, u64::saturating_add);
	n.div_ceil(4)
}

impl Drop for LlmStream {
	fn drop(&mut self) {
		// An abandoned stream was still billed by the provider.
		if let Some(p) = self.pending.take()
			&& let Ok(rt) = tokio::runtime::Handle::try_current()
		{
			let usage = self.used();
			rt.spawn(async move { p.record(usage).await });
		}
	}
}

/// A provider call's ledger row, waiting for its usage.
struct Pending {
	app: App,
	row: UsageRow,
	price: Option<Price>,
}

impl Pending {
	/// A recording failure is logged, not returned: the provider has already answered.
	async fn record(mut self, usage: Usage) {
		self.row.tokens_in = i64::try_from(usage.input).unwrap_or(i64::MAX);
		self.row.tokens_out = i64::try_from(usage.output).unwrap_or(i64::MAX);
		self.row.cost_micro_eur = if let Some(price) = self.price {
			ledger::cost_micro_eur(price, usage)
		} else {
			tracing::warn!(provider = %self.row.provider, model = %self.row.model, "no llm.price for model, recorded at 0");
			0
		};
		if let Err(e) = ledger::record(&self.app, &self.row).await {
			tracing::error!(error = %e, provider = %self.row.provider, cost = self.row.cost_micro_eur, "llm ledger row lost");
		}
	}
}

/// A collected completion.
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
	pub target: Target,
	pub text: String,
	pub tool_calls: Vec<ToolCall>,
	pub finish: Option<String>,
	pub usage: Option<Usage>,
	/// The validated reply, when the call carried a `schema`.
	pub json: Option<serde_json::Value>,
}

/// The service handle; cheap to build per call site, like `Invoices::new(app)`.
#[derive(Clone)]
pub struct Llm {
	app: App,
	state: Option<LlmState>,
}

impl Llm {
	pub fn new(app: App) -> Self {
		let state = app.extensions.get::<LlmState>().cloned();
		Self { app, state }
	}

	fn state(&self) -> ClResult<&LlmState> {
		self.state
			.as_ref()
			.ok_or_else(|| config("llm is not enabled (app.feature(\"llm\"))"))
	}

	/// Start a streamed completion. Fallback happens only before the first event: a 429, 5xx,
	/// timeout or slot-wait timeout moves to the next profile entry, any other error returns.
	///
	/// # Errors
	/// `E-LLM-CONFIG`, `E-LLM-BUSY`, `E-LLM-CEILING`, `E-LLM-BUDGET`, or the last entry's
	/// provider error.
	pub async fn stream(&self, ctx: &Ctx, spec: &CallSpec) -> ClResult<LlmStream> {
		self.stream_with(ctx, spec, &spec.request, false).await
	}

	/// `retry` marks every ledger row of a structured-output re-ask.
	async fn stream_with(
		&self,
		ctx: &Ctx,
		spec: &CallSpec,
		request: &ChatRequest,
		retry: bool,
	) -> ClResult<LlmStream> {
		let state = self.state()?;
		ledger::store(&self.app)?;
		let settings = &self.app.settings;
		let profile = settings.text(&format!("llm.profile.{}", spec.role)).await?;
		if profile.trim().is_empty() {
			return Err(config(format!("no llm.profile.{} configured", spec.role)));
		}
		let mut targets = parse_profile(&profile)?;
		if let Some(prefer) = &spec.prefer
			&& let Some(i) =
				targets.iter().position(|t| format!("{}:{}", t.provider, t.model) == *prefer)
		{
			let t = targets.remove(i);
			targets.insert(0, t);
		}
		let wait = Duration::from_secs(
			u64::try_from(settings.int("llm.slot_wait_secs").await?).unwrap_or(0),
		);

		let mut last = None;
		let mut attempts = 0;
		for target in targets {
			let Some(permit) = self.slot(state, &target.provider, wait).await? else {
				tracing::warn!(provider = %target.provider, "llm provider slots full, falling back");
				last = Some(Error::coded_retry(
					StatusCode::SERVICE_UNAVAILABLE,
					E_BUSY,
					"every LLM provider is at capacity",
				));
				continue;
			};
			let provider = self.provider(state, &target.provider).await?;
			ledger::check(&self.app, spec.subject.as_deref()).await?;
			let pending = Pending {
				app: self.app.clone(),
				row: UsageRow {
					at: Timestamp::now(),
					run: spec.run.clone(),
					account_id: ctx.actor.account_id(),
					org_id: ctx.org_id,
					subject: spec.subject.clone(),
					step: spec.step.clone(),
					kind: UsageKind::Llm,
					provider: target.provider.clone(),
					model: target.model.clone(),
					tokens_in: 0,
					tokens_out: 0,
					cost_micro_eur: 0,
					retry: retry || attempts > 0,
				},
				price: ledger::price(&self.app, &target).await?,
			};
			attempts += 1;
			match provider.stream(&target.model, request).await {
				Ok(stream) => {
					return Ok(LlmStream {
						target,
						stream,
						_permit: permit,
						pending: Some(pending),
						usage: None,
						in_est: input_estimate(request),
						out_chars: 0,
					});
				}
				Err(e) => {
					// A refused request was not billed.
					pending.record(Usage::default()).await;
					if e.retry() != Retry::Backoff {
						return Err(e);
					}
					tracing::warn!(provider = %target.provider, model = %target.model, error = %e, "llm entry failed, falling back");
					last = Some(e);
				}
			}
		}
		Err(last.unwrap_or_else(|| config("empty llm profile")))
	}

	/// [`Llm::stream`], collected. With `spec.schema`, the reply is parsed and validated into
	/// `json`; a failure is re-asked with the error at most `structured::MAX_RETRIES` times.
	///
	/// # Errors
	/// As [`Llm::stream`], plus a mid-stream provider error, or `E-LLM-SCHEMA` (502) when the
	/// last re-ask still fails its schema.
	pub async fn complete(&self, ctx: &Ctx, spec: &CallSpec) -> ClResult<Completion> {
		let Some(schema) = &spec.schema else {
			return self.collect(ctx, spec, &spec.request, false).await;
		};
		let mut request = spec.request.clone();
		request.messages.push(Message::System {
			content: format!("Reply with only a JSON value matching this JSON schema:\n{schema}"),
		});
		let mut attempt = 0;
		loop {
			let mut out = self.collect(ctx, spec, &request, attempt > 0).await?;
			let err = match structured::parse(&out.text) {
				Ok(v) => match structured::validate(schema, &v) {
					Ok(()) => {
						out.json = Some(v);
						return Ok(out);
					}
					Err(e) => e,
				},
				Err(e) => e,
			};
			if attempt == structured::MAX_RETRIES {
				return Err(Error::coded(
					StatusCode::BAD_GATEWAY,
					structured::E_SCHEMA,
					format!("LLM reply failed its schema: {err}"),
				));
			}
			attempt += 1;
			request
				.messages
				.push(Message::Assistant { content: Some(out.text), tool_calls: out.tool_calls });
			request.messages.push(Message::User {
				content: format!(
					"That reply is invalid ({err}). Reply with only the corrected JSON."
				),
			});
		}
	}

	async fn collect(
		&self,
		ctx: &Ctx,
		spec: &CallSpec,
		request: &ChatRequest,
		retry: bool,
	) -> ClResult<Completion> {
		let mut s = self.stream_with(ctx, spec, request, retry).await?;
		let mut out = Completion {
			target: s.target.clone(),
			text: String::new(),
			tool_calls: Vec::new(),
			finish: None,
			usage: None,
			json: None,
		};
		while let Some(ev) = s.next().await? {
			match ev {
				ChatEvent::Text(t) => out.text.push_str(&t),
				ChatEvent::ToolCall(c) => out.tool_calls.push(c),
				ChatEvent::Finish(f) => out.finish = Some(f),
				ChatEvent::Usage(u) => out.usage = Some(u),
			}
		}
		Ok(out)
	}

	/// A slot on `provider`, or `None` after `wait`. The semaphore is re-created when
	/// `llm.max_concurrent.<p>` changes.
	// Permits held on a replaced semaphore still run, so a resize briefly allows old + new.
	async fn slot(
		&self,
		state: &LlmState,
		provider: &str,
		wait: Duration,
	) -> ClResult<Option<OwnedSemaphorePermit>> {
		let max = usize::try_from(
			self.app.settings.int(&format!("llm.max_concurrent.{provider}")).await?,
		)
		.unwrap_or(1);
		let sem = {
			let mut slots = state.0.slots.lock();
			let entry = slots
				.entry(provider.to_owned())
				.or_insert_with(|| (max, Arc::new(Semaphore::new(max))));
			if entry.0 != max {
				*entry = (max, Arc::new(Semaphore::new(max)));
			}
			entry.1.clone()
		};
		let permit = if wait.is_zero() {
			sem.try_acquire_owned().ok()
		} else {
			match tokio::time::timeout(wait, sem.acquire_owned()).await {
				Ok(p) => Some(p.map_err(|_| Error::internal("llm semaphore closed"))?),
				Err(_) => None,
			}
		};
		Ok(permit)
	}

	async fn provider(&self, state: &LlmState, name: &str) -> ClResult<Provider> {
		let settings = &self.app.settings;
		if settings.text(&format!("llm.kind.{name}")).await? == "fake" {
			return Ok(Provider::Fake(state.0.fake.clone()));
		}
		let base_url = settings.text(&format!("llm.base_url.{name}")).await?;
		if base_url.trim().is_empty() {
			return Err(config(format!("no llm.base_url.{name} configured")));
		}
		let api_key = match self.app.secrets.get(&format!("llm.api_key.{name}")).await? {
			Some(k) => Some(
				String::from_utf8(k)
					.map_err(|_| config(format!("llm.api_key.{name} is not UTF-8")))?,
			),
			None => None,
		};
		let mut p = OpenAi::new(base_url, api_key);
		p.allow_internal = settings.flag(&format!("llm.allow_internal.{name}")).await?;
		let path = settings.text(&format!("llm.path.{name}")).await?;
		if !path.trim().is_empty() {
			p.path = path;
		}
		Ok(Provider::OpenAi(p))
	}
}

pub(crate) fn config(msg: impl Into<String>) -> Error {
	Error::coded(StatusCode::SERVICE_UNAVAILABLE, E_CONFIG, msg)
}

// vim: ts=4
