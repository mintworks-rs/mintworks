// SPDX-License-Identifier: MPL-2.0
//! Redeemable refs: one code — random, or a chosen slug — that a signup, an org invitation, an
//! affiliate link or a coupon is carried by. `type` is an open string; this module owns the
//! code, its lifetime and the attribution row (`ref_uses`), never what a type means.
//!
//! The store reaches the service as `Arc<dyn RefStore>` in `app.extensions`. Activation and
//! password-reset links are **not** refs: they stay stateless HMAC in `mintworks-auth`.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Path as UriPath, Query, State};
use axum::http::StatusCode;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::{App, RouterScopeExt, Scoped};
use crate::auth_mw::{RouteGate, require_auth, require_operator, require_role};
use crate::ctx::Ctx;
use crate::error::{ClResult, Error, Json};
use crate::ids::RefId;
use crate::ratelimit::{AUTHENTICATED, scoped_account_mw, scoped_ip_mw};
use crate::store::Role;
use crate::types::Timestamp;

/// Slugs nobody may choose, for refs and org slugs alike: they collide with app routes.
pub const RESERVED_SLUGS: &[&str] =
	&["admin", "api", "www", "app", "auth", "static", "assets", "r"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum RefStatus {
	Active,
	Revoked,
}

crate::str_enum!(RefStatus { Active => "ACTIVE", Revoked => "REVOKED" });

/// One `refs` row. `id`, `org_id`, `created_by` and `org_name` never reach the wire.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ref {
	#[serde(skip)]
	pub id: i64,
	pub uid: RefId,
	pub code: String,
	#[serde(rename = "type")]
	pub ref_type: String,
	/// The owner: whose link it is.
	#[serde(skip)]
	pub org_id: i64,
	/// `accounts.id`, no FK: the ref outlives its creator.
	#[serde(skip)]
	pub created_by: Option<i64>,
	pub target: Option<String>,
	/// The only address that may use it, lowercased; `None` = anyone.
	pub email: Option<String>,
	pub params: Value,
	/// `None` = unlimited.
	pub uses_left: Option<i64>,
	pub expires_at: Option<Timestamp>,
	pub status: RefStatus,
	pub created_at: Timestamp,
	/// The owning org's name, joined in by every read for the public preview.
	#[serde(skip)]
	pub org_name: String,
}

impl Ref {
	/// Whether a redeem at `now` could succeed — the guard [`RefStore::ref_redeem`] applies.
	pub fn is_redeemable(&self, now: Timestamp) -> bool {
		self.status == RefStatus::Active
			&& self.uses_left != Some(0)
			&& self.expires_at.is_none_or(|t| t > now)
	}
}

/// What [`RefStore::ref_insert`] writes; the adapter stamps `created_at` and `status = ACTIVE`.
#[derive(Clone, Debug)]
pub struct NewRef {
	pub uid: RefId,
	pub code: String,
	pub ref_type: String,
	pub org_id: i64,
	pub created_by: Option<i64>,
	pub target: Option<String>,
	pub email: Option<String>,
	pub params: Value,
	pub uses_left: Option<i64>,
	pub expires_at: Option<Timestamp>,
}

/// One `ref_uses` row: the attribution rewards key on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUse {
	pub id: i64,
	pub ref_id: i64,
	pub account_id: i64,
	/// The redeemer's org (typically their personal org), not the ref owner's.
	pub org_id: i64,
	pub at: Timestamp,
}

