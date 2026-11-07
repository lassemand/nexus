ALTER TABLE special_events ADD COLUMN body_text TEXT;

COMMENT ON COLUMN special_events.body_text IS
    'company_announcement rows: the release as plain text (NULL if MFN had none). NULL for every other row.';

-- A company can publish two different announcements in the same second, so
-- announcements are not deduped by instant; special_events_source_idx
-- (source, source_id) dedupes them instead.
DROP INDEX special_events_dedup_idx;
CREATE UNIQUE INDEX special_events_dedup_idx
    ON special_events (ticker, exchange_mic, event_type, occurred_at)
    WHERE status = 'occurred' AND event_type <> 'company_announcement';
