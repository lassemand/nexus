# ADR-0003: Saxo OAuth2 Refresh Token Write-Back via Postgres

**Status:** Accepted  
**Date:** 2026-07-15  
**Linear:** NEX-86  

---

## Problem

Saxo Bank's OAuth2 refresh tokens rotate on each use (RFC 6749 §10.4). The
`saxo_stream` Deployment obtains a new `(access_token, refresh_token)` pair
every time it calls the token endpoint. A plain `ExternalSecret` (Vault → k8s
Secret) is pull-once at pod start and cannot be updated by the running service.

Without write-back, a pod restart after a token rotation would present an
already-consumed refresh token to Saxo, causing `invalid_grant` and requiring
manual Vault intervention to inject the latest token.

## Options considered

| Option | Verdict |
|---|---|
| Pull fresh from Vault on every restart; manually update Vault after each rotation | ❌ Requires manual intervention after every rotation |
| Store rotating token in a k8s Secret, updated via the k8s API from the pod | ❌ Requires RBAC to write Secrets — security anti-pattern |
| Vault write-back from the pod via Vault Agent Injector | ⚠️ Possible but adds Vault Agent sidecar complexity |
| **Postgres table `saxo_tokens`** | ✅ Simplest; the service already has a DB connection |

## Decision

Write the current refresh token to a single-row Postgres table (`saxo_tokens`)
after each successful token rotation. On startup, read from this table first;
fall back to the `SAXO_REFRESH_TOKEN` env var (bootstrap token from Vault) only
if the table is empty.

## Write-back flow

```
Pod starts
  │
  ├─ Table empty? ──yes──► use SAXO_REFRESH_TOKEN from k8s Secret (Vault)
  │
  └─ Table has row ──────► use refresh_token from DB
          │
          ▼
  Call Saxo token endpoint with refresh_token
          │
          ▼
  Receive new (access_token, refresh_token)
          │
          ▼
  INSERT INTO saxo_tokens (id, refresh_token, updated_at)
    VALUES (1, $new_token, NOW())
    ON CONFLICT (id) DO UPDATE SET
      refresh_token = EXCLUDED.refresh_token,
      updated_at    = EXCLUDED.updated_at
```

## Security notes

- The `saxo_tokens` table lives in the `backtest` Postgres database, which is
  only accessible from within the cluster.
- The bootstrap token in Vault (`nexus/saxo.refresh_token`) should be rotated
  once after first deployment — after the service has written its first live
  token back to the DB, the Vault copy is no longer used.
- If the Postgres table is ever corrupted or the token in it is expired, delete
  the row and update `nexus/saxo.refresh_token` in Vault to a fresh token
  obtained via the Saxo developer portal full OAuth flow.

## Schema

See `signal/migrations/20260715000000_create_saxo_tokens.sql`.

Single-row enforced via `CHECK (id = 1)`.
