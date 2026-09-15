-- `idx_job_claim` was `(run_at)` only, which cannot serve `ORDER BY run_at, id`, so the
-- planner took `idx_job_status` and sorted the whole PENDING backlog on the single writer
-- connection once per claim. A correction appends a step rather than editing `001_core.sql`,
-- whose checksum is recorded.
DROP INDEX IF EXISTS idx_job_claim;
CREATE INDEX IF NOT EXISTS idx_job_claim ON jobs(run_at, id) WHERE status = 'PENDING';
