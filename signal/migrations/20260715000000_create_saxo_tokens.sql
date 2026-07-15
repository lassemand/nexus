-- Persists the Saxo Bank OAuth2 refresh token so the saxo_stream service
-- survives restarts without requiring manual re-authentication.
--
-- # Why this table exists
--
-- Saxo's OAuth2 refresh tokens rotate on each use (RFC 6749 §10.4 rotation).
-- A plain ExternalSecret (Vault → k8s Secret) is pull-once-at-pod-start and
-- cannot be updated by the running service. Without write-back, a pod restart
-- would require manual Vault intervention to inject the latest token.
--
-- # Write-back flow
--
-- 1. Pod starts → reads latest refresh_token from this table (falls back to
--    the bootstrap token in the k8s Secret if the table is empty).
-- 2. Service uses refresh_token to obtain a new access_token from Saxo.
-- 3. Saxo returns a new refresh_token → service writes it back here.
-- 4. On next restart the cycle continues without manual intervention.
--
-- There is at most one row (the current token). The service uses
-- INSERT ... ON CONFLICT DO UPDATE to replace it.

CREATE TABLE IF NOT EXISTS saxo_tokens (
    id          INT         PRIMARY KEY DEFAULT 1
                            CHECK (id = 1),   -- enforce single-row
    refresh_token TEXT      NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
