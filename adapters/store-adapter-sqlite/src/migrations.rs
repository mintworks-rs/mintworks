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
use saas_core::ids::SellerId;
use saas_core::types::Timestamp;

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
		// `BILLING` is the *current* DDL, so it already says `org_id` — but every block below
		// is written against the shape version 4 really shipped. Put the column back, and let
		// `from < 8` rename it forward once, down the same path a shipped v4 database takes.
		sqlx::query("ALTER TABLE payments RENAME COLUMN org_id TO tenant_id")
			.execute(&mut *conn)
			.await
			.db()?;
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

	// `tenants` becomes `orgs`, a tree: every existing row is reparented onto a newly seeded root
	// org, and an operator becomes an OWNER membership on that root instead of a column. Every
	// referencing table is rebuilt rather than renamed in place, because the runner holds foreign
	// keys off, so SQLite does not retarget a `REFERENCES tenants(id)` clause — the trap
	// `drop_moved_seller_columns` below documents.
	if from < 8 {
		sqlx::raw_sql(
			"CREATE TABLE orgs (
				id			INTEGER NOT NULL PRIMARY KEY,
				uid			TEXT NOT NULL UNIQUE,
				parent_id		INTEGER REFERENCES orgs(id),
				kind			TEXT NOT NULL CHECK (kind IN ('ROOT','PERSONAL','SHARED')),
				name			TEXT NOT NULL,
				owner_account_id	INTEGER REFERENCES accounts(id),
				billing_currency	TEXT REFERENCES currencies(code),
				status			TEXT NOT NULL DEFAULT 'ACTIVE'
							CHECK (status IN ('ACTIVE','SUSPENDED')),
				created_at		INTEGER NOT NULL
			 );
			 INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, billing_currency, status, created_at)
			   SELECT id, uid, NULL, CASE kind WHEN 'P' THEN 'PERSONAL' ELSE 'SHARED' END, name, owner_account_id, billing_currency, status, created_at FROM tenants;
			 DROP TABLE tenants;
			 CREATE UNIQUE INDEX idx_org_personal
				ON orgs(owner_account_id) WHERE kind = 'PERSONAL';
			 CREATE UNIQUE INDEX idx_org_root   ON orgs(kind) WHERE kind = 'ROOT';
			 CREATE INDEX        idx_org_parent ON orgs(parent_id);",
		)
		.execute(&mut *conn)
		.await
		.db()?;

		// The seed the fresh install runs, so an upgrade and a fresh database mint the root's
		// uid the same way.
		crate::schema::seed_root_org(&mut *conn).await?;

		sqlx::raw_sql(
			"UPDATE orgs SET parent_id = (SELECT id FROM orgs WHERE kind = 'ROOT') WHERE kind <> 'ROOT';
			 CREATE TABLE memberships_new (
				org_id		INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
				account_id	INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
				role		TEXT NOT NULL CHECK (role IN ('OWNER','ADMIN','MEMBER')),
				accepted_at	INTEGER,
				created_at	INTEGER NOT NULL,
				PRIMARY KEY (org_id, account_id)
			 ) WITHOUT ROWID;
			 INSERT INTO memberships_new (org_id, account_id, role, accepted_at, created_at)
			   SELECT tenant_id, account_id, role, accepted_at, created_at FROM memberships;
			 DROP TABLE memberships;
			 ALTER TABLE memberships_new RENAME TO memberships;
			 CREATE INDEX idx_membership_account ON memberships(account_id);
			 INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			   SELECT r.id, a.id, 'OWNER', r.created_at, r.created_at
			     FROM accounts a, (SELECT id, created_at FROM orgs WHERE kind = 'ROOT') r
			    WHERE a.is_operator = 1;
			 ALTER TABLE accounts DROP COLUMN is_operator;
			 ALTER TABLE audit_logs RENAME COLUMN tenant_id TO org_id;
			 CREATE TABLE api_keys_new (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,
				org_id		INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
				account_id	INTEGER NOT NULL REFERENCES accounts(id),
				name		TEXT NOT NULL,
				prefix		TEXT NOT NULL UNIQUE,
				key_hash	TEXT NOT NULL,
				scopes		TEXT NOT NULL DEFAULT '[]',
				last_used_at	INTEGER,
				expires_at	INTEGER,
				revoked_at	INTEGER,
				created_at	INTEGER NOT NULL
			 );
			 INSERT INTO api_keys_new (id, uid, org_id, account_id, name, prefix, key_hash, scopes, last_used_at, expires_at, revoked_at, created_at)
			   SELECT id, uid, tenant_id, account_id, name, prefix, key_hash, scopes, last_used_at, expires_at, revoked_at, created_at FROM api_keys;
			 DROP TABLE api_keys;
			 ALTER TABLE api_keys_new RENAME TO api_keys;
			 CREATE INDEX idx_api_key_org ON api_keys(org_id);
			 CREATE TABLE consents_new (
				id		INTEGER NOT NULL PRIMARY KEY,
				account_id	INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
				org_id		INTEGER REFERENCES orgs(id),
				kind		TEXT NOT NULL
						CHECK (kind IN ('TOS','PRIVACY','EINVOICE','WITHDRAWAL_WAIVER')),
				legal_doc_id	INTEGER REFERENCES legal_docs(id),
				doc_version	TEXT NOT NULL,
				doc_sha256	TEXT NOT NULL,
				granted		INTEGER NOT NULL DEFAULT 1 CHECK (granted IN (0,1)),
				at		INTEGER NOT NULL,
				ip		TEXT,
				user_agent	TEXT,
				withdrawn_at	INTEGER
			 );
			 INSERT INTO consents_new (id, account_id, org_id, kind, legal_doc_id, doc_version, doc_sha256, granted, at, ip, user_agent, withdrawn_at)
			   SELECT id, account_id, tenant_id, kind, legal_doc_id, doc_version, doc_sha256, granted, at, ip, user_agent, withdrawn_at FROM consents;
			 DROP TABLE consents;
			 ALTER TABLE consents_new RENAME TO consents;
			 CREATE INDEX idx_consent_account ON consents(account_id, kind, at DESC);
			 CREATE TABLE billing_parties_new (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,
				org_id		INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
				kind		TEXT NOT NULL CHECK (kind IN ('P','C')),
				name		TEXT NOT NULL,
				country		TEXT NOT NULL,
				tax_number	TEXT,
				eu_vat_id	TEXT,
				group_tax_no	TEXT,
				postcode	TEXT,
				city		TEXT,
				street		TEXT,
				email		TEXT,
				is_default	INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0,1)),
				created_at	INTEGER NOT NULL,
				updated_at	INTEGER NOT NULL
			 );
			 INSERT INTO billing_parties_new (id, uid, org_id, kind, name, country, tax_number, eu_vat_id, group_tax_no, postcode, city, street, email, is_default, created_at, updated_at)
			   SELECT id, uid, tenant_id, kind, name, country, tax_number, eu_vat_id, group_tax_no, postcode, city, street, email, is_default, created_at, updated_at FROM billing_parties;
			 DROP TABLE billing_parties;
			 ALTER TABLE billing_parties_new RENAME TO billing_parties;
			 CREATE UNIQUE INDEX idx_billing_party_tax
				ON billing_parties(org_id, country, tax_number) WHERE tax_number IS NOT NULL;
			 CREATE UNIQUE INDEX idx_billing_party_default
				ON billing_parties(org_id) WHERE is_default = 1;
			 CREATE INDEX idx_billing_party_org
				ON billing_parties(org_id, is_default DESC, name);
			 CREATE TABLE invoices_new (
				id			INTEGER NOT NULL PRIMARY KEY,
				uid			TEXT NOT NULL UNIQUE,
				request_id		TEXT,
				org_id			INTEGER NOT NULL REFERENCES orgs(id),
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
				CHECK (status IN ('DRAFT','PENDING') OR number           IS NOT NULL),
				CHECK (status IN ('DRAFT','PENDING') OR issued_at        IS NOT NULL),
				CHECK (status IN ('DRAFT','PENDING') OR fulfilment_date  IS NOT NULL),
				CHECK (status IN ('DRAFT','PENDING') OR buyer_name       IS NOT NULL),
				CHECK (status IN ('DRAFT','PENDING') OR seller_ver      IS NOT NULL),
				CHECK (kind  <> 'STORNO' OR original_invoice_id IS NOT NULL),
				CHECK (currency <> 'HUF' OR huf_rate_e6 IS NULL),
				CHECK (status IN ('DRAFT','PENDING') OR currency = 'HUF' OR huf_rate_e6 IS NOT NULL),
				CHECK (rate_e6 > 0),
				CHECK (huf_rate_e6 IS NULL OR huf_rate_e6 > 0),
				UNIQUE (org_id, request_id),
				CHECK (paid_amount >= 0),
				CHECK (discount_kind IS NULL OR discount_value IS NOT NULL)
			 );
			 INSERT INTO invoices_new (id, uid, request_id, org_id, seller_id, seller_ver, billing_party_id, kind, status, series_code, series_year, number, issued_at, fulfilment_date, due_date, payment_method, original_invoice_id, modification_index, currency, rate_e6, rate_date, rate_source, huf_rate_e6, net, vat, gross, paid_amount, paid_at, vat_note, notes, discount_kind, discount_value, buyer_kind, buyer_name, buyer_country, buyer_tax_number, buyer_eu_vat_id, buyer_group_tax_no, buyer_postcode, buyer_city, buyer_street, buyer_vies_request_id, buyer_vies_checked_at, created_at, updated_at, version)
			   SELECT id, uid, request_id, tenant_id, seller_id, seller_ver, billing_party_id, kind, status, series_code, series_year, number, issued_at, fulfilment_date, due_date, payment_method, original_invoice_id, modification_index, currency, rate_e6, rate_date, rate_source, huf_rate_e6, net, vat, gross, paid_amount, paid_at, vat_note, notes, discount_kind, discount_value, buyer_kind, buyer_name, buyer_country, buyer_tax_number, buyer_eu_vat_id, buyer_group_tax_no, buyer_postcode, buyer_city, buyer_street, buyer_vies_request_id, buyer_vies_checked_at, created_at, updated_at, version FROM invoices;
			 DROP TABLE invoices;
			 ALTER TABLE invoices_new RENAME TO invoices;
			 CREATE UNIQUE INDEX idx_invoice_number
				ON invoices(seller_id, number) WHERE number IS NOT NULL;
			 CREATE INDEX idx_invoice_org        ON invoices(org_id, id DESC);
			 CREATE INDEX idx_invoice_due        ON invoices(due_date) WHERE status = 'ISSUED';
			 CREATE INDEX idx_invoice_draft_age  ON invoices(updated_at) WHERE status IN ('DRAFT','PENDING');
			 CREATE INDEX idx_invoice_party      ON invoices(billing_party_id);
			 CREATE INDEX idx_invoice_issued
				ON invoices(seller_id, issued_at) WHERE number IS NOT NULL;
			 CREATE UNIQUE INDEX idx_invoice_storno_once
				ON invoices(original_invoice_id) WHERE kind = 'STORNO';
			 CREATE TABLE payments_new (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,
				org_id		INTEGER NOT NULL REFERENCES orgs(id),
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
			 INSERT INTO payments_new (id, uid, org_id, kind, provider, provider_ref, redirect_url, request_id, status, amount, currency, refunded_amount, received_at, ext_ref, note, created_by, created_at, updated_at)
			   SELECT id, uid, tenant_id, kind, provider, provider_ref, redirect_url, request_id, status, amount, currency, refunded_amount, received_at, ext_ref, note, created_by, created_at, updated_at FROM payments;
			 DROP TABLE payments;
			 ALTER TABLE payments_new RENAME TO payments;
			 CREATE INDEX idx_payment_org    ON payments(org_id, id DESC);
			 CREATE UNIQUE INDEX idx_payment_request_id ON payments(org_id, request_id);
			 CREATE INDEX idx_payment_ext    ON payments(ext_ref) WHERE ext_ref IS NOT NULL;
			 CREATE UNIQUE INDEX idx_payment_provider_ref
				ON payments(provider, provider_ref) WHERE provider_ref IS NOT NULL;",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}

	// `sellers` and `services` become org-scoped. Both are full rewrites rather than
	// `ALTER TABLE … ADD COLUMN`: the new columns are NOT NULL with no default, and
	// `services.code`'s column-level UNIQUE leaves an autoindex no `DROP INDEX` can remove.
	if from < 9 {
		sqlx::raw_sql(
			"CREATE TABLE sellers_new (
				id			INTEGER NOT NULL PRIMARY KEY,
				uid			TEXT NOT NULL UNIQUE,
				org_id			INTEGER NOT NULL REFERENCES orgs(id),
				nav_base_url		TEXT NOT NULL,
				nav_login		TEXT,
				series_code		TEXT NOT NULL DEFAULT 'A',
				created_at		INTEGER NOT NULL
			 );",
		)
		.execute(&mut *conn)
		.await
		.db()?;

		// SQLite cannot mint a ULID, so the uid backfill is a Rust loop. `id` is carried over
		// verbatim: `invoices`, `doc_series` and `seller_versions` point at it by value, and the
		// runner holds foreign keys off, so the drop below does not retarget them.
		let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM sellers ORDER BY id")
			.fetch_all(&mut *conn)
			.await
			.db()?;
		for id in ids {
			sqlx::query(
				"INSERT INTO sellers_new (id, uid, org_id, nav_base_url, nav_login, series_code, created_at)
				   SELECT id, ?, (SELECT id FROM orgs WHERE kind = 'ROOT'), nav_base_url, nav_login, series_code, created_at
				     FROM sellers WHERE id = ?",
			)
			.bind(SellerId::generate().into_string())
			.bind(id)
			.execute(&mut *conn)
			.await
			.db()?;
		}

		sqlx::raw_sql(
			"DROP TABLE sellers;
			 ALTER TABLE sellers_new RENAME TO sellers;
			 CREATE INDEX idx_seller_org ON sellers(org_id);
			 CREATE TABLE services_new (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,
				org_id		INTEGER NOT NULL REFERENCES orgs(id),
				code		TEXT,
				name		TEXT NOT NULL,
				description	TEXT,
				unit		TEXT NOT NULL DEFAULT 'db',
				unit_price	INTEGER NOT NULL,
				vat_code	TEXT NOT NULL
						CHECK (vat_code IN ('STD27','RED18','RED05','AAM','TAM','EUFAD37','HO','ATK')),
				active		INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1)),
				created_at	INTEGER NOT NULL,
				updated_at	INTEGER NOT NULL
			 );
			 INSERT INTO services_new (id, uid, org_id, code, name, description, unit, unit_price, vat_code, active, created_at, updated_at)
			   SELECT id, uid, (SELECT id FROM orgs WHERE kind = 'ROOT'), code, name, description, unit, unit_price, vat_code, active, created_at, updated_at FROM services;
			 DROP TABLE services;
			 ALTER TABLE services_new RENAME TO services;
			 CREATE UNIQUE INDEX idx_service_code   ON services(org_id, code);
			 CREATE INDEX        idx_service_active ON services(org_id, active, name);",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}

	// Passkeys. `credential_id UNIQUE` is integrity, not performance: a credential the
	// authenticator made discoverable must resolve to exactly one account, and that lookup is
	// what the usernameless login does before it has any account in hand.
	if from < 10 {
		sqlx::raw_sql(
			"CREATE TABLE webauthn_credentials (
				id\t\tINTEGER NOT NULL PRIMARY KEY,
				account_id\tINTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
				credential_id\tTEXT NOT NULL UNIQUE,
				credential\tTEXT NOT NULL,
				name\t\tTEXT NOT NULL,
				created_at\tINTEGER NOT NULL,
				last_used_at\tINTEGER
			 );
			 CREATE INDEX idx_webauthn_account ON webauthn_credentials(account_id);",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}

	// `payments.expires_at`. Probing rather than a version range, because whether the column is
	// already there does not follow from `from`: below 4 `schema::BILLING` creates it, and the
	// `from < 8` rebuild then recreates `payments` from the v8 column list, which predates it.
	if from < 11 {
		let present: i64 = sqlx::query_scalar(
			"SELECT COUNT(*) FROM pragma_table_info('payments') WHERE name = 'expires_at'",
		)
		.fetch_one(&mut *conn)
		.await
		.db()?;
		if present == 0 {
			sqlx::query("ALTER TABLE payments ADD COLUMN expires_at INTEGER")
				.execute(&mut *conn)
				.await
				.db()?;
		}
	}

	// A data-only version: the previous release's `abandon` gave up on a payment locally and told
	// the gateway nothing, so a gateway that captured it afterwards was found only by re-asking
	// the row — which `CANCELED` no longer is. Back to `PENDING` for one more walk, and a gateway
	// that answers `Canceled` returns it to a terminal state the same way. `created_at` is left
	// alone and the sweep's own horizon still bounds this at seven days; older rows keep
	// `CANCELED` rather than becoming live rows nothing will ever ask about.
	if from < 12 {
		sqlx::query(
			"UPDATE payments SET status = 'PENDING'
			  WHERE status = 'CANCELED' AND expires_at IS NULL
			    AND provider IS NOT NULL AND provider_ref IS NOT NULL
			    AND created_at > ?",
		)
		.bind(Timestamp::now().0 - 604_800)
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
