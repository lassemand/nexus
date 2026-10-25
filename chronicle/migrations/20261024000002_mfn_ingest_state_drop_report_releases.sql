-- Nordic reports are stored only by signal (special_events, NEX-171/172).
-- chronicle fetches, classifies and publishes; it keeps only how far it has
-- read per company, never the reports themselves.
CREATE TABLE mfn_ingest_state (
    ticker         TEXT        NOT NULL,
    exchange_mic   TEXT        NOT NULL,
    -- publish_date of the newest MFN item successfully published
    feed_cursor    TIMESTAMPTZ,
    -- hash of the last ReportCalendar snapshot published
    calendar_hash  TEXT,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (ticker, exchange_mic),
    FOREIGN KEY (ticker, exchange_mic) REFERENCES companies (ticker, exchange_mic)
);

-- Re-derived in signal by `nordic-reports --backfill` and `nordic-calendar`.
DROP TABLE report_releases;
