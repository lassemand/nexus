-- Nordic periodic reports are stored here (NEX-171/NEX-172), alongside the
-- legacy US earnings events. Legacy producers send none of the new fields and
-- keep working: their rows get exchange_mic = '' and status = 'occurred'.
ALTER TABLE special_events
    ADD COLUMN exchange_mic        TEXT        NOT NULL DEFAULT '',
    ADD COLUMN status              TEXT        NOT NULL DEFAULT 'occurred'
        CHECK (status IN ('scheduled', 'occurred')),
    ADD COLUMN source              TEXT,
    ADD COLUMN source_id           TEXT,
    ADD COLUMN url                 TEXT,
    ADD COLUMN lang                TEXT,
    ADD COLUMN tags                TEXT[]      NOT NULL DEFAULT '{}',
    ADD COLUMN report_kind         TEXT CHECK (report_kind IN ('interim', 'annual')),
    ADD COLUMN fiscal_period       TEXT,
    ADD COLUMN is_correction       BOOLEAN     NOT NULL DEFAULT false,
    ADD COLUMN superseded_by       BIGINT      REFERENCES special_events (id),
    ADD COLUMN company_name        TEXT,
    ADD COLUMN expected_date       DATE,
    ADD COLUMN expected_time       TIME,
    ADD COLUMN calendar_label      TEXT,
    ADD COLUMN calendar_fetched_at TIMESTAMPTZ,
    ADD CONSTRAINT special_events_scheduled_shape
        CHECK (status = 'occurred' OR (source_id IS NULL AND expected_date IS NOT NULL));

COMMENT ON COLUMN special_events.occurred_at IS
    'When the event happened. For status = scheduled rows: the expected instant, '
    '(expected_date + COALESCE(expected_time, ''00:00'')) AT TIME ZONE ''Europe/Stockholm''.';
COMMENT ON COLUMN special_events.expected_time IS
    'Europe/Stockholm local; an estimate (MFN repeats the previous report''s time).';

-- Dedup for occurred events now includes the exchange, so a Nordic and a US
-- ticker with the same symbol are different companies.
DROP INDEX special_events_dedup_idx;
CREATE UNIQUE INDEX special_events_dedup_idx
    ON special_events (ticker, exchange_mic, event_type, occurred_at)
    WHERE status = 'occurred';
CREATE UNIQUE INDEX special_events_source_idx
    ON special_events (source, source_id)
    WHERE source_id IS NOT NULL;
CREATE UNIQUE INDEX special_events_scheduled_idx
    ON special_events (ticker, exchange_mic, expected_date, calendar_label)
    WHERE status = 'scheduled';
CREATE INDEX special_events_company_occurred_idx
    ON special_events (ticker, exchange_mic, occurred_at);
