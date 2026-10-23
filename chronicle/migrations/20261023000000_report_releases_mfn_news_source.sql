-- Switch report_releases from the Nasdaq news API (disclosure_id) to MFN's JSON
-- news feed (mfn_news_id) (NEX-166).
--
-- Nasdaq-sourced released rows are fully reproduced by a `nordic-reports
-- --backfill`, so they are removed here. Released rows that carry calendar data
-- (expected_date, written by the MFN calendar job) become scheduled rows again so
-- that the backfill re-attaches them through record_release's +-7 day match.

UPDATE report_releases SET superseded_by = NULL WHERE superseded_by IS NOT NULL;

-- Plain released rows (no calendar data) are history that the backfill recreates.
DELETE FROM report_releases WHERE released_at IS NOT NULL AND expected_date IS NULL;

-- Converting released rows back into scheduled ones could collide with another
-- scheduled row on report_releases_scheduled_key; keep the lowest id.
DELETE FROM report_releases r
USING report_releases k
WHERE r.expected_date IS NOT NULL
  AND r.calendar_label IS NOT NULL
  AND k.ticker = r.ticker
  AND k.exchange_mic = r.exchange_mic
  AND k.expected_date = r.expected_date
  AND k.calendar_label = r.calendar_label
  AND k.id < r.id;

-- Every remaining released row has an expected_date: turn it back into a
-- scheduled row, keeping expected_*, calendar_label, fiscal_period, report_kind.
UPDATE report_releases
   SET released_at     = NULL,
       disclosure_id   = NULL,
       headline        = NULL,
       url             = NULL,
       is_correction   = false,
       calendar_update = false
 WHERE released_at IS NOT NULL;

ALTER TABLE report_releases DROP CONSTRAINT IF EXISTS report_releases_disclosure_iff_released;
ALTER TABLE report_releases
    DROP COLUMN disclosure_id,
    DROP COLUMN category,
    ADD COLUMN mfn_news_id UUID UNIQUE,
    ADD COLUMN lang        TEXT,
    ADD COLUMN tags        TEXT[] NOT NULL DEFAULT '{}';

-- A row counts as released exactly when it has an MFN news item.
ALTER TABLE report_releases
    ADD CONSTRAINT report_releases_news_iff_released
    CHECK ((mfn_news_id IS NULL) = (released_at IS NULL));

-- One row per actual release: the sv/en twin of a report must not become a second row.
CREATE UNIQUE INDEX IF NOT EXISTS report_releases_release_key
    ON report_releases (ticker, exchange_mic, released_at, report_kind)
    WHERE report_kind IS NOT NULL;
