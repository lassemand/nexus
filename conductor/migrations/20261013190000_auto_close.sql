-- Automatic closing of finished session groups.
--
-- close_requested_at: a Linear issue in the group's set was completed (or
--   removed), so the set should be checked for completeness.
-- close_blocked: the last automatic close was refused by git (typically a dirty
--   worktree). The group is not re-checked automatically until an operator
--   closes it by hand.
ALTER TABLE session_groups ADD COLUMN IF NOT EXISTS close_requested_at TIMESTAMPTZ;
ALTER TABLE session_groups ADD COLUMN IF NOT EXISTS close_blocked TEXT;

-- Audit trail: one row per closed group, manual closes included.
CREATE TABLE IF NOT EXISTS closed_groups (
    id         BIGSERIAL   PRIMARY KEY,
    slug       TEXT        NOT NULL,
    session_id UUID        NOT NULL,
    issues     TEXT[]      NOT NULL DEFAULT '{}',
    branches   TEXT[]      NOT NULL DEFAULT '{}',
    -- 'auto' or 'manual'
    mode       TEXT        NOT NULL,
    closed_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS closed_groups_recent ON closed_groups (closed_at DESC, id DESC);
