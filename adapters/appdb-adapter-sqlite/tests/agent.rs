//! `ThreadStore` conformance: what a second app-DB adapter must reproduce, driven through the
//! trait alone. Org confinement lives in `saas_agent::Agent`, not here.

#![cfg(feature = "ai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use appdb_adapter_sqlite::{AGENT, Fut, Module, SqliteAppDb};
use saas_agent::store::{NewMessage, ThreadStore};
use saas_core::prelude::ThreadId;
use sqlx::SqliteConnection;

struct TmpDb(PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("appdb-agent-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	async fn open(&self) -> SqliteAppDb {
		let db = SqliteAppDb::new(self.0.join("app.db"));
		db.migrate(&[AGENT]).await.unwrap();
		db
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

#[tokio::test]
async fn a_title_round_trips() {
	let tmp = TmpDb::new("title");
	let db = tmp.open().await;
	let t = db.thread_create("tnt_a", None, Some("Why is the sky blue?")).await.unwrap();
	assert_eq!(t.title.as_deref(), Some("Why is the sky blue?"));
	assert_eq!(db.thread_get(&t.uid).await.unwrap(), Some(t));
}

/// The `agent` module as it shipped at version 1: `agent_threads` without `title`.
fn agent_v1(conn: &mut SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		for sql in [
			"CREATE TABLE agent_threads (id INTEGER PRIMARY KEY, uid TEXT NOT NULL UNIQUE, \
			 org TEXT NOT NULL, subject TEXT, summary TEXT, token_estimate INTEGER NOT NULL \
			 DEFAULT 0, last_model TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)",
			"INSERT INTO agent_threads (uid, org, created_at, updated_at) \
			 VALUES ('thr_old', 'tnt_a', 1, 1)",
		] {
			sqlx::query(sql).execute(&mut *conn).await.unwrap();
		}
		Ok(())
	})
}

#[tokio::test]
async fn v1_upgrades_to_titled_threads() {
	let tmp = TmpDb::new("v1-upgrade");
	let db = SqliteAppDb::new(tmp.0.join("app.db"));
	db.migrate(&[Module { name: "agent", version: 1, apply: agent_v1 }])
		.await
		.unwrap();
	db.migrate(&[AGENT]).await.unwrap();

	let old = db
		.thread_get(&ThreadId::from_trusted("thr_old".to_owned()))
		.await
		.unwrap()
		.unwrap();
	assert_eq!(old.title, None);
	let t = db.thread_create("tnt_a", None, Some("new")).await.unwrap();
	assert_eq!(db.thread_get(&t.uid).await.unwrap().unwrap().title.as_deref(), Some("new"));
}

const fn msg<'a>(role: &'a str, content: &'a str) -> NewMessage<'a> {
	NewMessage { role, content, tool_calls: None, tool_call_id: None }
}

#[tokio::test]
async fn threads_are_created_read_and_listed_per_org() {
	let tmp = TmpDb::new("threads");
	let db = tmp.open().await;
	let a = db.thread_create("tnt_a", Some("first"), None).await.unwrap();
	let b = db.thread_create("tnt_a", None, None).await.unwrap();
	db.thread_create("tnt_b", None, None).await.unwrap();

	assert!(a.uid.as_str().starts_with("thr_"));
	assert_eq!(a.subject.as_deref(), Some("first"));
	assert_eq!(db.thread_get(&a.uid).await.unwrap(), Some(a.clone()));
	assert!(
		db.thread_get(&ThreadId::from_trusted("thr_none".to_owned()))
			.await
			.unwrap()
			.is_none()
	);

	let mut listed: Vec<_> =
		db.threads_list("tnt_a").await.unwrap().into_iter().map(|t| t.id).collect();
	listed.sort_unstable();
	assert_eq!(listed, [a.id, b.id]);
	assert!(db.threads_list("tnt_none").await.unwrap().is_empty());
}

#[tokio::test]
async fn append_updates_the_thread_and_keeps_order() {
	let tmp = TmpDb::new("append");
	let db = tmp.open().await;
	let t = db.thread_create("tnt_a", None, None).await.unwrap();

	let call = NewMessage {
		role: "assistant",
		content: "",
		tool_calls: Some(r#"[{"id":"c1","name":"x","arguments":"{}"}]"#),
		tool_call_id: None,
	};
	let result =
		NewMessage { role: "tool", content: "ok", tool_calls: None, tool_call_id: Some("c1") };
	db.messages_append(t.id, &[msg("user", "hi"), call, result], Some("fake:demo"), 42)
		.await
		.unwrap();

	let msgs = db.messages_live(t.id).await.unwrap();
	let roles: Vec<_> = msgs.iter().map(|m| m.role.as_str()).collect();
	assert_eq!(roles, ["user", "assistant", "tool"]);
	assert!(msgs[1].tool_calls.is_some());
	assert_eq!(msgs[2].tool_call_id.as_deref(), Some("c1"));

	let t = db.thread_get(&t.uid).await.unwrap().unwrap();
	assert_eq!(t.last_model.as_deref(), Some("fake:demo"));
	assert_eq!(t.token_estimate, 42);
}

#[tokio::test]
async fn compact_hides_old_messages_but_keeps_them() {
	let tmp = TmpDb::new("compact");
	let db = tmp.open().await;
	let t = db.thread_create("tnt_a", None, None).await.unwrap();
	db.messages_append(
		t.id,
		&[msg("user", "1"), msg("assistant", "2"), msg("user", "3")],
		None,
		30,
	)
	.await
	.unwrap();
	let all = db.messages_all(t.id).await.unwrap();

	db.compact(t.id, "summary", all[1].id, 10).await.unwrap();
	let live = db.messages_live(t.id).await.unwrap();
	assert_eq!(live.len(), 1);
	assert_eq!(live[0].content, "3");
	let all = db.messages_all(t.id).await.unwrap();
	assert_eq!(all.len(), 3);
	assert!(all[0].compacted && all[1].compacted && !all[2].compacted);

	let t = db.thread_get(&t.uid).await.unwrap().unwrap();
	assert_eq!(t.summary.as_deref(), Some("summary"));
	assert_eq!(t.token_estimate, 10);
}

#[tokio::test]
async fn org_erase_removes_only_that_org() {
	let tmp = TmpDb::new("erase");
	let db = tmp.open().await;
	let gone = db.thread_create("tnt_a", None, None).await.unwrap();
	let kept = db.thread_create("tnt_b", None, None).await.unwrap();
	db.messages_append(gone.id, &[msg("user", "x")], None, 1).await.unwrap();
	db.messages_append(kept.id, &[msg("user", "y")], None, 1).await.unwrap();

	assert!(db.org_erase("tnt_a").await.unwrap() > 0);
	assert!(db.thread_get(&gone.uid).await.unwrap().is_none());
	assert!(db.messages_all(gone.id).await.unwrap().is_empty());
	assert_eq!(db.messages_all(kept.id).await.unwrap().len(), 1);
	assert_eq!(db.org_erase("tnt_a").await.unwrap(), 0, "idempotent");
}

// vim: ts=4
