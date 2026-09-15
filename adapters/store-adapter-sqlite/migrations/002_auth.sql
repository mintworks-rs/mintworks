-- saas-auth — accounts, tenants, memberships, api keys, TOTP, legal docs, consents.
-- Verbatim from claude-docs/db-schema.md §3. Applied in the same transaction as
-- saas_invoice::M_INIT: tenants.billing_currency references currencies(code).

CREATE TABLE IF NOT EXISTS accounts (
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

CREATE INDEX IF NOT EXISTS idx_account_status ON accounts(status);

CREATE TABLE IF NOT EXISTS tenants (
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
CREATE UNIQUE INDEX IF NOT EXISTS idx_tenant_personal
	ON tenants(owner_account_id) WHERE kind = 'P';

CREATE TABLE IF NOT EXISTS memberships (
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

CREATE INDEX IF NOT EXISTS idx_membership_account ON memberships(account_id);

CREATE TABLE IF NOT EXISTS api_keys (
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

CREATE INDEX IF NOT EXISTS idx_api_key_tenant ON api_keys(tenant_id);

CREATE TABLE IF NOT EXISTS totp_credentials (
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

CREATE TABLE IF NOT EXISTS legal_docs (
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

CREATE TABLE IF NOT EXISTS consents (
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

CREATE INDEX IF NOT EXISTS idx_consent_account ON consents(account_id, kind, at DESC);

