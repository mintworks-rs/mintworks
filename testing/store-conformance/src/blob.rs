// SPDX-License-Identifier: MPL-2.0
//! `blobs`/`blob_refs` conformance: claims and releases are idempotent per holder, the last
//! release starts the grace period, and the sweep takes only what no holder claims.

use mintworks_core::error::Error;
use mintworks_core::ids::{AccountId, BlobId};
use mintworks_core::store::{Blob, CoreStore};
use mintworks_core::types::Timestamp;
use serde_json::json;

use crate::{Harness, fresh};

async fn put<H: Harness>(h: &H, sha: &str, created_by: Option<i64>, at: i64) -> (i64, BlobId)
where
	H::Store: CoreStore,
{
	let store = h.store();
	let uid = BlobId::generate();
	let row = Blob {
		id: 0,
		uid: uid.clone(),
		org_id: store.root_org_id().await.unwrap(),
		sha256: sha.to_owned(),
		mime: "text/plain".to_owned(),
		size: 3,
		name: Some("a.txt".to_owned()),
		created_by,
		created_at: Timestamp(at),
		unref_at: Some(Timestamp(at)),
	};
	(store.blob_insert(&row, i64::MAX).await.unwrap().unwrap(), uid)
}

async fn unref_at<H: Harness>(h: &H, uid: &BlobId) -> Option<Timestamp>
where
	H::Store: CoreStore,
{
	h.store().blob_get(uid).await.unwrap().unwrap().unref_at
}

/// A repeated claim by one holder is one row; the first claim clears `unref_at`.
pub async fn a_claim_is_idempotent_per_holder<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-claim");
	let store = h.store();
	h.exec("INSERT INTO accounts (uid, email, created_at) VALUES ('acc_blob', 'b@e.st', 0)", &[])
		.await;
	let acc = h
		.scalar_i64("SELECT id FROM accounts WHERE uid = ?", &[json!("acc_blob")])
		.await;
	let (id, uid) = put(&h, "aa11", Some(acc), 100).await;
	assert_eq!(unref_at(&h, &uid).await, Some(Timestamp(100)));

	store.blob_claim(id, "lesson:1", None, Timestamp(110)).await.unwrap();
	store.blob_claim(id, "lesson:1", None, Timestamp(120)).await.unwrap();
	assert_eq!(store.blob_refs(id).await.unwrap(), ["lesson:1"]);
	assert_eq!(unref_at(&h, &uid).await, None);
	assert!(store.blob_sweep(Timestamp(i64::MAX), 10).await.unwrap().is_empty());

	let mine = store
		.blobs_by_creator(&AccountId::from_trusted("acc_blob".into()))
		.await
		.unwrap();
	assert_eq!(mine.len(), 1);
	assert_eq!(mine[0].uid, uid);
}

/// Releasing stamps `unref_at` once (a retry keeps the first stamp), and a re-claim within the
/// grace period saves the blob from the sweep.
pub async fn release_then_reclaim_survives_the_sweep<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-reclaim");
	let store = h.store();
	let (id, uid) = put(&h, "bb22", None, 100).await;
	store.blob_claim(id, "a", None, Timestamp(110)).await.unwrap();
	store.blob_release(id, "a", None, Timestamp(200)).await.unwrap();
	store.blob_release(id, "a", None, Timestamp(300)).await.unwrap();
	assert!(store.blob_refs(id).await.unwrap().is_empty());
	assert_eq!(unref_at(&h, &uid).await, Some(Timestamp(200)));
	assert!(store.blob_sweep(Timestamp(200), 10).await.unwrap().is_empty());

	store.blob_claim(id, "a", None, Timestamp(250)).await.unwrap();
	assert_eq!(unref_at(&h, &uid).await, None);
	assert!(store.blob_sweep(Timestamp(i64::MAX), 10).await.unwrap().is_empty());
	assert!(store.blob_get(&uid).await.unwrap().is_some());
}

