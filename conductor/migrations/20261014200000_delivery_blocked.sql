-- Set when a pasted prompt was never confirmed by a UserPromptSubmit hook.
--
-- Pasting again could submit the same task twice, so the dispatcher stops
-- delivering to the group until an operator clears this.
ALTER TABLE session_groups ADD COLUMN IF NOT EXISTS delivery_blocked TEXT;
