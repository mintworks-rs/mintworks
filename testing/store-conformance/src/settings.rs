// SPDX-License-Identifier: MPL-2.0
//! Scoped `settings` conformance: each `(org_id, account_id, key)` is its own row, and
//! `CoreStore::setting_rows_chain` answers every candidate along the ancestor chain in one query.

use mintworks_core::store::{CoreStore, SettingChainRow};
use serde_json::json;

use crate::{Harness, fresh};

async fn org<H: Harness>(h: &H, parent: i64, name: &str) -> i64 {
	let uid = format!("org_settings_{name}");
	h.exec(
		"INSERT INTO orgs (uid, parent_id, kind, name, created_at) VALUES (?, ?, 'SHARED', ?, 0)",
		&[json!(uid), json!(parent), json!(name)],
	)
	.await;
	h.scalar_i64("SELECT id FROM orgs WHERE uid = ?", &[json!(uid)]).await
}

/// The four levels of one key never overwrite each other, and a delete removes one level only.
pub async fn each_level_is_its_own_row<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "settings-levels");
	let store = h.store();
	let o = org(&h, store.root_org_id().await.unwrap(), "school").await;
	let levels = [(0, 0), (o, 0), (0, 7), (o, 7)];
	for (org_id, account_id) in levels {
		let v = format!("{org_id}/{account_id}");
		store.setting_set(org_id, account_id, "app.k", &v, None).await.unwrap();
	}
	store.setting_set(o, 7, "app.k", "again", Some(7)).await.unwrap();
	assert_eq!(store.setting_get(o, 7, "app.k").await.unwrap().as_deref(), Some("again"));
	assert_eq!(store.setting_get(0, 0, "app.k").await.unwrap().as_deref(), Some("0/0"));

	store.setting_delete(o, 0, "app.k").await.unwrap();
	store.setting_delete(o, 0, "app.k").await.unwrap();
	assert_eq!(store.setting_get(o, 0, "app.k").await.unwrap(), None);
	assert_eq!(store.setting_get(0, 7, "app.k").await.unwrap().as_deref(), Some("0/7"));
	assert_eq!(store.setting_get(o, 7, "app.other").await.unwrap(), None);
}

/// root → school (suspended: status does not stop inheritance) → class. The chain from
/// `(class, 7)` holds each ancestor's org row with its depth, the class's own account row, and the
/// two `org_id = 0` rows; never another account's row or another key's.
pub async fn the_chain_returns_every_candidate_with_its_depth<H: Harness>()
where
	H::Store: CoreStore,
{
	let h = fresh!(H, "settings-chain");
	let store = h.store();
	let root = store.root_org_id().await.unwrap();
	let school = org(&h, root, "school").await;
	let class = org(&h, school, "class").await;
	h.exec("UPDATE orgs SET status = 'SUSPENDED' WHERE id = ?", &[json!(school)])
		.await;
	for (org_id, account_id, v) in [
		(0, 0, "global"),
		(0, 7, "account"),
		(root, 0, "root"),
		(school, 0, "school"),
		(school, 7, "school-7"),
		(class, 0, "class"),
		(class, 7, "class-7"),
		(class, 8, "class-8"),
	] {
		store.setting_set(org_id, account_id, "app.k", v, None).await.unwrap();
	}
	store.setting_set(class, 7, "app.other", "x", None).await.unwrap();

	let chain = |org_id: i64, account_id: i64| async move {
		let mut rows = store.setting_rows_chain(org_id, account_id, "app.k").await.unwrap();
		rows.sort_by(|a, b| a.value.cmp(&b.value));
		rows
	};
	let r = |org_id, account_id, depth, value: &str| SettingChainRow {
		org_id,
		account_id,
		depth,
		value: value.to_owned(),
	};
	let mut want = vec![
		r(0, 7, None, "account"),
		r(class, 0, Some(0), "class"),
		r(class, 7, Some(0), "class-7"),
		r(0, 0, None, "global"),
		r(root, 0, Some(2), "root"),
		r(school, 0, Some(1), "school"),
		r(school, 7, Some(1), "school-7"),
	];
	want.sort_by(|a, b| a.value.cmp(&b.value));
	assert_eq!(chain(class, 7).await, want);
	assert_eq!(chain(0, 7).await, vec![r(0, 7, None, "account"), r(0, 0, None, "global")]);
	assert_eq!(chain(school, 0).await.len(), 3, "school, root and global only");
}

// vim: ts=4
