-- Language-twin matching for Slack claims (NEX-238): twins are found by company,
-- kind and a 120 s publish window instead of an exact-second key.
ALTER TABLE slack_report_posts
    ADD COLUMN kind         TEXT NOT NULL DEFAULT 'report' CHECK (kind IN ('report', 'announcement')),
    ADD COLUMN published_at TIMESTAMPTZ,
    ADD COLUMN report_kind  TEXT,
    ADD COLUMN tags         TEXT[] NOT NULL DEFAULT '{}';

UPDATE slack_report_posts
   SET published_at = to_timestamp(split_part(report_key, ':', 3)::bigint),
       report_kind  = split_part(report_key, ':', 4);

ALTER TABLE slack_report_posts ALTER COLUMN published_at SET NOT NULL;

CREATE INDEX slack_report_posts_twin_idx
    ON slack_report_posts (ticker, exchange_mic, kind, published_at);
