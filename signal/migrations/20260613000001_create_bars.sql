CREATE TABLE bars (
    ticker  TEXT             NOT NULL,
    date    DATE             NOT NULL,
    open    DOUBLE PRECISION NOT NULL,
    high    DOUBLE PRECISION NOT NULL,
    low     DOUBLE PRECISION NOT NULL,
    close   DOUBLE PRECISION NOT NULL,
    volume  BIGINT           NOT NULL,
    PRIMARY KEY (ticker, date)
);
