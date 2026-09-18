//! The framework's current schema, as one `CREATE` pass for a fresh database.
//!
//! This is what the tables look like *now*, not a replay of how they got here — the upgrade
//! path for a database carrying an earlier version lives in [`crate::migrations`]. Editing the
//! DDL below without adding the matching upgrade block there silently gives fresh installs a
//! shape no existing database will ever reach.
//!
//! No trigger appears anywhere in it; [`crate::migrate`]'s module doc has the reason.

use sqlx::SqliteConnection;

use saas_core::error::ClResult;

use crate::migrate::{Fut, Module};
use crate::util::DbExt;

/// Bump this for every change to [`create`], and add the matching block in
/// [`crate::migrations::upgrade`].
pub const VERSION: i64 = 3;

/// The framework's row in `schema_version`.
pub const MODULE_NAME: &str = "saas";

/// Everything `saas-core`, `saas-auth`, `saas-invoice` and `saas-nav` persist. Pass it to
/// `SqliteStore::migrate`, alone or beside the consumer's own modules.
pub const FRAMEWORK: Module = Module { name: MODULE_NAME, version: VERSION, apply };

fn apply(conn: &mut SqliteConnection, from: i64) -> Fut<'_> {
	Box::pin(async move {
		if from == 0 { create(conn).await } else { crate::migrations::upgrade(conn, from).await }
	})
}

/// `saas-auth`'s tables and `saas-invoice`'s reference each other
/// (`tenants.billing_currency` → `currencies(code)`, `invoices.tenant_id` → `tenants(id)`), so no
/// order satisfies both. The runner holds foreign keys off across the whole transaction and runs
/// `PRAGMA foreign_key_check` before the commit, which is what makes the cycle legal.
async fn create(conn: &mut SqliteConnection) -> ClResult<()> {
	sqlx::raw_sql(CORE).execute(&mut *conn).await.db()?;
	sqlx::raw_sql(AUTH).execute(&mut *conn).await.db()?;
	sqlx::raw_sql(INVOICE).execute(&mut *conn).await.db()?;
	sqlx::raw_sql(SELLER_VERSIONS).execute(&mut *conn).await.db()?;
	sqlx::raw_sql(NAV).execute(&mut *conn).await.db()?;
	sqlx::raw_sql(NAV_XML).execute(&mut *conn).await.db()?;
	Ok(())
}

/// saas-core: the framework's own tables.
const CORE: &str = r#"
CREATE TABLE vars (
	name		TEXT NOT NULL PRIMARY KEY,
	value		TEXT
) WITHOUT ROWID;

CREATE TABLE settings (
	key		TEXT NOT NULL PRIMARY KEY,
	value		TEXT NOT NULL,			-- the registry owns the type
	updated_at	INTEGER NOT NULL,
	updated_by	INTEGER				-- accounts.id; no FK, survives anonymization
) WITHOUT ROWID;

CREATE TABLE secrets (
	key		TEXT NOT NULL PRIMARY KEY,
	nonce		BLOB NOT NULL,			-- 12 bytes, fresh per write
	ciphertext	BLOB NOT NULL,
	updated_at	INTEGER NOT NULL,
	updated_by	INTEGER				-- accounts.id; no FK
) WITHOUT ROWID;

-- There is deliberately no `max_attempts` column: the runner takes its ceiling from the
-- `jobs.max_attempts.<KIND>` settings family, so a per-row `DEFAULT 8` contradicted the number
-- an operator actually changes.
CREATE TABLE jobs (
	id		INTEGER NOT NULL PRIMARY KEY,
	kind		TEXT NOT NULL,
	payload		TEXT NOT NULL DEFAULT '{}',	-- JSON
	dedup_key	TEXT UNIQUE,			-- NULL = no dedup; NULLs are distinct in SQLite
	status		TEXT NOT NULL DEFAULT 'PENDING'
			CHECK (status IN ('PENDING','RUNNING','DONE','FAILED')),
	run_at		INTEGER NOT NULL,
	attempts	INTEGER NOT NULL DEFAULT 0,
	last_error	TEXT,				-- the human message
	created_at	INTEGER NOT NULL,
	done_at		INTEGER,
	-- `last_error` is the human message and nothing more: "should this job be retried" used
	-- to be re-derived at each call site and never recorded. `saas_core::Error::retry()`
	-- answers that from the error itself, and `Runner::fail`/`Runner::terminate` write the
	-- stable errCode (`Error::parts()`) that classified it beside the message. Nullable,
	-- because the one live path that writes NULL is `tick`'s "no handler registered for this
	-- kind", where no `Error` stands behind the failure at all.
	err_code	TEXT,
	-- When the claim happened. `job_reclaim` had no age predicate, so a second process starting
	-- during a rolling deploy flipped a live sibling's `RUNNING` rows back to `PENDING` and both
	-- processes ran the same handler.
	claimed_at	INTEGER
);

-- `(run_at, id)`, not `(run_at)`: the latter cannot serve `ORDER BY run_at, id`, so the planner
-- took `idx_job_status` and sorted the whole PENDING backlog on the single writer connection
-- once per claim.
CREATE INDEX idx_job_claim  ON jobs(run_at, id) WHERE status = 'PENDING';
CREATE INDEX idx_job_status ON jobs(status, created_at);

