//! `MeterInviteGate`: every `signup`/`org_invite` ref costs one `invites` meter unit. Not the
//! default gate — the application registers it as `Arc<dyn mintworks_auth::InviteGate>`.

use async_trait::async_trait;
use mintworks_auth::InviteGate;
use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::error::StatusCode;
use mintworks_core::ids::RefId;
use mintworks_core::prelude::*;
use mintworks_entitle::{Entitle, EntitlementRegistry};

pub const METER: &str = "invites";

pub struct MeterInviteGate;

/// An app that declares no `invites` meter has nothing to meter: every invite is allowed.
fn metered(app: &App) -> bool {
	app.extensions
		.get::<EntitlementRegistry>()
		.is_some_and(|r| r.kind(METER).is_some())
}

#[async_trait]
impl InviteGate for MeterInviteGate {
	/// ponytail: check-then-charge, so concurrent invites can overdraw by the race width; a
	/// reservation would close it.
	async fn may_invite(&self, app: &App, ctx: &Ctx, _ref_type: &str) -> ClResult<()> {
		if metered(app) && Entitle::from_app(app)?.balance(ctx, METER).await? < 1 {
			return Err(Error::coded(
				StatusCode::PAYMENT_REQUIRED,
				mintworks_entitle::service::E_EXHAUSTED,
				"no invites left",
			));
		}
		Ok(())
	}

	/// `charge`, not `consume`: the ref already exists, so the use is recorded even past zero.
	async fn invited(&self, app: &App, ctx: &Ctx, ref_uid: &RefId) -> ClResult<()> {
		if metered(app) {
			Entitle::from_app(app)?.charge(ctx, METER, 1, ref_uid.as_str()).await?;
		}
		Ok(())
	}
}

// vim: ts=4
