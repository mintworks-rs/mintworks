// SPDX-License-Identifier: MPL-2.0
//! `Entitle`: the service handle. Every method takes `&Ctx` first; the checks read
//! `ctx.org_id` only — no ancestor walk.

use std::collections::BTreeMap;
use std::sync::Arc;

use mintworks_core::app::App;
use mintworks_core::auth_mw::{require_member_role_on, require_operator};
use mintworks_core::ctx::{Actor, Ctx};
use mintworks_core::error::{ClResult, Error, StatusCode};
use mintworks_core::ids::{AccountId, GrantId, OrgId};
use mintworks_core::prelude::Timestamp;
use mintworks_core::store::Role;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::store::{Debit, EntitleStore, Grant, NewGrant, SeatOutcome, Source};
use crate::{EntitlementRegistry, Kind};

/// 422: the key was never declared, or is declared as another kind.
pub const E_UNKNOWN: &str = "E-ENT-UNKNOWN";
/// 402, not 403: the SPA answers it with an upsell.
pub const E_DENIED: &str = "E-ENT-DENIED";
/// 402: the meter's balance is short of the amount asked.
pub const E_EXHAUSTED: &str = "E-ENT-EXHAUSTED";
/// 409: an idempotency key replayed with a different key or amount than its first debit.
pub const E_IDEM: &str = "E-ENT-IDEM";
/// 409: every seat the org's limit allows is assigned.
pub const E_SEATS_FULL: &str = "E-ENT-SEATS-FULL";

/// A grant as a caller asks for it; the uid is minted here.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantReq {
	pub key: String,
	pub amount: i64,
	/// Defaults to now.
	pub valid_from: Option<Timestamp>,
	pub valid_until: Option<Timestamp>,
	pub source: Source,
	pub source_ref: Option<String>,
}

/// `POST /api/admin/orgs/{uid}/grants`: always `MANUAL`, with a fresh `source_ref`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminGrant {
	pub key: String,
	pub amount: i64,
	pub valid_until: Option<Timestamp>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
	pub features: Vec<String>,
	pub limits: BTreeMap<String, i64>,
	pub meters: BTreeMap<String, MeterView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterView {
	pub balance: i64,
	/// The soonest `valid_until` among grants with something left; `None` when none expires.
	pub next_expiry: Option<Timestamp>,
}

#[derive(Clone)]
pub struct Entitle {
	app: App,
	store: Arc<dyn EntitleStore>,
	registry: EntitlementRegistry,
}

impl Entitle {
	pub fn new(app: App, store: Arc<dyn EntitleStore>, registry: EntitlementRegistry) -> Self {
		Self { app, store, registry }
	}

	/// Over the `Arc<dyn EntitleStore>` and the [`EntitlementRegistry`] in `app.extensions`.
	pub fn from_app(app: &App) -> ClResult<Self> {
		let store = app.extensions.get::<Arc<dyn EntitleStore>>().cloned().ok_or_else(|| {
			Error::internal("mintworks-entitle: no EntitleStore extension registered")
		})?;
		let registry = app.extensions.get::<EntitlementRegistry>().cloned().unwrap_or_default();
		Ok(Self::new(app.clone(), store, registry))
	}

	pub async fn has(&self, ctx: &Ctx, key: &str) -> ClResult<bool> {
		let kind = self.declared(key)?;
		let gs = self.active(ctx.org()?, Some(key)).await?;
		Ok(match kind {
			Kind::Feature => gs.iter().any(|g| g.amount > 0),
			Kind::Limit => limit_of(&gs).is_some_and(|n| n > 0),
			Kind::Meter => balance_of(&gs) > 0,
		})
	}

	pub async fn limit(&self, ctx: &Ctx, key: &str) -> ClResult<Option<i64>> {
		self.expect(key, Kind::Limit)?;
		Ok(limit_of(&self.active(ctx.org()?, Some(key)).await?))
	}

	/// May be negative after a `charge`.
	pub async fn balance(&self, ctx: &Ctx, key: &str) -> ClResult<i64> {
		self.expect(key, Kind::Meter)?;
		Ok(balance_of(&self.active(ctx.org()?, Some(key)).await?))
	}

	/// [`Self::has`], or 402 `E-ENT-DENIED`.
	pub async fn require(&self, ctx: &Ctx, key: &str) -> ClResult<()> {
		if self.has(ctx, key).await? {
			return Ok(());
		}
		Err(Error::coded(
			StatusCode::PAYMENT_REQUIRED,
			E_DENIED,
			format!("'{key}' is not included"),
		))
	}

