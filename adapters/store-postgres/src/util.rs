//! The conversion edge between PostgreSQL's primitives and the framework's domain types — the
//! counterpart of `adapters/store-sqlite/src/util.rs`, with the same vocabulary:
//!
//! | domain type | bind as | read as |
//! |---|---|---|
//! | `Money` / `Qty` | `m.0` (`i64`) | [`read_money`] / [`read_qty`] |
//! | `Timestamp` | `t.0` (`i64`) | `Timestamp(raw)` |
//! | id newtype | `id.as_str()` (`&str`) | `AccountId::from_trusted(raw)` |
//!
//! [`DbExt::db`], [`map_db`] and [`unique_as_conflict`] are public and classify exactly as the
//! SQLite adapter's do (`adapter-contract.md` §3): a consumer's own store trait for `PgStore`
//! needs the same mapping rather than a hand-rolled copy that flattens a busy database into a 500.

use mintworks_core::prelude::*;

/// A unique-constraint violation (`23505`) is a conflict, not an internal error. Everything else
/// collapses through [`map_db`].
pub fn unique_as_conflict(err: &sqlx::Error, msg: &str) -> Error {
	match err {
		sqlx::Error::Database(db) if db.is_unique_violation() => Error::conflict(msg),
		_ => map_db(err),
	}
}

/// Collapse a driver error at the one boundary that understands it.
///
/// `mintworks_core::Error` has no `sqlx` variant — no `sqlx` type appears in the framework's public
/// API — so every `?` in this crate goes through `.db()?`.
pub trait DbExt<T> {
	/// Map a driver failure to `E-CORE-UNAVAILABLE` when the statement is worth sending
	/// again — a lock wait past `lock_timeout`, a serialization failure or deadlock, a pool-acquire
	/// timeout — and to `E-CORE-INTERNAL` when it is a defect.
	fn db(self) -> ClResult<T>;
}

impl<T> DbExt<T> for Result<T, sqlx::Error> {
	fn db(self) -> ClResult<T> {
		self.map_err(|err| map_db(&err))
	}
}

/// `.db()?.as_ref().map(row_fn).transpose()`, which every single-row read ended in.
pub(crate) trait RowExt<R> {
	fn one<T>(self, f: fn(&R) -> ClResult<T>) -> ClResult<Option<T>>;
}

impl<R> RowExt<R> for Result<Option<R>, sqlx::Error> {
	fn one<T>(self, f: fn(&R) -> ClResult<T>) -> ClResult<Option<T>> {
		self.db()?.as_ref().map(f).transpose()
	}
}

/// The multi-row companion of [`RowExt::one`].
pub(crate) trait RowsExt<R> {
	fn all<T>(self, f: fn(&R) -> ClResult<T>) -> ClResult<Vec<T>>;
}

impl<R> RowsExt<R> for Result<Vec<R>, sqlx::Error> {
	fn all<T>(self, f: fn(&R) -> ClResult<T>) -> ClResult<Vec<T>> {
		self.db()?.iter().map(f).collect()
	}
}

/// The mapping itself, so the call sites that pick a unique violation off first classify their
/// fallback exactly as [`DbExt::db`] does.
pub fn map_db(err: &sqlx::Error) -> Error {
	match err {
		// Not `Timeout`: a pool acquire that never handed out a connection means no
		// statement ran, and `mintworks_nav::job::report` parks a filing as `UNKNOWN` on
		// `Error::Timeout` alone.
		sqlx::Error::PoolTimedOut => {
			Error::Unavailable("database pool acquire timed out".to_owned())
		}
		sqlx::Error::Database(db) if is_transient(db.as_ref()) => {
			Error::Unavailable("database is busy, retry".to_owned())
		}
		// A constraint violation or a decode failure answers identically however often the
		// statement is sent: `Error::internal` is `Retry::Never`, which is what stops an unbounded
		// `NAV_REPORT` retry loop over one.
		sqlx::Error::Database(_)
		| sqlx::Error::ColumnDecode { .. }
		| sqlx::Error::ColumnNotFound(_)
		| sqlx::Error::RowNotFound
		| sqlx::Error::TypeNotFound { .. }
		| sqlx::Error::Configuration(_) => Error::internal(format!("database error: {err}")),
		// Only genuinely transient variants reach here: `Io`, `Tls`, `WorkerCrashed`.
		_ => Error::Unavailable(format!("database error: {err}")),
	}
}

/// `serialization_failure`, `deadlock_detected`, `lock_not_available` (the `lock_timeout` on the
/// write lock), `query_canceled` (statement timeout) — worth sending again.
const TRANSIENT: [&str; 4] = ["40001", "40P01", "55P03", "57014"];

fn is_transient(db: &dyn sqlx::error::DatabaseError) -> bool {
	db.code().is_some_and(|code| TRANSIENT.contains(&code.as_ref()))
}

/// [`Money`] from a `BIGINT` column of minor units, [`bounded`] for the reason the SQLite
/// adapter's `read_money` gives: an out-of-range row traps under `overflow-checks`.
pub(crate) fn read_money(raw: i64) -> ClResult<Money> {
	bounded(raw).map(Money)
}

/// [`Qty`] from a `BIGINT` column scaled 1e6. Bounded for the same reason as [`read_money`].
pub(crate) fn read_qty(raw: i64) -> ClResult<Qty> {
	bounded(raw).map(Qty)
}

/// The `discount_value` column on `invoices` and `invoice_lines`: basis points or minor units,
/// bounded because `mintworks_invoice::storno` negates it and `-i64::MIN` traps.
pub(crate) fn read_discount_value(raw: Option<i64>) -> ClResult<Option<i64>> {
	raw.map(bounded).transpose()
}

#[cfg(test)]
mod tests {
	// Twin: `adapters/appdb-postgres/src/util.rs` — both engines' retry set must match.
	#[test]
	fn transient_codes() {
		assert_eq!(super::TRANSIENT, ["40001", "40P01", "55P03", "57014"]);
	}
}

// vim: ts=4
