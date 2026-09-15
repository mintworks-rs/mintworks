-- saas-core: the framework's own tables. Applied as the single `saas_core::M_INIT` step.

CREATE TABLE IF NOT EXISTS vars (
	name		TEXT NOT NULL PRIMARY KEY,
	value		TEXT
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS migrations (
	idx		INTEGER NOT NULL PRIMARY KEY,	-- insertion order only, never a version
	name		TEXT NOT NULL UNIQUE,		-- 'saas-invoice/init'
	checksum	TEXT NOT NULL,			-- hex SHA-256 of the step's SQL text
	applied_at	INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
	key		TEXT NOT NULL PRIMARY KEY,
	value		TEXT NOT NULL,			-- the registry owns the type
	updated_at	INTEGER NOT NULL,
	updated_by	INTEGER				-- accounts.id; no FK, survives anonymization
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS secrets (
	key		TEXT NOT NULL PRIMARY KEY,
	nonce		BLOB NOT NULL,			-- 12 bytes, fresh per write
	ciphertext	BLOB NOT NULL,
	updated_at	INTEGER NOT NULL,
	updated_by	INTEGER				-- accounts.id; no FK
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS jobs (
	id		INTEGER NOT NULL PRIMARY KEY,
	kind		TEXT NOT NULL,
	payload		TEXT NOT NULL DEFAULT '{}',	-- JSON
	dedup_key	TEXT UNIQUE,			-- NULL = no dedup; NULLs are distinct in SQLite
	status		TEXT NOT NULL DEFAULT 'PENDING'
			CHECK (status IN ('PENDING','RUNNING','DONE','FAILED')),
	run_at		INTEGER NOT NULL,
	attempts	INTEGER NOT NULL DEFAULT 0,
	max_attempts	INTEGER NOT NULL DEFAULT 8,
	last_error	TEXT,				-- the human message
	created_at	INTEGER NOT NULL,
	done_at		INTEGER,
	-- `last_error` is the human message and nothing more: "should this job be retried" used
	-- to be re-derived at each call site and never recorded. `saas_core::Error::retry()`
	-- answers that from the error itself, and `Runner::fail`/`Runner::terminate` write the
	-- stable errCode (`Error::parts()`) that classified it beside the message. Nullable,
	-- because the one live path that writes NULL is `tick`'s "no handler registered for this
	-- kind", where no `Error` stands behind the failure at all.
	err_code	TEXT
);

CREATE INDEX IF NOT EXISTS idx_job_claim  ON jobs(run_at) WHERE status = 'PENDING';
CREATE INDEX IF NOT EXISTS idx_job_status ON jobs(status, created_at);

-- `saas_invoice::store::issued_without_document` correlates
-- `j.payload = '{"invoiceId":' || i.id || '}'` as a subquery over the whole `jobs` table, once
-- per issued invoice with no document row, and `CoreStore::job_cancel`/`job_redrive` address a
-- row by the same pair. Retention (`CoreStore::job_sweep`, driven by the daily `SWEEP_JOBS`
-- tick) bounds the table; this bounds the correlation.
CREATE INDEX IF NOT EXISTS idx_job_kind_payload ON jobs (kind, payload);

-- Append-only record of every mutation, written by `crate::audit::log`. No FK to
-- accounts or tenants: the log outlives both.
CREATE TABLE IF NOT EXISTS audit_logs (
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

CREATE INDEX IF NOT EXISTS idx_audit_log_at      ON audit_logs(at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_entity  ON audit_logs(entity, entity_id, at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_account ON audit_logs(account_id, at DESC);

-- Append-only is a property of the trait's shape, not of a trigger: `CoreStore::audit_log` is
-- the only method there is. Two `RAISE(ABORT)` triggers used to guard these rows and were
-- dropped — they defended against an actor who could `DROP TRIGGER` first, and against a Rust
-- caller that does not exist. See `migrate.rs`'s module doc for the general rule.