	/// Debits `n` if the balance covers it, else 402 `E-ENT-EXHAUSTED`. A retried `idem`
	/// debits once. Returns the balance after.
	pub async fn consume(&self, ctx: &Ctx, key: &str, n: i64, idem: &str) -> ClResult<i64> {
		if !self.debit(ctx, key, n, idem, false).await? {
			return Err(Error::coded(
				StatusCode::PAYMENT_REQUIRED,
				E_EXHAUSTED,
				format!("'{key}' has less than {n} left"),
			));
		}
		self.balance(ctx, key).await
	}

	/// [`Self::consume`] that may overdraw: for usage that already happened.
	pub async fn charge(&self, ctx: &Ctx, key: &str, n: i64, idem: &str) -> ClResult<i64> {
		self.debit(ctx, key, n, idem, true).await?;
		self.balance(ctx, key).await
	}

	/// [`Self::grant_to`] for the acting org.
	pub async fn grant(&self, ctx: &Ctx, req: &GrantReq) -> ClResult<Grant> {
		self.grant_to(ctx, ctx.org()?, req).await
	}

	/// Operator or `Actor::System` only (a renewal job acts as the latter). Idempotent on
	/// `(org, key, source, source_ref)`.
	pub async fn grant_to(&self, ctx: &Ctx, org_id: i64, req: &GrantReq) -> ClResult<Grant> {
		require_operator(&self.app, ctx).await?;
		self.declared(&req.key)?;
		if req.amount < 0 {
			return Err(Error::validation("amount must not be negative"));
		}
		let valid_from = req.valid_from.unwrap_or_else(Timestamp::now);
		if req.valid_until.is_some_and(|u| u <= valid_from) {
			return Err(Error::validation("validUntil must be after validFrom"));
		}
		let g = self
			.store
			.grant_insert(&NewGrant {
				uid: GrantId::generate(),
				org_id,
				key: req.key.clone(),
				amount: req.amount,
				valid_from,
				valid_until: req.valid_until,
				source: req.source,
				source_ref: req.source_ref.clone(),
			})
			.await?;
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"grant",
			Some(g.uid.as_str()),
			"GRANT",
			Some(json!({"key": g.key, "amount": g.amount, "source": g.source.as_str()})),
		)
		.await;
		Ok(g)
	}

	pub async fn extend(
		&self,
		ctx: &Ctx,
		source: Source,
		source_ref: &str,
		valid_until: Option<Timestamp>,
	) -> ClResult<u64> {
		self.extend_to(ctx, ctx.org()?, source, source_ref, valid_until).await
	}

	/// Operator or system only. Moves `valid_until` later (never earlier) on every grant from
	/// `(source, source_ref)`; see [`EntitleStore::grants_extend`].
	pub async fn extend_to(
		&self,
		ctx: &Ctx,
		org_id: i64,
		source: Source,
		source_ref: &str,
		valid_until: Option<Timestamp>,
	) -> ClResult<u64> {
		require_operator(&self.app, ctx).await?;
		let n = self.store.grants_extend(org_id, source, source_ref, valid_until).await?;
		self.audit_ref(ctx, "EXTEND", source, source_ref).await;
		Ok(n)
	}

	pub async fn cut(
		&self,
		ctx: &Ctx,
		source: Source,
		source_ref: &str,
		at: Timestamp,
	) -> ClResult<u64> {
		self.cut_to(ctx, ctx.org()?, source, source_ref, at).await
	}

	/// Operator or system only. Ends the grants from `(source, source_ref)` at `at` at the
	/// latest; what was consumed stays consumed.
	pub async fn cut_to(
		&self,
		ctx: &Ctx,
		org_id: i64,
		source: Source,
		source_ref: &str,
		at: Timestamp,
	) -> ClResult<u64> {
		require_operator(&self.app, ctx).await?;
		let n = self.store.grants_cut(org_id, source, source_ref, at).await?;
		self.audit_ref(ctx, "CUT", source, source_ref).await;
		Ok(n)
	}

	/// The acting org's entitlements over every declared key: features held, limits granted,
	/// and every meter (zero when never granted).
	pub async fn summary(&self, ctx: &Ctx) -> ClResult<Summary> {
		let all = self.active(ctx.org()?, None).await?;
		let mut out = Summary::default();
		for (key, kind) in self.registry.iter() {
			let gs: Vec<&Grant> = all.iter().filter(|g| g.key == key).collect();
			match kind {
				Kind::Feature if gs.iter().any(|g| g.amount > 0) => {
					out.features.push(key.to_owned());
				}
				Kind::Feature => {}
				Kind::Limit => {
					if let Some(n) = gs.iter().map(|g| g.amount).max() {
						out.limits.insert(key.to_owned(), n);
					}
				}
				Kind::Meter => {
					let balance = gs.iter().map(|g| g.amount - g.used).sum();
					let next_expiry =
						gs.iter().filter(|g| g.amount > g.used).filter_map(|g| g.valid_until).min();
					out.meters.insert(key.to_owned(), MeterView { balance, next_expiry });
				}
			}
		}
		Ok(out)
	}

	/// Operator only: a `MANUAL` grant to the org `org_uid`.
	pub async fn admin_grant(&self, ctx: &Ctx, org_uid: &str, req: &AdminGrant) -> ClResult<Grant> {
		require_operator(&self.app, ctx).await?;
		let org_id = self.org_by_uid(org_uid).await?;
		let req = GrantReq {
			key: req.key.clone(),
			amount: req.amount,
			valid_from: None,
			valid_until: req.valid_until,
			source: Source::Manual,
			source_ref: Some(GrantId::generate().as_str().to_owned()),
		};
		self.grant_to(ctx, org_id, &req).await
	}

	/// Operator only: every grant of the org `org_uid`, expired included.
	pub async fn admin_grants(&self, ctx: &Ctx, org_uid: &str) -> ClResult<Vec<Grant>> {
		require_operator(&self.app, ctx).await?;
		self.store.grants_of_org(self.org_by_uid(org_uid).await?).await
	}

	/// `Admin` on `org_uid`. Seats `account_uid` — an accepted member of the org or of an org
	/// below it — on the `Limit` key `key`, up to the org's limit (no grant = no seats).
	/// `true` when newly seated, `false` when it already was; full is 409 `E-ENT-SEATS-FULL`.
	pub async fn seat_assign(
		&self,
		ctx: &Ctx,
		org_uid: &str,
		key: &str,
		account_uid: &str,
	) -> ClResult<bool> {
		let org_id = self.seat_org(ctx, org_uid, key).await?;
		let account_id = self.account_id(account_uid).await?;
		match self.store.seat_assign(org_id, key, account_id, Timestamp::now()).await? {
			SeatOutcome::AlreadySeated => Ok(false),
			SeatOutcome::NotMember => Err(Error::NotFound),
			SeatOutcome::Full(max) => Err(Error::coded(
				StatusCode::CONFLICT,
				E_SEATS_FULL,
				format!("every '{key}' seat ({max}) is taken"),
			)),
			SeatOutcome::Inserted => {
				self.audit_seat(ctx, org_uid, key, account_uid, "SEAT_ASSIGNED").await;
				Ok(true)
			}
		}
	}

	/// `Admin` on `org_uid`. `false` (a no-op) when the account held no seat.
	pub async fn seat_release(
		&self,
		ctx: &Ctx,
		org_uid: &str,
		key: &str,
		account_uid: &str,
	) -> ClResult<bool> {
		let org_id = self.seat_org(ctx, org_uid, key).await?;
		let account_id = self.account_id(account_uid).await?;
		let released = self.store.seat_release(org_id, key, account_id, Timestamp::now()).await?;
		if released {
			self.audit_seat(ctx, org_uid, key, account_uid, "SEAT_RELEASED").await;
		}
		Ok(released)
	}

	/// `Admin` on `org_uid`: the active seats of `key`, oldest first.
	pub async fn seats(
		&self,
		ctx: &Ctx,
		org_uid: &str,
		key: &str,
	) -> ClResult<Vec<(AccountId, Timestamp)>> {
		let org_id = self.seat_org(ctx, org_uid, key).await?;
		self.store.seats_of_org(org_id, key).await
	}

	/// The account's active seats: for the account itself, an operator or `System`; anyone
	/// else gets `E-CORE-NOTFOUND`.
	pub async fn seats_of_account(
		&self,
		ctx: &Ctx,
		account_uid: &str,
	) -> ClResult<Vec<(OrgId, String, Timestamp)>> {
		let account_id = self.account_id(account_uid).await?;
		let own = ctx.actor.account_id() == Some(account_id);
		if !own && !matches!(ctx.actor, Actor::System { .. }) {
			require_operator(&self.app, ctx).await.map_err(|e| match e {
				Error::Coded { status: StatusCode::FORBIDDEN, .. } => Error::NotFound,
				e => e,
			})?;
		}
		self.store.seats_of_account(account_id).await
	}

	/// Releases every seat `account_uid` holds in an org it no longer belongs to, directly or
	/// through a descendant org. Run on `MembershipRemoved`; returns how many were released.
	pub async fn release_stale_seats(&self, account_uid: &str) -> ClResult<u64> {
		let id = self.account_id(account_uid).await?;
		let ctx = Ctx::system("entitle");
		let rows = self.store.seats_release_stale(id, Timestamp::now()).await?;
		for (org, key) in &rows {
			self.audit_seat(&ctx, org.as_str(), key, account_uid, "SEAT_RELEASED").await;
		}
		Ok(u64::try_from(rows.len()).unwrap_or(u64::MAX))
	}

	/// The org behind a seat call: not a member is `E-CORE-NOTFOUND`, below `Admin` 403.
	async fn seat_org(&self, ctx: &Ctx, org_uid: &str, key: &str) -> ClResult<i64> {
		let org_id = self.org_by_uid(org_uid).await?;
		require_member_role_on(&self.app, ctx, org_id, Role::Admin).await?;
		self.expect(key, Kind::Limit)?;
		Ok(org_id)
	}

	/// Any status: an admin must be able to free a suspended account's seat.
	async fn account_id(&self, uid: &str) -> ClResult<i64> {
		let acc = self.app.store.account_for_token(uid).await?.ok_or(Error::NotFound)?;
		Ok(acc.id)
	}

	async fn audit_seat(&self, ctx: &Ctx, org_uid: &str, key: &str, account: &str, action: &str) {
		let detail = json!({"key": key, "account": account});
		mintworks_core::audit::log(
			&self.app.store,
			ctx,
			"org",
			Some(org_uid),
			action,
			Some(detail),
		)
		.await;
	}

	async fn debit(
		&self,
		ctx: &Ctx,
		key: &str,
		n: i64,
		idem: &str,
		overdraw: bool,
	) -> ClResult<bool> {
		self.expect(key, Kind::Meter)?;
		if n <= 0 {
			return Err(Error::validation("amount must be positive"));
		}
		if idem.trim().is_empty() {
			return Err(Error::validation("an idempotency key is required"));
		}
		self.store
			.usage_debit(&Debit {
				org_id: ctx.org()?,
				key: key.to_owned(),
				amount: n,
				idem_key: idem.to_owned(),
				account_id: ctx.actor.account_id(),
				at: Timestamp::now(),
				overdraw,
			})
			.await
	}

	async fn active(&self, org_id: i64, key: Option<&str>) -> ClResult<Vec<Grant>> {
		self.store.grants_active(org_id, key, Timestamp::now()).await
	}

	async fn org_by_uid(&self, uid: &str) -> ClResult<i64> {
		let uid = OrgId::parse(uid).map_err(|_| Error::NotFound)?;
		self.store.entitle_org_id(&uid).await?.ok_or(Error::NotFound)
	}

	fn declared(&self, key: &str) -> ClResult<Kind> {
		self.registry.kind(key).ok_or_else(|| {
			Error::coded(
				StatusCode::UNPROCESSABLE_ENTITY,
				E_UNKNOWN,
				format!("'{key}' is not a declared entitlement"),
			)
		})
	}

	fn expect(&self, key: &str, want: Kind) -> ClResult<()> {
		match self.declared(key)? {
			k if k == want => Ok(()),
			k => Err(Error::coded(
				StatusCode::UNPROCESSABLE_ENTITY,
				E_UNKNOWN,
				format!("'{key}' is a {}, not a {}", k.as_str(), want.as_str()),
			)),
		}
	}

	async fn audit_ref(&self, ctx: &Ctx, action: &str, source: Source, source_ref: &str) {
		let detail = json!({"source": source.as_str(), "sourceRef": source_ref});
		mintworks_core::audit::log(&self.app.store, ctx, "grant", None, action, Some(detail)).await;
	}
}

fn limit_of(gs: &[Grant]) -> Option<i64> {
	gs.iter().map(|g| g.amount).max()
}

fn balance_of(gs: &[Grant]) -> i64 {
	gs.iter().map(|g| g.amount - g.used).sum()
}

// vim: ts=4
