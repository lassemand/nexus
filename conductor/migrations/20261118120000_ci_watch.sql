-- CI gate (NEX-275): issues bounced out of review because the checks on their
-- pull requests were not green. One row per issue, so a restart keeps watching.
CREATE TABLE IF NOT EXISTS ci_watch (
    issue_id   TEXT        PRIMARY KEY,
    identifier TEXT        NOT NULL,
    -- The open pull requests and the head SHAs last seen; a SHA is empty when
    -- GitHub could not be read at the time.
    prs        JSONB       NOT NULL,
    -- When the wait started; reset by a new push. CONDUCTOR_CI_MAX_WAIT counts from here.
    created_at TIMESTAMPTZ NOT NULL
);
