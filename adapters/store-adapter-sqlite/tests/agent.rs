//! `AgentRunStore` conformance tests — what a second store adapter must pass: one live run per
//! thread, per-run event sequencing, status stamps and the lease sweep.
//!
//! Every test opens a real file database, per `tests/objects.rs`.

#![cfg(feature = "ai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_agent::store::{AgentRunStore, EventKind, NewRun, Run, RunStatus};
use saas_core::config::Config;
use saas_core::prelude::{RunId, ThreadId, Timestamp};
use store_adapter_sqlite::SqliteStore;

struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-agent-store-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn setup(db: &TmpDb) -> SqliteStore {
	let store = SqliteStore::open(&Config {
		master_key: [0; 32],
		db_path: db.0.join("test.db").to_string_lossy().into_owned(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	})
	.await
	.unwrap();
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	store
}

async fn insert(store: &SqliteStore, thread: &ThreadId) -> Option<Run> {
	let uid = RunId::generate();
	store
		.run_insert(&NewRun {
			uid: &uid,
			thread,
			org_id: 1,
			account_id: Some(7),
			role: "USER",
			spec: "{}",
		})
		.await
		.unwrap()
}

#[tokio::test]
async fn one_live_run_per_thread() {
	let db = TmpDb::new("d2");
	let store = setup(&db).await;
	let thread = ThreadId::from_trusted("thr_a".to_owned());

	let run = insert(&store, &thread).await.unwrap();
	assert_eq!(run.status, RunStatus::Queued);
	assert!(insert(&store, &thread).await.is_none(), "a second live run on the thread");
	assert!(insert(&store, &ThreadId::from_trusted("thr_b".to_owned())).await.is_some());

	store.run_set_status(run.id, RunStatus::Done, None).await.unwrap();
	assert!(insert(&store, &thread).await.is_some(), "a finished run frees the thread");
}

#[tokio::test]
async fn status_stamps_start_and_finish() {
	let db = TmpDb::new("status");
	let store = setup(&db).await;
	let run = insert(&store, &ThreadId::from_trusted("thr_a".to_owned())).await.unwrap();
	assert!(run.started_at.is_none() && run.finished_at.is_none());

	store.run_set_status(run.id, RunStatus::Running, None).await.unwrap();
	let got = store.run_get(&run.uid).await.unwrap().unwrap();
	assert_eq!(got.status, RunStatus::Running);
	assert!(got.started_at.is_some() && got.finished_at.is_none());

	store.run_set_status(run.id, RunStatus::Error, Some("boom")).await.unwrap();
	let got = store.run_get(&run.uid).await.unwrap().unwrap();
	assert_eq!(got.status, RunStatus::Error);
	assert_eq!(got.error.as_deref(), Some("boom"));
	assert!(got.finished_at.is_some());
	assert_eq!(got.started_at, store.run_get(&run.uid).await.unwrap().unwrap().started_at);
}

#[tokio::test]
async fn events_are_sequenced_per_run() {
	let db = TmpDb::new("events");
	let store = setup(&db).await;
	let a = insert(&store, &ThreadId::from_trusted("thr_a".to_owned())).await.unwrap();
	let b = insert(&store, &ThreadId::from_trusted("thr_b".to_owned())).await.unwrap();

	assert_eq!(store.event_append(a.id, EventKind::Queued, "{}").await.unwrap(), 1);
	assert_eq!(store.event_append(b.id, EventKind::Delta, r#"{"text":"x"}"#).await.unwrap(), 1);
	assert_eq!(store.event_append(a.id, EventKind::Delta, r#"{"text":"y"}"#).await.unwrap(), 2);
	assert_eq!(store.event_append(a.id, EventKind::Done, r#"{"status":"done"}"#).await.unwrap(), 3);

	let all = store.events_after(a.id, 0).await.unwrap();
	let kinds: Vec<_> = all.iter().map(|e| (e.seq, e.kind)).collect();
	assert_eq!(kinds, [(1, EventKind::Queued), (2, EventKind::Delta), (3, EventKind::Done)]);
	assert_eq!(all[1].payload, r#"{"text":"y"}"#);

	let tail = store.events_after(a.id, 2).await.unwrap();
	assert_eq!(tail.len(), 1);
	assert_eq!(tail[0].kind, EventKind::Done);
}

#[tokio::test]
async fn sweep_interrupts_only_live_runs() {
	let db = TmpDb::new("sweep");
	let store = setup(&db).await;
	let queued = insert(&store, &ThreadId::from_trusted("thr_a".to_owned())).await.unwrap();
	let running = insert(&store, &ThreadId::from_trusted("thr_b".to_owned())).await.unwrap();
	let done = insert(&store, &ThreadId::from_trusted("thr_c".to_owned())).await.unwrap();
	store.run_set_status(running.id, RunStatus::Running, None).await.unwrap();
	store.run_set_status(done.id, RunStatus::Done, None).await.unwrap();

	let future = Timestamp(Timestamp::now().0 + 1);
	let mut swept: Vec<_> = store
		.runs_interrupt_stale(future)
		.await
		.unwrap()
		.into_iter()
		.map(|r| r.id)
		.collect();
	swept.sort_unstable();
	assert_eq!(swept, [queued.id, running.id]);
	for (run, want) in [
		(&queued, RunStatus::Interrupted),
		(&running, RunStatus::Interrupted),
		(&done, RunStatus::Done),
	] {
		assert_eq!(store.run_get(&run.uid).await.unwrap().unwrap().status, want);
	}
	assert!(store.runs_interrupt_stale(future).await.unwrap().is_empty());
}

#[tokio::test]
async fn sweep_takes_only_expired_leases() {
	let db = TmpDb::new("lease");
	let store = setup(&db).await;
	let fresh = insert(&store, &ThreadId::from_trusted("thr_a".to_owned())).await.unwrap();
	let old = insert(&store, &ThreadId::from_trusted("thr_b".to_owned())).await.unwrap();
	let now = Timestamp::now().0;
	store.runs_heartbeat(&[old.id], Timestamp(now - 300)).await.unwrap();

	let swept = store.runs_interrupt_stale(Timestamp(now - 120)).await.unwrap();
	assert_eq!(swept.iter().map(|r| r.id).collect::<Vec<_>>(), [old.id]);
	assert_eq!(store.run_get(&fresh.uid).await.unwrap().unwrap().status, RunStatus::Queued);

	// The owner, still running, must not overwrite the sweep's verdict.
	assert!(!store.run_set_status(old.id, RunStatus::Done, None).await.unwrap());
	assert_eq!(store.run_get(&old.uid).await.unwrap().unwrap().status, RunStatus::Interrupted);
	assert!(store.run_set_status(fresh.id, RunStatus::Running, None).await.unwrap());
}

// vim: ts=4
