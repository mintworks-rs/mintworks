//! Upgrades for databases already carrying an earlier [`crate::schema`]. One `if from < N` block
//! per version, each an ordinary async fn body — it may probe with `PRAGMA table_info`, branch
//! and backfill, which the `.sql` files this replaced could not.
//!
//! Adding a change is three edits, all of them required:
//!   1. change the DDL in `schema.rs`'s `create` to the new shape
//!   2. add `if from < N { … }` here
//!   3. bump [`crate::schema::VERSION`] to N
//!
//! Never edit `schema.rs` for a change a shipped database has already seen — a fresh install
//! would then get a shape no upgraded database ever reaches, and nothing detects it.
//!
//! A block below [`crate::schema::OLDEST_UPGRADABLE`] is deleted when that floor moves: no live
//! database carries the version, so the block can only ratchet a shape nothing reaches. What it
//! did survives in git, and nowhere else.
//!
//! The runner stamps `schema_version` once, after `upgrade` returns, so no block sets a version.

use sqlx::SqliteConnection;

use saas_core::error::ClResult;

use crate::util::DbExt;

pub(crate) async fn upgrade(conn: &mut SqliteConnection, from: i64) -> ClResult<()> {
	// The object store. New tables, nothing to backfill, and the DDL spelled out rather than
	// taken from `schema::OBJECTS`: a later version must not change what an upgrading database got.
	if from < 13 {
		sqlx::raw_sql(
			"CREATE TABLE objects (
				id		INTEGER NOT NULL PRIMARY KEY,
				org_id		INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
				type		TEXT NOT NULL,
				uid		TEXT NOT NULL,
				body		TEXT NOT NULL DEFAULT '{}',
				created_at	INTEGER NOT NULL,
				updated_at	INTEGER NOT NULL,
				UNIQUE (org_id, type, uid)
			 );
			 CREATE INDEX idx_object_page ON objects(org_id, type, id DESC);
			 CREATE TABLE object_index (
				object_id	INTEGER NOT NULL REFERENCES objects(id) ON DELETE CASCADE,
				path		TEXT NOT NULL,
				value		TEXT,
				PRIMARY KEY (object_id, path)
			 ) WITHOUT ROWID;
			 CREATE INDEX idx_object_index_lookup ON object_index (path, value, object_id);",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	Ok(())
}

// vim: ts=4