/// With two holders the first release leaves the blob held; the second makes it sweepable.
/// The sweep returns the sha, which stays in use while another upload of the same bytes lives.
pub async fn a_two_holder_release_waits_for_the_last<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-two");
	let store = h.store();
	let (id, uid) = put(&h, "cc33", None, 100).await;
	let (twin, _) = put(&h, "cc33", None, 100).await;
	store.blob_claim(twin, "keep", None, Timestamp(105)).await.unwrap();
	store.blob_claim(id, "a", None, Timestamp(110)).await.unwrap();
	store.blob_claim(id, "b", None, Timestamp(110)).await.unwrap();
	assert_eq!(store.blob_refs(id).await.unwrap(), ["a", "b"]);

	store.blob_release(id, "a", None, Timestamp(200)).await.unwrap();
	assert_eq!(unref_at(&h, &uid).await, None);
	assert_eq!(store.blob_refs(id).await.unwrap(), ["b"]);
	store.blob_release(id, "b", None, Timestamp(300)).await.unwrap();
	assert_eq!(unref_at(&h, &uid).await, Some(Timestamp(300)));

	assert_eq!(store.blob_sweep(Timestamp(301), 10).await.unwrap(), ["cc33"]);
	assert!(store.blob_get(&uid).await.unwrap().is_none());
	assert!(store.blob_sha_in_use("cc33").await.unwrap());
	assert!(!store.blob_sha_in_use("dd44").await.unwrap());
}

/// A claim on a blob the sweep already deleted is `NotFound`, never a generic FK failure.
pub async fn claiming_a_swept_blob_is_not_found<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-swept");
	let store = h.store();
	let (id, _) = put(&h, "ee55", None, 100).await;
	assert_eq!(store.blob_sweep(Timestamp(101), 10).await.unwrap(), ["ee55"]);
	let err = store.blob_claim(id, "late", None, Timestamp(200)).await.unwrap_err();
	assert!(matches!(err, Error::NotFound), "{err:?}");
}

/// A release naming another account keeps the hold; an unrestricted (`None`) one drops it.
pub async fn a_release_by_another_account_keeps_the_hold<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-claimer");
	let store = h.store();
	for (uid, email) in [("acc_own", "o@e.st"), ("acc_oth", "x@e.st")] {
		let sql = "INSERT INTO accounts (uid, email, created_at) VALUES (?, ?, 0)";
		h.exec(sql, &[json!(uid), json!(email)]).await;
	}
	let sel = "SELECT id FROM accounts WHERE uid = ?";
	let own = h.scalar_i64(sel, &[json!("acc_own")]).await;
	let other = h.scalar_i64(sel, &[json!("acc_oth")]).await;
	let (id, _) = put(&h, "ff66", None, 100).await;
	store.blob_claim(id, "a", Some(own), Timestamp(110)).await.unwrap();

	assert!(!store.blob_release(id, "a", Some(other), Timestamp(200)).await.unwrap());
	assert_eq!(store.blob_refs(id).await.unwrap(), ["a"]);
	assert!(store.blob_release(id, "a", None, Timestamp(200)).await.unwrap());
	assert!(store.blob_refs(id).await.unwrap().is_empty());
}

/// Claimed and unclaimed bytes both count, and only the org's own.
pub async fn org_bytes_counts_the_orgs_own<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-org-bytes");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let (claimed, _) = put(&h, "ee55", None, 100).await;
	put(&h, "ff66", None, 100).await;
	store.blob_claim(claimed, "h", None, Timestamp(110)).await.unwrap();
	h.exec(
		"INSERT INTO orgs (uid, parent_id, kind, name, created_at) VALUES ('org_blob_x', ?, 'SHARED', 'x', 0)",
		&[json!(root)],
	)
	.await;
	let other = h.scalar_i64("SELECT id FROM orgs WHERE uid = 'org_blob_x'", &[]).await;
	let row = Blob {
		id: 0,
		uid: BlobId::generate(),
		org_id: other,
		sha256: "ab77".to_owned(),
		mime: "text/plain".to_owned(),
		size: 50,
		name: None,
		created_by: None,
		created_at: Timestamp(100),
		unref_at: Some(Timestamp(100)),
	};
	store.blob_insert(&row, i64::MAX).await.unwrap().unwrap();
	assert_eq!(store.blob_org_bytes(root).await.unwrap(), 6);
	assert_eq!(store.blob_org_bytes(other).await.unwrap(), 50);
}

