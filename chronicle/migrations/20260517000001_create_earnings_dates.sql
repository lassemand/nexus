CREATE TABLE earnings_dates (
    id              BIGSERIAL PRIMARY KEY,
    ticker          TEXT             NOT NULL,
    fiscal_year     SMALLINT         NOT NULL,
    fiscal_quarter  SMALLINT         NOT NULL CHECK (fiscal_quarter BETWEEN 1 AND 4),
    period_end      DATE             NOT NULL,
    filing_date     DATE             NOT NULL,
    eps_actual      DOUBLE PRECISION,   -- informational only, not for monetary arithmetic
    revenue_actual  DOUBLE PRECISION,   -- informational only, not for monetary arithmetic
    published_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ      NOT NULL DEFAULT now(),
    CONSTRAINT uq_ticker_quarter UNIQUE (ticker, fiscal_year, fiscal_quarter)
);
CREATE INDEX idx_earnings_dates_ticker ON earnings_dates (ticker);
