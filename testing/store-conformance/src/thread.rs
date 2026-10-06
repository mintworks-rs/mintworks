//! `ThreadStore` conformance: what every app-DB adapter must reproduce, driven through the trait
//! alone. Org confinement lives in `saas_agent::Agent`, not here.

use saas_agent::store::{NewMessage, ThreadStore};
use saas_core::prelude::ThreadId;

use crate::{AppDbHarness, TestModule, fresh_db};

pub async fn a_title_round_trips<H: AppDbHarness>()
where
	H::Db: ThreadStore,
{
	let h = fresh_db!(H, "thread-title");
	let db = h.db();
	H::migrate(db, &[TestModule::Agent]).await.unwrap();
	let t = db.thread_create("tnt_a", None, Some("Why is the sky blue?")).await.unwrap();
	assert_eq!(t.title.as_deref(), Some("Why is the sky blue?"));
	assert_eq!(db.thread_get(&t.uid).await.unwrap(), Some(t));
}

const fn msg<'a>(role: &'a str, content: &'a str) -> NewMessage<'a> {
	NewMessage { role, content, tool_calls: None, tool_call_id: None }
}

pub async fn threads_are_created_read_and_listed_per_org<H: AppDbHarness>()
where
	H::Db: ThreadStore,
{
	let h = fresh_db!(H, "thread-threads");
	let db = h.db();
	H::migrate(db, &[TestModule::Agent]).await.unwrap();
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

pub async fn append_updates_the_thread_and_keeps_order<H: AppDbHarness>()
where
	H::Db: ThreadStore,
{
	let h = fresh_db!(H, "thread-append");
	let db = h.db();
	H::migrate(db, &[TestModule::Agent]).await.unwrap();
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

pub async fn compact_hides_old_messages_but_keeps_them<H: AppDbHarness>()
where
	H::Db: ThreadStore,
{
	let h = fresh_db!(H, "thread-compact");
	let db = h.db();
	H::migrate(db, &[TestModule::Agent]).await.unwrap();
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

pub async fn org_erase_removes_only_that_org<H: AppDbHarness>()
where
	H::Db: ThreadStore,
{
	let h = fresh_db!(H, "thread-erase");
	let db = h.db();
	H::migrate(db, &[TestModule::Agent]).await.unwrap();
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
