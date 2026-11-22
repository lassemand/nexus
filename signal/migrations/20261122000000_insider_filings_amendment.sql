-- NEX-282: a Danish MAR amendment replaces the original notification's rows.
-- Rows written from an amendment are marked so a later amendment message (its
-- language twin, or a re-delivery) never deletes them.
ALTER TABLE insider_filings
    ADD COLUMN is_amendment BOOLEAN NOT NULL DEFAULT false;
