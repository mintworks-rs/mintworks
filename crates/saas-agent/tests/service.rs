//! `Agent` over the real stores: the core DB for orgs, the app DB for threads. Covers the org
//! confinement of the thread reads; runs need the pool and live in the Rune suites.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{path::PathBuf, sync::Arc};

use appdb_adapter_sqlite::{AGENT, SqliteAppDb};
use saas_agent::{
	Agent,
	store::{NewMessage, ThreadStore},
};
use saas_auth::store::{Account, AuthStore, NewAccount, Org};
use saas_core::{App, Ctx, Error, config::Config};
use store_adapter_sqlite::SqliteStore;

struct TmpDb(PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("saas-agent-{}-{name}", std::process::id()));
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

async fn app(db: &TmpDb) -> (App, Arc<SqliteStore>, Arc<dyn ThreadStore>) {
	let config = Config {
		master_key: [0; 32],
		db_path: db.0.join("core.db").to_string_lossy().into_owned(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	let core = SqliteStore::open(&config).await.unwrap();
	core.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	let core = Arc::new(core);
	let appdb = SqliteAppDb::new(db.0.join("app.db"));
	appdb.migrate(&[AGENT]).await.unwrap();
	let threads: Arc<dyn ThreadStore> = Arc::new(appdb);
	let app = saas_core::AppBuilder::new()
		.config(config)
		.store(core.clone() as Arc<dyn saas_core::store::CoreStore>)
		.extension(core.clone() as Arc<dyn AuthStore>)
		.extension(threads.clone())
		.build()
		.await
		.unwrap();
	(app, core, threads)
}

async fn account(core: &SqliteStore, email: &str) -> Ctx {
	let new = NewAccount {
		email: email.to_owned(),
		pwd_hash: None,
		name: None,
		locale: "en".to_owned(),
		org_name: email.to_owned(),
	};
	let (acc, org): (Account, Org) = core.create_account(&new, &[]).await.unwrap();
	Ctx::system("test").as_user(acc.id).with_org(org.id)
}

#[tokio::test]
async fn threads_are_newest_first_and_confined_to_the_org() {
	let db = TmpDb::new("threads");
	let (app, core, _) = app(&db).await;
	let (a, b) = (account(&core, "a@e.st").await, account(&core, "b@e.st").await);
	let agent = Agent::new(app);

	let first = agent.create_thread(&a, Some("s"), Some("first")).await.unwrap();
	let second = agent.create_thread(&a, None, None).await.unwrap();
	let listed: Vec<_> = agent.threads(&a).await.unwrap().into_iter().map(|t| t.uid).collect();
	assert_eq!(listed, [second.uid, first.uid.clone()]);
	assert_eq!(first.title.as_deref(), Some("first"));
	assert!(agent.threads(&b).await.unwrap().is_empty());
}

#[tokio::test]
async fn another_orgs_messages_are_notfound() {
	let db = TmpDb::new("messages");
	let (app, core, threads) = app(&db).await;
	let (a, b) = (account(&core, "a@e.st").await, account(&core, "b@e.st").await);
	let agent = Agent::new(app);

	let t = agent.create_thread(&a, None, None).await.unwrap();
	let msg = NewMessage { role: "user", content: "hi", tool_calls: None, tool_call_id: None };
	threads.messages_append(t.id, &[msg], None, 1).await.unwrap();

	let got = agent.messages(&a, t.uid.as_str()).await.unwrap();
	assert_eq!(got.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["hi"]);
	let err = agent.messages(&b, t.uid.as_str()).await.unwrap_err();
	assert!(matches!(err, Error::NotFound), "{err:?}");
	let err = agent.messages(&a, "thr_nope").await.unwrap_err();
	assert!(matches!(err, Error::NotFound), "{err:?}");
}

// vim: ts=4
