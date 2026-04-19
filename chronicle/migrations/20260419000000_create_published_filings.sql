CREATE TABLE IF NOT EXISTS published_filings (
    accession_number TEXT PRIMARY KEY,
    ticker           TEXT NOT NULL,
    published_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
