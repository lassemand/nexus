-- One row per release the analyst saw and decided on: the moment of truth of a
-- report. A decision is never deleted; `news_id` (MFN's id of the release) makes
-- recording it idempotent.

CREATE TABLE decisions (
    id            BIGSERIAL   PRIMARY KEY,
    target_id     BIGINT      REFERENCES report_targets (id) ON DELETE RESTRICT,
    kernel_id     BIGINT      REFERENCES kernels (id) ON DELETE RESTRICT,
    news_id       TEXT        NOT NULL UNIQUE,
    ticker        TEXT        NOT NULL,
    exchange_mic  TEXT        NOT NULL,
    lang          TEXT        NOT NULL,
    fiscal_period TEXT,
    published_at  TIMESTAMPTZ NOT NULL,
    received_at   TIMESTAMPTZ NOT NULL,
    decided_at    TIMESTAMPTZ NOT NULL,
    decision      TEXT        NOT NULL
        CHECK (decision IN ('buy', 'no_buy', 'abstain', 'skipped_no_kernel', 'kernel_error', 'stale', 'twin_ignored')),
    reason        TEXT,
    metrics       JSONB       NOT NULL DEFAULT '[]',
    kernel_micros BIGINT      CHECK (kernel_micros IS NULL OR kernel_micros >= 0),
    error         TEXT,
    trace_id      TEXT        CHECK (trace_id IS NULL OR trace_id ~ '^[0-9a-f]{32}$')
);

COMMENT ON COLUMN decisions.target_id IS 'The report target the release matched; NULL for an unscheduled or early report.';
COMMENT ON COLUMN decisions.kernel_id IS 'The kernel that ran, when one did.';
COMMENT ON COLUMN decisions.news_id IS 'MFN news id of the release (special_events.source_id); one decision per release.';
COMMENT ON COLUMN decisions.published_at IS 'When MFN published the release (the event''s occurred_at).';
COMMENT ON COLUMN decisions.received_at IS 'When the analyst consumed the event.';
COMMENT ON COLUMN decisions.decided_at IS 'When the decision was made (the kernel returned, or the release was ruled out).';
COMMENT ON COLUMN decisions.metrics IS 'The kernel''s verdict metrics: [{key, current, prior, expected, unit}], decimal strings.';
COMMENT ON COLUMN decisions.trace_id IS 'The OpenTelemetry trace this decision was made in (32 lowercase hex), to find it in Tempo.';

CREATE INDEX decisions_company_idx ON decisions (ticker, exchange_mic, published_at);
CREATE INDEX decisions_target_idx ON decisions (target_id) WHERE target_id IS NOT NULL;
