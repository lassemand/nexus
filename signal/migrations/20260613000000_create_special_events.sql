CREATE TABLE special_events (
    id           BIGSERIAL PRIMARY KEY,
    ticker       TEXT        NOT NULL,
    event_type   TEXT        NOT NULL,
    occurred_at  TIMESTAMPTZ NOT NULL,
    description  TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX special_events_occurred_at_idx ON special_events (occurred_at);
CREATE INDEX special_events_ticker_idx      ON special_events (ticker);
CREATE UNIQUE INDEX special_events_dedup_idx ON special_events (ticker, event_type, occurred_at);
