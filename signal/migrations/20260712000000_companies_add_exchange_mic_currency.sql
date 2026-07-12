-- Add exchange_mic and currency to the companies registry.
--
-- exchange_mic: ISO 10383 MIC identifying the exchange (e.g. 'XNAS', 'FNSE').
-- currency:     ISO 4217 trading currency (e.g. 'USD', 'SEK').
--
-- Backfill: all existing rows are US-listed instruments registered before this
-- migration — they get XNAS and USD as correct explicit values.

ALTER TABLE companies
    ADD COLUMN IF NOT EXISTS exchange_mic TEXT NOT NULL DEFAULT 'XNAS',
    ADD COLUMN IF NOT EXISTS currency     TEXT NOT NULL DEFAULT 'USD';

-- Remove defaults after backfill so future inserts must be explicit.
ALTER TABLE companies
    ALTER COLUMN exchange_mic DROP DEFAULT,
    ALTER COLUMN currency     DROP DEFAULT;

-- The primary key was ticker only. With exchange_mic, the same ticker string
-- on different exchanges is a distinct instrument. Re-key on (ticker, exchange_mic).
-- Drop and recreate the PK to avoid silent collisions.
ALTER TABLE companies DROP CONSTRAINT IF EXISTS companies_pkey;
ALTER TABLE companies ADD PRIMARY KEY (ticker, exchange_mic);
