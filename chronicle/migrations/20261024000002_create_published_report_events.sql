-- Outbox bookkeeping for publishing report_releases rows as SpecialEvents
-- (chronicle_lib::nordic::events). One row per report last published, with a
-- fingerprint of what was sent: a changed fingerprint is re-published, a key
-- whose report is gone is published as a removal and deleted here.
CREATE TABLE published_report_events (
    source_key   TEXT        PRIMARY KEY,
    fingerprint  TEXT        NOT NULL,
    published_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
