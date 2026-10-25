-- Progress of saxo-stream's daily-bar sync (NEX-162). The bars themselves are
-- stored by signal (`daily_bars`); this only remembers how far saxo-stream has
-- published them to Kafka. Deleting a row forces a re-backfill of that ticker.
CREATE TABLE IF NOT EXISTS saxo_daily_bars_state (
    ticker              TEXT        NOT NULL,
    exchange_mic        TEXT        NOT NULL,
    -- Oldest date published so far; the backfill floor once the history is complete.
    backfilled_from     DATE,
    last_published_date DATE,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (ticker, exchange_mic)
);
