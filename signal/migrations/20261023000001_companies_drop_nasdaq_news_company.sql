-- Mirror of signal migration. Using IF EXISTS guards means this is a no-op if
-- both services share the same database. Releases now come from MFN
-- (companies.mfn_slug), so the Nasdaq issuer mapping is no longer needed.

DROP INDEX IF EXISTS companies_nasdaq_news_company_key;
ALTER TABLE companies DROP COLUMN IF EXISTS nasdaq_news_company;
