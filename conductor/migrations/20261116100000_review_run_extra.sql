-- The pull request a `/review` named that the Linear issue may not list, kept
-- on the run so a review queued again after a new head still covers it.
ALTER TABLE review_runs ADD COLUMN IF NOT EXISTS extra_repo TEXT;
ALTER TABLE review_runs ADD COLUMN IF NOT EXISTS extra_pr BIGINT;