-- `saas_invoice::store::issued_without_document` correlates
-- `j.payload = '{"invoiceId":' || i.id || '}'` as a subquery over the whole `jobs` table, once
-- per issued invoice with no document row, and `CoreStore::job_cancel`/`job_redrive` address a
-- row by the same pair. Retention (`CoreStore::job_sweep`, driven by the daily `SWEEP_JOBS`
-- tick) bounds the table; this bounds the correlation.
CREATE INDEX idx_job_kind_payload ON jobs (kind, payload);

-- Append-only record of every mutation, written by `crate::audit::log`. No FK to
-- accounts or tenants: the log outlives both.
CREATE TABLE audit_logs (
	id		INTEGER NOT NULL PRIMARY KEY,
	at		INTEGER NOT NULL,
	account_id	INTEGER,			-- no FK: the log outlives the account
	tenant_id	INTEGER,			-- no FK: same reason
	ip		TEXT,
	entity		TEXT NOT NULL,			-- 'invoice', 'payment', 'secret', …
	entity_id	TEXT,				-- the uid, or the natural key
	action		TEXT NOT NULL,			-- 'ISSUE', 'STORNO', 'REFUND', 'AUDIT_EXPORT', …
	detail		TEXT,				-- JSON
	request_id	TEXT				-- ties to the structured log line
);

CREATE INDEX idx_audit_log_at      ON audit_logs(at DESC);
CREATE INDEX idx_audit_log_entity  ON audit_logs(entity, entity_id, at DESC);
CREATE INDEX idx_audit_log_account ON audit_logs(account_id, at DESC);

-- Append-only is a property of the trait's shape, not of a trigger: `CoreStore::audit_log` is
-- the only method there is. Two `RAISE(ABORT)` triggers used to guard these rows and were
-- dropped — they defended against an actor who could `DROP TRIGGER` first, and against a Rust
-- caller that does not exist. See `migrate.rs`'s module doc for the general rule.
"#;

/// saas-auth — accounts, tenants, memberships, api keys, TOTP, legal docs, consents.
const AUTH: &str = r"
CREATE TABLE accounts (
	id		INTEGER NOT NULL PRIMARY KEY,
	uid		TEXT NOT NULL UNIQUE,		-- 'acc_<ULID>'
	email		TEXT NOT NULL UNIQUE,		-- stored lowercased+trimmed; [GDPR]
	pwd_hash	TEXT,				-- argon2id; NULL = invited, no password yet
	name		TEXT,				-- [GDPR]
	locale		TEXT NOT NULL DEFAULT 'hu',
	status		TEXT NOT NULL DEFAULT 'PENDING'
			CHECK (status IN ('PENDING','ACTIVE','SUSPENDED','ANONYMIZED')),
	token_epoch	INTEGER NOT NULL DEFAULT 0,	-- bump to invalidate this account's live JWTs
	is_operator	INTEGER NOT NULL DEFAULT 0 CHECK (is_operator IN (0,1)),
	failed_logins	INTEGER NOT NULL DEFAULT 0,
	locked_until	INTEGER,			-- lockout ladder
	activated_at	INTEGER,
	last_login_at	INTEGER,
	anonymized_at	INTEGER,
	created_at	INTEGER NOT NULL
);

CREATE INDEX idx_account_status ON accounts(status);

CREATE TABLE tenants (
	id			INTEGER NOT NULL PRIMARY KEY,
	uid			TEXT NOT NULL UNIQUE,	-- 'tnt_<ULID>'
	kind			TEXT NOT NULL CHECK (kind IN ('P','O')),
	name			TEXT NOT NULL,
	owner_account_id	INTEGER NOT NULL REFERENCES accounts(id),
	billing_currency	TEXT REFERENCES currencies(code),	-- NULL = setting `currency.base`
	status			TEXT NOT NULL DEFAULT 'ACTIVE'
				CHECK (status IN ('ACTIVE','SUSPENDED')),
	created_at		INTEGER NOT NULL
);

-- exactly one personal tenant per account; organisations are unconstrained
CREATE UNIQUE INDEX idx_tenant_personal
	ON tenants(owner_account_id) WHERE kind = 'P';

CREATE TABLE memberships (
	tenant_id	INTEGER NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
	account_id	INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
	role		TEXT NOT NULL CHECK (role IN ('OWNER','ADMIN','MEMBER')),
	-- NULL while the invitation is outstanding. `token::pick_tenant` skips those, so an
	-- invite nobody accepted can never become the invitee's default tenant and quietly
	-- collect their data. Switching into it explicitly (POST /api/tenant/switch) is fine.
	accepted_at	INTEGER,
	created_at	INTEGER NOT NULL,
	PRIMARY KEY (tenant_id, account_id)
) WITHOUT ROWID;

CREATE INDEX idx_membership_account ON memberships(account_id);

CREATE TABLE api_keys (
	id		INTEGER NOT NULL PRIMARY KEY,
	uid		TEXT NOT NULL UNIQUE,		-- 'key_<ULID>'
	tenant_id	INTEGER NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
	account_id	INTEGER NOT NULL REFERENCES accounts(id),
	name		TEXT NOT NULL,
	prefix		TEXT NOT NULL UNIQUE,		-- first 8 chars of the key: the lookup handle
	key_hash	TEXT NOT NULL,			-- argon2id of the full key
	scopes		TEXT NOT NULL DEFAULT '[]',	-- JSON array of route scopes
	last_used_at	INTEGER,
	expires_at	INTEGER,
	revoked_at	INTEGER,
	created_at	INTEGER NOT NULL
);

