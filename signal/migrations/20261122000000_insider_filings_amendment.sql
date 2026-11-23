-- NEX-282: Danish MAR releases.
-- is_amendment: a Danish amendment replaces the original notification's rows and
-- those of an amendment filed before it; rows written from an amendment are
-- marked so the amendment arriving again (its language twin, a re-delivery)
-- never deletes them.
-- source_registry, release_lang: the registry and the language of the copy a
-- row was written from, so a language twin that spells a name differently is
-- recognised as the same trade, not stored under a second person.
ALTER TABLE insider_filings
    ADD COLUMN is_amendment BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN source_registry INTEGER,
    ADD COLUMN release_lang TEXT;
