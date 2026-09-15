-- saas-invoice schema. Applied in one transaction with PRAGMA foreign_keys=OFF, after
-- saas_core::M_INIT and saas_auth::M_INIT (claude-docs/db-schema.md §1).

-- Enabled currencies and how a base-currency price converts into them.
CREATE TABLE IF NOT EXISTS currencies (
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
CREATE TABLE IF NOT EXISTS currency_rates (
	pair		TEXT NOT NULL,
	date		TEXT NOT NULL,			-- 'YYYY-MM-DD', the publication date
	source		TEXT NOT NULL CHECK (source IN ('MNB','ECB','BANK','MANUAL')),
	rate_e6		INTEGER NOT NULL CHECK (rate_e6 > 0),
	fetched_at	INTEGER NOT NULL,
	PRIMARY KEY (pair, date, source)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_currency_rate_lookup ON currency_rates(pair, source, date DESC);

-- VIES results, cached settings['vies.cache_days']. A stale row is a cache miss, not valid=0.
CREATE TABLE IF NOT EXISTS vies_checks (
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
CREATE TABLE IF NOT EXISTS sellers (
	id			INTEGER NOT NULL PRIMARY KEY,
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
	nav_base_url		TEXT NOT NULL,			-- wins over settings['nav.base_url']; '' falls back to it
	nav_login		TEXT,				-- technical user login name
	small_business		INTEGER NOT NULL DEFAULT 0 CHECK (small_business IN (0,1)),
	vat_scheme		TEXT NOT NULL DEFAULT 'NORMAL'
				CHECK (vat_scheme IN ('NORMAL','KATA','ALANYI_MENTES')),
	series_code		TEXT NOT NULL DEFAULT 'A',	-- default series for new invoices
	created_at		INTEGER NOT NULL
);

-- The invoice recipient, owned by a tenant and never globally deduped: two tenants billing
-- the same company hold two rows, and editing one never touches the other. Editing a party
-- never rewrites an issued invoice, which froze its own copy at ISSUE.
CREATE TABLE IF NOT EXISTS billing_parties (
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

CREATE UNIQUE INDEX IF NOT EXISTS idx_billing_party_tax
	ON billing_parties(tenant_id, country, tax_number) WHERE tax_number IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_billing_party_default
	ON billing_parties(tenant_id) WHERE is_default = 1;

-- `list_parties` and `anonymize_account` both filter on `tenant_id` alone, which neither
-- partial index above can serve, so both full-scanned the table across every tenant.
-- `is_default DESC, name` is included in that order so `list_parties`' `ORDER BY is_default
-- DESC, name` reads straight off the index rather than through a temp B-tree — the DESC is
-- load-bearing, `(tenant_id, name)` alone still sorted.
CREATE INDEX IF NOT EXISTS idx_billing_party_tenant
	ON billing_parties(tenant_id, is_default DESC, name);

-- Priced master data, in the base currency. `vat_code` is only the default: the effective
-- code for a given invoice comes from `taxrule::determine`: the buyer's zone can override it.
CREATE TABLE IF NOT EXISTS services (
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

CREATE INDEX IF NOT EXISTS idx_service_active ON services(active, name);

-- The gapless counter, one row per (seller, kind, code, year). `kind` is an open string, not
-- an enum: a later voucher type is a new value and a new row, never a migration. Allocation
-- happens only inside the issue transaction, so a rollback consumes no number.
CREATE TABLE IF NOT EXISTS doc_series (
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
CREATE TABLE IF NOT EXISTS invoices (
	id			INTEGER NOT NULL PRIMARY KEY,
	uid			TEXT NOT NULL UNIQUE,		-- 'inv_<ULID>'
	request_id		TEXT,				-- consumer idempotency key, per tenant
	tenant_id		INTEGER NOT NULL REFERENCES tenants(id),
	seller_id		INTEGER NOT NULL REFERENCES sellers(id),
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

CREATE UNIQUE INDEX IF NOT EXISTS idx_invoice_number
	ON invoices(seller_id, number) WHERE number IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_invoice_tenant     ON invoices(tenant_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_invoice_due        ON invoices(due_date) WHERE status = 'ISSUED';
-- `sweep_drafts` deletes *abandoned* drafts, so it keys on `updated_at`: a cart created a
-- month ago and edited this morning is not abandoned.
CREATE INDEX IF NOT EXISTS idx_invoice_draft_age  ON invoices(updated_at) WHERE status = 'DRAFT';
CREATE INDEX IF NOT EXISTS idx_invoice_party      ON invoices(billing_party_id);

-- The audit export's index — `NavStore::export_ids_by_date`'s `BY_DATE`, which filters
-- `seller_id = ? AND number IS NOT NULL AND issued_at >= ? AND issued_at < ?`. Its shape is
-- the query's: `seller_id` leads, `issued_at` ranges, and the partial predicate is the
-- query's own `number IS NOT NULL`, which is the issued test every export selection uses.
-- SQLite uses a partial index only when the query's WHERE terms *imply* its predicate, so a
-- `WHERE status <> 'DRAFT'` version of this was never used at all and a one-month export
-- scanned every invoice ever issued.
CREATE INDEX IF NOT EXISTS idx_invoice_issued
	ON invoices(seller_id, issued_at) WHERE number IS NOT NULL;

-- an invoice can be cancelled at most once
CREATE UNIQUE INDEX IF NOT EXISTS idx_invoice_storno_once
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
CREATE TABLE IF NOT EXISTS invoice_lines (
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
CREATE TABLE IF NOT EXISTS invoice_vat_groups (
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
CREATE TABLE IF NOT EXISTS invoice_documents (
	invoice_id		INTEGER NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
	kind			TEXT NOT NULL DEFAULT 'PDF' CHECK (kind IN ('PDF')),
	sha256			TEXT NOT NULL,		-- hex SHA-256 of the file; also its location
	bytes			INTEGER NOT NULL,
	template_version	TEXT NOT NULL,		-- the Typst template that produced it
	rendered_at		INTEGER NOT NULL,
	PRIMARY KEY (invoice_id, kind)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_invoice_document_sha ON invoice_documents(sha256);
