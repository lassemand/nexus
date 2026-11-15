-- Where each simulated price came from (NEX-271): the quote book fed by
-- saxo-stream's /quotes/stream, or a GET /quote request. Saxo's timestamp and the
-- quote's age at the moment it was taken are kept so a trade can be audited for
-- look-ahead and staleness.

ALTER TABLE sim_trades
    ADD COLUMN entry_quote_source    TEXT        CHECK (entry_quote_source IN ('stream', 'request')),
    ADD COLUMN entry_quote_saxo_time TIMESTAMPTZ,
    ADD COLUMN entry_quote_age_ms    BIGINT,
    ADD COLUMN exit_quote_source     TEXT        CHECK (exit_quote_source IN ('stream', 'request')),
    ADD COLUMN exit_quote_saxo_time  TIMESTAMPTZ,
    ADD COLUMN exit_quote_age_ms     BIGINT;

ALTER TABLE sim_trades DROP CONSTRAINT sim_trades_status_check;
ALTER TABLE sim_trades ADD CONSTRAINT sim_trades_status_check
    CHECK (status IN ('pending_entry', 'open', 'closed', 'no_entry_quote', 'no_exit_quote',
                      'exit_missed', 'calendar_unknown', 'currency_mismatch', 'invalid_entry',
                      'quote_too_old', 'exit_quote_too_old'));

ALTER TABLE sim_observations
    ADD COLUMN quote_source TEXT   CHECK (quote_source IN ('stream', 'request')),
    ADD COLUMN quote_age_ms BIGINT;

ALTER TABLE sim_observations DROP CONSTRAINT sim_observations_status_check;
ALTER TABLE sim_observations ADD CONSTRAINT sim_observations_status_check
    CHECK (status IN ('ok', 'missed', 'beyond_close', 'no_quote', 'quote_too_old'));

COMMENT ON COLUMN sim_trades.entry_quote_source IS 'stream (read from the quote book) or request (GET /quote fallback).';
COMMENT ON COLUMN sim_trades.entry_quote_saxo_time IS 'Saxo''s LastUpdated of the entry quote, parsed, to audit that no earlier price was used. entry_saxo_at (NEX-255) is the raw text of the same stamp; this one is also set when the entry was rejected as too old.';
COMMENT ON COLUMN sim_trades.entry_quote_age_ms IS 'NULL when Saxo''s timestamp is unknown; never negative (clock skew is clamped to 0). The decision time (or the quote''s own time at open) minus entry_quote_saxo_time. A streamed quote of a thin share can be hours old and still current.';
COMMENT ON COLUMN sim_trades.exit_quote_source IS 'stream or request, for the exit quote (also set when it was rejected as too old).';
COMMENT ON COLUMN sim_trades.exit_quote_saxo_time IS 'Saxo''s LastUpdated of the exit quote.';
COMMENT ON COLUMN sim_trades.exit_quote_age_ms IS 'The target time minus exit_quote_saxo_time.';
COMMENT ON COLUMN sim_trades.status IS 'quote_too_old: the only entry quote had was a fallback request older than QUOTE_MAX_AGE, so there is no trade. exit_quote_too_old: the same at exit, so there is no result.';
COMMENT ON COLUMN sim_observations.quote_source IS 'stream or request.';
COMMENT ON COLUMN sim_observations.quote_age_ms IS 'NULL when unknown, never negative. The target time minus Saxo''s LastUpdated of the quote.';
