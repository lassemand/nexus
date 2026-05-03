-- Mirror of the signal migration. Using IF NOT EXISTS means this is a
-- no-op when both services share the same database and signal has already
-- applied its own companies migration.
CREATE TABLE IF NOT EXISTS companies (
    ticker     TEXT        PRIMARY KEY,
    sector     TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
