//! The two things about `STEPS` that only a real database can answer, and that no other test
//! covers: an **already-migrated** database must take exactly the newly appended steps and no
//! checksum may move (editing an applied step is a hard startup failure), and the composition
//! the module doc promises — `store.migrate(&[MY_STEPS, STEPS].concat())`, a consumer step
//! ahead of the framework baseline — must actually boot. The second used to die with
//! `no such table: migrations`; `migrate.rs`'s own `mod tests` pins the mechanism, this
//! pins it against the real `STEPS`.
//!
//! Append a step here too whenever `STEPS` grows: the counts are deliberately exact, so a new
//! step that forgets its ledger entry fails loudly.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_core::config::Config;
use store_adapter_sqlite::{SqliteStore, Step};

/// A temp directory that takes the database with it — the same shape every other suite
/// copies. The PID is in the name because two `cargo test` runs overlap routinely (CI beside
/// a local run, or two worktrees of this repo) and `new` wipes the directory up front, so a
/// fixed path meant the second run deleted the first's live database mid-test.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-migration-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [7; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: String::new(),
			listen: String::new(),
			base_url: String::new(),
			jobs_workers: None,
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

#[tokio::test]
async fn an_existing_database_takes_exactly_the_new_steps() {
	let db = TmpDb::new("upgrade-check");
	let store = SqliteStore::open(&db.config()).await.unwrap();

	// A database at an earlier baseline: the first two steps.
	let old = &store_adapter_sqlite::STEPS[..2];
	store.migrate(old).await.unwrap();
	let before: i64 = sqlx::query_scalar("SELECT count(*) FROM migrations")
		.fetch_one(store.reader())
		.await
		.unwrap();
	assert_eq!(before, 2);

	// Upgrading applies exactly the appended tail, in order, and no checksum moves.
	store.migrate(store_adapter_sqlite::STEPS).await.unwrap();
	let names: Vec<String> = sqlx::query_scalar("SELECT name FROM migrations ORDER BY idx")
		.fetch_all(store.reader())
		.await
		.unwrap();
	// Against `STEPS` rather than a copied list: appending a corrective step is routine and
	// must not make this test a second place to edit.
	let expected: Vec<&str> = store_adapter_sqlite::STEPS.iter().map(|s| s.name).collect();
	assert_eq!(names, expected, "{names:?}");
}

#[tokio::test]
async fn a_consumer_step_ahead_of_the_baseline_boots() {
	const MY_STEPS: &[Step] = &[Step {
		name: "myapp/init",
		sql: "CREATE TABLE mine (id INTEGER PRIMARY KEY, note TEXT);",
	}];

	let db = TmpDb::new("consumer-first-check");
	let store = SqliteStore::open(&db.config()).await.unwrap();

	store
		.migrate(&[MY_STEPS, store_adapter_sqlite::STEPS].concat())
		.await
		.expect("the contract promises this composition boots");

	sqlx::query("INSERT INTO mine (note) VALUES ('ok')")
		.execute(store.writer())
		.await
		.unwrap();
	let n: i64 = sqlx::query_scalar("SELECT count(*) FROM migrations")
		.fetch_one(store.reader())
		.await
		.unwrap();
	assert_eq!(usize::try_from(n).unwrap(), store_adapter_sqlite::STEPS.len() + 1);
}

/// Name uniqueness is what replaced positional order as a step's identity, and it was
/// unchecked: on an *existing* database a consumer step colliding with an applied framework
/// name was filtered out as already-applied, so its DDL never ran while the ledger claimed it
/// had. (A fresh database fails loudly on `migrations.name`'s UNIQUE, so only upgrades were
/// silent.)
#[tokio::test]
async fn a_duplicate_step_name_is_refused_rather_than_silently_skipped() {
	const CLASH: &[Step] =
		&[Step { name: "saas-core/init", sql: "CREATE TABLE mine (id INTEGER PRIMARY KEY);" }];

	let db = TmpDb::new("duplicate-name-check");
	let store = SqliteStore::open(&db.config()).await.unwrap();
	store.migrate(store_adapter_sqlite::STEPS).await.unwrap();

	let err = store
		.migrate(&[store_adapter_sqlite::STEPS, CLASH].concat())
		.await
		.expect_err("a repeated step name must not report success");
	assert!(err.to_string().contains("saas-core/init"), "{err}");

	let mine: Option<String> =
		sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'mine'")
			.fetch_optional(store.reader())
			.await
			.unwrap();
	assert!(mine.is_none(), "the colliding step must not have run either");
}

// vim: ts=4
