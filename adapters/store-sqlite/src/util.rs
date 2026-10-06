//! The conversion edge between SQLite's primitives and the framework's domain types.
//!
//! `mintworks-core` carries no `sqlx` dependency, so nothing in `crates/` can implement `Encode` /
//! `Decode` for [`Money`], [`Qty`], `Timestamp` or the id newtypes — and neither can this crate,
//! where trait, backend type and target type would all be foreign and the orphan rule applies.
//! Every query here therefore binds and reads primitives and converts on this side.
//!
//! The vocabulary, fixed once so the four adapter modules do not each invent their own:
//!
//! | domain type | bind as | read as |
//! |---|---|---|
//! | `Money` / `Qty` | `m.0` (`i64`) | [`read_money`] / [`read_qty`] |
//! | `Timestamp` | `t.0` (`i64`) | `Timestamp(raw)` |
//! | id newtype | `id.as_str()` (`&str`) | `AccountId::from_trusted(raw)` |
//!
//! `Timestamp` and the ids need no helper: their `from_trusted` / public field is the conversion.
//!
//! [`DbExt::db`], [`map_db`] and [`unique_as_conflict`] are public: a consumer implementing its
//! own store trait for `SqliteStore` contends for the same single writer connection, so it needs
//! the same classification the four adapter modules use rather than a hand-rolled copy that
//! flattens a busy database into a 500.

use mintworks_core::prelude::*;

/// A unique-constraint violation is a conflict, not an internal error. Everything else
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
	/// again — a lock still held past `BUSY_TIMEOUT`, a pool-acquire timeout on the single
	/// writer connection, or a driver error this mapping does not recognise — and to
	/// `E-CORE-INTERNAL` when it is a defect. The detail is free-form either way: `Error`'s
	/// `IntoResponse` masks every 5xx body with no exemption, so none of it reaches a client.
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

/// The mapping itself, so the handful of call sites that pick a unique violation off first
/// (`unique_as_conflict` and friends) classify their fallback exactly as [`DbExt::db`] does
/// rather than flattening a busy database into a 500.
pub fn map_db(err: &sqlx::Error) -> Error {
	match err {
		// Not `Timeout`: a pool acquire that never handed out a connection means no
		// statement ran, and `mintworks_nav::job::report` parks a filing as `UNKNOWN` on
		// `Error::Timeout` alone.
		sqlx::Error::PoolTimedOut => {
			Error::Unavailable("database pool acquire timed out".to_owned())
		}
		sqlx::Error::Database(db) if is_busy(db.as_ref()) => {
			Error::Unavailable("database is busy, retry".to_owned())
		}
		// A constraint violation or a decode failure answers identically however often the
		// statement is sent: a defect, not a busy database. `Error::internal` is `Retry::Never`,
		// which is what stops an unbounded `NAV_REPORT` retry loop over one.
		sqlx::Error::Database(_)
		| sqlx::Error::ColumnDecode { .. }
		| sqlx::Error::ColumnNotFound(_)
		| sqlx::Error::RowNotFound
		| sqlx::Error::TypeNotFound { .. } => Error::internal(format!("database error: {err}")),
		// Only genuinely transient variants reach here: `Io`, `Tls`, `WorkerCrashed`.
		_ => Error::Unavailable(format!("database error: {err}")),
	}
}

/// `SQLITE_BUSY` (5) or `SQLITE_LOCKED` (6) — another connection holds the lock.
///
/// sqlx reports the *extended* result code, so `SQLITE_BUSY_SNAPSHOT` arrives as 517 and
/// `SQLITE_LOCKED_SHAREDCACHE` as 262; the primary code is the low byte.
fn is_busy(db: &dyn sqlx::error::DatabaseError) -> bool {
	matches!(db.code().and_then(|c| c.parse::<u32>().ok()), Some(code) if code & 0xff == 5 || code & 0xff == 6)
}

/// [`Money`] from an `INTEGER` column of minor units.
///
/// Applies [`bounded`]: `Money::parse` bounds what comes off the wire, this bounds what comes out
/// of the database, which a consumer's own SQL and a data import also write to. Without it an
/// out-of-range row re-enters the unchecked `Add`/`Sub`/`Sum` impls and traps under
/// `overflow-checks` — a dropped connection, not a `400`.
pub(crate) fn read_money(raw: i64) -> ClResult<Money> {
	bounded(raw).map(Money)
}

/// [`Qty`] from an `INTEGER` column scaled 1e6. Bounded for the same reason as [`read_money`].
pub(crate) fn read_qty(raw: i64) -> ClResult<Qty> {
	bounded(raw).map(Qty)
}

/// The `discount_value` column pair member, on `invoices` and on `invoice_lines`. It stays an
/// `Option<i64>` — a `PERCENT` row holds basis points, not minor units — but an `AMOUNT` row is
/// money, and the column has no `CHECK` of any kind, so it is bounded for exactly the reason
/// [`read_money`] is. `mintworks_invoice::storno` negates it (`.map(|v| -v)`), and negating
/// `i64::MIN` traps under `overflow-checks`: a panic inside the storno path rather than a `400`.
pub(crate) fn read_discount_value(raw: Option<i64>) -> ClResult<Option<i64>> {
	raw.map(bounded).transpose()
}

// vim: ts=4
