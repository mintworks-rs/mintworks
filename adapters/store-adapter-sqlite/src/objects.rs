//! `ObjectStore` over `objects` and `object_index`.
//!
//! Index rows are written by the writer, not by a trigger (arch-9): every declared path is
//! re-extracted from the body inside the same transaction as the body, so the two are never
//! observed apart.
//!
//! `json_extract` reads a path on both sides — the write that stores a value and the query that
//! matches one — so the two cannot disagree about what a path means. It is also why `value` in
//! `object_index` holds JSON text, not a Rust-side extraction.

use async_trait::async_trait;
use serde_json::Value;

use saas_core::objects::{Object, ObjectStore, ObjectType};
use saas_core::prelude::*;

use crate::SqliteStore;
use crate::util::{DbExt, RowExt, RowsExt};

/// The object type an entity's ext blob lives under — `invoice.ext`, `party.ext`, `service.ext` —
/// keyed by that entity's own uid.
///
/// The `(type, uid)` pair is polymorphic: nothing in the schema ties it to `invoices` or
/// `billing_parties`, so an entity delete has to sweep its own blob by hand, and a script
/// writing one has to spell the same name.
pub(crate) fn ext_type(entity: &str) -> String {
	format!("{entity}.ext")
}

/// The `vars` key holding the declaration set the index was last reconciled against: the set
/// almost never changes between boots, and a reconcile is a full pass per declared path.
const FINGERPRINT: &str = "objects.declared";

/// A row of `objects`, in the column order every read below spells out.
type ObjectRow = (i64, String, String, String, i64, i64);

fn to_object(row: &ObjectRow) -> ClResult<Object> {
	Ok(Object {
		id: row.0,
		uid: row.1.clone(),
		type_name: row.2.clone(),
		body: serde_json::from_str(&row.3)
			.map_err(|err| Error::internal(format!("objects.body: {err}")))?,
		created_at: Timestamp(row.4),
		updated_at: Timestamp(row.5),
	})
}

