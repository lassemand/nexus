-- Base ref a group's worktree should be created on.
--
-- Normally a group starts a fresh `agent/<slug>` branch from main. A group
-- created to answer comments on someone else's pull request has to start on
-- that PR's head branch instead, or the session would be editing the wrong code.
ALTER TABLE session_groups ADD COLUMN IF NOT EXISTS worktree_ref TEXT;
