CREATE TABLE insider_filings (
    id                BIGSERIAL        PRIMARY KEY,
    ticker            TEXT             NOT NULL,
    cik               TEXT             NOT NULL,
    filer_name        TEXT             NOT NULL,
    filer_role        TEXT             NOT NULL,
    transaction_date  DATE             NOT NULL,
    filing_date       DATE             NOT NULL,
    shares            DOUBLE PRECISION NOT NULL,
    price_per_share   DOUBLE PRECISION NOT NULL,
    transaction_code  TEXT             NOT NULL,
    created_at        TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    UNIQUE (cik, ticker, transaction_date, filer_name, shares)
);
CREATE INDEX insider_filings_ticker_transaction_date
    ON insider_filings (ticker, transaction_date DESC);
