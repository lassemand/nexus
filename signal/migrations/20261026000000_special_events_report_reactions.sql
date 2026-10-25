-- Share-price reaction of each released Nordic report (NEX-163), stored on the
-- report's special_events row. All NULL for every other kind of event.
--
-- release_session : when the report came out relative to the Stockholm session
-- reaction_date   : first trading day the market could react (r)
-- base_date       : last trading day before r (b); base_close is the last close
--                   on or before b
-- move_pct        : (reaction_close - base_close) / base_close * 100, NUMERIC
-- data_status     : 'ok', or why move_pct is NULL
-- needs_review    : |move_pct| > 40, usually a split/rights issue in unadjusted prices
ALTER TABLE special_events
    ADD COLUMN release_session TEXT
        CHECK (release_session IN ('pre_open', 'intraday', 'after_close', 'non_trading_day')),
    ADD COLUMN reaction_date          DATE,
    ADD COLUMN base_date              DATE,
    ADD COLUMN base_close             NUMERIC(20, 6),
    ADD COLUMN reaction_close         NUMERIC(20, 6),
    ADD COLUMN move_pct               NUMERIC(20, 4),
    ADD COLUMN reaction_volume        BIGINT,
    ADD COLUMN data_status            TEXT
        CHECK (data_status IN ('ok', 'missing_reaction_bar', 'missing_base_bar', 'future',
                               'zero_base', 'stale_base', 'calendar_unknown',
                               'implausible_move')),
    ADD COLUMN needs_review           BOOLEAN,
    ADD COLUMN reaction_computed_at   TIMESTAMPTZ,
    ADD CONSTRAINT special_events_move_only_when_ok
        CHECK (move_pct IS NULL OR data_status = 'ok');

CREATE INDEX special_events_reaction_company_idx
    ON special_events (ticker, exchange_mic, reaction_date)
    WHERE data_status IS NOT NULL;
