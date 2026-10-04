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
	// `saas_core::refs` and the optional org slug. New tables; the DDL spelled out, as for v13.
	if from < 25 {
		sqlx::raw_sql(
			"ALTER TABLE orgs ADD COLUMN slug TEXT COLLATE NOCASE;
			CREATE UNIQUE INDEX idx_org_slug ON orgs(slug) WHERE slug IS NOT NULL;

			CREATE TABLE refs (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,
				code		TEXT NOT NULL UNIQUE COLLATE NOCASE,
				type		TEXT NOT NULL,
				org_id		INTEGER NOT NULL REFERENCES orgs(id),
				created_by	INTEGER,
				target		TEXT,
				email		TEXT,
				params		TEXT NOT NULL DEFAULT '{}',
				uses_left	INTEGER CHECK (uses_left >= 0),
				expires_at	INTEGER,
				status		TEXT NOT NULL DEFAULT 'ACTIVE'
						CHECK (status IN ('ACTIVE','REVOKED')),
				created_at	INTEGER NOT NULL
			);
			CREATE INDEX idx_ref_org ON refs(org_id, type);

			CREATE TABLE ref_uses (
				id		INTEGER NOT NULL PRIMARY KEY,
				ref_id		INTEGER NOT NULL REFERENCES refs(id),
				account_id	INTEGER NOT NULL,
				org_id		INTEGER NOT NULL,
				at		INTEGER NOT NULL,
				UNIQUE (ref_id, account_id)
			);",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// Registration and invitations on refs. Each outstanding invitation becomes an `org_invite`
	// ref; a still-`PENDING` invitee also gets it as `pending_ref_id`, so the activation link it
	// already holds still joins the org.
	if from < 26 {
		sqlx::raw_sql(
			"ALTER TABLE accounts ADD COLUMN pending_ref_id INTEGER REFERENCES refs(id);",
		)
		.execute(&mut *conn)
		.await
		.db()?;
		let pending: Vec<(i64, i64, String, String, String, String)> = sqlx::query_as(
			"SELECT m.org_id, m.account_id, m.role, a.email, a.status, o.uid
			 FROM memberships m JOIN accounts a ON a.id = m.account_id JOIN orgs o ON o.id = m.org_id
			 WHERE m.accepted_at IS NULL",
		)
		.fetch_all(&mut *conn)
		.await
		.db()?;
		let now = saas_core::types::Timestamp::now().0;
		for (org_id, account_id, role, email, status, org_uid) in pending {
			let uid = saas_core::ids::RefId::generate();
			// The same 12 Crockford chars `saas_core::refs` mints: a ULID's random tail.
			let code = uid.as_str()[uid.as_str().len() - 12..].to_owned();
			let ref_id: i64 = sqlx::query_scalar(
				"INSERT INTO refs (uid, code, type, org_id, target, email, params, uses_left,
					expires_at, created_at)
				 VALUES (?, ?, 'org_invite', ?, ?, ?, ?, 1, ?, ?) RETURNING id",
			)
			.bind(uid.as_str())
			.bind(code)
			.bind(org_id)
			.bind(org_uid)
			.bind(email)
			.bind(serde_json::json!({ "role": role }).to_string())
			.bind(now + 14 * 86_400)
			.bind(now)
			.fetch_one(&mut *conn)
			.await
			.db()?;
			if status == "PENDING" {
				sqlx::query("UPDATE accounts SET pending_ref_id = ? WHERE id = ?")
					.bind(ref_id)
					.bind(account_id)
					.execute(&mut *conn)
					.await
					.db()?;
			}
		}
		sqlx::raw_sql(
			"DELETE FROM memberships WHERE accepted_at IS NULL;
			INSERT INTO settings (key, value, updated_at, updated_by)
				SELECT 'auth.registration',
					CASE WHEN lower(value) IN ('0', 'false', 'no', 'off') THEN 'closed' ELSE 'open' END,
					updated_at, updated_by
				FROM settings WHERE key = 'auth.registration_open'
				ON CONFLICT (key) DO NOTHING;
			DELETE FROM settings WHERE key = 'auth.registration_open';",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// saas-entitle: grants and the usage ledger.
	if from < 27 {
		sqlx::raw_sql(
			"CREATE TABLE grants (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,			-- 'grt_<ULID>'
				org_id		INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
				key		TEXT NOT NULL,
				amount		INTEGER NOT NULL CHECK (amount >= 0),
				valid_from	INTEGER NOT NULL,
				valid_until	INTEGER,				-- NULL = forever
				source		TEXT NOT NULL
						CHECK (source IN ('SUBSCRIPTION', 'PURCHASE', 'REWARD', 'MANUAL', 'TRIAL')),
				source_ref	TEXT,					-- NULL is never deduplicated
				created_at	INTEGER NOT NULL,
				UNIQUE (org_id, key, source, source_ref)
			);

			CREATE INDEX idx_grants_active ON grants(org_id, key, valid_until);

			-- One row per grant a debit drew from, numbered by `seq`; the unique index is what makes a
			-- retried (org, idem_key) debit once. `grant_id` NULL: debited with no active grant.
			CREATE TABLE usage (
				id		INTEGER NOT NULL PRIMARY KEY,
				org_id		INTEGER NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
				key		TEXT NOT NULL,
				grant_id	INTEGER REFERENCES grants(id) ON DELETE CASCADE,
				amount		INTEGER NOT NULL,
				at		INTEGER NOT NULL,
				idem_key	TEXT NOT NULL,
				seq		INTEGER NOT NULL,
				account_id	INTEGER,				-- no FK: the ledger outlives the account
				UNIQUE (org_id, idem_key, seq)
			);

			CREATE INDEX idx_usage_grant ON usage(grant_id);
			",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// saas-plans: offers, subscriptions and plan_invoices.
	if from < 28 {
		sqlx::raw_sql(
			"CREATE TABLE offers (
				id		INTEGER NOT NULL PRIMARY KEY,
				uid		TEXT NOT NULL UNIQUE,			-- 'ofr_<ULID>'
				seller_org_id	INTEGER NOT NULL REFERENCES orgs(id),
				code		TEXT NOT NULL,
				name		TEXT NOT NULL,
				kind		TEXT NOT NULL CHECK (kind IN ('ONE_TIME', 'RECURRING')),
				service_id	INTEGER NOT NULL REFERENCES services(id),
				family		TEXT,					-- tiers: one live sub per (org, family)
				rank		INTEGER NOT NULL DEFAULT 0,
				interval	TEXT CHECK (interval IN ('MONTH', 'YEAR')),
				interval_count	INTEGER CHECK (interval_count > 0),
				validity_days	INTEGER CHECK (validity_days > 0),	-- ONE_TIME; NULL = forever
				trial_days	INTEGER NOT NULL DEFAULT 0 CHECK (trial_days >= 0),
				active		INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
				created_at	INTEGER NOT NULL,
				updated_at	INTEGER NOT NULL,
				UNIQUE (seller_org_id, code),
				CHECK ((kind = 'RECURRING') = (interval IS NOT NULL AND interval_count IS NOT NULL))
			);

			CREATE TABLE offer_prices (
				offer_id	INTEGER NOT NULL REFERENCES offers(id) ON DELETE CASCADE,
				currency	TEXT NOT NULL REFERENCES currencies(code),
				amount		INTEGER NOT NULL CHECK (amount >= 0),	-- minor units
				PRIMARY KEY (offer_id, currency)
			) WITHOUT ROWID;

			CREATE TABLE offer_entitlements (
				offer_id	INTEGER NOT NULL REFERENCES offers(id) ON DELETE CASCADE,
				key		TEXT NOT NULL,
				amount		INTEGER NOT NULL CHECK (amount >= 0),
				per_seat	INTEGER NOT NULL DEFAULT 0 CHECK (per_seat IN (0, 1)),
				PRIMARY KEY (offer_id, key)
			) WITHOUT ROWID;

			CREATE TABLE subscriptions (
				id			INTEGER NOT NULL PRIMARY KEY,
				uid			TEXT NOT NULL UNIQUE,		-- 'sub_<ULID>'
				org_id			INTEGER NOT NULL REFERENCES orgs(id),
				offer_id		INTEGER NOT NULL REFERENCES offers(id),
				family			TEXT,
				qty			INTEGER NOT NULL DEFAULT 1 CHECK (qty > 0),
				status			TEXT NOT NULL
							CHECK (status IN ('TRIALING', 'ACTIVE', 'PAST_DUE', 'SUSPENDED', 'CANCELED')),
				currency		TEXT NOT NULL REFERENCES currencies(code),
				price			INTEGER NOT NULL CHECK (price >= 0),	-- per seat per period, grandfathered
				period_start		INTEGER NOT NULL,
				period_end		INTEGER NOT NULL,
				cancel_at_period_end	INTEGER NOT NULL DEFAULT 0 CHECK (cancel_at_period_end IN (0, 1)),
				next_offer_id		INTEGER REFERENCES offers(id),
				next_qty		INTEGER CHECK (next_qty > 0),
				pay_method		TEXT NOT NULL CHECK (pay_method IN ('CARD', 'TRANSFER')),
				provider		TEXT,
				recurrence_ref		TEXT,				-- gateway reference, not a credential
				coupon_ref_id		INTEGER REFERENCES refs(id),
				coupon_periods_left	INTEGER,
				created_at		INTEGER NOT NULL,
				updated_at		INTEGER NOT NULL
			);

			-- Integrity, not a lookup aid: one live subscription per (org, family).
			CREATE UNIQUE INDEX idx_sub_family_live ON subscriptions(org_id, family)
				WHERE family IS NOT NULL AND status <> 'CANCELED';
			CREATE INDEX idx_sub_org ON subscriptions(org_id);
			CREATE INDEX idx_sub_due ON subscriptions(period_end)
				WHERE status IN ('TRIALING', 'ACTIVE', 'PAST_DUE');

			-- Why an invoice exists. CASCADE: a deleted or swept draft takes its link with it.
			CREATE TABLE plan_invoices (
				invoice_id	INTEGER NOT NULL PRIMARY KEY REFERENCES invoices(id) ON DELETE CASCADE,
				subscription_id	INTEGER REFERENCES subscriptions(id),
				offer_id	INTEGER NOT NULL REFERENCES offers(id),
				kind		TEXT NOT NULL CHECK (kind IN ('PURCHASE', 'SUBSCRIBE', 'RENEWAL', 'UPGRADE')),
				qty		INTEGER NOT NULL CHECK (qty > 0),
				period_start	INTEGER,
				period_end	INTEGER,
				coupon_ref_id	INTEGER REFERENCES refs(id),
				CHECK ((kind = 'PURCHASE') = (subscription_id IS NULL))
			);

			CREATE INDEX idx_plan_invoices_sub ON plan_invoices(subscription_id, period_start);
			",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// saas-plans: the renewal anchor, and an upgrade link's replaced tier.
	if from < 29 {
		sqlx::raw_sql(
			"ALTER TABLE subscriptions ADD COLUMN billing_anchor INTEGER NOT NULL DEFAULT 0;
			ALTER TABLE plan_invoices ADD COLUMN prev_offer_id INTEGER REFERENCES offers(id);
			ALTER TABLE plan_invoices ADD COLUMN prev_qty INTEGER;
			UPDATE subscriptions SET billing_anchor = COALESCE(
				(SELECT MIN(p.period_start) FROM plan_invoices p
				 WHERE p.subscription_id = subscriptions.id AND p.kind IN ('SUBSCRIBE', 'RENEWAL')),
				CASE status WHEN 'TRIALING' THEN period_end ELSE period_start END);
			",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	// saas-plans: a coupon use held by its draft invoice until payment.
	if from < 30 {
		sqlx::raw_sql(
			"ALTER TABLE ref_uses ADD COLUMN invoice_uid TEXT;
			ALTER TABLE ref_uses ADD COLUMN held INTEGER NOT NULL DEFAULT 0;
			",
		)
		.execute(&mut *conn)
		.await
		.db()?;
	}
	Ok(())
}

// vim: ts=4
