CREATE TABLE event_signals (
    id                  BIGSERIAL PRIMARY KEY,
    ticker              TEXT             NOT NULL,
    event_type          TEXT             NOT NULL,
    event_date          DATE             NOT NULL,
    -- pre-event features
    car_20d             DOUBLE PRECISION,
    car_10d             DOUBLE PRECISION,
    car_5d              DOUBLE PRECISION,
    mean_abvol_20d      DOUBLE PRECISION,
    max_abvol_20d       DOUBLE PRECISION,
    realized_vol_20d    DOUBLE PRECISION,
    price_momentum_20d  DOUBLE PRECISION,
    vol_trend_slope     DOUBLE PRECISION,
    pre_event_bars      INT,
    -- post-event truth (filled by NEX-53)
    post_ar_1d          DOUBLE PRECISION,
    post_ar_5d          DOUBLE PRECISION,
    truth_labeled_at    TIMESTAMPTZ,
    -- ML score (filled by NEX-54)
    insider_score       DOUBLE PRECISION,
    scored_at           TIMESTAMPTZ,
    created_at          TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    UNIQUE (ticker, event_type, event_date)
);
