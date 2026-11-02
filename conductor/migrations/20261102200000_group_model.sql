-- Model the group's tmux session was last started or resumed with, and whether
-- it came from the set's `session:` label (TRUE) or the global default.
-- NULL model means no `--model` flag was passed.
ALTER TABLE session_groups ADD COLUMN IF NOT EXISTS model TEXT;
ALTER TABLE session_groups ADD COLUMN IF NOT EXISTS model_from_set BOOLEAN NOT NULL DEFAULT FALSE;
