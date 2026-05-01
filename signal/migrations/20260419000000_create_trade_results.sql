CREATE TABLE IF NOT EXISTS trade_results (
    id              BIGSERIAL PRIMARY KEY,
    ticker          TEXT NOT NULL,
    earnings_date   DATE NOT NULL,
    buy_date        DATE NOT NULL,
    buy_price       DOUBLE PRECISION NOT NULL,
    sell_price      DOUBLE PRECISION NOT NULL,
    pnl             DOUBLE PRECISION NOT NULL,
    pnl_pct         DOUBLE PRECISION NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS trade_results_ticker_idx ON trade_results (ticker);
CREATE INDEX IF NOT EXISTS trade_results_earnings_date_idx ON trade_results (earnings_date);
