-- When the claim happened. `job_reclaim` had no age predicate, so a second process starting
-- during a rolling deploy flipped a live sibling's `RUNNING` rows back to `PENDING` and both
-- processes ran the same handler.
ALTER TABLE jobs ADD COLUMN claimed_at INTEGER;
