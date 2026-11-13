-- The analyst's own database: which reports to prepare a kernel for, the
-- kernels themselves (kept forever, for audit), the preparation runs, and the
-- notes users add for a report.

CREATE TABLE report_targets (
    id                  BIGSERIAL   PRIMARY KEY,
    ticker              TEXT        NOT NULL,
    exchange_mic        TEXT        NOT NULL,
    report_kind         TEXT        NOT NULL CHECK (report_kind IN ('interim', 'annual')),
    fiscal_period       TEXT,
    expected_date       DATE        NOT NULL,
    expected_time_known BOOLEAN     NOT NULL,
    expected_at         TIMESTAMPTZ NOT NULL,
    signal_event_id     BIGINT      NOT NULL,
    status              TEXT        NOT NULL DEFAULT 'waiting'
        CHECK (status IN ('waiting', 'preparing', 'ready', 'failed', 'decided', 'skipped', 'cancelled')),
    prep_due_at         TIMESTAMPTZ NOT NULL,
    prep_attempts       INTEGER     NOT NULL DEFAULT 0 CHECK (prep_attempts >= 0),
    retry_after         TIMESTAMPTZ,
    prepared_for_at     TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL
);

COMMENT ON COLUMN report_targets.expected_at IS
    'The instant used for scheduling: the report''s expected publish instant when its time is known, '
    'otherwise the start of expected_date in the exchange''s time zone, the earliest it can come.';
COMMENT ON COLUMN report_targets.signal_event_id IS
    'The winning scheduled special_events row in signal this target came from; it changes when the other calendar source wins.';
COMMENT ON COLUMN report_targets.prep_due_at IS 'expected_at minus PREP_LEAD.';
COMMENT ON COLUMN report_targets.prep_attempts IS 'Preparations started in the current cycle; reset when a preparation succeeds or a new cycle starts.';
COMMENT ON COLUMN report_targets.retry_after IS 'A failed preparation is not retried before this instant.';
COMMENT ON COLUMN report_targets.prepared_for_at IS
    'expected_at when the preparation that produced the ready kernel started; a date that later moves '
    'by more than PREP_LEAD past it makes the kernel''s expectations stale.';

CREATE UNIQUE INDEX report_targets_report_idx
    ON report_targets (ticker, exchange_mic, report_kind, COALESCE(fiscal_period, ''), expected_date)
    WHERE status <> 'cancelled';
CREATE UNIQUE INDEX report_targets_signal_event_idx
    ON report_targets (signal_event_id)
    WHERE status <> 'cancelled';
CREATE INDEX report_targets_status_idx ON report_targets (status, expected_at);
CREATE INDEX report_targets_company_idx ON report_targets (ticker, exchange_mic, expected_at);

CREATE TABLE kernels (
    id               BIGSERIAL   PRIMARY KEY,
    target_id        BIGINT      NOT NULL REFERENCES report_targets (id) ON DELETE RESTRICT,
    version          INTEGER     NOT NULL CHECK (version >= 1),
    source           TEXT        NOT NULL,
    wasm             BYTEA       NOT NULL,
    sha256           TEXT        NOT NULL,
    metadata         JSONB       NOT NULL DEFAULT '{}',
    status           TEXT        NOT NULL
        CHECK (status IN ('candidate', 'accepted', 'rejected', 'superseded', 'spent')),
    rejection_reason TEXT,
    created_at       TIMESTAMPTZ NOT NULL,
    accepted_at      TIMESTAMPTZ,
    UNIQUE (target_id, version)
);

COMMENT ON TABLE kernels IS
    'Every kernel ever prepared, kept for audit. Never deleted: once its report is decided a kernel becomes spent.';

CREATE UNIQUE INDEX kernels_one_accepted_idx ON kernels (target_id) WHERE status = 'accepted';

CREATE FUNCTION kernels_are_never_deleted() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'kernels are never deleted (kernel %): mark it spent or rejected instead', OLD.id;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER kernels_no_delete BEFORE DELETE ON kernels
    FOR EACH ROW EXECUTE FUNCTION kernels_are_never_deleted();

CREATE TABLE prep_runs (
    id                BIGSERIAL   PRIMARY KEY,
    target_id         BIGINT      NOT NULL REFERENCES report_targets (id) ON DELETE RESTRICT,
    job_name          TEXT,
    upload_token_hash TEXT,
    started_at        TIMESTAMPTZ NOT NULL,
    finished_at       TIMESTAMPTZ,
    outcome           TEXT        NOT NULL DEFAULT 'running'
        CHECK (outcome IN ('running', 'succeeded', 'failed', 'timed_out', 'rejected')),
    detail            TEXT,
    attempt           INTEGER     NOT NULL CHECK (attempt >= 1),
    prepared_for_at   TIMESTAMPTZ NOT NULL
);

COMMENT ON COLUMN prep_runs.prepared_for_at IS 'The target''s expected_at when this run started.';

CREATE INDEX prep_runs_target_idx ON prep_runs (target_id, started_at);
CREATE INDEX prep_runs_running_idx ON prep_runs (target_id) WHERE outcome = 'running';

CREATE TABLE notes (
    id              BIGSERIAL   PRIMARY KEY,
    ticker          TEXT        NOT NULL,
    exchange_mic    TEXT        NOT NULL,
    fiscal_period   TEXT,
    text            TEXT        NOT NULL CHECK (octet_length(text) BETWEEN 1 AND 4096),
    author          TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL,
    received_at     TIMESTAMPTZ NOT NULL,
    source_event_id UUID        NOT NULL UNIQUE
);

COMMENT ON COLUMN notes.fiscal_period IS 'NULL: the note applies to the company''s next report after it arrived.';
COMMENT ON COLUMN notes.created_at IS 'When the author wrote the note (from the event).';
COMMENT ON COLUMN notes.received_at IS
    'When analyst stored it. A kernel prepared before this instant did not see the note.';

CREATE INDEX notes_company_idx ON notes (ticker, exchange_mic, received_at);
