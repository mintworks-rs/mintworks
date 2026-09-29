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
//! A block below [`crate::schema::OLDEST_UPGRADABLE`] is deleted when that floor moves: no live
//! database carries the version, so the block can only ratchet a shape nothing reaches. What it
//! did survives in git, and nowhere else.
//!
//! The runner stamps `schema_version` once, after `upgrade` returns, so no block sets a version.

use sqlx::SqliteConnection;

use saas_core::error::ClResult;

use crate::util::DbExt;

pub(crate) async fn upgrade(conn: &mut SqliteConnection, from: i64) -> ClResult<()> {
	// The object store. New tables, nothing to backfill, and the DDL spelled out rather than
	// taken from `schema::OBJECTS`: a later version must not change what an upgrading database got.
	if from < 13 {
		sqlx::raw_sql(
			"CREATE TABLE objects (
				id		INTEGER NOT NULL PRIMARY KEY,
				org_id		INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
				type		TEXT NOT NULL,
				uid		TEXT NOT NULL,
				body		TEXT NOT NULL DEFAULT '{}',
				created_at	INTEGER NOT NULL,
				updated_at	INTEGER NOT NULL,
				UNIQUE (org_id, type, uid)
			 );
			 CREATE INDEX idx_object_page ON objects(org_id, type, id DESC);
			 CREATE TABLE object_index (
				object_id	INTEGER NOT NULL REFERENCES objects(id) ON DELETE CASCADE,
				path		TEXT NOT NULL,
				value		TEXT,
				PRIMARY KEY (object_id, path)
			 ) WITHOUT ROWID;
			 CREATE INDEX idx_object_index_lookup ON object_index (path, value, object_id);",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// The settlement period of a periodic invoice (Áfa tv. 58. §); NULL on every existing row.
	if from < 14 {
		sqlx::raw_sql(
			"ALTER TABLE invoices ADD COLUMN period_start TEXT;
			 ALTER TABLE invoices ADD COLUMN period_end TEXT;",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// Org-scoped secrets: every existing row is a global one, so it lands at org 0.
	if from < 15 {
		sqlx::raw_sql(
			"CREATE TABLE secrets_new (
				org_id		INTEGER NOT NULL DEFAULT 0,
				key		TEXT NOT NULL,
				nonce		BLOB NOT NULL,
				ciphertext	BLOB NOT NULL,
				updated_at	INTEGER NOT NULL,
				updated_by	INTEGER,
				PRIMARY KEY (org_id, key)
			 ) WITHOUT ROWID;
			 INSERT INTO secrets_new SELECT 0, key, nonce, ciphertext, updated_at, updated_by
				FROM secrets;
			 DROP TABLE secrets;
			 ALTER TABLE secrets_new RENAME TO secrets;",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// A read-only company: NULL on every existing seller.
	if from < 16 {
		sqlx::raw_sql("ALTER TABLE sellers ADD COLUMN closed_at INTEGER;")
			.execute(&mut *conn)
			.await
			.db()?;
	}
	// Payment terms: NULL means "inherit", so every existing row keeps its behaviour. HUF gets
	// the 5 Ft cash rounding (2008. évi III. tv.) the fresh seed carries.
	if from < 17 {
		sqlx::raw_sql(
			"ALTER TABLE sellers ADD COLUMN payment_days INTEGER
				CHECK (payment_days BETWEEN 0 AND 36500);
			 ALTER TABLE billing_parties ADD COLUMN payment_days INTEGER
				CHECK (payment_days BETWEEN 0 AND 36500);
			 ALTER TABLE billing_parties ADD COLUMN payment_method TEXT
				CHECK (payment_method IN ('TRANSFER','CASH'));
			 ALTER TABLE currencies ADD COLUMN cash_round_step INTEGER
				CHECK (cash_round_step > 0);
			 UPDATE currencies SET cash_round_step = 500 WHERE code = 'HUF';",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// `vat_scheme = 'KATA'` split into `NORMAL` + `income_regime = 'KATA'`: the VAT engine only
	// ever acted on `ALANYI_MENTES`, so behaviour is unchanged. A rebuild, since SQLite cannot
	// alter a CHECK; `invoices.seller_ver` references the table by name, so it survives.
	if from < 18 {
		sqlx::raw_sql(
			"CREATE TABLE seller_versions_new (
				seller_ver		INTEGER NOT NULL PRIMARY KEY,
				seller_id		INTEGER NOT NULL REFERENCES sellers(id),
				status			TEXT NOT NULL DEFAULT 'DRAFT'
							CHECK (status IN ('DRAFT','CURRENT','ARCHIVED')),
				name			TEXT NOT NULL,
				country			TEXT NOT NULL DEFAULT 'HU',
				tax_number		TEXT NOT NULL,
				group_member_tax_no	TEXT,
				eu_vat_id		TEXT,
				postcode		TEXT NOT NULL,
				city			TEXT NOT NULL,
				street			TEXT NOT NULL,
				bank_account		TEXT,
				bank_name		TEXT,
				small_business		INTEGER NOT NULL DEFAULT 0 CHECK (small_business IN (0,1)),
				vat_scheme		TEXT NOT NULL DEFAULT 'NORMAL'
							CHECK (vat_scheme IN ('NORMAL','ALANYI_MENTES')),
				income_regime		TEXT NOT NULL DEFAULT 'NONE'
							CHECK (income_regime IN ('NONE','KATA','ATALANY')),
				expense_ratio_pct	INTEGER CHECK (expense_ratio_pct IS NULL
							OR expense_ratio_pct IN (40,45,50,80,90)),
				regime_since		TEXT,
				created_at		INTEGER NOT NULL,
				valid_from		INTEGER,
				superseded_at		INTEGER,
				CHECK ((status = 'DRAFT')    = (valid_from    IS NULL)),
				CHECK ((status = 'ARCHIVED') = (superseded_at IS NOT NULL))
			 );
			 INSERT INTO seller_versions_new SELECT seller_ver, seller_id, status, name, country,
				tax_number, group_member_tax_no, eu_vat_id, postcode, city, street, bank_account,
				bank_name, small_business,
				CASE vat_scheme WHEN 'KATA' THEN 'NORMAL' ELSE vat_scheme END,
				CASE vat_scheme WHEN 'KATA' THEN 'KATA' ELSE 'NONE' END,
				NULL, NULL, created_at, valid_from, superseded_at
				FROM seller_versions;
			 DROP TABLE seller_versions;
			 ALTER TABLE seller_versions_new RENAME TO seller_versions;
			 CREATE UNIQUE INDEX idx_seller_version_draft
				ON seller_versions(seller_id) WHERE status = 'DRAFT';
			 CREATE UNIQUE INDEX idx_seller_version_current
				ON seller_versions(seller_id) WHERE status = 'CURRENT';
			 CREATE INDEX idx_seller_version_history
				ON seller_versions(seller_id, valid_from DESC) WHERE status <> 'DRAFT';",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// `saas-llm`'s ledger and budgets. New tables; the DDL spelled out, as for v13.
	if from < 19 {
		sqlx::raw_sql(
			"
			CREATE TABLE llm_usage (
				id		INTEGER NOT NULL PRIMARY KEY,
				at		INTEGER NOT NULL,
				run_uid		TEXT,
				account_id	INTEGER,			-- no FK: the ledger outlives the account
				org_id		INTEGER,			-- no FK: same reason
				subject		TEXT,				-- opaque budget key, e.g. 'project:prj_…'
				step		TEXT NOT NULL,
				kind		TEXT NOT NULL CHECK (kind IN ('llm','search','fetch')),
				provider	TEXT NOT NULL,
				model		TEXT NOT NULL,
				tokens_in	INTEGER NOT NULL,
				tokens_out	INTEGER NOT NULL,
				cost_micro_eur	INTEGER NOT NULL,
				retry		INTEGER NOT NULL CHECK (retry IN (0,1))
			);

			CREATE INDEX idx_llm_usage_at      ON llm_usage(at, cost_micro_eur);
			CREATE INDEX idx_llm_usage_subject ON llm_usage(subject, cost_micro_eur) WHERE subject IS NOT NULL;

			CREATE TABLE llm_budgets (
				subject			TEXT NOT NULL PRIMARY KEY,
				budget_micro_eur	INTEGER NOT NULL CHECK (budget_micro_eur >= 0),
				updated_at		INTEGER NOT NULL
			) WITHOUT ROWID;",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// `saas-agent`'s runs and run events. New tables; the DDL spelled out, as for v13.
	if from < 20 {
		sqlx::raw_sql(
			"
			CREATE TABLE agent_runs (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,
				thread_uid	TEXT NOT NULL,			-- agent_threads.uid, in the app DB
				org_id		INTEGER NOT NULL,		-- no FK: the run record outlives the org, as llm_usage does
				account_id	INTEGER,			-- no FK; NULL once the account is erased
				role		TEXT NOT NULL,
				spec		TEXT NOT NULL,			-- JSON
				status		TEXT NOT NULL CHECK (status IN
						('queued','running','done','error','cancelled','interrupted')),
				error		TEXT,
				created_at	INTEGER NOT NULL,
				started_at	INTEGER,
				finished_at	INTEGER
			);

			-- D2, one live run per thread: integrity, not performance.
			CREATE UNIQUE INDEX idx_agent_runs_live ON agent_runs(thread_uid) WHERE status IN ('queued','running');
			CREATE INDEX idx_agent_runs_account ON agent_runs(account_id) WHERE account_id IS NOT NULL;

			CREATE TABLE agent_run_events (
				run_id		INTEGER NOT NULL REFERENCES agent_runs(id) ON DELETE CASCADE,
				seq		INTEGER NOT NULL,
				kind		TEXT NOT NULL CHECK (kind IN
						('queued','delta','tool_call','tool_result','message','done','error')),
				payload		TEXT NOT NULL,
				at		INTEGER NOT NULL,
				PRIMARY KEY (run_id, seq)
			) WITHOUT ROWID;",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// `saas-search`'s sources and search cache. New tables; the DDL spelled out, as for v13.
	if from < 21 {
		sqlx::raw_sql(
			"
			CREATE TABLE sources (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,
				url		TEXT NOT NULL,
				title		TEXT NOT NULL,
				fetched_at	INTEGER NOT NULL,
				sha256		TEXT NOT NULL,			-- hex, of text
				text		TEXT NOT NULL
			);

			CREATE INDEX idx_sources_url ON sources(url, fetched_at);

			CREATE TABLE search_cache (
				provider	TEXT NOT NULL,
				query		TEXT NOT NULL,
				lang		TEXT NOT NULL,			-- '' when absent, so it takes part in the key
				market		TEXT NOT NULL,			-- same
				results		TEXT NOT NULL,			-- JSON
				fetched_at	INTEGER NOT NULL,
				PRIMARY KEY (provider, query, lang, market)
			) WITHOUT ROWID;",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// `saas-pdf`'s `RENDER_DOC` hands its document sha256 back through the job row.
	if from < 22 {
		sqlx::raw_sql("ALTER TABLE jobs ADD COLUMN result TEXT;")
			.execute(&mut *conn)
			.await
			.db()?;
	}
	// `saas-pdf`'s `documents`. A new table; the DDL spelled out, as for v13.
	if from < 23 {
		sqlx::raw_sql(
			"
			CREATE TABLE documents (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,			-- 'doc_<ULID>'
				org_id		INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
				template	TEXT NOT NULL,
				job_key		TEXT NOT NULL,			-- the RENDER_DOC dedup_key
				sha256		TEXT,				-- NULL until rendered; also its location
				bytes		INTEGER,
				created_at	INTEGER NOT NULL
			);

			CREATE INDEX idx_documents_sha ON documents(sha256);",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// `orgs` gains AUTOINCREMENT: FK-less `agent_runs`/`llm_usage` rows outlive an org, and a
	// reused id would hand them to the next org. `agent_runs.heartbeat_at` is the run lease.
	if from < 24 {
		sqlx::raw_sql(
			"
			CREATE TABLE orgs_new (
				id			INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
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
			INSERT INTO orgs_new (id, uid, parent_id, kind, name, owner_account_id,
					billing_currency, status, created_at)
				SELECT id, uid, parent_id, kind, name, owner_account_id,
					billing_currency, status, created_at FROM orgs;
			DROP TABLE orgs;
			ALTER TABLE orgs_new RENAME TO orgs;
			CREATE UNIQUE INDEX idx_org_personal
				ON orgs(owner_account_id) WHERE kind = 'PERSONAL';
			CREATE UNIQUE INDEX idx_org_root   ON orgs(kind) WHERE kind = 'ROOT';
			CREATE INDEX        idx_org_parent ON orgs(parent_id);

			ALTER TABLE agent_runs ADD COLUMN heartbeat_at INTEGER;
			UPDATE agent_runs SET heartbeat_at = created_at;",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	Ok(())
}

// vim: ts=4
