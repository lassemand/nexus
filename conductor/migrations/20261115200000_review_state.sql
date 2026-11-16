-- Automatic reviews (NEX-272).
--
-- review_state: one row per Linear issue that has been reviewed or triggered a
-- review. Only reviewer verdicts change send_backs; a human moving the issue
-- back to Todo does not.
CREATE TABLE IF NOT EXISTS review_state (
    issue_id             TEXT        PRIMARY KEY,
    identifier           TEXT        NOT NULL,
    send_backs           INTEGER     NOT NULL DEFAULT 0,
    reviews_total        INTEGER     NOT NULL DEFAULT 0,
    -- 'automatic' or 'manual'
    last_trigger         TEXT,
    -- 'approve', 'changes_requested', 'failed' or 'discarded'
    last_verdict         TEXT,
    last_review_at       TIMESTAMPTZ,
    limit_notice_sent_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS review_state_identifier ON review_state (identifier);

-- The review queued or running for an issue. The primary key is what makes
-- "at most one review per issue" hold across concurrent triggers.
CREATE TABLE IF NOT EXISTS review_runs (
    issue_id     TEXT        PRIMARY KEY,
    identifier   TEXT        NOT NULL,
    issue_url    TEXT        NOT NULL,
    -- session_groups.slug of the review set, `review-<identifier>`.
    group_slug   TEXT        NOT NULL,
    -- 'automatic' or 'manual'
    trigger      TEXT        NOT NULL,
    review_number INTEGER    NOT NULL,
    -- The pull requests and the head SHAs given in the prompt.
    prs          JSONB       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL,
    -- Set when the issue left the review state while the review ran.
    cancelled_at TIMESTAMPTZ
);