/// Storage for refs. `UNIQUE(code) COLLATE NOCASE` and `UNIQUE(ref_id, account_id)` are
/// integrity, not performance: a second adapter must reproduce both.
#[async_trait]
pub trait RefStore: Send + Sync + 'static {
	/// `E-CORE-SLUG-TAKEN` (409, [`slug_taken`]) when the code exists in any case spelling.
	async fn ref_insert(&self, new: &NewRef) -> ClResult<Ref>;

	/// Case-insensitive.
	async fn ref_by_code(&self, code: &str) -> ClResult<Option<Ref>>;

	async fn ref_by_uid(&self, uid: &RefId) -> ClResult<Option<Ref>>;

	/// Newest first. Strict equality on `org_id`; nothing descends a subtree.
	async fn refs_of_org(&self, org_id: i64, ref_type: Option<&str>) -> ClResult<Vec<Ref>>;

	/// `Ok(false)` when no ref of that uid belongs to `org_id`. Setting the current status
	/// again is a no-op.
	async fn ref_set_status(&self, org_id: i64, uid: &RefId, status: RefStatus) -> ClResult<bool>;

	/// One transaction: an existing use of the ref by `account_id`, or else by anyone in
	/// `org_id`, is returned as is, with no decrement (one use per account and per org, checked
	/// under the write lock); otherwise the guarded `uses_left` decrement (active, not exhausted,
	/// not expired at `now`) and the `ref_uses` insert. `None` when the guard refused. The flag is `true`
	/// only when this call inserted the use: a single-use caller (a coupon) refuses a `false`,
	/// since the existing use may have been spent in another org or by a concurrent checkout.
	///
	/// `hold = Some(invoice_id)` writes the use *held* by that draft until
	/// [`RefStore::ref_use_settle`]. A held use whose invoice no longer exists is an *orphan*:
	/// every read skips it, and this call first deletes the ref's orphans and gives their
	/// `uses_left` back, so an abandoned checkout frees its coupon.
	async fn ref_redeem(
		&self,
		ref_id: i64,
		account_id: i64,
		org_id: i64,
		hold: Option<i64>,
		now: Timestamp,
	) -> ClResult<Option<(RefUse, bool)>>;

	/// Clears the hold of every use held by `invoice_id`, which then counts for good.
	/// Idempotent.
	async fn ref_use_settle(&self, invoice_id: i64) -> ClResult<()>;

	async fn ref_use_of(&self, ref_id: i64, account_id: i64) -> ClResult<Option<RefUse>>;

	async fn ref_by_id(&self, id: i64) -> ClResult<Option<Ref>>;

	/// Every use redeemed in `org_id` (the redeemer's org), oldest first.
	async fn ref_uses_of_org(&self, org_id: i64) -> ClResult<Vec<RefUse>>;
}

pub fn slug_taken() -> Error {
	Error::coded(StatusCode::CONFLICT, "E-CORE-SLUG-TAKEN", "that code is already taken")
}

fn slug_invalid() -> Error {
	Error::coded(StatusCode::UNPROCESSABLE_ENTITY, "E-CORE-SLUG-INVALID", "invalid slug")
}

fn ref_invalid() -> Error {
	Error::coded(StatusCode::NOT_FOUND, "E-CORE-REF-INVALID", "unknown code")
}

/// Trims and lowercases, then checks `^[a-z0-9][a-z0-9-]{2,31}$` and [`RESERVED_SLUGS`].
/// Shared by ref codes and `orgs.slug`, which are separate namespaces.
pub fn validate_slug(raw: &str) -> ClResult<String> {
	let s = raw.trim().to_ascii_lowercase();
	let b = s.as_bytes();
	let charset = b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
	if !(3..=32).contains(&b.len()) || !charset || b[0] == b'-' || RESERVED_SLUGS.contains(&&*s) {
		return Err(slug_invalid());
	}
	Ok(s)
}

/// Ref types only `mintworks-auth` mints, through its own authorization.
pub const AUTH_TYPES: &[&str] = &["signup", "org_invite"];

/// 12 Crockford base32 chars: the random tail of a fresh ULID (its first 10 are the clock).
fn random_code() -> String {
	ulid::Ulid::new().to_string()[14..].to_owned()
}

/// `POST /api/refs` and Rune `refs::create`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRef {
	/// Defaulted so `POST /api/auth/signup-refs`, which fixes the type, can omit it.
	#[serde(rename = "type", default)]
	pub ref_type: String,
	/// A chosen slug; absent mints a random code. Gated by `refs.slug_by`.
	#[serde(default)]
	pub code: Option<String>,
	#[serde(default)]
	pub target: Option<String>,
	#[serde(default)]
	pub email: Option<String>,
	#[serde(default)]
	pub params: Option<Value>,
	#[serde(default)]
	pub uses_left: Option<i64>,
	#[serde(default)]
	pub expires_at: Option<Timestamp>,
}

/// `GET /api/refs/{code}`. Deliberately no reason when `valid` is false.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
	#[serde(rename = "type")]
	pub ref_type: String,
	pub valid: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub org_name: Option<String>,
}

/// The refs service handle.
pub struct Refs {
	app: App,
	store: Arc<dyn RefStore>,
}

impl Refs {
	/// `E-CORE-INTERNAL` when the application registered no `Arc<dyn RefStore>`.
	pub fn from_app(app: &App) -> ClResult<Self> {
		let store = app.extensions.get::<Arc<dyn RefStore>>().cloned().ok_or_else(|| {
			Error::internal("mintworks-core: no RefStore was registered on the app")
		})?;
		Ok(Self { app: app.clone(), store })
	}

	/// Unauthorized lookup: the caller authorizes.
	pub async fn by_code(&self, code: &str) -> ClResult<Option<Ref>> {
		self.store.ref_by_code(code).await
	}

	/// Unauthorized lookup: the caller authorizes.
	pub async fn by_uid(&self, uid: &RefId) -> ClResult<Option<Ref>> {
		self.store.ref_by_uid(uid).await
	}

	/// Unauthorized lookup: the caller authorizes.
	pub async fn by_id(&self, id: i64) -> ClResult<Option<Ref>> {
		self.store.ref_by_id(id).await
	}

