// SPDX-License-Identifier: MPL-2.0
//! `Memory` over the real stores: the core DB for org and account uids, the app DB for memory.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;

use mintworks_appdb_sqlite::{MEMORY, SqliteAppDb};
use mintworks_auth::store::{Account, AuthStore, NewAccount, Org, OrgKind};
use mintworks_core::account_data::AccountDataHook;
use mintworks_core::store::CoreStore;
use mintworks_core::{Ctx, Error, config::Config};
use mintworks_memory::{Memory, MemoryStore};
use mintworks_store_sqlite::SqliteStore;

struct TmpDb(PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("memory-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		Self(dir)
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

struct Env {
	core: Arc<SqliteStore>,
	app: Arc<SqliteAppDb>,
	memory: Memory,
}

async fn setup(db: &TmpDb) -> Env {
	let core = SqliteStore::open(&Config {
		master_key: [0; 32],
		db_path: db.0.join("core.db").to_string_lossy().into_owned(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	})
	.await
	.unwrap();
	core.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let core = Arc::new(core);
	let app = SqliteAppDb::new(db.0.join("app.db"));
	app.migrate(&[MEMORY]).await.unwrap();
	let app = Arc::new(app);
	let memory = Memory::new(app.clone(), core.clone());
	Env { core, app, memory }
}

async fn account(env: &Env, email: &str) -> (Account, Org) {
	let new = NewAccount {
		email: email.to_owned(),
		pwd_hash: None,
		name: None,
		locale: "hu".to_owned(),
		org_name: email.to_owned(),
	};
	env.core.create_account(&new, &[]).await.unwrap()
}

fn ctx(acc: &Account, org: &Org) -> Ctx {
	Ctx::system("test").as_user(acc.id).with_org(org.id)
}

#[tokio::test]
async fn versions_accumulate_and_read_returns_current() {
	let db = TmpDb::new("versions");
	let env = setup(&db).await;
	let (acc, org) = account(&env, "a@e.st").await;
	let c = ctx(&acc, &org);

	env.memory.write(&c, "account:x", "summary.md", "one", None).await.unwrap();
	env.memory.write(&c, "account:x", "summary.md", "two", None).await.unwrap();
	let v3 = env
		.memory
		.append(&c, "account:x", "summary.md", " more", Some("run_1"))
		.await
		.unwrap();
	assert_eq!(v3.version, 3);
	assert_eq!(v3.body, "two more");

	let cur = env.memory.read(&c, "account:x", "summary.md", None).await.unwrap();
	assert_eq!((cur.version, cur.body.as_str()), (3, "two more"));
	let old = env.memory.read(&c, "account:x", "summary.md", Some(1)).await.unwrap();
	assert_eq!(old.body, "one");
	assert_eq!(old.author, acc.uid.to_string());

	let hist = env.memory.history(&c, "account:x", "summary.md").await.unwrap();
	assert_eq!(hist.iter().map(|v| v.version).collect::<Vec<_>>(), [1, 2, 3]);
	assert_eq!(hist[2].author, "run_1");
	assert_eq!(env.memory.list(&c, "account:x").await.unwrap().len(), 1);

	assert!(env.memory.write(&c, "account:x", "../x", "b", None).await.is_err());
	assert!(env.memory.write(&c, "nokind", "a.md", "b", None).await.is_err());
	assert!(env.memory.write(&c, "account:x", "a.md", "b", Some("usr_1")).await.is_err());
}

#[tokio::test]
async fn another_orgs_space_is_notfound() {
	let db = TmpDb::new("cross-org");
	let env = setup(&db).await;
	let (a, org_a) = account(&env, "a@e.st").await;
	let (b, org_b) = account(&env, "b@e.st").await;

	env.memory
		.write(&ctx(&a, &org_a), "project:p", "doc.md", "secret", None)
		.await
		.unwrap();
	let cb = ctx(&b, &org_b);
	let err = env.memory.read(&cb, "project:p", "doc.md", None).await.unwrap_err();
	assert!(matches!(err, Error::NotFound), "{err:?}");
	assert!(matches!(env.memory.history(&cb, "project:p", "doc.md").await, Err(Error::NotFound)));
	assert!(env.memory.list(&cb, "project:p").await.unwrap().is_empty());
	assert!(env.memory.search(&cb, None, "secret", 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn search_hits_only_current_bodies() {
	let db = TmpDb::new("search");
	let env = setup(&db).await;
	let (acc, org) = account(&env, "a@e.st").await;
	let c = ctx(&acc, &org);

	env.memory.write(&c, "account:x", "a.md", "alpha bravo", None).await.unwrap();
	env.memory.write(&c, "account:x", "a.md", "charlie delta", None).await.unwrap();
	env.memory.write(&c, "project:p", "b.md", "charlie echo", None).await.unwrap();

	assert!(env.memory.search(&c, None, "alpha", 10).await.unwrap().is_empty());
	let hits = env.memory.search(&c, None, "charlie", 10).await.unwrap();
	assert_eq!(hits.len(), 2);
	let hits = env.memory.search(&c, Some("account:x"), "charlie", 10).await.unwrap();
	assert_eq!((hits[0].path.as_str(), hits[0].version), ("a.md", 2));
	assert!(
		env.memory
			.search(&c, Some("account:none"), "charlie", 10)
			.await
			.unwrap()
			.is_empty()
	);
}

#[tokio::test]
async fn erase_removes_the_personal_org_only() {
	let db = TmpDb::new("erase");
	let env = setup(&db).await;
	let (acc, personal) = account(&env, "a@e.st").await;
	let (other, other_org) = account(&env, "b@e.st").await;
	let shared = env
		.core
		.create_org(OrgKind::Shared, env.core.root_org_id().await.unwrap(), "Kft.", other.id, None)
		.await
		.unwrap();

	env.memory
		.write(&ctx(&acc, &personal), "account:x", "a.md", "mine", None)
		.await
		.unwrap();
	env.memory
		.write(&ctx(&acc, &shared), "project:p", "b.md", "theirs", None)
		.await
		.unwrap();
	env.memory
		.write(&ctx(&other, &other_org), "account:y", "c.md", "keep", None)
		.await
		.unwrap();

	let export = env.memory.export(&acc.uid).await.unwrap();
	assert_eq!(export["spaces"][0]["key"], "account:x");
	assert_eq!(export["spaces"][0]["docs"][0]["versions"][0]["body"], "mine");
	assert_eq!(export["spaces"].as_array().unwrap().len(), 1);

	env.memory.erase(&acc.uid).await.unwrap();
	env.memory.erase(&acc.uid).await.unwrap(); // idempotent

	let org = personal.uid.to_string();
	assert!(env.app.spaces_list(&org).await.unwrap().is_empty());
	assert!(env.app.search(&org, None, "mine", 10).await.unwrap().is_empty());
	let c = ctx(&acc, &shared);
	assert_eq!(env.memory.read(&c, "project:p", "b.md", None).await.unwrap().body, "theirs");
	let c = ctx(&other, &other_org);
	assert_eq!(env.memory.read(&c, "account:y", "c.md", None).await.unwrap().body, "keep");
}

// vim: ts=4
