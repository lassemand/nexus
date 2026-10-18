-- Mirror of signal migration. Using IF NOT EXISTS / IF EXISTS guards means
-- this is a no-op if both services share the same database.

ALTER TABLE companies
    ADD COLUMN IF NOT EXISTS name                TEXT,
    ADD COLUMN IF NOT EXISTS mfn_slug            TEXT,
    ADD COLUMN IF NOT EXISTS nasdaq_news_company TEXT;

-- mfn_slug is interpolated into a URL, so only allow URL-safe slugs.
ALTER TABLE companies DROP CONSTRAINT IF EXISTS companies_mfn_slug_format;
ALTER TABLE companies
    ADD CONSTRAINT companies_mfn_slug_format
    CHECK (mfn_slug IS NULL OR mfn_slug ~ '^[a-z0-9][a-z0-9-]*$');

CREATE UNIQUE INDEX IF NOT EXISTS companies_mfn_slug_key
    ON companies (mfn_slug) WHERE mfn_slug IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS companies_nasdaq_news_company_key
    ON companies (nasdaq_news_company) WHERE nasdaq_news_company IS NOT NULL;

-- Seed GomSpace only if the row already exists; never inserts.
UPDATE companies
   SET name                = 'GomSpace Group AB (GOMX)',
       mfn_slug            = 'gomspace-group',
       nasdaq_news_company = 'GomSpace Group AB'
 WHERE ticker = 'GOMX' AND exchange_mic = 'FNSE';