CREATE INDEX idx_api_key_tenant ON api_keys(tenant_id);

CREATE TABLE totp_credentials (
	account_id	INTEGER NOT NULL PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
	secret_nonce	BLOB NOT NULL,
	secret_enc	BLOB NOT NULL,			-- AES-256-GCM under HKDF(MASTER_KEY, 'totp')
	digits		INTEGER NOT NULL DEFAULT 6,
	period		INTEGER NOT NULL DEFAULT 30,
	recovery_hashes	TEXT NOT NULL DEFAULT '[]',	-- JSON array of argon2id hashes; consumed by removal
	last_used_step	INTEGER,			-- replay guard: the last accepted time step
	confirmed_at	INTEGER,			-- NULL = enrolment begun, not yet verified
	created_at	INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE legal_docs (
	id		INTEGER NOT NULL PRIMARY KEY,
	kind		TEXT NOT NULL
			CHECK (kind IN ('TOS','PRIVACY','EINVOICE','WITHDRAWAL_WAIVER')),
	locale		TEXT NOT NULL,			-- 'hu', 'en'
	version		TEXT NOT NULL,			-- '2026-09-01'
	title		TEXT NOT NULL,
	body		TEXT NOT NULL,			-- Markdown, verbatim as presented
	sha256		TEXT NOT NULL,			-- hex SHA-256 of `body`
	effective_from	INTEGER NOT NULL,
	created_at	INTEGER NOT NULL,
	UNIQUE (kind, locale, version)
);

CREATE TABLE consents (
	id		INTEGER NOT NULL PRIMARY KEY,
	account_id	INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
	tenant_id	INTEGER REFERENCES tenants(id),	-- NULL for account-level consent
	kind		TEXT NOT NULL
			CHECK (kind IN ('TOS','PRIVACY','EINVOICE','WITHDRAWAL_WAIVER')),
	legal_doc_id	INTEGER REFERENCES legal_docs(id),
	doc_version	TEXT NOT NULL,			-- copied, survives a legal_doc purge
	doc_sha256	TEXT NOT NULL,			-- copied, ditto
	granted		INTEGER NOT NULL DEFAULT 1 CHECK (granted IN (0,1)),
	at		INTEGER NOT NULL,
	ip		TEXT,				-- evidence; retained through anonymization
	user_agent	TEXT,
	withdrawn_at	INTEGER
);

CREATE INDEX idx_consent_account ON consents(account_id, kind, at DESC);
";

/// `seller_versions` alone, as its own constant: [`crate::migrations`]'s `from < 2` block
/// execs this same text, so the upgraded and the fresh shape cannot drift.
pub(crate) const SELLER_VERSIONS: &str = r#"
-- The seller's statutory data, versioned. `invoices.seller_ver` freezes the version that was
-- CURRENT at ISSUE, so editing the seller never rewrites an issued invoice — the same rule the
-- `buyer_*` snapshot gives the buyer.
--
-- An edit does not make a version: it rewrites the one DRAFT row. Only `publish` ("élesít")
-- archives the CURRENT row and promotes the draft, so a half-typed address is never captured
-- by an invoice issued mid-edit. A CURRENT or ARCHIVED row is never updated again — the same
-- immutability rule as an ISSUED invoice, and it lives in `saas-invoice`, not in a trigger
-- (arch-9).
-- `seller_ver`, not `id`: this table is the one place where a bare `id` would be genuinely
-- ambiguous — `invoices` ends up carrying both `seller_id` (the identity, for numbering and
-- NAV batching) and `seller_ver` (the frozen version). A deliberate, documented departure from
-- the house `INTEGER PRIMARY KEY id` convention; do not "fix" it back.
CREATE TABLE seller_versions (
	seller_ver		INTEGER NOT NULL PRIMARY KEY,
	seller_id		INTEGER NOT NULL REFERENCES sellers(id),
	status			TEXT NOT NULL DEFAULT 'DRAFT'
				CHECK (status IN ('DRAFT','CURRENT','ARCHIVED')),
	name			TEXT NOT NULL,
	country			TEXT NOT NULL DEFAULT 'HU',	-- ISO-3166-1 alpha-2
	tax_number		TEXT NOT NULL,			-- HU: 11 digits, punctuation stripped
	group_member_tax_no	TEXT,
	eu_vat_id		TEXT,				-- 'HU12345678'
	postcode		TEXT NOT NULL,
	city			TEXT NOT NULL,
	street			TEXT NOT NULL,
	bank_account		TEXT,				-- IBAN, or HU 16/24 digits
	bank_name		TEXT,
	small_business		INTEGER NOT NULL DEFAULT 0 CHECK (small_business IN (0,1)),
	vat_scheme		TEXT NOT NULL DEFAULT 'NORMAL'
				CHECK (vat_scheme IN ('NORMAL','KATA','ALANYI_MENTES')),
	created_at		INTEGER NOT NULL,		-- when the draft was opened
	valid_from		INTEGER,			-- when it was published; NULL while DRAFT
	superseded_at		INTEGER,			-- when the next one was published
	-- The two timestamps *are* the audit trail ("which version was in force on 2026-03-14"),
	-- and `status` is the queryable name for the same fact. These two keep them from drifting
	-- apart, which is the only way a redundant column earns its place.
	CHECK ((status = 'DRAFT')    = (valid_from    IS NULL)),
	CHECK ((status = 'ARCHIVED') = (superseded_at IS NOT NULL))
);

-- Both are integrity, not performance: at most one open draft and at most one live version
-- per seller. `adapter-contract.md` material.
CREATE UNIQUE INDEX idx_seller_version_draft
	ON seller_versions(seller_id) WHERE status = 'DRAFT';

CREATE UNIQUE INDEX idx_seller_version_current
	ON seller_versions(seller_id) WHERE status = 'CURRENT';

CREATE INDEX idx_seller_version_history
	ON seller_versions(seller_id, valid_from DESC) WHERE status <> 'DRAFT';
"#;

/// saas-invoice — currencies and rates, sellers, parties, services, the numbering series and
/// the invoice itself.
const INVOICE: &str = r#"
-- Enabled currencies and how a base-currency price converts into them.
CREATE TABLE currencies (
	code			TEXT NOT NULL PRIMARY KEY,	-- ISO-4217 alpha-3
	price_round_step	INTEGER NOT NULL DEFAULT 1,	-- minor units; HUF display step = 100
	mode			TEXT NOT NULL DEFAULT 'OFFICIAL'
				CHECK (mode IN ('FIXED','OFFICIAL')),
	fixed_rate_e6		INTEGER,			-- base units per 1 of this currency, when mode='FIXED'
	fee_bp			INTEGER NOT NULL DEFAULT 0,	-- conversion markup, basis points
	enabled			INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
	CHECK (mode <> 'FIXED' OR fixed_rate_e6 IS NOT NULL),
	CHECK (fixed_rate_e6 IS NULL OR fixed_rate_e6 > 0),
	-- A 0 makes `round_to_step` a 500 on every catalogue line while `ensure_price_on_step`
	-- waves ad-hoc prices through: the two halves of one rule disagreed.
	CHECK (price_round_step > 0),
	-- A negative markup zeroes the numerator in `currency::price_in`, so a 40 000 HUF item
	-- priced at 0.00 and issued. The 100x ceiling also keeps the i128 product from trapping.
	CHECK (fee_bp >= 0 AND fee_bp <= 1000000)
) WITHOUT ROWID;

-- The default base currency. A deployment with a different currency.base seeds its own.
INSERT OR IGNORE INTO currencies (code, price_round_step, mode, fixed_rate_e6)
	VALUES ('HUF', 100, 'FIXED', 1000000);

-- Daily published rates, one row per (pair, date, source). 'EURHUF' is 1 EUR in HUF.
CREATE TABLE currency_rates (
	pair		TEXT NOT NULL,
	date		TEXT NOT NULL,			-- 'YYYY-MM-DD', the publication date
	source		TEXT NOT NULL CHECK (source IN ('MNB','ECB','BANK','MANUAL')),
	rate_e6		INTEGER NOT NULL CHECK (rate_e6 > 0),
	fetched_at	INTEGER NOT NULL,
	PRIMARY KEY (pair, date, source)
) WITHOUT ROWID;

CREATE INDEX idx_currency_rate_lookup ON currency_rates(pair, source, date DESC);

-- VIES results, cached settings['vies.cache_days']. A stale row is a cache miss, not valid=0.
CREATE TABLE vies_checks (
	eu_vat_id	TEXT NOT NULL PRIMARY KEY,	-- country prefix + number, uppercased
	valid		INTEGER NOT NULL CHECK (valid IN (0,1)),
	name		TEXT,
	address		TEXT,
	request_id	TEXT,				-- VIES consultation number: the proof of the check
	checked_at	INTEGER NOT NULL
) WITHOUT ROWID;

-- The SaaS operator as a row, so multi-seller later is not a migration. seller_id = 1 is
-- hardcoded at call sites in v1. NAV credentials live in `secrets`, never here, and
-- nav_software_id is deliberately absent: the software identity is a `settings` block.
--
-- Only the **operational** half is here; the statutory supplier data is versioned in
-- `seller_versions`. These three columns are read live on purpose: a filing redriven days
-- later must reach today's endpoint under today's technical user, and a storno must not land
-- in a different series from its original.
CREATE TABLE sellers (
	id			INTEGER NOT NULL PRIMARY KEY,
	nav_base_url		TEXT NOT NULL,			-- wins over settings['nav.base_url']; '' falls back to it
	nav_login		TEXT,				-- technical user login name
	series_code		TEXT NOT NULL DEFAULT 'A',	-- default series for new invoices
	created_at		INTEGER NOT NULL
);

-- The invoice recipient, owned by a tenant and never globally deduped: two tenants billing
-- the same company hold two rows, and editing one never touches the other. Editing a party
-- never rewrites an issued invoice, which froze its own copy at ISSUE.
CREATE TABLE billing_parties (
	id		INTEGER NOT NULL PRIMARY KEY,
	uid		TEXT NOT NULL UNIQUE,		-- 'prt_<ULID>'
	tenant_id	INTEGER NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
	kind		TEXT NOT NULL CHECK (kind IN ('P','C')),	-- private person | company
	name		TEXT NOT NULL,			-- [GDPR when kind='P']
	country		TEXT NOT NULL,			-- ISO-3166-1 alpha-2
	tax_number	TEXT,				-- HU: 11 digits (8 core + VAT code + county)
	eu_vat_id	TEXT,				-- country prefix + number, VIES-checkable
	group_tax_no	TEXT,
	postcode	TEXT,				-- [GDPR when kind='P']
	city		TEXT,				-- [GDPR when kind='P']
	street		TEXT,				-- [GDPR when kind='P']
	email		TEXT,				-- [GDPR when kind='P']
	is_default	INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0,1)),
	created_at	INTEGER NOT NULL,
	updated_at	INTEGER NOT NULL
);

