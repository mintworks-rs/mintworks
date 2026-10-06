//! Upgrades for databases already carrying an earlier [`crate::schema`]. One `if from < N` block
//! per version. Adding a change is three edits, all required: the DDL in `schema.rs`'s `create`,
//! the `if from < N { … }` block here, and [`crate::schema::VERSION`] bumped to N.
//!
//! The runner stamps `schema_version` once, after `upgrade` returns, so no block sets a version.

use sqlx::PgConnection;

use mintworks_core::error::ClResult;

/// No upgrade exists yet: version 1 is the first PostgreSQL schema.
pub(crate) async fn upgrade(_conn: &mut PgConnection, _from: i64) -> ClResult<()> {
	Ok(())
}

// vim: ts=4
