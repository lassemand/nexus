-- Official daily bars for Nordic instruments (NEX-162), fed by `market.daily_bars`
-- from saxo-stream. Separate from `bars`, which for FNSE tickers only holds the last
-- minute bar of each day. NUMERIC end to end: these feed percentage returns.
CREATE TABLE IF NOT EXISTS daily_bars (
    ticker       TEXT          NOT NULL,
    exchange_mic TEXT          NOT NULL,
    date         DATE          NOT NULL,
    open         NUMERIC(20,6) NOT NULL,
    high         NUMERIC(20,6) NOT NULL,
    low          NUMERIC(20,6) NOT NULL,
    close        NUMERIC(20,6) NOT NULL,
    volume       BIGINT        NOT NULL,
    currency     TEXT          NOT NULL,
    source       TEXT          NOT NULL,
    fetched_at   TIMESTAMPTZ   NOT NULL,
    PRIMARY KEY (ticker, exchange_mic, date)
);
