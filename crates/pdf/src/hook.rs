// SPDX-License-Identifier: MPL-2.0
//! GDPR: an account's documents are its personal org's. Shared orgs' documents are the org's,
//! not the person's, so neither path touches them.

use std::sync::Arc;

use async_trait::async_trait;
use mintworks_core::{ClResult, Error, account_data::AccountDataHook, ids::AccountId};
use serde_json::{Value, json};

use crate::{DocumentStore, doc_path};

pub struct DocumentHook {
	pub docs: Arc<dyn DocumentStore>,
	/// `Config::data_dir`, the root [`doc_path`] fans out from.
	pub data_dir: String,
}

#[async_trait]
impl AccountDataHook for DocumentHook {
	fn name(&self) -> &'static str {
		"documents"
	}

	async fn export(&self, acc: &AccountId) -> ClResult<Value> {
		let docs: Vec<Value> = self
			.docs
			.documents_for_account(acc)
			.await?
			.into_iter()
			.map(|d| {
				json!({
					"uid": d.uid.as_str(),
					"template": d.template,
					"createdAt": d.created_at,
					"sha256": d.sha256,
					"bytes": d.bytes,
				})
			})
			.collect();
		Ok(json!(docs))
	}

	/// Idempotent: files go before rows, so a failed removal leaves the rows for the retry to
	/// find, and a file already removed is not an error.
	async fn erase(&self, acc: &AccountId) -> ClResult<()> {
		for sha in self.docs.documents_orphaned_by_account(acc).await? {
			match std::fs::remove_file(doc_path(&self.data_dir, &sha)?) {
				Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
					return Err(Error::internal(format!("mintworks-pdf: remove {sha}: {e}")));
				}
				_ => {}
			}
		}
		self.docs.documents_erase_account(acc).await
	}
}

// vim: ts=4
