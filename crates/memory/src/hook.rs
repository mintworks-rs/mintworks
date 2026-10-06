// SPDX-License-Identifier: MPL-2.0
//! GDPR: an account's memory is its personal org's spaces. Shared-org spaces belong to the org;
//! versions the account wrote there keep its `acc_` author, since versions are immutable.

use async_trait::async_trait;
use mintworks_auth::store::personal_org;
use mintworks_core::ClResult;
use mintworks_core::account_data::AccountDataHook;
use mintworks_core::ids::{AccountId, OrgId};
use serde_json::{Value, json};

use crate::service::Memory;

#[async_trait]
impl AccountDataHook for Memory {
	fn name(&self) -> &'static str {
		"memory"
	}

	/// Every space of the personal org, with every version of every doc.
	async fn export(&self, acc: &AccountId) -> ClResult<Value> {
		let Some(org) = personal_org(self.orgs.as_ref(), acc).await? else {
			return Ok(Value::Null);
		};
		let mut spaces = Vec::new();
		for space in self.store.spaces_list(&org).await? {
			let mut docs = Vec::new();
			for doc in self.store.docs_list(space.id).await? {
				let versions: Vec<Value> = self
					.store
					.versions_list(doc.id)
					.await?
					.into_iter()
					.map(|v| {
						json!({
							"version": v.version,
							"body": v.body,
							"author": v.author,
							"createdAt": v.created_at,
						})
					})
					.collect();
				docs.push(json!({ "path": doc.path, "versions": versions }));
			}
			spaces.push(json!({ "key": space.key, "createdAt": space.created_at, "docs": docs }));
		}
		Ok(json!({ "spaces": spaces }))
	}

	async fn erase(&self, acc: &AccountId) -> ClResult<()> {
		if let Some(org) = personal_org(self.orgs.as_ref(), acc).await? {
			self.store.org_erase(&org).await?;
		}
		Ok(())
	}

	async fn org_deleted(&self, org: &OrgId) -> ClResult<()> {
		self.store.org_erase(org.as_str()).await.map(|_| ())
	}
}

// vim: ts=4
