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
//! The runner stamps `schema_version` once, after `upgrade` returns, so no block sets a version.

use sqlx::SqliteConnection;

use saas_core::error::ClResult;

use crate::util::DbExt;

/// The twelve columns that moved from `sellers` to `seller_versions` in version 2, in the
/// order both the `SELECT` and the `INSERT` below list them.
const MOVED: &str = "name, country, tax_number, group_member_tax_no, eu_vat_id, postcode, \
	 city, street, bank_account, bank_name, small_business, vat_scheme";

pub(crate) async fn upgrade(conn: &mut SqliteConnection, from: i64) -> ClResult<()> {
	// The seller's statutory data moves to the versioned `seller_versions`, and every invoice
	// freezes the version it was issued under. No table is rebuilt: `sellers.id` keeps its
	// meaning, so `invoices.seller_id` and `doc_series.seller_id` are untouched.
	if from < 2 {
		// The invoice-line note and the NAV batching columns ship in the same version as the
		// seller move; each is nullable, so no table is rebuilt for them.
		sqlx::raw_sql(
			"ALTER TABLE invoice_lines   ADD COLUMN note        TEXT;
			 ALTER TABLE nav_submissions ADD COLUMN batch_uid   TEXT;
			 ALTER TABLE nav_submissions ADD COLUMN resolved_at INTEGER;
			 CREATE INDEX idx_nav_submission_batch ON nav_submissions(batch_uid)
				 WHERE batch_uid IS NOT NULL;",
		)
		.execute(&mut *conn)
		.await
		.db()?;

		sqlx::raw_sql(crate::schema::SELLER_VERSIONS).execute(&mut *conn).await.db()?;

		// The existing seller becomes one CURRENT version and no draft — a draft appears the
		// first time somebody edits. `created_at` doubles as `valid_from`: the row has been in
		// force since it was written, which is what the invoices it stamped already say.
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"INSERT INTO seller_versions (seller_id, status, {MOVED}, created_at, valid_from)
			 SELECT id, 'CURRENT', {MOVED}, created_at, created_at FROM sellers"
		)))
		.execute(&mut *conn)
		.await
		.db()?;

		sqlx::raw_sql(
			"ALTER TABLE invoices
			 ADD COLUMN seller_ver INTEGER REFERENCES seller_versions(seller_ver)",
		)
		.execute(&mut *conn)
		.await
		.db()?;

		// The one current version is the best available truth, and is exactly what those
		// invoices render today. Drafts get one too: `issue` overwrites it with the version
		// current at the time, and a DRAFT carries no `seller_ver` constraint either way.
		sqlx::query(
			"UPDATE invoices SET seller_ver =
				(SELECT v.seller_ver FROM seller_versions v WHERE v.seller_id = invoices.seller_id)",
		)
		.execute(&mut *conn)
		.await
		.db()?;

		drop_moved_seller_columns(&mut *conn).await?;
	}

	// The archive XML moves out of `nav_submissions`: inline it left every metadata column
	// behind an overflow chain, since both blobs are declared before them.
	if from < 3 {
		sqlx::raw_sql(crate::schema::NAV_XML).execute(&mut *conn).await.db()?;
		sqlx::raw_sql(
			"INSERT INTO nav_submission_xml (submission_id, request_xml, response_xml)
			   SELECT id, request_xml, response_xml FROM nav_submissions
			    WHERE request_xml IS NOT NULL OR response_xml IS NOT NULL;
			 ALTER TABLE nav_submissions DROP COLUMN request_xml;
			 ALTER TABLE nav_submissions DROP COLUMN response_xml;",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	Ok(())
}

/// `sellers` is referenced by `invoices`, `doc_series` and now `seller_versions`, but the runner
/// holds foreign keys off across the whole migration, so the drop does not retarget them —
/// SQLite only rewrites referencing FKs under `legacy_alter_table = OFF` *and*
/// `foreign_keys = ON`.
///
/// No version probe: `libsqlite3-sys` is pinned `bundled`, so SQLite is never below the 3.35
/// `ALTER TABLE … DROP COLUMN` needs.
async fn drop_moved_seller_columns(conn: &mut SqliteConnection) -> ClResult<()> {
	for column in MOVED.split(',') {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"ALTER TABLE sellers DROP COLUMN {}",
			column.trim()
		)))
		.execute(&mut *conn)
		.await
		.db()?;
	}
	Ok(())
}

// vim: ts=4
