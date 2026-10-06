//! Driver-error classification, the PostgreSQL counterpart of `adapters/appdb-sqlite/src/util.rs`.

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

/// A serialization failure, deadlock or timeout stays `E-CORE-UNAVAILABLE` and retryable; a
/// duplicate key is a 409 the caller can act on; any other constraint violation or a decode
/// failure is a defect and answers identically however often the statement is sent.
pub(crate) fn map_db(err: &sqlx::Error) -> Error {
	match err {
		// Not `Timeout`: a pool acquire that never handed out a connection means no statement ran.
		sqlx::Error::PoolTimedOut => {
			Error::Unavailable("script database pool acquire timed out".to_owned())
		}
		sqlx::Error::Database(db) if is_transient(db.as_ref()) => {
			Error::Unavailable("script database is busy, retry".to_owned())
		}
		// `read_only_sql_transaction`: a write through `db::query`, which runs `READ ONLY`.
		sqlx::Error::Database(db) if db.code().as_deref() == Some("25006") => {
			mintworks_script::error::db("db::query cannot write: use db::exec")
		}
		sqlx::Error::Database(db) if db.is_unique_violation() => {
			Error::conflict("script database row already exists")
		}
		sqlx::Error::Database(_)
		| sqlx::Error::ColumnDecode { .. }
		| sqlx::Error::ColumnNotFound(_)
		| sqlx::Error::RowNotFound
		| sqlx::Error::TypeNotFound { .. }
		| sqlx::Error::Configuration(_) => Error::internal(format!("script database error: {err}")),
		// Only genuinely transient variants reach here: `Io`, `Tls`, `WorkerCrashed`.
		_ => Error::Unavailable(format!("script database error: {err}")),
	}
}

/// `serialization_failure`, `deadlock_detected`, `lock_not_available`, `query_canceled`
/// (statement timeout) — the statement may succeed if the caller sends it again.
const TRANSIENT: [&str; 4] = ["40001", "40P01", "55P03", "57014"];

fn is_transient(db: &dyn sqlx::error::DatabaseError) -> bool {
	db.code().is_some_and(|code| TRANSIENT.contains(&code.as_ref()))
}

#[cfg(test)]
mod tests {
	// Twin: `adapters/store-postgres/src/util.rs` — both engines' retry set must match.
	#[test]
	fn transient_codes() {
		assert_eq!(super::TRANSIENT, ["40001", "40P01", "55P03", "57014"]);
	}
}

// vim: ts=4
