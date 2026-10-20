-- Scheduled AND released Nordic reports (NEX-160, NEX-161).
--
-- A row is "scheduled" while released_at / disclosure_id are NULL (expected_*
-- filled from the MFN calendar) and becomes "released" when the matching
-- Nasdaq disclosure arrives. released_at is the actual release instant;
-- classify sessions later in SQL, in Europe/Stockholm. expected_time is only
-- an estimate (MFN often knows the date but not the time).
CREATE TABLE IF NOT EXISTS report_releases (
    id              BIGSERIAL PRIMARY KEY,
    ticker          TEXT        NOT NULL,
    exchange_mic    TEXT        NOT NULL,
    -- NULL = scheduled but not yet released (multiple NULLs allowed by UNIQUE).
    disclosure_id   BIGINT UNIQUE,
    -- Actual release instant from Nasdaq; NULL = not released yet.
    released_at     TIMESTAMPTZ,
    -- Expected release, Europe/Stockholm local date/time (from the MFN calendar).
    expected_date   DATE,
    expected_time   TIME,
    fiscal_period   TEXT,
    calendar_label  TEXT,
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
    FOREIGN KEY (ticker, exchange_mic) REFERENCES companies (ticker, exchange_mic),
    CONSTRAINT report_releases_has_date
        CHECK (released_at IS NOT NULL OR expected_date IS NOT NULL),
    -- Released exactly when it has a Nasdaq disclosure.
    CONSTRAINT report_releases_disclosure_iff_released
        CHECK ((disclosure_id IS NULL) = (released_at IS NULL))
);

CREATE INDEX IF NOT EXISTS report_releases_company_released_idx
    ON report_releases (ticker, exchange_mic, released_at);

CREATE INDEX IF NOT EXISTS report_releases_company_expected_idx
    ON report_releases (ticker, exchange_mic, expected_date);

CREATE UNIQUE INDEX IF NOT EXISTS report_releases_scheduled_key
    ON report_releases (ticker, exchange_mic, expected_date, calendar_label)
    WHERE released_at IS NULL;
