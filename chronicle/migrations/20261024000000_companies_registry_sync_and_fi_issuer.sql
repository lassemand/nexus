-- Companies now arrive from the `company.tickers` topic (chronicle_lib::registry),
-- which carries no sector. chronicle never reads the sector; signal keeps its own
-- copy in the backtest database.
ALTER TABLE companies ALTER COLUMN sector DROP NOT NULL;

-- Issuer name exactly as Finansinspektionen's insider register spells it. pdmr
-- otherwise searches FI's issuer names with the ticker, which finds nothing for
-- e.g. GOMX ("GomSpace Group AB").
ALTER TABLE companies ADD COLUMN fi_issuer TEXT;
CREATE UNIQUE INDEX companies_fi_issuer_key ON companies (fi_issuer) WHERE fi_issuer IS NOT NULL;
