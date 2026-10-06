// SPDX-License-Identifier: MPL-2.0
//! GDPR: an account's threads are its personal org's. `agent_runs` rows are anonymised by
//! `mintworks_auth::gdpr::ERASURE`, not here.

use std::sync::Arc;

use async_trait::async_trait;
use mintworks_auth::store::{AuthStore, personal_org};
use mintworks_core::{
	ClResult,
	account_data::AccountDataHook,
	ids::{AccountId, OrgId},
};
use serde_json::{Value, json};

use crate::store::ThreadStore;

pub struct AgentHook {
	pub threads: Arc<dyn ThreadStore>,
	pub orgs: Arc<dyn AuthStore>,
}

#[async_trait]
impl AccountDataHook for AgentHook {
	fn name(&self) -> &'static str {
		"agent"
	}

	/// Every thread of the personal org with every message, compacted ones included.
	async fn export(&self, acc: &AccountId) -> ClResult<Value> {
		let Some(org) = personal_org(self.orgs.as_ref(), acc).await? else {
			return Ok(Value::Null);
		};
		let mut threads = Vec::new();
		for t in self.threads.threads_list(&org).await? {
			let messages: Vec<Value> = self
				.threads
				.messages_all(t.id)
				.await?
				.into_iter()
				.map(|m| {
					json!({
						"role": m.role,
						"content": m.content,
						"toolCalls": m.tool_calls,
						"toolCallId": m.tool_call_id,
						"compacted": m.compacted,
						"createdAt": m.created_at,
					})
				})
				.collect();
			threads.push(json!({
				"uid": t.uid.as_str(),
				"subject": t.subject,
				"summary": t.summary,
				"createdAt": t.created_at,
				"messages": messages,
			}));
		}
		Ok(json!({ "threads": threads }))
	}

	async fn erase(&self, acc: &AccountId) -> ClResult<()> {
		if let Some(org) = personal_org(self.orgs.as_ref(), acc).await? {
			self.threads.org_erase(&org).await?;
		}
		Ok(())
	}

	async fn org_deleted(&self, org: &OrgId) -> ClResult<()> {
		self.threads.org_erase(org.as_str()).await.map(|_| ())
	}
}

// vim: ts=4
