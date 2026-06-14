CREATE TABLE persons (
    id         BIGSERIAL    PRIMARY KEY,
    filer_cik  TEXT         NULL,
    filer_name TEXT         NOT NULL,
    created_at TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- Uniqueness enforced only among non-NULL CIKs; multiple NULL-CIK rows are allowed.
CREATE UNIQUE INDEX persons_filer_cik_unique
    ON persons (filer_cik)
    WHERE filer_cik IS NOT NULL;

CREATE TABLE insider_filings (
    id                BIGSERIAL        PRIMARY KEY,
    person_id         BIGINT           NOT NULL REFERENCES persons(id),
    ticker            TEXT             NOT NULL,
    issuer_cik        TEXT             NOT NULL,
    filer_role        TEXT             NOT NULL,
    transaction_date  DATE             NOT NULL,
    filing_date       DATE             NOT NULL,
    shares            DOUBLE PRECISION NOT NULL,
    price_per_share   DOUBLE PRECISION NOT NULL,
    transaction_code  TEXT             NOT NULL,
    created_at        TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    UNIQUE (person_id, ticker, transaction_date, shares, transaction_code)
);

CREATE INDEX insider_filings_ticker_transaction_date
    ON insider_filings (ticker, transaction_date DESC);
CREATE INDEX insider_filings_ticker_code
    ON insider_filings (ticker, transaction_code, transaction_date DESC);
