-- The analyst's decisions and simulated trades (NEX-257), fed by the Kafka topics
-- `analyst.decisions` and `analyst.trades`. Separate from `trade_results`, which
-- belongs to the old US earnings strategy (f64 prices, dates only).
--
-- There is deliberately no foreign key from simulated_trades.news_id to
-- analyst_decisions: the two come from different topics, so a trade can arrive
-- before its decision, and a DEFERRABLE key would still make that transaction
-- fail at commit. An index serves the join instead; rows are joined on read.
CREATE TABLE analyst_decisions (
    news_id        TEXT        PRIMARY KEY,
    ticker         TEXT        NOT NULL,
    exchange_mic   TEXT        NOT NULL,
    fiscal_period  TEXT,
    lang           TEXT        NOT NULL,
    published_at   TIMESTAMPTZ NOT NULL,
    received_at    TIMESTAMPTZ NOT NULL,
    decided_at     TIMESTAMPTZ NOT NULL,
    decision       TEXT        NOT NULL,
    reason         TEXT        NOT NULL,
    kernel_id      BIGINT,
    kernel_micros  BIGINT      NOT NULL DEFAULT 0,
    error          TEXT,
    metrics        JSONB       NOT NULL DEFAULT '[]'::jsonb,
    trace_id       TEXT        CHECK (trace_id ~ '^[0-9a-f]{32}$'),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX analyst_decisions_company_idx
    ON analyst_decisions (ticker, exchange_mic, published_at DESC);

CREATE TABLE simulated_trades (
    trade_id              BIGINT        PRIMARY KEY,
    news_id               TEXT          NOT NULL,
    ticker                TEXT          NOT NULL,
    exchange_mic          TEXT          NOT NULL,
    decision              TEXT          NOT NULL,
    shadow                BOOLEAN       NOT NULL,
    published_at          TIMESTAMPTZ   NOT NULL,
    hold_minutes          NUMERIC(10,3) NOT NULL,
    entry_target_at       TIMESTAMPTZ,
    entry_at              TIMESTAMPTZ,
    entry_ask             NUMERIC(20,6),
    entry_bid             NUMERIC(20,6),
    exit_target_at        TIMESTAMPTZ,
    exit_at               TIMESTAMPTZ,
    exit_bid              NUMERIC(20,6),
    exit_ask              NUMERIC(20,6),
    pnl_per_share         NUMERIC(20,6),
    pnl_pct               NUMERIC(12,4),
    currency              TEXT,
    release_to_entry_ms   BIGINT,
    status                TEXT          NOT NULL,
    version               BIGINT        NOT NULL,
    updated_at            TIMESTAMPTZ   NOT NULL DEFAULT now(),
    entry_quote_source    TEXT          CHECK (entry_quote_source IN ('stream', 'request')),
    exit_quote_source     TEXT          CHECK (exit_quote_source IN ('stream', 'request')),
    entry_quote_saxo_time TIMESTAMPTZ,
    exit_quote_saxo_time  TIMESTAMPTZ,
    entry_quote_age_ms    BIGINT,
    exit_quote_age_ms     BIGINT,
    trace_id              TEXT          CHECK (trace_id ~ '^[0-9a-f]{32}$'),
    trade_trace_id        TEXT          CHECK (trade_trace_id ~ '^[0-9a-f]{32}$')
);
CREATE INDEX simulated_trades_news_idx ON simulated_trades (news_id);
CREATE INDEX simulated_trades_published_idx ON simulated_trades (published_at DESC);
CREATE INDEX simulated_trades_company_idx
    ON simulated_trades (ticker, exchange_mic, published_at DESC);

COMMENT ON TABLE simulated_trades IS
    'Forward-only and simulated: each row exists only because the analyst decided live, before the market reacted, so there is no look-ahead bias. '
    'It cannot be backfilled: a kernel prepared today for a past report would know the outcome. '
    'No fees and no position sizing: results are per-share returns. '
    'For releases before the open it measures the move after the open. '
    'Shadow trades (no_buy, abstain, skipped_no_kernel) show what would have happened without acting; they are not actions taken.';

CREATE TABLE simulated_trade_observations (
    trade_id        BIGINT        NOT NULL,
    horizon         TEXT          NOT NULL,
    scheduled_at    TIMESTAMPTZ   NOT NULL,
    fetched_at      TIMESTAMPTZ,
    bid             NUMERIC(20,6),
    ask             NUMERIC(20,6),
    bid_type        TEXT,
    ask_type        TEXT,
    delayed_minutes BIGINT,
    late            BOOLEAN       NOT NULL,
    status          TEXT          NOT NULL,
    quote_source    TEXT          CHECK (quote_source IN ('stream', 'request')),
    quote_age_ms    BIGINT,
    PRIMARY KEY (trade_id, horizon)
);
