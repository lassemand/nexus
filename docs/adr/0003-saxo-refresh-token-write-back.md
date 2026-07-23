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
| **Postgres table `oauth_tokens`** | ✅ Simplest; the service already has a DB connection |

## Decision

Write the current refresh token to a Postgres table (`oauth_tokens`, keyed by
a `source` column — `'saxo'` today) after each successful token rotation. On
startup, read the row for `source = 'saxo'` first; fall back to the
`SAXO_REFRESH_TOKEN` env var (bootstrap token from Vault) only if no such row
exists yet.

Originally this was a strict single-row table (`saxo_tokens`, enforced via
`CHECK (id = 1)`). It was generalized to a `source`-keyed table and renamed
to `oauth_tokens` so a second broker or environment can get its own row
without another schema redesign — see
`signal/migrations/20260723000000_oauth_tokens_source_key.sql`.

## Write-back flow

```
Pod starts
  │
  ├─ No row for source='saxo'? ──yes──► use SAXO_REFRESH_TOKEN from k8s Secret (Vault)
  │
  └─ Row exists ────────────────────────► use refresh_token from DB
          │
          ▼
  Call Saxo token endpoint with refresh_token
          │
          ▼
  Receive new (access_token, refresh_token)
          │
          ▼
  INSERT INTO oauth_tokens (source, refresh_token, updated_at)
    VALUES ('saxo', $new_token, NOW())
    ON CONFLICT (source) DO UPDATE SET
      refresh_token = EXCLUDED.refresh_token,
      updated_at    = EXCLUDED.updated_at
```

## Security notes

- The `oauth_tokens` table lives in the `backtest` Postgres database, which is
  only accessible from within the cluster.
- The bootstrap token in Vault (`nexus/saxo.refresh_token`) should be rotated
  once after first deployment — after the service has written its first live
  token back to the DB, the Vault copy is no longer used.
- If the Postgres table is ever corrupted or the token in it is expired, delete
  the `source = 'saxo'` row and update `nexus/saxo.refresh_token` in Vault to
  a fresh token obtained via the Saxo developer portal full OAuth flow.

## Schema

Created by `signal/migrations/20260715000000_create_saxo_tokens.sql`
(originally `saxo_tokens`, single-row enforced via `CHECK (id = 1)`), then
generalized by `signal/migrations/20260723000000_oauth_tokens_source_key.sql`
(renamed to `oauth_tokens`, `id` kept as primary key, `source` added as a
`UNIQUE` key so multiple sources can coexist).

Applied via a dedicated `initContainer` on the `saxo-stream` Deployment
(`infra/charts/chronicle/chronicle/templates/deployment-saxo-stream.yaml`),
not the app binary's own embedded `sqlx::migrate!` call — that call only
covers `chronicle/migrations` (e.g. `companies`), a deliberately separate
migration set from `signal/migrations` (which owns `oauth_tokens`).
