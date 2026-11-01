// SPDX-License-Identifier: MPL-2.0
//! The `Entitle` service over the real store: how each kind aggregates, and the error codes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mintworks_auth::store::{AuthStore, NewAccount, Org, OrgKind, Role};
use mintworks_core::event::Event;
use mintworks_core::ids::AccountId;
use mintworks_core::{App, AppBuilder, config::Config, ctx::Ctx, error::Error, types::Timestamp};
use mintworks_entitle::{
	E_DENIED, E_EXHAUSTED, E_SEATS_FULL, E_UNKNOWN, Entitle, EntitleStore, EntitlementDef,
	GrantReq, Source,
};
use mintworks_store_sqlite::{FRAMEWORK, SqliteStore};

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-entitle-svc-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn setup(db: &TmpDb) -> (App, Ctx, SqliteStore) {
	setup_with(db, |b| b).await
}

async fn setup_with(
	db: &TmpDb,
	hook: impl FnOnce(AppBuilder) -> AppBuilder,
) -> (App, Ctx, SqliteStore) {
	let config = Config {
		master_key: [0; 32],
		db_path: db.0.join("test.db").to_string_lossy().into_owned(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	let store = SqliteStore::open(&config).await.unwrap();
	store.migrate(&[FRAMEWORK]).await.unwrap();
	let root = mintworks_core::store::CoreStore::root_org_id(&store).await.unwrap();
	let builder = AppBuilder::new()
		.config(config)
		.store(Arc::new(store.clone()) as Arc<dyn mintworks_core::store::CoreStore>)
		.extension(Arc::new(store.clone()) as Arc<dyn EntitleStore>);
	let app = hook(mintworks_entitle::install(
		builder,
		[
			EntitlementDef::feature("export"),
			EntitlementDef::limit("seats"),
			EntitlementDef::meter("credits"),
		],
	))
	.build()
	.await
	.unwrap();
	(app, Ctx::system("test").with_org(root), store)
}

fn req(key: &str, amount: i64, source_ref: &str) -> GrantReq {
	GrantReq {
		key: key.to_owned(),
		amount,
		valid_from: None,
		valid_until: None,
		source: Source::Manual,
		source_ref: Some(source_ref.to_owned()),
	}
}

fn code(err: Error) -> &'static str {
	match err {
		Error::Coded { code, .. } => code,
		other => panic!("not a coded error: {other:?}"),
	}
}

#[tokio::test]
async fn kinds_aggregate_over_active_grants() {
	let db = TmpDb::new("kinds");
	let (app, ctx, _) = setup(&db).await;
	let ent = Entitle::from_app(&app).unwrap();

	assert!(!ent.has(&ctx, "export").await.unwrap());
	assert_eq!(ent.limit(&ctx, "seats").await.unwrap(), None);
	ent.grant(&ctx, &req("export", 1, "a")).await.unwrap();
	ent.grant(&ctx, &req("seats", 3, "b")).await.unwrap();
	ent.grant(&ctx, &req("seats", 5, "c")).await.unwrap();
	ent.grant(&ctx, &req("credits", 10, "d")).await.unwrap();
	ent.grant(&ctx, &req("credits", 20, "e")).await.unwrap();

	assert!(ent.has(&ctx, "export").await.unwrap());
	assert_eq!(ent.limit(&ctx, "seats").await.unwrap(), Some(5), "max, not sum");
	assert_eq!(ent.balance(&ctx, "credits").await.unwrap(), 30, "sum");
	assert_eq!(ent.consume(&ctx, "credits", 12, "x").await.unwrap(), 18);
	assert_eq!(ent.consume(&ctx, "credits", 12, "x").await.unwrap(), 18, "retry debits once");

	let s = ent.summary(&ctx).await.unwrap();
	assert_eq!(s.features, ["export"]);
	assert_eq!(s.limits["seats"], 5);
	assert_eq!(s.meters["credits"].balance, 18);
}

#[tokio::test]
async fn refusals_are_402_and_undeclared_keys_422() {
	let db = TmpDb::new("codes");
	let (app, ctx, _) = setup(&db).await;
	let ent = Entitle::from_app(&app).unwrap();

	assert_eq!(code(ent.require(&ctx, "export").await.unwrap_err()), E_DENIED);
	ent.grant(&ctx, &req("credits", 5, "a")).await.unwrap();
	assert_eq!(code(ent.consume(&ctx, "credits", 6, "y").await.unwrap_err()), E_EXHAUSTED);
	assert_eq!(ent.charge(&ctx, "credits", 6, "z").await.unwrap(), -1, "charge overdraws");
	assert_eq!(code(ent.has(&ctx, "nope").await.unwrap_err()), E_UNKNOWN);
	assert_eq!(code(ent.balance(&ctx, "seats").await.unwrap_err()), E_UNKNOWN, "wrong kind");
	assert_eq!(code(ent.grant(&ctx, &req("nope", 1, "b")).await.unwrap_err()), E_UNKNOWN);
}

#[tokio::test]
async fn cut_ends_access_and_keeps_what_was_consumed() {
	let db = TmpDb::new("cut");
	let (app, ctx, _) = setup(&db).await;
	let ent = Entitle::from_app(&app).unwrap();
	ent.grant(&ctx, &req("export", 1, "sub")).await.unwrap();
	assert_eq!(ent.cut(&ctx, Source::Manual, "sub", Timestamp::now()).await.unwrap(), 1);
	assert!(!ent.has(&ctx, "export").await.unwrap());
}

async fn account(store: &SqliteStore, name: &str) -> (i64, AccountId) {
	let new = NewAccount {
		email: format!("{name}@seat.test"),
		pwd_hash: None,
		name: None,
		locale: "en".to_owned(),
		org_name: name.to_owned(),
	};
	let (acc, _) = store.create_account(&new, &[]).await.unwrap();
	(acc.id, acc.uid)
}

async fn org(store: &SqliteStore, parent: i64, name: &str, owner: i64) -> Org {
	store.create_org(OrgKind::Shared, parent, name, owner, None).await.unwrap()
}

async fn join(store: &SqliteStore, org: &Org, account: i64, role: Role) {
	assert!(store.put_membership(org.id, account, role).await.unwrap());
	store.accept_membership(org.id, account, Timestamp::now()).await.unwrap();
}

fn user(id: i64) -> Ctx {
	Ctx::system("test").as_user(id)
}

/// A school whose owner holds `seats` limit 1, plus a plain member and an outsider.
struct School {
	org: Org,
	owner: Ctx,
	owner_uid: AccountId,
	member: (i64, AccountId),
	outsider: (i64, AccountId),
}

async fn school(app: &App, store: &SqliteStore, ctx: &Ctx, seats: Option<i64>) -> School {
	let (owner, owner_uid) = account(store, "owner").await;
	let org = org(store, ctx.org_id.unwrap(), "school", owner).await;
	let member = account(store, "member").await;
	join(store, &org, member.0, Role::Member).await;
	let outsider = account(store, "outsider").await;
	if let Some(n) = seats {
		let ent = Entitle::from_app(app).unwrap();
		ent.grant_to(ctx, org.id, &req("seats", n, "seats")).await.unwrap();
	}
	School { org, owner: user(owner), owner_uid, member, outsider }
}

#[tokio::test]
async fn seat_calls_need_admin_and_a_member_to_seat() {
	let db = TmpDb::new("seat-authz");
	let (app, ctx, store) = setup(&db).await;
	let s = school(&app, &store, &ctx, Some(1)).await;
	let ent = Entitle::from_app(&app).unwrap();
	let org = s.org.uid.as_str();
	let (member, outsider) = (s.member.1.as_str(), s.outsider.1.as_str());

	let err = ent.seat_assign(&user(s.member.0), org, "seats", member).await.unwrap_err();
	assert_eq!(code(err), "E-AUTH-FORBIDDEN", "a plain member is 403");
	let err = ent.seats(&user(s.outsider.0), org, "seats").await.unwrap_err();
	assert!(matches!(err, Error::NotFound), "a non-member is not found");
	let err = ent.seat_assign(&s.owner, org, "seats", outsider).await.unwrap_err();
	assert!(matches!(err, Error::NotFound), "only a member can be seated");
}

#[tokio::test]
async fn seats_fill_to_the_limit_and_a_reassign_is_false() {
	let db = TmpDb::new("seat-full");
	let (app, ctx, store) = setup(&db).await;
	let s = school(&app, &store, &ctx, Some(1)).await;
	let ent = Entitle::from_app(&app).unwrap();
	let org = s.org.uid.as_str();
	let owner = s.owner_uid.as_str();

	assert!(ent.seat_assign(&s.owner, org, "seats", s.member.1.as_str()).await.unwrap());
	assert!(!ent.seat_assign(&s.owner, org, "seats", s.member.1.as_str()).await.unwrap());
	let err = ent.seat_assign(&s.owner, org, "seats", owner).await.unwrap_err();
	assert_eq!(code(err), E_SEATS_FULL);
}

#[tokio::test]
async fn no_grant_means_no_seats() {
	let db = TmpDb::new("seat-none");
	let (app, ctx, store) = setup(&db).await;
	let s = school(&app, &store, &ctx, None).await;
	let ent = Entitle::from_app(&app).unwrap();
	let err = ent
		.seat_assign(&s.owner, s.org.uid.as_str(), "seats", s.member.1.as_str())
		.await;
	assert_eq!(code(err.unwrap_err()), E_SEATS_FULL);
}

#[tokio::test]
async fn seats_of_account_is_own_only_and_the_gdpr_hook_exports_and_erases() {
	let db = TmpDb::new("seat-gdpr");
	let (app, ctx, store) = setup(&db).await;
	let s = school(&app, &store, &ctx, Some(1)).await;
	let ent = Entitle::from_app(&app).unwrap();
	let member = s.member.1.as_str();
	ent.seat_assign(&s.owner, s.org.uid.as_str(), "seats", member).await.unwrap();

	assert_eq!(ent.seats_of_account(&user(s.member.0), member).await.unwrap().len(), 1);
	let err = ent.seats_of_account(&user(s.outsider.0), member).await.unwrap_err();
	assert!(matches!(err, Error::NotFound), "another user's seats are not found");

	let hook = app.account_data_hooks.iter().find(|h| h.name() == "seats").unwrap();
	let export = hook.export(&s.member.1).await.unwrap();
	assert_eq!(export[0]["org"], s.org.uid.as_str());
	assert_eq!(export[0]["key"], "seats");
	hook.erase(&s.member.1).await.unwrap();
	assert!(ent.seats(&s.owner, s.org.uid.as_str(), "seats").await.unwrap().is_empty());
}

#[tokio::test]
async fn removed_member_loses_seat() {
	let db = TmpDb::new("seat-removed");
	let (app, ctx, store) = setup(&db).await;
	let s = school(&app, &store, &ctx, Some(2)).await;
	let ent = Entitle::from_app(&app).unwrap();
	let org = s.org.uid.as_str();
	// The outsider joins the school and a class below it, so leaving the school keeps the seat.
	let class = self::org(&store, s.org.id, "class", s.owner.actor.account_id().unwrap()).await;
	join(&store, &s.org, s.outsider.0, Role::Member).await;
	join(&store, &class, s.outsider.0, Role::Member).await;
	for (_, uid) in [&s.member, &s.outsider] {
		assert!(ent.seat_assign(&s.owner, org, "seats", uid.as_str()).await.unwrap());
	}

	for (id, uid) in [&s.member, &s.outsider] {
		assert!(store.remove_membership(s.org.id, *id).await.unwrap());
		ent.release_stale_seats(uid.as_str()).await.unwrap();
	}
	// `seats` already hides a non-member's seat; `seats_of_account` shows the release itself.
	let sys = Ctx::system("test");
	assert!(ent.seats_of_account(&sys, s.member.1.as_str()).await.unwrap().is_empty());
	assert_eq!(ent.seats_of_account(&sys, s.outsider.1.as_str()).await.unwrap().len(), 1);
	let seated = ent.seats(&s.owner, org, "seats").await.unwrap();
	assert_eq!(seated.into_iter().map(|(a, _)| a).collect::<Vec<_>>(), [s.outsider.1]);
}

#[tokio::test]
async fn membership_removed_event_releases_seat() {
	let db = TmpDb::new("seat-removed-event");
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
	// Handlers run in registration order, so this one fires after entitle's release.
	let (app, ctx, store) = setup_with(&db, |b| {
		b.on_event(move |_, _| {
			let _ = tx.send(());
			async { Ok(()) }
		})
	})
	.await;
	let s = school(&app, &store, &ctx, Some(1)).await;
	let ent = Entitle::from_app(&app).unwrap();
	let (id, uid) = &s.member;
	assert!(
		ent.seat_assign(&s.owner, s.org.uid.as_str(), "seats", uid.as_str())
			.await
			.unwrap()
	);
	assert!(store.remove_membership(s.org.id, *id).await.unwrap());
	let ev = Event::MembershipRemoved { org: s.org.uid.clone(), account: uid.clone() };
	mintworks_core::event::emit(&app, ev);
	tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
		.await
		.unwrap()
		.unwrap();
	let sys = Ctx::system("test");
	assert!(ent.seats_of_account(&sys, uid.as_str()).await.unwrap().is_empty());
}

// vim: ts=4
