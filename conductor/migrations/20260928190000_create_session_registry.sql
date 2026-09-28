-- Session registry: which Claude Code session serves which group.
--
-- Replaces the JSON-file design from the original ticket. Postgres provides the
-- locking, atomicity and crash-safety that design hand-rolled.

CREATE TABLE IF NOT EXISTS session_groups (
    -- GroupKey::slug() — also the worktree directory and branch component.
    slug            TEXT        PRIMARY KEY,
    -- The GroupKey itself, so Named and Issue keys stay distinguishable.
    key             JSONB       NOT NULL,
    -- Passed to `claude --session-id` on a fresh run, `--resume` thereafter.
    session_id      UUID        NOT NULL,
    -- False until a run has used session_id; resuming before then would fail.
    session_started BOOLEAN     NOT NULL DEFAULT FALSE,
    -- GroupState: Idle, or Running/Attached with pid and process start time.
    state           JSONB       NOT NULL,
    -- Empty until the dispatcher creates the worktree.
    worktree        TEXT        NOT NULL DEFAULT '',
    branches        TEXT[]      NOT NULL DEFAULT '{}',
    issues          TEXT[]      NOT NULL DEFAULT '{}',
    last_active     TIMESTAMPTZ NOT NULL,
    last_run        JSONB,
    created_at      TIMESTAMPTZ NOT NULL
);

-- Accepted events awaiting dispatch. These are webhooks that already received
-- 200, so they must outlive a process restart.
CREATE TABLE IF NOT EXISTS pending_events (
    id          BIGSERIAL   PRIMARY KEY,
    group_slug  TEXT        NOT NULL REFERENCES session_groups(slug) ON DELETE CASCADE,
    event       JSONB       NOT NULL,
    received_at TIMESTAMPTZ NOT NULL
);

-- Dispatch order is FIFO per group.
CREATE INDEX IF NOT EXISTS pending_events_group_order
    ON pending_events (group_slug, id);

-- Delivery IDs already processed, for webhook idempotency. `seq` gives the FIFO
-- ordering used to cap the table.
CREATE TABLE IF NOT EXISTS seen_deliveries (
    delivery_id TEXT        PRIMARY KEY,
    seq         BIGSERIAL   NOT NULL,
    seen_at     TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS seen_deliveries_seq ON seen_deliveries (seq);
