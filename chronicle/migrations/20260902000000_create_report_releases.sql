-- Past report releases from the Nasdaq Nordic company-news feed (NEX-160).
-- released_at is an instant; classify sessions later in SQL, in Europe/Stockholm.
CREATE TABLE IF NOT EXISTS report_releases (
    id              BIGSERIAL PRIMARY KEY,
    ticker          TEXT        NOT NULL,
    exchange_mic    TEXT        NOT NULL,
    disclosure_id   BIGINT      NOT NULL UNIQUE,
    released_at     TIMESTAMPTZ NOT NULL,
    category        TEXT,
    -- NULL = not a report (kept so the classification rule can be re-run).
    report_kind     TEXT CHECK (report_kind IN ('interim', 'annual')),
    headline        TEXT,
    url             TEXT,
    is_correction   BOOLEAN     NOT NULL DEFAULT false,
    -- On the ORIGINAL release: id of the correction that replaces it.
    superseded_by   BIGINT REFERENCES report_releases (id),
    calendar_update BOOLEAN     NOT NULL DEFAULT false,
    fetched_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (ticker, exchange_mic) REFERENCES companies (ticker, exchange_mic)
);

CREATE INDEX IF NOT EXISTS report_releases_company_released_idx
    ON report_releases (ticker, exchange_mic, released_at);
