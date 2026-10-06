// SPDX-License-Identifier: MPL-2.0
//! `DocumentStore`: the `documents` rows that give a rendered file an org owner.

use async_trait::async_trait;
use mintworks_core::error::ClResult;
use mintworks_core::ids::{AccountId, DocId};
use mintworks_core::prelude::Timestamp;

#[derive(Debug, Clone)]
pub struct Document {
	pub uid: DocId,
	/// Path relative to the template root, as passed to [`crate::Documents::create`].
	pub template: String,
	/// The `RENDER_DOC` job's `dedup_key`.
	pub job_key: String,
	/// `None` until the job has written the file.
	pub sha256: Option<String>,
	pub bytes: Option<i64>,
	pub created_at: Timestamp,
}

#[async_trait]
pub trait DocumentStore: Send + Sync {
	async fn document_insert(
		&self,
		org_id: i64,
		uid: &DocId,
		template: &str,
		job_key: &str,
	) -> ClResult<()>;

	/// Records the rendered file. Unknown `uid` is a no-op: the org may be gone.
	async fn document_rendered(&self, uid: &DocId, sha256: &str, bytes: i64) -> ClResult<()>;

	/// Confined to `org_id`: another org's uid is `None`.
	async fn document_get(&self, org_id: i64, uid: &DocId) -> ClResult<Option<Document>>;

	/// The documents of `acc`'s personal org, oldest first; empty once the account is gone.
	async fn documents_for_account(&self, acc: &AccountId) -> ClResult<Vec<Document>>;

	/// The sha256 of each file of `acc`'s personal org that no other org's document and no
	/// `invoice_documents` row references: files are content-addressed and shared, so only
	/// those may be removed. Read-only, so the caller removes files before the rows go.
	async fn documents_orphaned_by_account(&self, acc: &AccountId) -> ClResult<Vec<String>>;

	/// Deletes the documents of `acc`'s personal org; a no-op once they are gone.
	async fn documents_erase_account(&self, acc: &AccountId) -> ClResult<()>;
}

// vim: ts=4
