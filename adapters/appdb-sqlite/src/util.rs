// SPDX-License-Identifier: MPL-2.0
//! Driver-error classification, copied from `adapters/store-sqlite/src/util.rs`.
//!
//! Copied rather than imported: importing means depending on the framework's store adapter, which
//! is the one dependency this crate exists to not have.

use mintworks_core::error::{ClResult, Error};

/// Collapse a driver error at the one boundary that understands it.
///
/// `mintworks_core::Error` has no `sqlx` variant — no `sqlx` type appears in the framework's public
/// API — so every `?` in this crate goes through `.db()?`.
pub(crate) trait DbExt<T> {
	fn db(self) -> ClResult<T>;
}

impl<T> DbExt<T> for Result<T, sqlx::Error> {
	fn db(self) -> ClResult<T> {
		self.map_err(|err| map_db(&err))
	}
}

/// A busy database stays `E-CORE-UNAVAILABLE` and retryable; a duplicate key is a 409 the caller
/// can act on; any other constraint violation or a decode failure is a defect and answers
/// identically however often the statement is sent.
pub(crate) fn map_db(err: &sqlx::Error) -> Error {
	match err {
		// Not `Timeout`: a pool acquire that never handed out a connection means no statement ran.
		sqlx::Error::PoolTimedOut => {
			Error::Unavailable("script database pool acquire timed out".to_owned())
		}
		sqlx::Error::Database(db) if is_busy(db.as_ref()) => {
			Error::Unavailable("script database is busy, retry".to_owned())
		}
		// A write through `db::query`: the reader pool is `read_only`, a held writer `query_only`.
		sqlx::Error::Database(db) if primary(db.as_ref()) == Some(8) => {
			mintworks_script::error::db("db::query cannot write: use db::exec")
		}
		sqlx::Error::Database(db) if db.is_unique_violation() => {
			Error::conflict("script database row already exists")
		}
		sqlx::Error::Database(_)
		| sqlx::Error::ColumnDecode { .. }
		| sqlx::Error::ColumnNotFound(_)
		| sqlx::Error::RowNotFound
		| sqlx::Error::TypeNotFound { .. } => Error::internal(format!("script database error: {err}")),
		// Only genuinely transient variants reach here: `Io`, `Tls`, `WorkerCrashed`.
		_ => Error::Unavailable(format!("script database error: {err}")),
	}
}

/// `SQLITE_BUSY` (5) or `SQLITE_LOCKED` (6) — another connection holds the lock.
///
/// sqlx reports the *extended* result code, so `SQLITE_BUSY_SNAPSHOT` arrives as 517 and
/// `SQLITE_LOCKED_SHAREDCACHE` as 262; the primary code is the low byte.
fn is_busy(db: &dyn sqlx::error::DatabaseError) -> bool {
	matches!(primary(db), Some(5 | 6))
}

/// The primary result code, `SQLITE_READONLY` (8) for any `SQLITE_READONLY_*`.
fn primary(db: &dyn sqlx::error::DatabaseError) -> Option<u32> {
	db.code().and_then(|c| c.parse::<u32>().ok()).map(|c| c & 0xff)
}

// vim: ts=4
