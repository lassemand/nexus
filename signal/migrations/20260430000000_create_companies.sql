CREATE TABLE IF NOT EXISTS companies (
    ticker     TEXT        PRIMARY KEY,
    sector     TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
