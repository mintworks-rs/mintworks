//! The `Entitle` service over the real store: how each kind aggregates, and the error codes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use saas_core::{App, AppBuilder, config::Config, ctx::Ctx, error::Error, types::Timestamp};
use saas_entitle::{
	E_DENIED, E_EXHAUSTED, E_UNKNOWN, Entitle, EntitleStore, EntitlementDef, GrantReq, Source,
};
use store_adapter_sqlite::{FRAMEWORK, SqliteStore};

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-entitle-svc-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn setup(db: &TmpDb) -> (App, Ctx) {
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
	let root = saas_core::store::CoreStore::root_org_id(&store).await.unwrap();
	let builder = AppBuilder::new()
		.config(config)
		.store(Arc::new(store.clone()) as Arc<dyn saas_core::store::CoreStore>)
		.extension(Arc::new(store) as Arc<dyn EntitleStore>);
	let app = saas_entitle::install(
		builder,
		[
			EntitlementDef::feature("export"),
			EntitlementDef::limit("seats"),
			EntitlementDef::meter("credits"),
		],
	)
	.build()
	.await
	.unwrap();
	(app, Ctx::system("test").with_org(root))
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
	let (app, ctx) = setup(&db).await;
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
	let (app, ctx) = setup(&db).await;
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
	let (app, ctx) = setup(&db).await;
	let ent = Entitle::from_app(&app).unwrap();
	ent.grant(&ctx, &req("export", 1, "sub")).await.unwrap();
	assert_eq!(ent.cut(&ctx, Source::Manual, "sub", Timestamp::now()).await.unwrap(), 1);
	assert!(!ent.has(&ctx, "export").await.unwrap());
}

// vim: ts=4