CREATE UNIQUE INDEX idx_billing_party_tax
	ON billing_parties(tenant_id, country, tax_number) WHERE tax_number IS NOT NULL;

CREATE UNIQUE INDEX idx_billing_party_default
	ON billing_parties(tenant_id) WHERE is_default = 1;

-- `list_parties` and `anonymize_account` both filter on `tenant_id` alone, which neither
-- partial index above can serve, so both full-scanned the table across every tenant.
-- `is_default DESC, name` is included in that order so `list_parties`' `ORDER BY is_default
-- DESC, name` reads straight off the index rather than through a temp B-tree — the DESC is
-- load-bearing, `(tenant_id, name)` alone still sorted.
CREATE INDEX idx_billing_party_tenant
	ON billing_parties(tenant_id, is_default DESC, name);

-- Priced master data, in the base currency. `vat_code` is only the default: the effective
-- code for a given invoice comes from `taxrule::determine`: the buyer's zone can override it.
CREATE TABLE services (
	id		INTEGER NOT NULL PRIMARY KEY,
	uid		TEXT NOT NULL UNIQUE,		-- 'svc_<ULID>'
	code		TEXT UNIQUE,			-- stable human handle, e.g. 'PLAN_PRO_M'
	name		TEXT NOT NULL,
	description	TEXT,
	unit		TEXT NOT NULL DEFAULT 'db',	-- NAV unitOfMeasure=OWN + unitOfMeasureOwn
	unit_price	INTEGER NOT NULL,		-- Money, base-currency minor units
	vat_code	TEXT NOT NULL
			CHECK (vat_code IN ('STD27','RED18','RED05','AAM','TAM','EUFAD37','HO','ATK')),
	active		INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1)),
	created_at	INTEGER NOT NULL,
	updated_at	INTEGER NOT NULL
);

CREATE INDEX idx_service_active ON services(active, name);

-- The gapless counter, one row per (seller, kind, code, year). `kind` is an open string, not
-- an enum: a later voucher type is a new value and a new row, never a migration. Allocation
-- happens only inside the issue transaction, so a rollback consumes no number.
CREATE TABLE doc_series (
	seller_id	INTEGER NOT NULL REFERENCES sellers(id),
	kind		TEXT NOT NULL,			-- 'INVOICE'; 'ORDER', 'QUOTE', … later
	code		TEXT NOT NULL,			-- 'A'
	year		INTEGER NOT NULL,		-- 2026
	next_no		INTEGER NOT NULL DEFAULT 1,
	format		TEXT NOT NULL DEFAULT '{code}{year}/{no:06}',
	PRIMARY KEY (seller_id, kind, code, year)
) WITHOUT ROWID;

-- The central row: mutable as DRAFT, frozen from ISSUED on. The buyer snapshot makes the
-- invoice self-sufficient, which is why billing_party_id is ON DELETE SET NULL — deleting a
-- customer record must neither be blocked by nor cascade into five years of invoices.
CREATE TABLE invoices (
	id			INTEGER NOT NULL PRIMARY KEY,
	uid			TEXT NOT NULL UNIQUE,		-- 'inv_<ULID>'
	request_id		TEXT,				-- consumer idempotency key, per tenant
	tenant_id		INTEGER NOT NULL REFERENCES tenants(id),
	seller_id		INTEGER NOT NULL REFERENCES sellers(id),
	-- The seller as frozen at ISSUE — always a CURRENT version at the time, never a DRAFT.
	-- `seller_id` above stays the identity: numbering, NAV batching and the export ranges all
	-- key on it and must not split when the seller is edited.
	seller_ver		INTEGER REFERENCES seller_versions(seller_ver),
	billing_party_id	INTEGER REFERENCES billing_parties(id) ON DELETE SET NULL,
	kind			TEXT NOT NULL DEFAULT 'NORMAL'
				CHECK (kind IN ('NORMAL','STORNO')),
	status			TEXT NOT NULL DEFAULT 'DRAFT'
				CHECK (status IN ('DRAFT','ISSUED','PAID','STORNOED')),

	series_code		TEXT,				-- frozen at ISSUE
	series_year		INTEGER,			-- frozen at ISSUE
	number			TEXT,				-- rendered; NULL while DRAFT
	issued_at		INTEGER,
	fulfilment_date		TEXT,				-- 'YYYY-MM-DD'; Áfa tv. 55-58. §
	due_date		TEXT,				-- 'YYYY-MM-DD'
	payment_method		TEXT NOT NULL DEFAULT 'TRANSFER'
				CHECK (payment_method IN ('TRANSFER','CARD','CASH','OTHER')),

	original_invoice_id	INTEGER REFERENCES invoices(id),	-- STORNO -> the cancelled invoice
	modification_index	INTEGER,			-- always NULL in v1; MODIFY-ready

	currency		TEXT NOT NULL REFERENCES currencies(code),
	rate_e6			INTEGER NOT NULL DEFAULT 1000000,	-- base units per 1 of invoice currency, frozen
							-- (huf_rate_e6 below runs the other way)
	rate_date		TEXT,				-- = fulfilment_date, or the last publication before it
	rate_source		TEXT CHECK (rate_source IN ('MNB','ECB','BANK','MANUAL')),
	huf_rate_e6		INTEGER,			-- invoice currency -> HUF; NULL iff currency='HUF'

	net			INTEGER NOT NULL DEFAULT 0,	-- = SUM(invoice_vat_groups.net)
	vat			INTEGER NOT NULL DEFAULT 0,	-- = SUM(invoice_vat_groups.vat)
	gross			INTEGER NOT NULL DEFAULT 0,	-- = net + vat
	paid_amount		INTEGER NOT NULL DEFAULT 0,	-- = SUM(payment_allocations.amount)
	paid_at			INTEGER,			-- set when paid_amount first reaches gross

	vat_note		TEXT,				-- mandatory legal text, from Verdict::note_key
	notes			TEXT,

	-- The invoice-level discount, apportioned across the lines pro rata by net. Kept here
	-- because the apportioned share lands in `invoice_lines.discount_amount` and is
	-- indistinguishable there from the line's own discount, so re-pricing at ISSUE or on a
	-- line edit could not reconstruct it and silently dropped it. Same spelling as the
	-- `invoice_lines` pair below.
	discount_kind		TEXT CHECK (discount_kind IN ('AMOUNT','PERCENT')),
	discount_value		INTEGER,			-- AMOUNT: minor units; PERCENT: basis points

	-- ---- frozen buyer snapshot: written at ISSUE, never updated, never erased ----
	buyer_kind		TEXT CHECK (buyer_kind IN ('P','C')),
	buyer_name		TEXT,
	buyer_country		TEXT,
	buyer_tax_number	TEXT,
	buyer_eu_vat_id		TEXT,
	buyer_group_tax_no	TEXT,
	buyer_postcode		TEXT,
	buyer_city		TEXT,
	buyer_street		TEXT,
	-- The VIES verdict that justified reverse charge. `vies_checks` is keyed by EU VAT id and
	-- upserted on every refresh, so it cannot serve as the evidence for a five-year-old EUFAD37.
	buyer_vies_request_id	TEXT,				-- VIES consultation number
	buyer_vies_checked_at	INTEGER,

	created_at		INTEGER NOT NULL,
	updated_at		INTEGER NOT NULL,
	-- Monotonic optimistic-concurrency token. `updated_at` is unix seconds, so two edits
	-- inside one second both matched `AND updated_at = ?` and the second silently won.
	version			INTEGER NOT NULL DEFAULT 0,

	CHECK (status = 'DRAFT' OR number           IS NOT NULL),
	CHECK (status = 'DRAFT' OR issued_at        IS NOT NULL),
	CHECK (status = 'DRAFT' OR fulfilment_date  IS NOT NULL),
	CHECK (status = 'DRAFT' OR buyer_name       IS NOT NULL),
	-- Fresh installs only: `ALTER TABLE` cannot add a CHECK, so an upgraded database from v1
	-- carries this rule in `saas-invoice` alone. Deliberate — the alternative was a 12-step
	-- rebuild of the widest table in the schema.
	CHECK (status = 'DRAFT' OR seller_ver      IS NOT NULL),
	CHECK (kind  <> 'STORNO' OR original_invoice_id IS NOT NULL),
	CHECK (currency <> 'HUF' OR huf_rate_e6 IS NULL),
	CHECK (status = 'DRAFT' OR currency = 'HUF' OR huf_rate_e6 IS NOT NULL),
	-- A zero rate multiplies the statutory HUF figures (Áfa tv. 172. §) to 0.00 and freezes
	-- them onto an ISSUED invoice.
	CHECK (rate_e6 > 0),
	CHECK (huf_rate_e6 IS NULL OR huf_rate_e6 > 0),
	-- Per tenant, not global. A globally unique `request_id` let one tenant squat another's
	-- natural idempotency keys ("sub-2026-01"), and the loser's create then answered 404
	-- permanently. SQLite treats NULLs as distinct in a unique index, so invoices with no
	-- `request_id` stay unconstrained.
	UNIQUE (tenant_id, request_id),

	CHECK (paid_amount >= 0),
	CHECK (discount_kind IS NULL OR discount_value IS NOT NULL)
);

CREATE UNIQUE INDEX idx_invoice_number
	ON invoices(seller_id, number) WHERE number IS NOT NULL;

CREATE INDEX idx_invoice_tenant     ON invoices(tenant_id, id DESC);
CREATE INDEX idx_invoice_due        ON invoices(due_date) WHERE status = 'ISSUED';
-- `sweep_drafts` deletes *abandoned* drafts, so it keys on `updated_at`: a cart created a
-- month ago and edited this morning is not abandoned.
CREATE INDEX idx_invoice_draft_age  ON invoices(updated_at) WHERE status = 'DRAFT';
CREATE INDEX idx_invoice_party      ON invoices(billing_party_id);

-- The audit export's index — `NavStore::export_ids_by_date`'s `BY_DATE`, which filters
-- `seller_id = ? AND number IS NOT NULL AND issued_at >= ? AND issued_at < ?`. Its shape is
-- the query's: `seller_id` leads, `issued_at` ranges, and the partial predicate is the
-- query's own `number IS NOT NULL`, which is the issued test every export selection uses.
-- SQLite uses a partial index only when the query's WHERE terms *imply* its predicate, so a
-- `WHERE status <> 'DRAFT'` version of this was never used at all and a one-month export
-- scanned every invoice ever issued.
CREATE INDEX idx_invoice_issued
	ON invoices(seller_id, issued_at) WHERE number IS NOT NULL;

-- an invoice can be cancelled at most once
CREATE UNIQUE INDEX idx_invoice_storno_once
	ON invoices(original_invoice_id) WHERE kind = 'STORNO';

-- Immutability after ISSUE is the application's, not the database's. Four triggers used to
-- enforce it here — a status-transition whitelist, a frozen-column list, a delete guard and
-- the line/group guards below — and each now has a named owner: every `UPDATE invoices` in
-- `src/invoice.rs` carries `AND status = 'DRAFT'` or is one of the three writes an issued
-- invoice still permits, `mark_paid`/`mark_stornoed` are the only transitions the trait can
-- name, and `delete_draft`/`sweep_drafts` both carry the same predicate. The columns an issued
-- invoice may still change are status, paid_amount, paid_at, notes, billing_party_id (to NULL,
-- by the FK) and updated_at. See `migrate.rs`'s module doc for why none of it is a trigger.

-- One row per billed item, frozen with its invoice. `vat` and `gross` here are DISPLAY
-- values apportioned back out of the group figure — never a source of truth. Summing them
-- can differ from the group VAT by a few fillér and NAV rejects that.
CREATE TABLE invoice_lines (
	id			INTEGER NOT NULL PRIMARY KEY,
	invoice_id		INTEGER NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
	line_no			INTEGER NOT NULL,		-- 1-based; NAV lineNumber
	service_id		INTEGER REFERENCES services(id) ON DELETE SET NULL,
	description		TEXT NOT NULL,
	unit			TEXT NOT NULL,
	qty			INTEGER NOT NULL,		-- Qty, scaled 1e6
	unit_price		INTEGER NOT NULL,		-- Money, invoice-currency minor units
	discount_kind		TEXT CHECK (discount_kind IN ('AMOUNT','PERCENT')),
	discount_value		INTEGER,			-- AMOUNT: minor units; PERCENT: basis points
	discount_amount		INTEGER NOT NULL DEFAULT 0,	-- resolved; NAV lineDiscountValue
	discount_description	TEXT,
	net			INTEGER NOT NULL,		-- round_half_up(unit_price*qty/1e6) - discount_amount
	vat_code		TEXT NOT NULL
				CHECK (vat_code IN ('STD27','RED18','RED05','AAM','TAM','EUFAD37','HO','ATK')),
	vat_rate_bp		INTEGER NOT NULL,
	vat			INTEGER NOT NULL DEFAULT 0,	-- INFORMATIONAL ONLY
	gross			INTEGER NOT NULL DEFAULT 0,	-- net + vat, informational
	-- The caller's free text on a line, kept apart from `description` so that `draft::resolve`
	-- overwriting a catalogue line's description cannot destroy it.
	note			TEXT,
	UNIQUE (invoice_id, line_no),
	CHECK (discount_kind IS NULL OR discount_value IS NOT NULL)
	-- No `discount_amount >= 0`: a STORNO line negates every monetary figure, discount
	-- included, and this constraint cannot reach the parent invoice to learn that it is
	-- one — it made discounted invoices impossible to cancel at all. The sign is checked
	-- where the untrusted value enters instead, in `routes::discount_of`.
);

-- The ON DELETE CASCADE from `invoices` therefore only ever fires for drafts, which is what
-- SWEEP_DRAFTS relies on. `insert_lines` and `replace_lines` are private to `src/invoice.rs`
-- and every caller holds a `status = 'DRAFT'` predicate in the same transaction before
-- reaching them; that is the guard, not a trigger.

-- The authoritative per-rate-group figures. Grouped by vat_code, not by rate: AAM and TAM
-- are both 0 bp but must be reported separately to NAV.
CREATE TABLE invoice_vat_groups (
	invoice_id	INTEGER NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
	vat_code	TEXT NOT NULL
			CHECK (vat_code IN ('STD27','RED18','RED05','AAM','TAM','EUFAD37','HO','ATK')),
	vat_rate_bp	INTEGER NOT NULL,
	net		INTEGER NOT NULL,		-- SUM of the group's line nets
	vat		INTEGER NOT NULL,		-- round_half_up(net * vat_rate_bp / 10000)
	gross		INTEGER NOT NULL,		-- net + vat
	net_huf		INTEGER,
	vat_huf		INTEGER,			-- MANDATORY when currency <> 'HUF'
	gross_huf	INTEGER,
	PRIMARY KEY (invoice_id, vat_code)
) WITHOUT ROWID;

-- `vat_huf` is Áfa tv. 172. §, not a convenience: the passed-on VAT must appear in HUF on a
-- foreign-currency invoice. Nothing here can express that — a `CHECK` cannot reach the parent
-- row to learn the currency — so it is `issue::plan`'s, which resolves `huf_rate_e6` to
-- `Some(_)` for every non-HUF currency before `draft::price` fills `vat_huf` from it;
-- `storno::run` copies the trio instead of computing it and checks it explicitly.
--
-- Freezing them after issue is the caller's gate too: `replace_groups` is private to
-- `src/invoice.rs` and every caller holds `status = 'DRAFT'` in the same transaction.

-- The rendered PDF as immutable evidence, content-addressed. There is deliberately no `path`
-- column: the file lives at {DATA_DIR}/documents/{sha[0..2]}/{sha[2..4]}/{sha}.pdf, so the
-- path cannot disagree with the row and the stored hash is itself the integrity check.
CREATE TABLE invoice_documents (
	invoice_id		INTEGER NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
	kind			TEXT NOT NULL DEFAULT 'PDF' CHECK (kind IN ('PDF')),
	sha256			TEXT NOT NULL,		-- hex SHA-256 of the file; also its location
	bytes			INTEGER NOT NULL,
	template_version	TEXT NOT NULL,		-- the Typst template that produced it
	rendered_at		INTEGER NOT NULL,
	PRIMARY KEY (invoice_id, kind)
) WITHOUT ROWID;

CREATE INDEX idx_invoice_document_sha ON invoice_documents(sha256);
"#;

/// saas-nav: `nav_submissions`, an append-only filing record — one row per
/// `(invoice_id, op)`, not one per attempt. `index` is spelled `idx` because INDEX is a SQLite
/// keyword.
///
/// Retry lives entirely on the `jobs` row, so there is no `attempts` and no `next_try_at` here,
/// and `status` is a nullable `verdict`: `PENDING`, `SENT`, `ERROR` and `UNKNOWN` were job state
/// wearing a domain column's clothes. `DONE`, `WARN` and `REJECTED` are what NAV said about the
/// invoice, and only those are recorded.
const NAV: &str = r"
CREATE TABLE nav_submissions (
	id		INTEGER NOT NULL PRIMARY KEY,
	invoice_id	INTEGER NOT NULL REFERENCES invoices(id),
	op		TEXT NOT NULL CHECK (op IN ('CREATE','STORNO','ANNUL')),
	transaction_id	TEXT,				-- NAV transactionId
	idx		INTEGER,			-- 1-based index within the batch
	verdict		TEXT CHECK (verdict IN ('DONE','WARN','REJECTED','FAILED')),
	error_code	TEXT,
	error_msg	TEXT,
	created_at	INTEGER NOT NULL,
	done_at		INTEGER,
	-- The leader invoice's uid, which is also the NAV `requestId` of the whole batch: one
	-- `manageInvoice` request carries up to `nav.batch_max` invoices under one exchange token.
	batch_uid	TEXT,
	-- An operator has dealt with a filing NAV refused or left without a verdict, so it stops
	-- being counted as needing a person. It never clears `verdict`, `error_code` or either
	-- archive: what NAV said is the statutory record, and this is only the note that a person
	-- acted on it. Nor does it make the invoice filable again — `unfiled_invoices` skips an
	-- invoice with any row, resolved or not.
	resolved_at	INTEGER
);

-- One filing record per (invoice_id, op), with no partial predicate: retries no longer write
-- rows, so there is nothing left for one to express.
--
-- It reads through the reader pool while `create_submission` writes through the writer, with
-- no transaction spanning the two. The primary serialisation of two runners is the `jobs`
-- claim — one invoice has exactly one `NAV_REPORT` row, and `Nav::submit` re-drives that row
-- rather than adding a second — and this index plus `job::report`'s re-read of the row are
-- the belt and braces behind it.
CREATE UNIQUE INDEX idx_nav_submission_live ON nav_submissions(invoice_id, op);

-- There is deliberately no `idx_nav_submission_invoice`: `idx_nav_submission_live` leads with
-- `invoice_id` and serves every seek such an index would, over at most two rows per invoice.
-- Nor an `idx_nav_submission_poll`: nothing polls off this table now that the job row owns the
-- schedule.
CREATE INDEX idx_nav_submission_tx ON nav_submissions(transaction_id)
	WHERE transaction_id IS NOT NULL;

CREATE INDEX idx_nav_submission_batch ON nav_submissions(batch_uid)
	WHERE batch_uid IS NOT NULL;
";

/// saas-nav: the archived NAV exchange, split out of `nav_submissions` because it is cold and
/// large — a batch leader's row holds the whole `manageInvoice` envelope, and inline it left
/// every metadata column of every row behind an overflow chain.
///
/// The cascade is integrity, not convenience: `NavStore::release_batch` deletes pristine member
/// rows, and without it the archive orphans.
pub(crate) const NAV_XML: &str = r"
CREATE TABLE nav_submission_xml (
	submission_id	INTEGER NOT NULL PRIMARY KEY
			REFERENCES nav_submissions(id) ON DELETE CASCADE,
	request_xml	TEXT,				-- archived for audit, `auth::redact`ed
	response_xml	TEXT				-- archived for audit, `auth::redact`ed
);
";

// vim: ts=4
