-- `jobs.max_attempts` was a per-row column nothing ever read or wrote: the runner takes its
-- ceiling from the `jobs.max_attempts.<KIND>` settings family, so a column with `DEFAULT 8`
-- contradicted the number an operator actually changes. A correction appends a step rather
-- than editing `001_core.sql`, whose checksum is recorded.
ALTER TABLE jobs DROP COLUMN max_attempts;
