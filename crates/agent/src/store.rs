//! The two stores behind the harness. [`ThreadStore`] is the app DB (module `agent`):
//! conversation content, org as a uid string. [`AgentRunStore`] is the core DB: who started a
//! run, under what spec, how it ended, and every event it streamed.
//!
//! Nothing here checks who may see a thread or a run — the `Agent` handle confines every call to
//! `ctx` before it reaches a store.

use async_trait::async_trait;
use mintworks_core::{
	ClResult,
	prelude::{RunId, ThreadId, Timestamp},
};

/// A conversation, unique by `uid`, owned by one org.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
	pub id: i64,
	pub uid: ThreadId,
	/// The owning org's `org_…` uid.
	pub org: String,
	/// Opaque budget key the thread's runs are charged to, e.g. `project:prj_…`.
	pub subject: Option<String>,
	/// Display name for a thread list; the framework never reads it.
	pub title: Option<String>,
	/// What compaction folded the `compacted` messages into.
	pub summary: Option<String>,
	/// Estimated tokens of summary plus uncompacted messages.
	pub token_estimate: i64,
	/// `provider:model` that answered last, preferred by the next run.
	pub last_model: Option<String>,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
}

/// One chat message. `tool_calls` is the assistant's JSON array; `tool_call_id` marks a tool result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
	pub id: i64,
	pub thread_id: i64,
	/// `system`, `user`, `assistant` or `tool`.
	pub role: String,
	pub content: String,
	pub tool_calls: Option<String>,
	pub tool_call_id: Option<String>,
	/// Folded into the thread summary: kept for export and audit, no longer sent to the model.
	pub compacted: bool,
	pub created_at: Timestamp,
}

#[cfg(test)]
pub(crate) fn msg(role: &str, content: &str, calls: Option<&str>, id: Option<&str>) -> Message {
	Message {
		id: 0,
		thread_id: 0,
		role: role.into(),
		content: content.into(),
		tool_calls: calls.map(Into::into),
		tool_call_id: id.map(Into::into),
		compacted: false,
		created_at: Timestamp(0),
	}
}

#[derive(Clone, Copy, Debug)]
pub struct NewMessage<'a> {
	pub role: &'a str,
	pub content: &'a str,
	pub tool_calls: Option<&'a str>,
	pub tool_call_id: Option<&'a str>,
}

#[async_trait]
pub trait ThreadStore: Send + Sync + 'static {
	/// A new thread with a fresh `thr_…` uid.
	async fn thread_create(
		&self,
		org: &str,
		subject: Option<&str>,
		title: Option<&str>,
	) -> ClResult<Thread>;
	async fn thread_get(&self, uid: &ThreadId) -> ClResult<Option<Thread>>;
	/// The org's threads, oldest first (GDPR export).
	async fn threads_list(&self, org: &str) -> ClResult<Vec<Thread>>;
	/// Append `msgs` in order, set `token_estimate`, and `last_model` unless `None` — atomically.
	async fn messages_append(
		&self,
		thread_id: i64,
		msgs: &[NewMessage<'_>],
		last_model: Option<&str>,
		token_estimate: i64,
	) -> ClResult<()>;
	/// The uncompacted messages, oldest first: what the next model call is built from.
	async fn messages_live(&self, thread_id: i64) -> ClResult<Vec<Message>>;
	/// Every message, compacted or not, oldest first.
	async fn messages_all(&self, thread_id: i64) -> ClResult<Vec<Message>>;
	/// Set the summary and `token_estimate`, and mark every message with `id <= through_id`
	/// compacted — atomically.
	async fn compact(
		&self,
		thread_id: i64,
		summary: &str,
		through_id: i64,
		token_estimate: i64,
	) -> ClResult<()>;
	/// Hard-delete the org's threads and messages (GDPR erase). Returns the threads removed.
	async fn org_erase(&self, org: &str) -> ClResult<u64>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
	Queued,
	Running,
	Done,
	Error,
	Cancelled,
	/// Its lease expired while `queued`/`running`: runs are never resumed.
	Interrupted,
}