/// The cap check is part of the insert: a refused row is not written.
pub async fn blob_insert_refuses_past_the_org_cap<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-cap");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let row = |size| Blob {
		id: 0,
		uid: BlobId::generate(),
		org_id: root,
		sha256: format!("cap{size}"),
		mime: "text/plain".to_owned(),
		size,
		name: None,
		created_by: None,
		created_at: Timestamp(100),
		unref_at: Some(Timestamp(100)),
	};
	assert!(store.blob_insert(&row(6), 10).await.unwrap().is_some());
	assert!(store.blob_insert(&row(5), 10).await.unwrap().is_none());
	assert_eq!(store.blob_org_bytes(root).await.unwrap(), 6);
	assert!(store.blob_insert(&row(4), 10).await.unwrap().is_some(), "exactly at the cap fits");
}

/// A claim answers the hold's first claimer, also to a second, different claimer.
pub async fn a_second_claim_reports_the_first_claimer<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-first-claimer");
	let store = h.store();
	for (uid, email) in [("acc_one", "1@e.st"), ("acc_two", "2@e.st")] {
		let sql = "INSERT INTO accounts (uid, email, created_at) VALUES (?, ?, 0)";
		h.exec(sql, &[json!(uid), json!(email)]).await;
	}
	let sel = "SELECT id FROM accounts WHERE uid = ?";
	let one = h.scalar_i64(sel, &[json!("acc_one")]).await;
	let two = h.scalar_i64(sel, &[json!("acc_two")]).await;
	let (id, _) = put(&h, "cd88", None, 100).await;
	assert_eq!(store.blob_claim(id, "a", Some(one), Timestamp(110)).await.unwrap(), Some(one));
	assert_eq!(store.blob_claim(id, "a", Some(two), Timestamp(120)).await.unwrap(), Some(one));
	assert_eq!(store.blob_claim(id, "b", None, Timestamp(130)).await.unwrap(), None);
}

/// Erasure deletes the personal org's blobs and clears the account's names on the rest.
pub async fn erasure_deletes_personal_blobs_and_forgets_names<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-forget");
	let store = h.store();
	h.exec("INSERT INTO accounts (uid, email, created_at) VALUES ('acc_forget', 'f@e.st', 0)", &[])
		.await;
	let acc = h
		.scalar_i64("SELECT id FROM accounts WHERE uid = ?", &[json!("acc_forget")])
		.await;
	let root = store.root_org_id().await.unwrap();
	let sql = "INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, created_at) \
	           VALUES ('org_forget', ?, 'PERSONAL', 'p', ?, 0)";
	h.exec(sql, &[json!(root), json!(acc)]).await;
	let personal = h.scalar_i64("SELECT id FROM orgs WHERE uid = ?", &[json!("org_forget")]).await;
	let (_, mine) = put(&h, "cc77", Some(acc), 100).await;
	let (_, theirs) = put(&h, "dd88", None, 100).await;
	let (pid, private) = put(&h, "ee99", Some(acc), 100).await;
	h.exec("UPDATE blobs SET org_id = ? WHERE id = ?", &[json!(personal), json!(pid)])
		.await;
	let who = AccountId::from_trusted("acc_forget".into());
	store.blobs_erase(&who).await.unwrap();
	store.blobs_erase(&who).await.unwrap();
	assert_eq!(store.blob_get(&mine).await.unwrap().unwrap().name, None);
	assert_eq!(store.blob_get(&theirs).await.unwrap().unwrap().name.as_deref(), Some("a.txt"));
	assert!(store.blob_get(&private).await.unwrap().is_none());
}

/// `blob_delete` removes the row, so it no longer counts toward the org's bytes.
pub async fn a_deleted_blob_is_gone<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "blob-delete");
	let store = h.store();
	let (id, uid) = put(&h, "ff00", None, 100).await;
	store.blob_delete(id).await.unwrap();
	assert!(store.blob_get(&uid).await.unwrap().is_none());
	assert_eq!(store.blob_org_bytes(store.root_org_id().await.unwrap()).await.unwrap(), 0);
	assert!(!store.blob_sha_in_use("ff00").await.unwrap());
}

// vim: ts=4
