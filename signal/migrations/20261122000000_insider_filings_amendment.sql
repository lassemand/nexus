-- NEX-282: a Danish MAR amendment replaces the original notification's rows and
-- those of an amendment filed before it. Rows written from an amendment are
-- marked so the amendment arriving again (its language twin, a re-delivery)
-- never deletes them.
ALTER TABLE insider_filings
    ADD COLUMN is_amendment BOOLEAN NOT NULL DEFAULT false;