#[async_trait]
impl ObjectStore for SqliteStore {
	async fn object_put(
		&self,
		org_id: i64,
		type_name: &str,
		uid: &str,
		body: &Value,
		indexed: &[String],
	) -> ClResult<Object> {
		let raw = body.to_string();
		let now = Timestamp::now().0;
		let tx = self.write_tx().await?;

		// `created_at` is deliberately absent from the `DO UPDATE` set: an overwrite moves
		// `updated_at` and leaves the object's own age alone.
		let (id, created_at, updated_at) = sqlx::query_as::<_, (i64, i64, i64)>(
			"INSERT INTO objects (org_id, type, uid, body, created_at, updated_at)
			 VALUES (?, ?, ?, ?, ?, ?)
			 ON CONFLICT(org_id, type, uid)
			 DO UPDATE SET body = excluded.body, updated_at = excluded.updated_at
			 RETURNING id, created_at, updated_at",
		)
		.bind(org_id)
		.bind(type_name)
		.bind(uid)
		.bind(&raw)
		.bind(now)
		.bind(now)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;

		// Rewritten, not merged, so a path this write no longer declares loses its row rather than
		// leaving a query answering from a value the body has dropped.
		sqlx::query("DELETE FROM object_index WHERE object_id = ?")
			.bind(id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		for path in indexed {
			// `OR REPLACE`: a declaration that repeats a path re-extracts the same value from the
			// same body, so the second row is the first one — not a primary-key violation.
			sqlx::query(
				"INSERT OR REPLACE INTO object_index (object_id, path, value)
				 VALUES (?, ?, json_extract(?, ?))",
			)
			.bind(id)
			.bind(path)
			.bind(&raw)
			.bind(path)
			.execute(&mut *tx.lock().await?)
			.await
			.db()
			// Named, not validated: the declaration set is the caller's, so a path
			// `json_extract` rejects is a programming error, and the driver's message names
			// nothing it can be found by. A busy database keeps its retryable code.
			.map_err(|err| match err {
				Error::Internal(msg) => {
					Error::internal(format!("declared index path {path:?}: {msg}"))
				}
				other => other,
			})?;
		}
		tx.commit().await?;

		Ok(Object {
			id,
			uid: uid.to_owned(),
			type_name: type_name.to_owned(),
			body: body.clone(),
			created_at: Timestamp(created_at),
			updated_at: Timestamp(updated_at),
		})
	}

	async fn object_get(
		&self,
		org_id: i64,
		type_name: &str,
		uid: &str,
	) -> ClResult<Option<Object>> {
		sqlx::query_as::<_, ObjectRow>(
			"SELECT id, uid, type, body, created_at, updated_at FROM objects
			  WHERE org_id = ? AND type = ? AND uid = ?",
		)
		.bind(org_id)
		.bind(type_name)
		.bind(uid)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(to_object)
	}

	async fn object_delete(&self, org_id: i64, type_name: &str, uid: &str) -> ClResult<bool> {
		let tx = self.write_tx().await?;
		// The index rows go with the body through `object_index`'s `ON DELETE CASCADE`.
		let deleted = sqlx::query("DELETE FROM objects WHERE org_id = ? AND type = ? AND uid = ?")
			.bind(org_id)
			.bind(type_name)
			.bind(uid)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?
			.rows_affected();
		tx.commit().await?;
		Ok(deleted > 0)
	}

	async fn object_list(
		&self,
		org_id: i64,
		type_name: &str,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Object>> {
		sqlx::query_as::<_, ObjectRow>(
			"SELECT id, uid, type, body, created_at, updated_at FROM objects
			  WHERE org_id = ? AND type = ? AND id < ?
			  ORDER BY id DESC LIMIT ?",
		)
		.bind(org_id)
		.bind(type_name)
		.bind(before_id.unwrap_or(i64::MAX))
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(to_object)
	}

	async fn object_query(
		&self,
		org_id: i64,
		type_name: &str,
		path: &str,
		value: &str,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Object>> {
		// An index row with a NULL `value` — the body has no such path — matches no `value` here,
		// including the string "null": `= ?` is never true of NULL.
		sqlx::query_as::<_, ObjectRow>(
			"SELECT o.id, o.uid, o.type, o.body, o.created_at, o.updated_at
			   FROM object_index i JOIN objects o ON o.id = i.object_id
			  WHERE o.org_id = ? AND o.type = ? AND i.path = ? AND i.value = ?
			    AND o.id < ?
			  ORDER BY o.id DESC LIMIT ?",
		)
		.bind(org_id)
		.bind(type_name)
		.bind(path)
		.bind(value)
		.bind(before_id.unwrap_or(i64::MAX))
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(to_object)
	}

	async fn object_index_reconcile(&self, declared: &[ObjectType]) -> ClResult<()> {
		// A `type_name` twice over is two declarations of one type — a consumer shadowing a
		// framework extension type, or a consumer shadowing itself — and merging them silently
		// rewrites both indexes. Startup is where that can still be fixed.
		let mut seen = std::collections::BTreeSet::new();
		for d in declared {
			if !seen.insert(d.type_name.as_str()) {
				return Err(Error::internal(format!(
					"object type {:?} is declared twice",
					d.type_name
				)));
			}
		}

		// Sorted, so the same declaration set in another order is the same fingerprint.
		let mut sorted: Vec<&ObjectType> = declared.iter().collect();
		sorted.sort_unstable_by(|a, b| a.type_name.cmp(&b.type_name));
		let fingerprint = serde_json::to_string(&sorted)
			.map_err(|err| Error::internal(format!("object declarations: {err}")))?;

		// The types come from the data, not from `declared`: a type whose declaration has just
		// been withdrawn has no entry in `declared` at all, and its rows are exactly the ones the
		// drop below has to reach.
		let tx = self.write_tx().await?;

		// Read inside the transaction that writes it, so a crash midway through the reconcile
		// cannot leave the fingerprint claiming an index it never finished.
		let current: Option<String> = sqlx::query_scalar("SELECT value FROM vars WHERE name = ?")
			.bind(FINGERPRINT)
			.fetch_optional(&mut *tx.lock().await?)
			.await
			.db()?;
		if current.as_deref() == Some(fingerprint.as_str()) {
			return Ok(());
		}

		let types: Vec<(String,)> = sqlx::query_as("SELECT DISTINCT type FROM objects")
			.fetch_all(&mut *tx.lock().await?)
			.await
			.db()?;

		for (type_name,) in &types {
			let paths: Vec<&String> = declared
				.iter()
				.filter(|d| d.type_name == *type_name)
				.flat_map(|d| d.paths.iter())
				.collect();
			let as_json = serde_json::to_string(&paths)
				.map_err(|err| Error::internal(format!("object paths: {err}")))?;

			// No declared path means an empty array, and `NOT IN` over nothing is true — which is
			// what empties a withdrawn type's index.
			sqlx::query(
				"DELETE FROM object_index
				  WHERE object_id IN (SELECT id FROM objects WHERE type = ?)
				    AND path NOT IN (SELECT value FROM json_each(?))",
			)
			.bind(type_name)
			.bind(&as_json)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;

			// `OR REPLACE` instead of an `id NOT IN (…)` anti-join: re-extracting a still-declared
			// path writes back the value the row already held, in one pass rather than two.
			//
			// A declaration change still costs one pass over every object of the type, per path.
			// Per-org or incremental reconcile is the upgrade if a deployment ever holds enough
			// objects to feel it.
			for path in paths {
				sqlx::query(
					"INSERT OR REPLACE INTO object_index (object_id, path, value)
					 SELECT id, ?, json_extract(body, ?) FROM objects WHERE type = ?",
				)
				.bind(path)
				.bind(path)
				.bind(type_name)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
			}
		}

		sqlx::query(
			"INSERT INTO vars (name, value) VALUES (?, ?) \
			 ON CONFLICT(name) DO UPDATE SET value = excluded.value",
		)
		.bind(FINGERPRINT)
		.bind(&fingerprint)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		tx.commit().await?;
		Ok(())
	}
}

// vim: ts=4
