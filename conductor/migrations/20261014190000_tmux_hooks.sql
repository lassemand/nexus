-- Hook intake looks groups up by Claude session id on every hook event.
--
-- The tmux-model states (no_session, starting, ready, delivering, busy) live in
-- the existing JSONB `state` column, so no new columns are needed.
CREATE INDEX IF NOT EXISTS session_groups_session_id ON session_groups (session_id);
