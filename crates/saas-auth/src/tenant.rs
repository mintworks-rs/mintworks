//! Tenants, memberships and the privileged re-check.
//!
//! `/api/tenant` (singular) is always the **active** tenant carried by the token;
//! `/api/tenants` (plural) is the account's membership list. `DELETE /api/tenants/{uid}` is the
//! one route that takes a tenant uid in its path — it names a tenant the caller is deleting,
//! which by definition is not the one they are working in.
//!
//! Nothing here is more than a body and a call: the logic, the tenant-admin re-check included,
//! lives on [`crate::service_api::Auth`]. That re-check reloads `memberships.role` from the
//! database on every tenant-admin call instead of trusting the token's `rol` claim — the second of
//! the two mitigations standing in for the session table this design does not have, and why
//! revoking a membership takes effect immediately on privileged routes even though the removed
//! member's access token stays valid.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::service_api::Auth;
use crate::store::{AccountStatus, Role, TenantKind, TenantStatus};
use crate::token;

// ---------------------------------------------------------------- wire

#[derive(Debug, Serialize)]
pub struct Items<T> {
	pub items: Vec<T>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantSummary {
	pub uid: String,
	pub kind: TenantKind,
	pub name: String,
	pub status: TenantStatus,
	pub role: Role,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantDetail {
	pub uid: String,
	pub kind: TenantKind,
	pub name: String,
	pub status: TenantStatus,
	pub billing_currency: Option<CurrencyCode>,
	pub role: Role,
	pub created_at: Timestamp,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberBody {
	/// Absent while the membership is pending: an invitation answers nothing about the
	/// address it names (see [`crate::store::AuthStore::members`]). A uid least of all — its
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
pub struct NewTenantRequest {
	pub name: String,
	#[serde(default)]
	pub billing_currency: Option<CurrencyCode>,
}

/// The switch response mirrors `stepup::StepUpResponse`: one access token, nothing else.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchResponse {
	pub access_token: String,
	pub expires_in: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchRequest {
	pub tenant_uid: String,
}

/// `POST /api/tenant/transfer-ownership`. The tenant is named explicitly rather than taken
/// from the token: handing away an organisation is not something to do to whichever one the
/// session happens to be switched into.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferRequest {
	pub tenant_uid: String,
	pub account_uid: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantPatch {
	#[serde(default)]
	pub name: Option<String>,
	#[serde(default)]
	pub billing_currency: Patch<CurrencyCode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRequest {
	pub email: String,
	pub role: Role,
}

/// `DELETE /api/tenant/members`. Not [`InviteRequest`] with an ignored `role`: that field has
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

/// `GET /api/tenants` — every tenant the account belongs to, with its role there.
pub async fn list(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Items<TenantSummary>>> {
	Ok(Json(Items { items: Auth::new(app).list_tenants(&ctx).await? }))
}

/// `POST /api/tenants` — a new organisation tenant plus an `OWNER` membership for the
/// caller. `kind = 'P'` is never creatable here: registration made the one personal tenant
/// the schema's `idx_tenant_personal` allows.
pub async fn create(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<NewTenantRequest>,
) -> ClResult<(StatusCode, Json<TenantDetail>)> {
	let tenant = Auth::new(app)
		.create_tenant(&ctx, &req.name, req.billing_currency.as_ref())
		.await?;
	Ok((
		StatusCode::CREATED,
		Json(TenantDetail {
			uid: tenant.uid.into_string(),
			kind: tenant.kind,
			name: tenant.name,
			status: tenant.status,
			billing_currency: tenant.billing_currency,
			role: Role::Owner,
			created_at: tenant.created_at,
		}),
	))
}

/// `POST /api/auth/switch-tenant` — a new **access** token whose `tnt` and `rol` are the
/// switched-to tenant's. The refresh token is untouched and `auth_at` is carried over, so
/// switching can neither extend a session nor manufacture step-up.
pub async fn switch(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<SwitchRequest>,
) -> ClResult<Response> {
	let out = Auth::new(app).switch_tenant(&ctx, &req.tenant_uid).await?;
	let access = out.access_token.clone();
	token::respond_access(out, &access)
}

/// `GET /api/tenant` — the active tenant in full.
pub async fn get(State(app): State<App>, ctx: Ctx) -> ClResult<Json<TenantDetail>> {
	Ok(Json(Auth::new(app).tenant(&ctx).await?))
}

/// `PATCH /api/tenant` — tenant-admin. `billingCurrency: null` clears the column, which is
/// how a tenant falls back to `settings['currency.base']`; `status` is operator-only and is
/// deliberately not patchable here.
pub async fn patch(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<TenantPatch>,
) -> ClResult<Json<TenantDetail>> {
	Ok(Json(Auth::new(app).update_tenant(&ctx, &req).await?))
}

/// `GET /api/tenant/members` — tenant-admin.
pub async fn members(State(app): State<App>, ctx: Ctx) -> ClResult<Json<Items<MemberBody>>> {
	Ok(Json(Items { items: Auth::new(app).members(&ctx).await? }))
}

/// `POST /api/tenant/members` — tenant-admin. An unknown address gets an account with
/// `pwd_hash = NULL`, the invited state, and the activation mail doubles as the invitation:
/// the invitee sets a password by supplying one to `POST /api/auth/activate`, which
/// requires it precisely when the account has none.
///
/// **`204`, no body.** Any tenant admin can post any address here, so anything read off the
/// resolved row is an account-existence oracle over arbitrary addresses — `accountUid` worst
/// of all, since a prefixed ULID opens with the millisecond it was minted and so says when
/// that address first registered. The membership reads back from `GET /api/tenant/members`.
pub async fn add_member(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<InviteRequest>,
) -> ClResult<StatusCode> {
	Auth::new(app).add_member(&ctx, &req.email, req.role).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/tenant/members` — tenant-admin. `{"email": "…"}`, `204`, no body.
/// The uid-keyed sibling cannot reach a pending invitation: its `accountUid` is withheld.
/// `204` regardless, for [`add_member`]'s reason — the caller supplied the address.
pub async fn remove_member_by_email(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<RemoveMemberRequest>,
) -> ClResult<StatusCode> {
	Auth::new(app).remove_member_by_email(&ctx, &req.email).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/tenant/members/{accountUid}` — tenant-admin. `OWNER` is neither assignable
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

/// `DELETE /api/tenant/members/{accountUid}` — tenant-admin. Removing the `OWNER` is a
/// conflict.
pub async fn remove_member(
	State(app): State<App>,
	Path(account_uid): Path<String>,
	ctx: Ctx,
) -> ClResult<StatusCode> {
	Auth::new(app).remove_member(&ctx, &account_uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/tenant/transfer-ownership` — owner-only, step-up. The caller stays on as
/// `ADMIN`; the new owner must be a member who has accepted their invitation.
pub async fn transfer_ownership(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<TransferRequest>,
) -> ClResult<StatusCode> {
	Auth::new(app)
		.transfer_ownership(&ctx, &req.tenant_uid, &req.account_uid)
		.await?;
	Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/tenants/{uid}` — owner-only, step-up. Refused while the organisation has other
/// members or holds records that must be retained.
pub async fn delete(
	State(app): State<App>,
	Path(uid): Path<String>,
	ctx: Ctx,
) -> ClResult<StatusCode> {
	Auth::new(app).delete_tenant(&ctx, &uid).await?;
	Ok(StatusCode::NO_CONTENT)
}

// vim: ts=4
