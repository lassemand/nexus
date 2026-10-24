-- One word for "has happened": the dashboard calls past reports and past
-- special events "occurred", and special_events already has occurred_at.
-- Indexes and checks follow the column; their names are renamed to match.
ALTER TABLE report_releases RENAME COLUMN released_at TO occurred_at;
ALTER INDEX report_releases_company_released_idx RENAME TO report_releases_company_occurred_idx;
ALTER INDEX report_releases_release_key RENAME TO report_releases_occurred_key;
ALTER TABLE report_releases
    RENAME CONSTRAINT report_releases_news_iff_released TO report_releases_news_iff_occurred;
