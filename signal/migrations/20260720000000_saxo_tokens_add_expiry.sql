-- Extend saxo_tokens with refresh token expiry timestamp.
--
-- Saxo's refresh_token_expires_in is ~3589 seconds (~1 hour).
-- Without a concrete deadline column there is nothing for Prometheus to
-- alert against — the monitoring task (NEX-94) requires this field.
--
-- NULL means no successful rotation has been recorded yet (bootstrap state).
-- The Prometheus alert suppresses until this field is NOT NULL.

ALTER TABLE saxo_tokens
    ADD COLUMN IF NOT EXISTS refresh_token_expires_at TIMESTAMPTZ;