impl RunStatus {
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Queued => "queued",
			Self::Running => "running",
			Self::Done => "done",
			Self::Error => "error",
			Self::Cancelled => "cancelled",
			Self::Interrupted => "interrupted",
		}
	}

	#[must_use]
	pub fn parse(s: &str) -> Option<Self> {
		Some(match s {
			"queued" => Self::Queued,
			"running" => Self::Running,
			"done" => Self::Done,
			"error" => Self::Error,
			"cancelled" => Self::Cancelled,
			"interrupted" => Self::Interrupted,
			_ => return None,
		})
	}

	#[must_use]
	pub const fn is_live(self) -> bool {
		matches!(self, Self::Queued | Self::Running)
	}
}

/// A run's last event is `Done` or `Error`, its payload carrying the final `status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
	Queued,
	Delta,
	ToolCall,
	ToolResult,
	Message,
	Done,
	Error,
}

impl EventKind {
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Queued => "queued",
			Self::Delta => "delta",
			Self::ToolCall => "tool_call",
			Self::ToolResult => "tool_result",
			Self::Message => "message",
			Self::Done => "done",
			Self::Error => "error",
		}
	}

	#[must_use]
	pub fn parse(s: &str) -> Option<Self> {
		Some(match s {
			"queued" => Self::Queued,
			"delta" => Self::Delta,
			"tool_call" => Self::ToolCall,
			"tool_result" => Self::ToolResult,
			"message" => Self::Message,
			"done" => Self::Done,
			"error" => Self::Error,
			_ => return None,
		})
	}
}

/// A run with the actor snapshot it started under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
	pub id: i64,
	pub uid: RunId,
	pub thread: ThreadId,
	pub org_id: i64,
	/// `None` once the account is erased, or for a system-started run.
	pub account_id: Option<i64>,
	/// The actor's role, re-read from the DB at start.
	pub role: String,
	/// The run spec as JSON.
	pub spec: String,
	pub status: RunStatus,
	pub error: Option<String>,
	pub created_at: Timestamp,
	pub started_at: Option<Timestamp>,
	pub finished_at: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug)]
pub struct NewRun<'a> {
	pub uid: &'a RunId,
	pub thread: &'a ThreadId,
	pub org_id: i64,
	pub account_id: Option<i64>,
	pub role: &'a str,
	pub spec: &'a str,
}

/// One persisted event; `seq` is 1-based per run and is the SSE `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEvent {
	pub seq: i64,
	pub kind: EventKind,
	/// JSON.
	pub payload: String,
	pub at: Timestamp,
}

#[async_trait]
pub trait AgentRunStore: Send + Sync + 'static {
	/// Insert the run `queued`. `None`, and nothing written, when the thread already has a live
	/// (`queued`/`running`) run, held by a partial unique index, not only by the pool.
	async fn run_insert(&self, new: &NewRun<'_>) -> ClResult<Option<Run>>;
	async fn run_get(&self, uid: &RunId) -> ClResult<Option<Run>>;
	/// `running` stamps `started_at`; a terminal status stamps `finished_at`. Applies only to a
	/// live (`queued`/`running`) row: `false`, and nothing written, once the run has ended — a
	/// sweep's `interrupted` is never overwritten.
	async fn run_set_status(
		&self,
		run_id: i64,
		status: RunStatus,
		error: Option<&str>,
	) -> ClResult<bool>;
	/// Append the run's next event; returns its `seq`.
	async fn event_append(&self, run_id: i64, kind: EventKind, payload: &str) -> ClResult<i64>;
	/// The run's events with `seq > after`, in order.
	async fn events_after(&self, run_id: i64, after: i64) -> ClResult<Vec<RunEvent>>;
	/// Renew the lease (`heartbeat_at = at`) of those of `run_ids` that are still live.
	async fn runs_heartbeat(&self, run_ids: &[i64], at: Timestamp) -> ClResult<()>;
	/// Mark every live run whose lease (`heartbeat_at`, else `created_at`) is older than `before`
	/// `interrupted` and return them. A run another process still heartbeats is never taken.
	async fn runs_interrupt_stale(&self, before: Timestamp) -> ClResult<Vec<Run>>;
}

// vim: ts=4
