-- ISIN: the one identifier MFN, Saxo and Yahoo all share, whatever they call
-- the ticker. Shape only here; the check digit is validated by model::isin.
ALTER TABLE companies ADD COLUMN IF NOT EXISTS isin TEXT;
ALTER TABLE companies DROP CONSTRAINT IF EXISTS companies_isin_format;
ALTER TABLE companies
    ADD CONSTRAINT companies_isin_format
    CHECK (isin IS NULL OR isin ~ '^[A-Z]{2}[A-Z0-9]{9}[0-9]$');
CREATE UNIQUE INDEX IF NOT EXISTS companies_isin_key
    ON companies (isin) WHERE isin IS NOT NULL;

-- Seed only if the row exists; never inserts.
UPDATE companies
   SET name     = 'Novo Nordisk A/S (NOVO B)',
       mfn_slug = 'novo-nordisk',
       isin     = 'DK0062498333'
 WHERE ticker = 'NOVO-B' AND exchange_mic = 'XCSE';

UPDATE companies
   SET isin = 'SE0008348304'
 WHERE ticker = 'GOMX' AND exchange_mic = 'FNSE';
