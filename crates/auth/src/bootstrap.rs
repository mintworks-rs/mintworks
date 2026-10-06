//! The first operator. Only an operator mints invites, so a server booted with `auth.registration`
//! at `invite` or `closed` and no operator yet has no way in; [`bootstrap_operator`] is it.

use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use mintworks_core::refs::{CreateRef, Ref, Refs};
use serde_json::json;

use crate::store::Role;

/// With no accepted ADMIN or OWNER on the ROOT org, logs a `/register?ref=` URL whose
/// `org_invite` makes its registrant an ADMIN there, i.e. the operator. A still redeemable one
/// from an earlier boot is reused, so restarts do not pile up refs.
///
/// **The URL is a credential**: whoever registers with it first becomes operator. It is one-use,
/// expires after `auth.invite_ttl_days`, and stops being printed once an operator exists or it
/// has been used.
pub async fn bootstrap_operator(app: &App) -> ClResult<()> {
	let root = app.store.root_org_id().await?;
	let refs = Refs::from_app(app)?;
	let invites = refs.of_org(root, Some("org_invite")).await?;
	let bootstrap = |r: &&Ref| r.email.is_none() && r.params["role"] == Role::Admin.as_str();
	// Used once means the operator was made: a later removal is an operator decision, not a gap.
	if invites.iter().filter(bootstrap).any(|r| r.uses_left == Some(0)) {
		return Ok(());
	}
	// ponytail: the ROOT org holds operators only in practice; page if it ever outgrows this.
	let members = crate::routes::store(app)?.members(root, 1000).await?;
	if members.iter().any(|m| matches!(m.role, Role::Admin | Role::Owner)) {
		return Ok(());
	}
	let now = Timestamp::now();
	let existing = invites.into_iter().find(|r| r.is_redeemable(now) && bootstrap(&r));
	let r = if let Some(r) = existing {
		r
	} else {
		let ttl = app.settings.int("auth.invite_ttl_days").await?;
		let req = CreateRef {
			ref_type: "org_invite".to_owned(),
			params: Some(json!({ "role": Role::Admin.as_str() })),
			uses_left: Some(1),
			expires_at: Some(Timestamp(now.0 + ttl * 86_400)),
			..CreateRef::default()
		};
		refs.mint(&Ctx::system("bootstrap").with_org(root), &req).await?
	};
	let base = app.config.base_url.trim_end_matches('/');
	let expires = r.expires_at.and_then(Timestamp::to_rfc3339).unwrap_or_else(|| "never".into());
	// Logged on purpose: only printed while the ref is unused, i.e. before the first operator registers.
	tracing::warn!(
		"no operator yet; register the first one at {base}/register?ref={} (expires {expires})",
		r.code
	);
	Ok(())
}

// vim: ts=4
