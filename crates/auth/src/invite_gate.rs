// SPDX-License-Identifier: MPL-2.0
//! The seam an invite quota hangs on without `mintworks-auth` depending on whatever meters it.

use std::sync::Arc;

use async_trait::async_trait;
use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::error::ClResult;
use mintworks_core::ids::RefId;

/// Called around minting a `signup` or `org_invite` ref. Registered as
/// `Arc<dyn InviteGate>` in `app.extensions`; absent means [`AllowAll`].
#[async_trait]
pub trait InviteGate: Send + Sync {
	/// Before the mint; an `Err` refuses it and is returned to the caller as is.
	async fn may_invite(&self, app: &App, ctx: &Ctx, ref_type: &str) -> ClResult<()>;
	/// After the mint committed; an `Err` is logged, the ref stands.
	async fn invited(&self, app: &App, ctx: &Ctx, ref_uid: &RefId) -> ClResult<()>;
}

pub struct AllowAll;

#[async_trait]
impl InviteGate for AllowAll {
	async fn may_invite(&self, _: &App, _: &Ctx, _: &str) -> ClResult<()> {
		Ok(())
	}
	async fn invited(&self, _: &App, _: &Ctx, _: &RefId) -> ClResult<()> {
		Ok(())
	}
}

pub(crate) fn of(app: &App) -> Arc<dyn InviteGate> {
	app.extensions
		.get::<Arc<dyn InviteGate>>()
		.cloned()
		.unwrap_or_else(|| Arc::new(AllowAll))
}

// vim: ts=4
