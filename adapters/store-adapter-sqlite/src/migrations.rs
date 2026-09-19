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

	// saas-billing's two tables are new, so the upgrade is the same `CREATE` pass a fresh
	// install runs.
	if from < 4 {
		sqlx::raw_sql(crate::schema::BILLING).execute(&mut *conn).await.db()?;
	}

	// Without the column a resumed checkout had no URL to hand back, so the customer landed on
	// a draft with nothing to click.
	//
	// `== 4`, not `< 5`: anything below 4 just ran the `CREATE` above, which already has it.
	if from == 4 {
		sqlx::query("ALTER TABLE payments ADD COLUMN redirect_url TEXT")
			.execute(&mut *conn)
			.await
			.db()?;
	}

	// `request_id` is client text, so a global UNIQUE let tenant B's `"sub-2026-01"` collide
	// with tenant A's — a permanent conflict on a key B never used, and a probe for A's keys.
	//
	// `4..=5`: both predate the per-tenant index, and anything below 4 ran the `CREATE` above,
	// which already has it. `== 5` left a database stamped 4 with the global UNIQUE forever.
	// A table rebuild rather than a `DROP INDEX`, because a column-level `UNIQUE` creates an
	// implicit `sqlite_autoindex` that cannot be dropped. The runner holds foreign keys off for
	// the whole migration and runs `PRAGMA foreign_key_check` before commit, so
	// `payment_allocations` survives: `payments.id` is copied verbatim.
	if (4..=5).contains(&from) {
		sqlx::raw_sql(
			"CREATE TABLE payments_new (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,
				tenant_id	INTEGER NOT NULL REFERENCES tenants(id),
				kind		TEXT NOT NULL,
				provider	TEXT,
				provider_ref	TEXT,
				redirect_url	TEXT,
				request_id	TEXT,
				status		TEXT NOT NULL DEFAULT 'PENDING'
						CHECK (status IN ('PENDING','AWAITING_USER','RESERVED','AUTHORIZED',
						                  'SUCCEEDED','PARTIALLY_SUCCEEDED','FAILED','CANCELED',
						                  'EXPIRED','REFUNDED')),
				amount		INTEGER NOT NULL,
				currency	TEXT NOT NULL REFERENCES currencies(code),
				refunded_amount	INTEGER NOT NULL DEFAULT 0,
				received_at	INTEGER,
				ext_ref		TEXT,
				note		TEXT,
				created_by	INTEGER,
				created_at	INTEGER NOT NULL,
				updated_at	INTEGER NOT NULL,
				CHECK (refunded_amount >= 0 AND refunded_amount <= amount)
			);
			 INSERT INTO payments_new
			   SELECT id, uid, tenant_id, kind, provider, provider_ref, redirect_url, request_id,
			          status, amount, currency, refunded_amount, received_at, ext_ref, note,
			          created_by, created_at, updated_at
			     FROM payments;
			 DROP TABLE payments;
			 ALTER TABLE payments_new RENAME TO payments;
			 CREATE INDEX idx_payment_tenant ON payments(tenant_id, id DESC);
			 CREATE INDEX idx_payment_ext    ON payments(ext_ref) WHERE ext_ref IS NOT NULL;
			 CREATE UNIQUE INDEX idx_payment_request_id ON payments(tenant_id, request_id);
			 CREATE UNIQUE INDEX idx_payment_provider_ref
				ON payments(provider, provider_ref) WHERE provider_ref IS NOT NULL;",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}

	// `PENDING` joins `invoices.status`: a draft a gateway payment has locked. SQLite cannot
	// alter a CHECK, so this is the same rebuild the `payments` block above is, and safe for the
	// same reason — the runner holds foreign keys off across the migration and runs
	// `PRAGMA foreign_key_check` before commit, so `payment_allocations`, `invoice_documents`
	// and `nav_submissions` survive: `invoices.id` is copied verbatim.
	//
	// The column list is explicit, not `SELECT *`: version 2 appended `seller_ver` with
	// `ALTER TABLE`, so an upgraded database's column *order* differs from a fresh one's. The
	// rebuild settles that, and hands upgraded databases the unnumbered-row CHECKs the
	// `schema.rs` comment says only fresh installs ever carried.
	if from < 7 {
		sqlx::raw_sql(
			"CREATE TABLE invoices_new (
				id			INTEGER NOT NULL PRIMARY KEY,
				uid			TEXT NOT NULL UNIQUE,
				request_id		TEXT,
				tenant_id		INTEGER NOT NULL REFERENCES tenants(id),
				seller_id		INTEGER NOT NULL REFERENCES sellers(id),
				seller_ver		INTEGER REFERENCES seller_versions(seller_ver),
				billing_party_id	INTEGER REFERENCES billing_parties(id) ON DELETE SET NULL,
				kind			TEXT NOT NULL DEFAULT 'NORMAL'
							CHECK (kind IN ('NORMAL','STORNO')),
				status			TEXT NOT NULL DEFAULT 'DRAFT'
							CHECK (status IN ('DRAFT','PENDING','ISSUED','PAID','STORNOED')),
				series_code		TEXT,
				series_year		INTEGER,
				number			TEXT,
				issued_at		INTEGER,
				fulfilment_date		TEXT,
				due_date		TEXT,
				payment_method		TEXT NOT NULL DEFAULT 'TRANSFER'
							CHECK (payment_method IN ('TRANSFER','CARD','CASH','OTHER')),
				original_invoice_id	INTEGER REFERENCES invoices(id),
				modification_index	INTEGER,
				currency		TEXT NOT NULL REFERENCES currencies(code),
				rate_e6			INTEGER NOT NULL DEFAULT 1000000,
				rate_date		TEXT,
				rate_source		TEXT CHECK (rate_source IN ('MNB','ECB','BANK','MANUAL')),
				huf_rate_e6		INTEGER,
				net			INTEGER NOT NULL DEFAULT 0,
				vat			INTEGER NOT NULL DEFAULT 0,
				gross			INTEGER NOT NULL DEFAULT 0,
				paid_amount		INTEGER NOT NULL DEFAULT 0,
				paid_at			INTEGER,
				vat_note		TEXT,
				notes			TEXT,
				discount_kind		TEXT CHECK (discount_kind IN ('AMOUNT','PERCENT')),
				discount_value		INTEGER,
				buyer_kind		TEXT CHECK (buyer_kind IN ('P','C')),
				buyer_name		TEXT,
				buyer_country		TEXT,
				buyer_tax_number	TEXT,
				buyer_eu_vat_id		TEXT,
				buyer_group_tax_no	TEXT,
				buyer_postcode		TEXT,
				buyer_city		TEXT,
				buyer_street		TEXT,
				buyer_vies_request_id	TEXT,
				buyer_vies_checked_at	INTEGER,
				created_at		INTEGER NOT NULL,
				updated_at		INTEGER NOT NULL,
				version			INTEGER NOT NULL DEFAULT 0,
				CHECK (status IN ('DRAFT','PENDING') OR number          IS NOT NULL),
				CHECK (status IN ('DRAFT','PENDING') OR issued_at       IS NOT NULL),
				CHECK (status IN ('DRAFT','PENDING') OR fulfilment_date IS NOT NULL),
				CHECK (status IN ('DRAFT','PENDING') OR buyer_name      IS NOT NULL),
				CHECK (status IN ('DRAFT','PENDING') OR seller_ver      IS NOT NULL),
				CHECK (kind  <> 'STORNO' OR original_invoice_id IS NOT NULL),
				CHECK (currency <> 'HUF' OR huf_rate_e6 IS NULL),
				CHECK (status IN ('DRAFT','PENDING') OR currency = 'HUF'
				       OR huf_rate_e6 IS NOT NULL),
				CHECK (rate_e6 > 0),
				CHECK (huf_rate_e6 IS NULL OR huf_rate_e6 > 0),
				UNIQUE (tenant_id, request_id),
				CHECK (paid_amount >= 0),
				CHECK (discount_kind IS NULL OR discount_value IS NOT NULL)
			);
			 INSERT INTO invoices_new
			   SELECT id, uid, request_id, tenant_id, seller_id, seller_ver, billing_party_id,
			          kind, status, series_code, series_year, number, issued_at, fulfilment_date,
			          due_date, payment_method, original_invoice_id, modification_index, currency,
			          rate_e6, rate_date, rate_source, huf_rate_e6, net, vat, gross, paid_amount,
			          paid_at, vat_note, notes, discount_kind, discount_value, buyer_kind,
			          buyer_name, buyer_country, buyer_tax_number, buyer_eu_vat_id,
			          buyer_group_tax_no, buyer_postcode, buyer_city, buyer_street,
			          buyer_vies_request_id, buyer_vies_checked_at, created_at, updated_at, version
			     FROM invoices;
			 DROP TABLE invoices;
			 ALTER TABLE invoices_new RENAME TO invoices;
			 CREATE UNIQUE INDEX idx_invoice_number
				ON invoices(seller_id, number) WHERE number IS NOT NULL;
			 CREATE INDEX idx_invoice_tenant    ON invoices(tenant_id, id DESC);
			 CREATE INDEX idx_invoice_due       ON invoices(due_date) WHERE status = 'ISSUED';
			 CREATE INDEX idx_invoice_draft_age ON invoices(updated_at)
				WHERE status IN ('DRAFT','PENDING');
			 CREATE INDEX idx_invoice_party     ON invoices(billing_party_id);
			 CREATE INDEX idx_invoice_issued
				ON invoices(seller_id, issued_at) WHERE number IS NOT NULL;
			 -- integrity, not performance: the only guard against a second storno
			 CREATE UNIQUE INDEX idx_invoice_storno_once
				ON invoices(original_invoice_id) WHERE kind = 'STORNO';",
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
