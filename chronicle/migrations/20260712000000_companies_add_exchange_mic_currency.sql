-- Mirror of signal migration. Using IF NOT EXISTS / IF EXISTS guards means
-- this is a no-op if both services share the same database.

ALTER TABLE companies
    ADD COLUMN IF NOT EXISTS exchange_mic TEXT NOT NULL DEFAULT 'XNAS',
    ADD COLUMN IF NOT EXISTS currency     TEXT NOT NULL DEFAULT 'USD';

ALTER TABLE companies
    ALTER COLUMN exchange_mic DROP DEFAULT,
    ALTER COLUMN currency     DROP DEFAULT;

ALTER TABLE companies DROP CONSTRAINT IF EXISTS companies_pkey;
ALTER TABLE companies ADD PRIMARY KEY (ticker, exchange_mic);
