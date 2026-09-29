//! Compaction: past `agent.compact_after_tokens`, an `extract`-role call folds the older turns
//! into the thread summary and marks them `compacted` — kept for export and audit, no longer sent.

use std::sync::Arc;

use saas_core::{ClResult, Ctx, prelude::RunId};
use saas_llm::{CallSpec, ChatRequest, Llm, Message as Wire};

use crate::{
	run::{est, est_msg},
	store::{Message, Thread, ThreadStore},
};

const ROLE: &str = "extract";
const INSTRUCTIONS: &str = "Summarise the conversation below for your own later use. Keep every \
	fact, decision, name, number and open task; drop pleasantries. Reply with the summary only.";

/// What [`compact`] folded away.
pub struct Compacted {
	pub summary: String,
	/// Index into the live messages of the first one still sent.
	pub kept_from: usize,
}

/// Where the kept tail starts: the latest `user` turn after which at most `keep` tokens remain,
/// or the last `user` turn when even that tail is larger. A cut elsewhere could split an
/// assistant tool call from its result, which the next model call rejects.
fn cut(live: &[Message], keep: i64) -> usize {
	let mut tail = 0;
	let mut at = 0;
	for (i, m) in live.iter().enumerate().rev() {
		tail += est_msg(m);
		if m.role == "user" {
			if at != 0 && tail > keep {
				break;
			}
			at = i;
		}
	}
	at
}

fn transcript(summary: Option<&str>, old: &[Message]) -> String {
	let mut out = String::new();
	if let Some(s) = summary {
		out.push_str("Earlier summary:\n");
		out.push_str(s);
		out.push_str("\n\n");
	}
	for m in old {
		out.push_str(&m.role);
		out.push_str(": ");
		out.push_str(&m.content);
		if let Some(calls) = &m.tool_calls {
			out.push_str(" [tool calls: ");
			out.push_str(calls);
			out.push(']');
		}
		out.push('\n');
	}
	out
}

/// Folds the older half of `live` into the summary. `None` when nothing could be cut.
///
/// # Errors
/// The model call's, or the store's; the caller carries on uncompacted.
#[allow(clippy::too_many_arguments)]
pub async fn compact(
	llm: &Llm,
	ctx: &Ctx,
	threads: &Arc<dyn ThreadStore>,
	thread: &Thread,
	run: &RunId,
	subject: Option<String>,
	live: &[Message],
	limit: i64,
) -> ClResult<Option<Compacted>> {
	let at = cut(live, limit / 2);
	let Some(last) = at.checked_sub(1).and_then(|i| live.get(i)) else {
		return Ok(None);
	};
	let call = CallSpec {
		role: ROLE.to_owned(),
		request: ChatRequest {
			messages: vec![
				Wire::System { content: INSTRUCTIONS.to_owned() },
				Wire::User { content: transcript(thread.summary.as_deref(), &live[..at]) },
			],
			..ChatRequest::default()
		},
		subject,
		step: "agent.compact".to_owned(),
		run: Some(run.as_str().to_owned()),
		..CallSpec::default()
	};
	let summary = llm.complete(ctx, &call).await?.text;
	let kept: i64 = live[at..].iter().map(est_msg).sum();
	let estimate = kept + est(&summary);
	threads.compact(thread.id, &summary, last.id, estimate).await?;
	Ok(Some(Compacted { summary, kept_from: at }))
}

// vim: ts=4
