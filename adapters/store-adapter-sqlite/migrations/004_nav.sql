-- saas-nav: `nav_submissions`, an append-only filing record — one row per
-- `(invoice_id, op)`, not one per attempt. `claude-docs/db-schema.md` §5. `index` is spelled
-- `idx` because INDEX is a SQLite keyword.
--
-- Retry lives entirely on the `jobs` row, so there is no `attempts`
-- and no `next_try_at` here, and `status` is a nullable `verdict`: `PENDING`, `SENT`, `ERROR`
-- and `UNKNOWN` were job state wearing a domain column's clothes. `DONE`, `WARN` and
-- `REJECTED` are what NAV said about the invoice, and only those are recorded.

CREATE TABLE IF NOT EXISTS nav_submissions (
	id		INTEGER NOT NULL PRIMARY KEY,
	invoice_id	INTEGER NOT NULL REFERENCES invoices(id),
	op		TEXT NOT NULL CHECK (op IN ('CREATE','STORNO','ANNUL')),
	transaction_id	TEXT,				-- NAV transactionId
	idx		INTEGER,			-- 1-based index within the batch
	verdict		TEXT CHECK (verdict IN ('DONE','WARN','REJECTED','FAILED')),
	request_xml	TEXT,				-- archived for audit, `auth::redact`ed
	response_xml	TEXT,				-- archived for audit, `auth::redact`ed
	error_code	TEXT,
	error_msg	TEXT,
	created_at	INTEGER NOT NULL,
	done_at		INTEGER
);

-- One filing record per (invoice_id, op), with no partial predicate: retries no longer write
-- rows, so there is nothing left for one to express.
--
-- It reads through the reader pool while `create_submission` writes through the writer, with
-- no transaction spanning the two. The primary serialisation of two runners is the `jobs`
-- claim — one invoice has exactly one `NAV_REPORT` row, and `Nav::submit` re-drives that row
-- rather than adding a second — and this index plus `job::report`'s re-read of the row are
-- the belt and braces behind it.
CREATE UNIQUE INDEX IF NOT EXISTS idx_nav_submission_live ON nav_submissions(invoice_id, op);

-- There is deliberately no `idx_nav_submission_invoice`: `idx_nav_submission_live` leads with
-- `invoice_id` and serves every seek such an index would, over at most two rows per invoice.
-- Nor an `idx_nav_submission_poll`: nothing polls off this table now that the job row owns the
-- schedule.
CREATE INDEX IF NOT EXISTS idx_nav_submission_tx ON nav_submissions(transaction_id)
	WHERE transaction_id IS NOT NULL;
