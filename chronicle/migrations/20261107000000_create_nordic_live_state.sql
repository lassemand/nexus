CREATE TABLE nordic_live_state (
    id         BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    watermark  TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

COMMENT ON TABLE nordic_live_state IS
    'Single row: nordic-live''s watermark, the newest MFN publish_date it has processed from the socket or a catch-up. A restart catches up from it.';
