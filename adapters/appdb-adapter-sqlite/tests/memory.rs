//! `MemoryStore` conformance: what a second app-DB adapter must reproduce, driven through the
//! trait alone. Service-level rules (scoping, path/key validation) live in
//! `crates/saas-memory/tests/memory.rs`.

#![cfg(feature = "ai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use appdb_adapter_sqlite::{MEMORY, SqliteAppDb};
use saas_memory::{MemoryStore, NewVersion, WriteMode};

struct TmpDb(PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("appdb-mem-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	async fn open(&self) -> SqliteAppDb {
		let db = SqliteAppDb::new(self.0.join("app.db"));
		db.migrate(&[MEMORY]).await.unwrap();
		db
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

fn new<'a>(org: &'a str, key: &'a str, path: &'a str, body: &'a str) -> NewVersion<'a> {
	NewVersion {
		org,
		space_key: key,
		path,
		body,
		author: "acc_author",
		pdf_sha256: None,
		mode: WriteMode::Replace,
	}
}

#[tokio::test]
async fn spaces_and_docs_are_unique_per_owner() {
	let tmp = TmpDb::new("unique");
	let db = tmp.open().await;
	db.version_write(&new("org_a", "project:p1", "a.md", "one")).await.unwrap();
	db.version_write(&new("org_a", "project:p1", "a.md", "two")).await.unwrap();
	db.version_write(&new("org_a", "project:p1", "b.md", "three")).await.unwrap();
	db.version_write(&new("org_b", "project:p1", "a.md", "four")).await.unwrap();

	let a = db.spaces_list("org_a").await.unwrap();
	assert_eq!(a.len(), 1);
	let b = db.space_get("org_b", "project:p1").await.unwrap().unwrap();
	assert_ne!(a[0].id, b.id);

	let docs = db.docs_list(a[0].id).await.unwrap();
	assert_eq!(docs.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(), ["a.md", "b.md"]);
	assert_eq!(docs[0].version, 2);
	assert_eq!(db.docs_list(b.id).await.unwrap().len(), 1);
	assert!(db.space_get("org_a", "project:p2").await.unwrap().is_none());
}

#[tokio::test]
async fn versions_are_immutable_and_append_concatenates() {
	let tmp = TmpDb::new("immutable");
	let db = tmp.open().await;
	let v1 = db
		.version_write(&new("org_a", "account:acc_1", "notes.md", "first"))
		.await
		.unwrap();
	assert_eq!(v1.version, 1);
	let v2 = db
		.version_write(&new("org_a", "account:acc_1", "notes.md", "second"))
		.await
		.unwrap();
	let v3 = db
		.version_write(&NewVersion {
			mode: WriteMode::Append,
			author: "run_1",
			..new("org_a", "account:acc_1", "notes.md", "+more")
		})
		.await
		.unwrap();
	assert_eq!((v2.version, v3.version), (2, 3));
	assert_eq!(v3.body, "second+more");
	assert_eq!(v3.author, "run_1");

	let all = db.versions_list(v1.doc_id).await.unwrap();
	assert_eq!(
		all.iter().map(|v| v.body.as_str()).collect::<Vec<_>>(),
		["first", "second", "second+more"]
	);
	assert_eq!(all[0], v1);
	assert_eq!(db.version_get(v1.doc_id, None).await.unwrap().unwrap(), v3);
	assert_eq!(db.version_get(v1.doc_id, Some(1)).await.unwrap().unwrap(), v1);
	assert!(db.version_get(v1.doc_id, Some(9)).await.unwrap().is_none());
}

#[tokio::test]
async fn search_sees_only_current_bodies_in_scope() {
	let tmp = TmpDb::new("search");
	let db = tmp.open().await;
	db.version_write(&new("org_a", "project:p1", "a.md", "the old zebra"))
		.await
		.unwrap();
	db.version_write(&new("org_a", "project:p1", "a.md", "the new giraffe"))
		.await
		.unwrap();
	db.version_write(&new("org_a", "project:p2", "b.md", "another giraffe"))
		.await
		.unwrap();
	db.version_write(&new("org_b", "project:p1", "c.md", "foreign giraffe"))
		.await
		.unwrap();

	assert!(db.search("org_a", None, "zebra", 10).await.unwrap().is_empty());
	let hits = db.search("org_a", None, "giraffe", 10).await.unwrap();
	assert_eq!(hits.len(), 2);
	assert!(hits.iter().all(|h| h.snippet.contains("**giraffe**")));

	let p1 = db.space_get("org_a", "project:p1").await.unwrap().unwrap();
	let hits = db.search("org_a", Some(p1.id), "giraffe", 10).await.unwrap();
	assert_eq!(hits.len(), 1);
	assert_eq!((hits[0].path.as_str(), hits[0].version), ("a.md", 2));

	// Words are ANDed, and query-language syntax is plain text rather than an error.
	assert!(db.search("org_a", None, "new another", 10).await.unwrap().is_empty());
	db.search("org_a", None, "\"giraffe AND (", 10).await.unwrap();
	assert!(db.search("org_a", None, "   ", 10).await.unwrap().is_empty());
	assert_eq!(db.search("org_a", None, "giraffe", 1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn org_erase_removes_only_that_org() {
	let tmp = TmpDb::new("erase");
	let db = tmp.open().await;
	let gone = db
		.version_write(&new("org_a", "account:acc_1", "a.md", "secret words"))
		.await
		.unwrap();
	db.version_write(&new("org_a", "project:p1", "b.md", "secret words"))
		.await
		.unwrap();
	db.version_write(&new("org_b", "project:p1", "c.md", "secret words"))
		.await
		.unwrap();

	assert_eq!(db.org_erase("org_a").await.unwrap(), 2);
	assert!(db.spaces_list("org_a").await.unwrap().is_empty());
	assert!(db.versions_list(gone.doc_id).await.unwrap().is_empty());
	assert!(db.search("org_a", None, "secret", 10).await.unwrap().is_empty());
	assert_eq!(db.search("org_b", None, "secret", 10).await.unwrap().len(), 1);
	assert_eq!(db.org_erase("org_a").await.unwrap(), 0);

	// A doc re-created after the erase starts a fresh chain and is indexed again.
	let v = db
		.version_write(&new("org_a", "account:acc_1", "a.md", "secret again"))
		.await
		.unwrap();
	assert_eq!(v.version, 1);
	assert_eq!(db.search("org_a", None, "again", 10).await.unwrap().len(), 1);
}

// vim: ts=4
