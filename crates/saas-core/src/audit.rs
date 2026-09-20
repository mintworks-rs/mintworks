//! Append-only audit trail. Every mutating service method writes one row.
//!
//! Best-effort: a failed insert is logged and never propagated, so auditing cannot break an
//! operation that succeeded. Append-only rests on [`crate::store::CoreStore`] exposing
//! `audit_log` and nothing else — no update method and no delete method. No schema trigger
//! asserts it; business rules live in the feature crates, not in the schema.
//!
//! The org refactor changed the written vocabulary: rows before it say
//! `tenant`/`TENANT_*`/`detail.tenant`, later rows `org`/`ORG_*`/`detail.org`. The log is
//! append-only and is not rewritten, so a reader spanning the change matches both spellings.
//!
//! An escalated call ([`crate::ctx::Ctx::as_system`]) keeps its `account_id` while still
//! recording `detail.source`: the id says who, `source` says through what.

use std::sync::Arc;

use serde_json::Value;

use crate::ctx::{Actor, Ctx};
use crate::store::{AuditEntry, CoreStore};
use crate::types::Timestamp;

/// `entity` is the table's singular name (`"invoice"`, `"payment"`, `"secret"`),
/// `entity_id` its uid or natural key, `action` a SCREAMING_CASE verb (`"ISSUE"`,
/// `"STORNO"`, `"REFUND"`).
pub async fn log(
	store: &Arc<dyn CoreStore>,
	ctx: &Ctx,
	entity: &str,
	entity_id: Option<&str>,
	action: &str,
	detail: Option<Value>,
) {
	// ponytail: logged, not propagated — auditing inside the caller's `write_tx` is still the
	// real fix; `try_log` is the narrower one, for rows that are statutory evidence.
	// `error!`, not `warn!`: at `error!` the alert sweep sees it.
	//
	// Through `thrice` because this writes in autocommit *after* the caller committed: one
	// `SQLITE_BUSY` on the single writer otherwise lost the audit row for an issued invoice.
	let out =
		crate::job::thrice(|| try_log(store, ctx, entity, entity_id, action, detail.clone())).await;
	if let Err(e) = out {
		tracing::error!(error = ?e, entity, action, "audit log write failed");
	}
}

/// [`log`] for an action whose audit row is statutory evidence rather than a diagnostic: the
/// caller decides what a lost row means, instead of it becoming a log line nobody reads.
///
/// The write still happens *after* the caller's transaction has committed, so propagating
/// turns a lost row into a failure on an operation that already succeeded. Only use it where
/// the operation is idempotent enough for the caller to retry.
pub async fn try_log(
	store: &Arc<dyn CoreStore>,
	ctx: &Ctx,
	entity: &str,
	entity_id: Option<&str>,
	action: &str,
	detail: Option<Value>,
) -> crate::error::ClResult<()> {
	tracing::info!(entity, entity_id, action, actor = ?ctx.actor, "audit");
	store.audit_log(&entry(ctx, entity, entity_id, action, detail)).await
}

fn entry(
	ctx: &Ctx,
	entity: &str,
	entity_id: Option<&str>,
	action: &str,
	detail: Option<Value>,
) -> AuditEntry {
	let detail = match ctx.actor {
		Actor::System { source } | Actor::Public { source } => {
			Some(annotate(detail, "source", Value::from(source)))
		}
		// Names which key acted, not a `source`: `account_id` alone cannot tell two keys of
		// the same account apart.
		Actor::Key { key_id, .. } => Some(annotate(detail, "key_id", Value::from(key_id))),
		Actor::User { .. } | Actor::Operator { .. } => detail,
	};

	AuditEntry {
		at: Timestamp::now(),
		account_id: ctx.actor.account_id().or(ctx.on_behalf_of),
		org_id: ctx.org_id,
		ip: ctx.ip.map(|ip| ip.to_string()),
		entity: entity.to_owned(),
		entity_id: entity_id.map(ToOwned::to_owned),
		action: action.to_owned(),
		detail: detail.map(|d| d.to_string()),
		request_id: Some(ctx.request_id.clone()).filter(|s| !s.is_empty()),
	}
}

/// Adds one provenance key to the audit `detail`.
///
/// A non-object detail (an array, a bare string) has nowhere to take a key, so it is nested —
/// leaving it alone drops the one field saying *which* of the application's own code paths
/// acted.
fn annotate(detail: Option<Value>, key: &str, value: Value) -> Value {
	match detail {
		Some(Value::Object(mut map)) => {
			map.insert(key.to_owned(), value);
			Value::Object(map)
		}
		None => serde_json::json!({ key: value }),
		Some(other) => serde_json::json!({ key: value, "detail": other }),
	}
}

// vim: ts=4
