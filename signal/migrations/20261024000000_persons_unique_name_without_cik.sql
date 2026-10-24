-- Persons without an SEC CIK (FI/PDMR filers) were inserted once per message,
-- so every re-published transaction got a new person_id and slipped past the
-- insider_filings unique key, duplicating the filing. FI publishes no person
-- identifier, so the name is the key for these rows.
CREATE UNIQUE INDEX persons_filer_name_without_cik_unique
    ON persons (filer_name)
    WHERE filer_cik IS NULL;