	/// Unauthorized lookup: the caller authorizes.
	pub async fn of_org(&self, org_id: i64, ref_type: Option<&str>) -> ClResult<Vec<Ref>> {
		self.store.refs_of_org(org_id, ref_type).await
	}

	/// Unauthorized lookup: the caller authorizes.
	pub async fn use_of(&self, ref_id: i64, account_id: i64) -> ClResult<Option<RefUse>> {
		self.store.ref_use_of(ref_id, account_id).await
	}

	/// Makes the uses held by `invoice_id` permanent ([`RefStore::ref_use_settle`]), on its
	/// payment. The caller authorizes.
	pub async fn settle(&self, invoice_id: i64) -> ClResult<()> {
		self.store.ref_use_settle(invoice_id).await
	}

	/// Unauthorized lookup: the caller authorizes.
	pub async fn uses_of_org(&self, org_id: i64) -> ClResult<Vec<RefUse>> {
		self.store.ref_uses_of_org(org_id).await
	}

	/// Org ADMIN on `ctx.org()`. `signup` and `org_invite` are refused (`E-CORE-REF-TYPE`):
	/// `mintworks-auth` mints those through [`Refs::mint`] after `auth.invite_by` and its
	/// `InviteGate`.
	pub async fn create(&self, ctx: &Ctx, req: &CreateRef) -> ClResult<Ref> {
		ctx.org()?;
		require_role(&self.app, ctx, Role::Admin).await?;
		if AUTH_TYPES.contains(&req.ref_type.trim()) {
			return Err(Error::coded(
				StatusCode::UNPROCESSABLE_ENTITY,
				"E-CORE-REF-TYPE",
				"this ref type is created through its own route",
			));
		}
		self.mint(ctx, req).await
	}

	/// [`Refs::create`] without the role check or the type refusal: the caller authorizes.
	/// Choosing `code` still needs operator when `refs.slug_by` is `operator`.
	pub async fn mint(&self, ctx: &Ctx, req: &CreateRef) -> ClResult<Ref> {
		let org_id = ctx.org()?;
		let ref_type = req.ref_type.trim();
		let type_ok = ref_type
			.bytes()
			.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-.".contains(&c));
		if ref_type.is_empty() || ref_type.len() > 32 || !type_ok {
			return Err(Error::validation("type must be 1-32 chars of [a-z0-9_.-]"));
		}
		let code = match &req.code {
			Some(raw) => {
				let slug = validate_slug(raw)?;
				if self.app.settings.text("refs.slug_by").await? == "operator" {
					require_operator(&self.app, ctx).await?;
				}
				slug
			}
			None => random_code(),
		};
		if req.uses_left.is_some_and(|n| n < 0) {
			return Err(Error::validation("usesLeft must not be negative"));
		}
		let single_use = matches!(ref_type, "signup" | "org_invite" | "affiliate");
		if single_use && !matches!(req.uses_left, None | Some(1)) {
			require_operator(&self.app, ctx).await?;
		}
		let now = Timestamp::now();
		if req.expires_at.is_some_and(|t| t <= now) {
			return Err(Error::validation("expiresAt must be in the future"));
		}
		let params = req.params.clone().unwrap_or_else(|| json!({}));
		if !params.is_object() {
			return Err(Error::validation("params must be an object"));
		}
		let email = req.email.as_deref().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty());
		let new = NewRef {
			uid: RefId::generate(),
			code,
			ref_type: ref_type.to_owned(),
			org_id,
			created_by: ctx.actor.account_id(),
			target: req.target.clone(),
			email,
			params,
			// One use unless an operator asks: a multi-use referral is a reward burner accounts farm.
			uses_left: req.uses_left.or(single_use.then_some(1)),
			expires_at: req.expires_at,
		};
		let r = self.store.ref_insert(&new).await?;
		crate::audit::log(
			&self.app.store,
			ctx,
			"ref",
			Some(r.uid.as_str()),
			"create",
			Some(json!({"type": r.ref_type})),
		)
		.await;
		Ok(r)
	}

	/// The public preview. `E-CORE-REF-INVALID` for an unknown code; a known one that can no
	/// longer be redeemed answers `valid: false` and nothing else.
	pub async fn preview(&self, code: &str) -> ClResult<Preview> {
		let r = self.store.ref_by_code(code.trim()).await?.ok_or_else(ref_invalid)?;
		let valid = r.is_redeemable(Timestamp::now());
		Ok(Preview { ref_type: r.ref_type, valid, org_name: valid.then_some(r.org_name) })
	}

	/// The org's refs, org ADMIN.
	pub async fn list(&self, ctx: &Ctx, ref_type: Option<&str>) -> ClResult<Vec<Ref>> {
		let org_id = ctx.org()?;
		require_role(&self.app, ctx, Role::Admin).await?;
		self.store.refs_of_org(org_id, ref_type).await
	}

	/// Org ADMIN. Another org's ref is `E-CORE-NOTFOUND`.
	pub async fn revoke(&self, ctx: &Ctx, uid: &RefId) -> ClResult<()> {
		let org_id = ctx.org()?;
		require_role(&self.app, ctx, Role::Admin).await?;
		if !self.store.ref_set_status(org_id, uid, RefStatus::Revoked).await? {
			return Err(Error::NotFound);
		}
		crate::audit::log(&self.app.store, ctx, "ref", Some(uid.as_str()), "revoke", None).await;
		Ok(())
	}

	/// Org ADMIN: back to `ACTIVE`. Expiry and `uses_left` still guard redemption, so an
	/// expired or used-up ref stays unredeemable. Another org's ref is `E-CORE-NOTFOUND`.
	pub async fn reactivate(&self, ctx: &Ctx, uid: &RefId) -> ClResult<()> {
		let org_id = ctx.org()?;
		require_role(&self.app, ctx, Role::Admin).await?;
		if !self.store.ref_set_status(org_id, uid, RefStatus::Active).await? {
			return Err(Error::NotFound);
		}
		crate::audit::log(&self.app.store, ctx, "ref", Some(uid.as_str()), "reactivate", None)
			.await;
		Ok(())
	}

	/// Records `account_id` (acting in `org_id`) as a use of `ref_id`. `None` when the ref is
	/// revoked, exhausted or expired; a repeat use returns the first with `false` (see
	/// [`RefStore::ref_redeem`]). The caller authorizes.
	pub async fn redeem(
		&self,
		_ctx: &Ctx,
		ref_id: i64,
		account_id: i64,
		org_id: i64,
	) -> ClResult<Option<(RefUse, bool)>> {
		self.store.ref_redeem(ref_id, account_id, org_id, None, Timestamp::now()).await
	}
}

