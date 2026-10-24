-- Events that change after publication (Nordic reports: scheduled, moved,
-- then released) carry a producer-chosen source_key and are upserted by it.
-- Keyless events (earnings) keep the old (ticker, event_type, occurred_at)
-- deduplication, now limited to them so a keyed event can move freely.
ALTER TABLE special_events
    ADD COLUMN source_key TEXT,
    ADD COLUMN scheduled  BOOLEAN NOT NULL DEFAULT false;

DROP INDEX special_events_dedup_idx;
CREATE UNIQUE INDEX special_events_dedup_idx
    ON special_events (ticker, event_type, occurred_at)
    WHERE source_key IS NULL;
CREATE UNIQUE INDEX special_events_source_key_idx
    ON special_events (source_key)
    WHERE source_key IS NOT NULL;
