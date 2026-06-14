CREATE TABLE ticker_scores (
    id                    BIGSERIAL        PRIMARY KEY,
    ticker                TEXT             NOT NULL,
    score_date            DATE             NOT NULL,
    insider_score         DOUBLE PRECISION NOT NULL,
    triggering_person_ids BIGINT[]         NOT NULL,
    car_20d               DOUBLE PRECISION,
    car_10d               DOUBLE PRECISION,
    car_5d                DOUBLE PRECISION,
    mean_abvol_20d        DOUBLE PRECISION,
    max_abvol_20d         DOUBLE PRECISION,
    realized_vol_20d      DOUBLE PRECISION,
    price_momentum_20d    DOUBLE PRECISION,
    vol_trend_slope       DOUBLE PRECISION,
    created_at            TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    UNIQUE (ticker, score_date)
);
CREATE INDEX ticker_scores_triggering_person_ids
    ON ticker_scores USING GIN (triggering_person_ids);
