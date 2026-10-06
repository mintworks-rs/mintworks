// SPDX-License-Identifier: MPL-2.0
//! Personal data a feature or application keeps outside the framework's own tables — the
//! app DB, a vector index — reached by `mintworks-auth`'s account export and erasure.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ClResult;
use crate::ids::{AccountId, OrgId};

/// Registered through [`crate::AppBuilder::account_data_hook`]; a hook captures its own
/// dependencies at construction.
#[async_trait]
pub trait AccountDataHook: Send + Sync {
	/// The key [`AccountDataHook::export`]'s value is placed under in the export document.
	/// Must not collide with a `mintworks-auth` section name or another hook's.
	fn name(&self) -> &'static str;

	async fn export(&self, acc: &AccountId) -> ClResult<Value>;

	/// Runs **before** the account is anonymised, so it must be idempotent: a failed erasure
	/// is retried from the top, and hooks that already succeeded run again.
	async fn erase(&self, acc: &AccountId) -> ClResult<()>;

	/// Runs after a shared org's row is deleted, for data the hook keys by the org's uid. The
	/// org is already gone, so a failure is logged by the caller, not retried.
	async fn org_deleted(&self, _org: &OrgId) -> ClResult<()> {
		Ok(())
	}
}

// vim: ts=4
