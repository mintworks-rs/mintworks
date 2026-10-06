// SPDX-License-Identifier: MPL-2.0
//! `ObjectStore` over `objects` and `object_index` — the SQLite adapter's `objects.rs`, with
//! `json_extract` translated to a `jsonb_path_query_first` expression ([`extract`]).
//!
//! Index rows are written by the writer, not by a trigger: every declared path is
//! re-extracted from the body inside the same transaction as the body.

use async_trait::async_trait;
use serde_json::Value;

use mintworks_core::objects::{Object, ObjectStore, ObjectType};
use mintworks_core::prelude::*;

use crate::PgStore;
use crate::util::{DbExt, RowExt, RowsExt};

/// The `vars` key holding the declaration set the index was last reconciled against.
const FINGERPRINT: &str = "objects.declared";

/// `json_extract(body, path)` as SQLite stores it in a TEXT column: a string unquoted, a JSON
/// `null` or a missing path as NULL, a boolean as `1`/`0`. An object or array keeps jsonb's own
/// spacing (`{"a": 1}`), which no SQLite-built index row is compared against.
fn extract(body: &str, path: &str) -> String {
	format!(
		"(SELECT CASE jsonb_typeof(v) WHEN 'boolean' THEN CASE WHEN v = 'true' THEN '1' ELSE '0' END
		  ELSE v #>> '{{}}' END
		  FROM (SELECT jsonb_path_query_first(({body})::jsonb, ({path})::jsonpath) AS v) x)"
	)
}

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
impl ObjectStore for PgStore {
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

		// `created_at` stays out of the `DO UPDATE` set: an overwrite leaves the object's age alone.
		let (id, created_at, updated_at) = sqlx::query_as::<_, (i64, i64, i64)>(
			"INSERT INTO objects (org_id, type, uid, body, created_at, updated_at)
			 VALUES ($1, $2, $3, $4, $5, $5)
			 ON CONFLICT (org_id, type, uid)
			 DO UPDATE SET body = excluded.body, updated_at = excluded.updated_at
			 RETURNING id, created_at, updated_at",
		)
		.bind(org_id)
		.bind(type_name)
		.bind(uid)
		.bind(&raw)
		.bind(now)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;

		// Rewritten, not merged, so a path this write no longer declares loses its row.
		sqlx::query("DELETE FROM object_index WHERE object_id = $1")
			.bind(id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		let insert = format!(
			"INSERT INTO object_index (object_id, path, value) VALUES ($1, $2, {})
			 ON CONFLICT (object_id, path) DO UPDATE SET value = excluded.value",
			extract("$3", "$2")
		);
		for path in indexed {
			sqlx::query(sqlx::AssertSqlSafe(insert.as_str()))
				.bind(id)
				.bind(path)
				.bind(&raw)
				.execute(&mut *tx.lock().await?)
				.await
				.db()
				// A path `jsonpath` rejects is the caller's programming error; name it.
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
			  WHERE org_id = $1 AND type = $2 AND uid = $3",
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
		let deleted =
			sqlx::query("DELETE FROM objects WHERE org_id = $1 AND type = $2 AND uid = $3")
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
			  WHERE org_id = $1 AND type = $2 AND id < $3
			  ORDER BY id DESC LIMIT $4",
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
		// A NULL `value` (no such path) matches nothing, the string "null" included.
		sqlx::query_as::<_, ObjectRow>(
			"SELECT o.id, o.uid, o.type, o.body, o.created_at, o.updated_at
			   FROM object_index i JOIN objects o ON o.id = i.object_id
			  WHERE o.org_id = $1 AND o.type = $2 AND i.path = $3 AND i.value = $4
			    AND o.id < $5
			  ORDER BY o.id DESC LIMIT $6",
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
		// A `type_name` twice over is two declarations of one type; startup is where that is fixed.
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

		let tx = self.write_tx().await?;

		// Read inside the transaction that writes it, so a crash midway cannot leave the
		// fingerprint claiming an index it never finished.
		let current: Option<String> = sqlx::query_scalar("SELECT value FROM vars WHERE name = $1")
			.bind(FINGERPRINT)
			.fetch_optional(&mut *tx.lock().await?)
			.await
			.db()?;
		if current.as_deref() == Some(fingerprint.as_str()) {
			return Ok(());
		}

		// The types come from the data: a withdrawn type has no entry in `declared` at all.
		let types: Vec<(String,)> = sqlx::query_as("SELECT DISTINCT type FROM objects")
			.fetch_all(&mut *tx.lock().await?)
			.await
			.db()?;

		let refill = format!(
			"INSERT INTO object_index (object_id, path, value)
			 SELECT id, $1, {} FROM objects WHERE type = $2
			 ON CONFLICT (object_id, path) DO UPDATE SET value = excluded.value",
			extract("body", "$1")
		);
		for (type_name,) in &types {
			let paths: Vec<String> = declared
				.iter()
				.filter(|d| d.type_name == *type_name)
				.flat_map(|d| d.paths.iter().cloned())
				.collect();

			// `<> ALL` over an empty array is true, which empties a withdrawn type's index.
			sqlx::query(
				"DELETE FROM object_index
				  WHERE object_id IN (SELECT id FROM objects WHERE type = $1)
				    AND path <> ALL($2)",
			)
			.bind(type_name)
			.bind(&paths)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;

			// One pass over every object of the type per path on a declaration change;
			// per-org or incremental reconcile if a deployment holds enough objects to feel it.
			for path in &paths {
				sqlx::query(sqlx::AssertSqlSafe(refill.as_str()))
					.bind(path)
					.bind(type_name)
					.execute(&mut *tx.lock().await?)
					.await
					.db()?;
			}
		}

		sqlx::query(
			"INSERT INTO vars (name, value) VALUES ($1, $2)
			 ON CONFLICT (name) DO UPDATE SET value = excluded.value",
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
