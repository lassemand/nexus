-- Simulated trades: one per decision that is traded (buy, and the shadow
-- decisions no_buy, abstain and skipped_no_kernel). Nothing is ever traded;
-- the prices are quotes read through saxo-stream. Per share only: no fees, no
-- position size.

CREATE TABLE sim_trades (
    id                  BIGSERIAL     PRIMARY KEY,
    decision_id         BIGINT        NOT NULL UNIQUE REFERENCES decisions (id) ON DELETE RESTRICT,
    ticker              TEXT          NOT NULL,
    exchange_mic        TEXT          NOT NULL,
    decision            TEXT          NOT NULL
        CHECK (decision IN ('buy', 'no_buy', 'abstain', 'skipped_no_kernel')),
    shadow              BOOLEAN       NOT NULL,
    published_at        TIMESTAMPTZ   NOT NULL,
    hold_minutes        NUMERIC(10,3) NOT NULL CHECK (hold_minutes > 0),
    entry_target_at     TIMESTAMPTZ,
    entry_at            TIMESTAMPTZ,
    entry_ask           NUMERIC(20,6),
    entry_bid           NUMERIC(20,6),
    entry_saxo_at       TEXT,
    exit_target_at      TIMESTAMPTZ,
    exit_at             TIMESTAMPTZ,
    exit_bid            NUMERIC(20,6),
    exit_ask            NUMERIC(20,6),
    pnl_per_share       NUMERIC(20,6),
    pnl_pct             NUMERIC(20,4),
    currency            TEXT,
    release_to_entry_ms BIGINT,
    status              TEXT          NOT NULL DEFAULT 'pending_entry'
        CHECK (status IN ('pending_entry', 'open', 'closed', 'no_entry_quote', 'no_exit_quote',
                          'exit_missed', 'calendar_unknown', 'currency_mismatch', 'invalid_entry')),
    version             BIGINT        NOT NULL DEFAULT 1,
    trace_id            TEXT          CHECK (trace_id IS NULL OR trace_id ~ '^[0-9a-f]{32}$'),
    release_traceparent TEXT,
    trade_trace_id      TEXT          CHECK (trade_trace_id IS NULL OR trade_trace_id ~ '^[0-9a-f]{32}$'),
    created_at          TIMESTAMPTZ   NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ   NOT NULL DEFAULT now()
);

COMMENT ON TABLE sim_trades IS 'Simulated trades. Per share only: no fees, no position size, no overnight holding.';
COMMENT ON COLUMN sim_trades.shadow IS 'True for no_buy, abstain and skipped_no_kernel: simulated to show whether not buying avoided losses.';
COMMENT ON COLUMN sim_trades.hold_minutes IS 'HOLD, in trading minutes counted from entry_at inside the session.';
COMMENT ON COLUMN sim_trades.entry_at IS 'When our request for the accepted entry quote was answered.';
COMMENT ON COLUMN sim_trades.entry_ask IS 'The ask paid to buy: the first Tradable, undelayed, positive ask at or after entry_target_at.';
COMMENT ON COLUMN sim_trades.exit_bid IS 'The bid received to sell, at exit_target_at.';
COMMENT ON COLUMN sim_trades.pnl_per_share IS 'exit_bid - entry_ask, in currency; no fees.';
COMMENT ON COLUMN sim_trades.pnl_pct IS '(exit_bid - entry_ask) / entry_ask * 100, 4 decimals.';
COMMENT ON COLUMN sim_trades.release_to_entry_ms IS 'entry_at - published_at (MFN), so pre-open and intraday releases can be compared.';
COMMENT ON COLUMN sim_trades.version IS 'Increases with every state change; the analyst.trades consumer keeps the newest.';
COMMENT ON COLUMN sim_trades.trace_id IS 'The release trace (the same as decisions.trace_id).';
COMMENT ON COLUMN sim_trades.release_traceparent IS 'W3C traceparent of the decision span, to link a trade resumed after a restart.';
COMMENT ON COLUMN sim_trades.trade_trace_id IS 'The trade''s own trace (the holding period).';

CREATE INDEX sim_trades_open_idx ON sim_trades (status) WHERE status IN ('pending_entry', 'open');
CREATE INDEX sim_trades_company_idx ON sim_trades (ticker, exchange_mic, published_at);

CREATE TABLE sim_observations (
    trade_id        BIGINT        NOT NULL REFERENCES sim_trades (id) ON DELETE CASCADE,
    horizon         TEXT          NOT NULL,
    scheduled_at    TIMESTAMPTZ   NOT NULL,
    fetched_at      TIMESTAMPTZ,
    bid             NUMERIC(20,6),
    ask             NUMERIC(20,6),
    bid_type        TEXT,
    ask_type        TEXT,
    delayed_minutes BIGINT,
    late            BOOLEAN       NOT NULL DEFAULT false,
    status          TEXT          NOT NULL
        CHECK (status IN ('ok', 'missed', 'beyond_close', 'no_quote')),
    PRIMARY KEY (trade_id, horizon)
);

COMMENT ON COLUMN sim_observations.horizon IS 'After entry: 1m, 5m, ... or close (the session cutoff), or hold (the exit quote).';
COMMENT ON COLUMN sim_observations.late IS 'fetched_at - scheduled_at over 2 s.';
COMMENT ON COLUMN sim_observations.status IS 'ok, missed (the target passed over 2 s before a restart), beyond_close, no_quote.';
