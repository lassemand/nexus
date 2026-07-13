-- Add currency column to trade_results for cross-currency safety (NEX-84).
--
-- Existing rows are all USD (Yahoo Finance was the only price provider and
-- hardcoded USD). New rows will carry the actual currency from the Price
-- response so SEK-denominated Nordic trades are stored correctly.
ALTER TABLE trade_results
    ADD COLUMN IF NOT EXISTS currency TEXT NOT NULL DEFAULT 'USD';

ALTER TABLE trade_results
    ALTER COLUMN currency DROP DEFAULT;
