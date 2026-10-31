-- Exactly-once claim for Slack report notifications (NEX-221). One row per
-- release: language twins share `report_key`
-- (`<ticker>:<mic>:<published unix secs>:<report_kind>`).
CREATE TABLE IF NOT EXISTS slack_report_posts (
    report_key   TEXT        PRIMARY KEY,
    ticker       TEXT        NOT NULL,
    exchange_mic TEXT        NOT NULL,
    news_id      UUID        NOT NULL,
    lang         TEXT,
    status       TEXT        NOT NULL CHECK (status IN ('claimed', 'posted', 'failed')),
    claimed_at   TIMESTAMPTZ NOT NULL,
    posted_at    TIMESTAMPTZ,
    channel      TEXT,
    slack_ts     TEXT,
    poster       TEXT        NOT NULL CHECK (poster IN ('live', 'poll')),
    attempts     INT         NOT NULL DEFAULT 0,
    last_error   TEXT
);
