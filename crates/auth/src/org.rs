// SPDX-License-Identifier: MPL-2.0
//! Orgs, memberships and the privileged re-check.
//!
//! `/api/org` (singular) is always the **active** org carried by the token;
//! `/api/orgs` (plural) is the account's membership list. `DELETE /api/orgs/{uid}` is the
//! one route that takes an org uid in its path — it names an org the caller is deleting,
//! which by definition is not the one they are working in.
//!
//! Nothing here is more than a body and a call: the logic, the org-admin re-check included,
//! lives on [`crate::service_api::Auth`]. That re-check reloads `memberships.role` from the
//! database on every org-admin call instead of trusting the token's `rol` claim — the second of
//! the two mitigations standing in for the session table this design does not have, and why
//! revoking a membership takes effect immediately on privileged routes even though the removed
//! member's access token stays valid.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use mintworks_core::refs::{CreateRef, Ref};
use serde::{Deserialize, Serialize};

use crate::service_api::Auth;
use crate::store::{AccountStatus, OrgKind, OrgStatus, Role};
use crate::token;

// ---------------------------------------------------------------- wire

#[derive(Debug, Serialize)]
pub struct Items<T> {
	pub items: Vec<T>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgSummary {
	pub uid: String,
	pub kind: OrgKind,
	pub name: String,
	pub status: OrgStatus,
	pub role: Role,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgDetail {
	pub uid: String,
	pub kind: OrgKind,
	pub name: String,
	pub status: OrgStatus,
	pub billing_currency: Option<CurrencyCode>,
	pub slug: Option<String>,
	pub role: Role,
	pub created_at: Timestamp,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberBody {
	/// Absent only in a role-change response for a pending membership (the listing never returns
	/// one): an invitation answers nothing about the address it names. A uid least of all — its
	/// leading ULID millisecond says when the account was minted, so a pre-existing address
	/// is distinguishable from one the invitation created.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub account_uid: Option<String>,
	/// Absent while the membership is pending, for the same reason as `account_uid`.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub email: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	pub role: Role,
	/// Absent while the membership is pending, for the same reason as `email`.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub status: Option<AccountStatus>,
	/// `false` while the invitation is unanswered — the only thing a pending row reports.
	pub accepted: bool,
	pub created_at: Timestamp,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewOrgRequest {
	pub name: String,
	#[serde(default)]
	pub billing_currency: Option<CurrencyCode>,
}

/// The switch response mirrors `stepup::StepUpResponse`: one access token, nothing else.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchResponse {
	pub access_token: String,
	pub refresh_token: String,
	pub expires_in: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchRequest {
	pub org_uid: String,
}

/// `POST /api/org/transfer-ownership`. The org is named explicitly rather than taken
/// from the token: handing away an organisation is not something to do to whichever one the
/// session happens to be switched into.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferRequest {
	pub org_uid: String,
	pub account_uid: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgPatch {
	#[serde(default)]
	pub name: Option<String>,
	#[serde(default)]
	pub billing_currency: Patch<CurrencyCode>,
	/// `null` clears it. Validated by `mintworks_core::refs::validate_slug`.
	#[serde(default)]
	pub slug: Patch<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRequest {
	pub email: String,
	pub role: Role,
}

/// `DELETE /api/org/members`. Not [`InviteRequest`] with an ignored `role`: that field has
/// no `serde` default, so reuse would make every cancellation carry a meaningless role.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveMemberRequest {
	pub email: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleRequest {
	pub role: Role,
}

// ---------------------------------------------------------------- handlers

/// `GET /api/orgs` — every org the account belongs to, with its role there.
pub async fn list(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Items<OrgSummary>>> {
	Ok(Json(Items { items: Auth::new(app).list_orgs(&ctx).await? }))
}

/// `POST /api/orgs` — a new organisation plus an `OWNER` membership for the
/// caller. `kind = 'PERSONAL'` is never creatable here: registration made the one personal org
/// the schema's `idx_org_personal` allows.
pub async fn create(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<NewOrgRequest>,
) -> ClResult<(StatusCode, Json<OrgDetail>)> {
	let org = Auth::new(app)
		.create_org(&ctx, &req.name, req.billing_currency.as_ref())
		.await?;
	Ok((
		StatusCode::CREATED,
		Json(OrgDetail {
			uid: org.uid.into_string(),
			kind: org.kind,
			name: org.name,
			slug: org.slug,
			status: org.status,
			billing_currency: org.billing_currency,
			role: Role::Owner,
			created_at: org.created_at,
		}),
	))
}

/// `POST /api/auth/switch-org` — a new pair whose `org` and `rol` are the switched-to org's.
/// `auth_at` is carried over, so switching can neither extend a session nor manufacture
/// step-up.
pub async fn switch(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<SwitchRequest>,
) -> ClResult<Response> {
	let out = Auth::new(app).switch_org(&ctx, &req.org_uid).await?;
	token::respond_rotated(&out, &out.access_token, &out.refresh_token)
}

/// `GET /api/org` — the active org in full.
pub async fn get(State(app): State<App>, ctx: Ctx) -> ClResult<Json<OrgDetail>> {
	Ok(Json(Auth::new(app).org(&ctx).await?))
}

/// `PATCH /api/org` — org-admin. `billingCurrency: null` clears the column, which is
/// how an org falls back to `settings['currency.base']`; `status` is operator-only and is
/// deliberately not patchable here.
pub async fn patch(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<OrgPatch>,
) -> ClResult<Json<OrgDetail>> {
	Ok(Json(Auth::new(app).update_org(&ctx, &req).await?))
}

/// `GET /api/org/members` — org-admin.
pub async fn members(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Items<MemberBody>>> {
	Ok(Json(Items { items: Auth::new(app).members(&ctx).await? }))
}

/// `POST /api/org/members` — org-admin. Mints an `org_invite` ref and mails its code; no
/// account is created (`Auth::add_member`).
///
/// **`204`, no body**, and the same work whether or not the address is registered: any org
/// admin can post any address here, so any difference is an account-existence oracle.
pub async fn add_member(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<InviteRequest>,
) -> ClResult<StatusCode> {
	Auth::new(app).add_member(&ctx, &req.email, req.role).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/org/invites` — org-admin. The org's `org_invite` refs.
pub async fn invites(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Items<Ref>>> {
	Ok(Json(Items { items: Auth::new(app).invites(&ctx).await? }))
}

/// `POST /api/org/invites/{code}/accept` — any signed-in account the invitation is addressed to.
pub async fn accept_invite(
	State(app): State<App>,
	ctx: Ctx,
	Path(code): Path<String>,
) -> ClResult<StatusCode> {
	Auth::new(app).accept_invite(&ctx, &code).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/auth/signup-refs` — whoever `auth.invite_by` names. `201` with the ref.
pub async fn create_signup_ref(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<CreateRef>,
) -> ClResult<(StatusCode, Json<Ref>)> {
	Ok((StatusCode::CREATED, Json(Auth::new(app).create_signup_ref(&ctx, &req).await?)))
}

/// `DELETE /api/org/members` — org-admin. `{"email": "…"}`, `204`, no body, for
/// [`add_member`]'s reason — the caller supplied the address. A pending invitation is a ref,
/// revoked through `DELETE /api/refs/{uid}`.
pub async fn remove_member_by_email(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<RemoveMemberRequest>,
) -> ClResult<StatusCode> {
	Auth::new(app).remove_member_by_email(&ctx, &req.email).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/org/members/{accountUid}` — org-admin. `OWNER` is neither assignable
/// nor removable here and the last `OWNER` cannot be demoted; ownership transfer is
/// deliberately not in v1.
pub async fn set_role(
	State(app): State<App>,
	Path(account_uid): Path<String>,
	ctx: Ctx,
	Json(req): Json<RoleRequest>,
) -> ClResult<Json<MemberBody>> {
	Ok(Json(Auth::new(app).set_member_role(&ctx, &account_uid, req.role).await?))
}

/// `DELETE /api/org/members/{accountUid}` — org-admin. Removing the `OWNER` is a
/// conflict.
pub async fn remove_member(
	State(app): State<App>,
	Path(account_uid): Path<String>,
	ctx: Ctx,
) -> ClResult<StatusCode> {
	Auth::new(app).remove_member(&ctx, &account_uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/org/transfer-ownership` — owner-only, step-up. The caller stays on as
/// `ADMIN`; the new owner must be a member who has accepted their invitation.
pub async fn transfer_ownership(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<TransferRequest>,
) -> ClResult<StatusCode> {
	Auth::new(app).transfer_ownership(&ctx, &req.org_uid, &req.account_uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/orgs/{uid}` — owner-only, step-up. Refused while the organisation has other
/// members or holds records that must be retained.
pub async fn delete(
	State(app): State<App>,
	Path(uid): Path<String>,
	ctx: Ctx,
) -> ClResult<StatusCode> {
	Auth::new(app).delete_org(&ctx, &uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

// vim: ts=4