/// `GET /api/refs/{code}` (public, IP-limited as `refs.preview`), and the authenticated
/// `POST /api/refs`, `GET /api/refs?type=`, `DELETE /api/refs/{uid}`,
/// `POST /api/refs/{uid}/reactivate` scoped `refs`.
pub fn routes(gate: &RouteGate) -> Scoped {
	let public = Router::new().route(
		"/api/refs/{code}",
		get(preview).layer(from_fn_with_state("refs.preview", scoped_ip_mw)),
	);
	let authed = gate
		.apply(
			Router::new()
				.route("/api/refs", post(create).get(list))
				.route("/api/refs/{code}", delete(revoke))
				.route("/api/refs/{code}/reactivate", post(reactivate)),
		)
		.layer(from_fn_with_state(AUTHENTICATED, scoped_account_mw))
		.layer(from_fn(require_auth));
	Scoped::from(public).merge(authed.scope("refs"))
}

async fn preview(
	State(app): State<App>,
	UriPath(code): UriPath<String>,
) -> ClResult<Json<Preview>> {
	Ok(Json(Refs::from_app(&app)?.preview(&code).await?))
}

async fn create(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<CreateRef>,
) -> ClResult<(StatusCode, Json<Ref>)> {
	Ok((StatusCode::CREATED, Json(Refs::from_app(&app)?.create(&ctx, &req).await?)))
}

#[derive(Deserialize)]
struct ListQuery {
	#[serde(rename = "type")]
	ref_type: Option<String>,
}

async fn list(
	State(app): State<App>,
	ctx: Ctx,
	Query(q): Query<ListQuery>,
) -> ClResult<Json<Value>> {
	let items = Refs::from_app(&app)?.list(&ctx, q.ref_type.as_deref()).await?;
	Ok(Json(json!({ "items": items })))
}

/// The path segment is named `code` to share the preview's route; here it holds the uid.
async fn revoke(
	State(app): State<App>,
	ctx: Ctx,
	UriPath(uid): UriPath<String>,
) -> ClResult<StatusCode> {
	Refs::from_app(&app)?.revoke(&ctx, &RefId::parse(&uid)?).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// Named `code` like [`revoke`]; it holds the uid.
async fn reactivate(
	State(app): State<App>,
	ctx: Ctx,
	UriPath(uid): UriPath<String>,
) -> ClResult<StatusCode> {
	Refs::from_app(&app)?.reactivate(&ctx, &RefId::parse(&uid)?).await?;
	Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn slug_rules() {
		assert_eq!(validate_slug(" Acme-Co ").unwrap(), "acme-co");
		for bad in ["ab", "-abc", "a_bc", "admin", "api", "ékezet", &"a".repeat(33)] {
			assert!(validate_slug(bad).is_err(), "{bad}");
		}
		assert_eq!(random_code().len(), 12);
	}
}

// vim: ts=4
