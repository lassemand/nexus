# Saxo Bank OpenAPI — OAuth2 Setup Guide

This guide walks through obtaining the credentials needed to populate
`secret/nexus/saxo` in Vault for the `saxo_stream` service.

---

## Prerequisites

- A Saxo Bank live brokerage account
- Market data terms accepted in SaxoTrader GO:
  **My Profile → Other → Open API Access → Enable**
- Exchange data subscription for Nasdaq OMX Stockholm/First North:
  **SaxoTrader GO → Subscription Tool → Nasdaq OMX Stockholm → Subscribe**
  (€7/month Level 1, refunded with ≥4 trades/month)

---

## Step 1 — Create an application in the developer portal

1. Go to **https://www.developer.saxo/openapi/appmanagement**
2. Log in with your Saxo account
3. Click **Create application**
4. Fill in:
   - **Application name**: `nexus-chronicle`
   - **Grant type**: `Code` (Authorization Code)
   - **Redirect URL**: `http://localhost:9999/callback` (only used during setup)
   - **Environment**: **Live** (not SIM — live data requires a live app)
5. Click **Create**
6. Note down:
   - **Client ID** → `SAXO_CLIENT_ID` / Vault `client_id`
   - **Client Secret** → `SAXO_CLIENT_SECRET` / Vault `client_secret`

---

## Step 2 — Perform the one-time OAuth2 Authorization Code flow

This exchanges your login for a refresh token. You only do this once; the
service handles all subsequent token refreshes automatically.

### 2a. Open the authorization URL in a browser

Replace `<CLIENT_ID>` with the value from Step 1:

```
https://live.logonvalidation.net/authorize
  ?client_id=<CLIENT_ID>
  &response_type=code
  &redirect_uri=http://localhost:9999/callback
```

Log in with your Saxo account when prompted. After login you'll be redirected
to `http://localhost:9999/callback?code=<AUTH_CODE>`. Copy the `code` value
from the URL.

> **Tip:** You can use `nc -l 9999` in a terminal to catch the redirect — the
> browser request will appear in the terminal output.

### 2b. Exchange the code for tokens

```bash
curl -X POST https://live.logonvalidation.net/token \
  -d "grant_type=authorization_code" \
  -d "code=<AUTH_CODE>" \
  -d "redirect_uri=http://localhost:9999/callback" \
  -d "client_id=<CLIENT_ID>" \
  -d "client_secret=<CLIENT_SECRET>"
```

Response:

```json
{
  "access_token": "eyJ...",
  "token_type": "Bearer",
  "expires_in": 1200,
  "refresh_token": "eyJ..."
}
```

- **`access_token`** → `SAXO_ACCESS_TOKEN` / Vault `access_token`
- **`refresh_token`** → `SAXO_REFRESH_TOKEN` / Vault `refresh_token`
- **`expires_in`** is in seconds. Calculate the Unix expiry timestamp:

```bash
echo $(($(date +%s) + 1200))
```

→ `SAXO_TOKEN_EXPIRES_AT` / Vault `token_expires_at`

---

## Step 3 — Populate Vault

```bash
export VAULT_ADDR=http://192.168.139.15:8200
export VAULT_TOKEN=<your-vault-token>

vault kv put secret/nexus/saxo \
  client_id="<CLIENT_ID>" \
  client_secret="<CLIENT_SECRET>" \
  refresh_token="<REFRESH_TOKEN>" \
  access_token="<ACCESS_TOKEN>" \
  token_expires_at="<UNIX_TIMESTAMP>" \
  api_base="https://gateway.saxobank.com/openapi" \
  streaming_base="https://live-streaming.saxobank.com/oapi/streaming/ws"
```

Verify:

```bash
vault kv get secret/nexus/saxo
```

---

## Step 4 — First deployment

After the Vault secret is populated and the `saxo-secret` ExternalSecret syncs:

```bash
# Force ESO to pick up the new secret immediately
kubectl annotate externalsecret saxo-secret -n chronicle \
  force-sync=$(date +%s) --overwrite

# Check it synced
kubectl get externalsecret saxo-secret -n chronicle
# → should show SecretSynced
```

The `saxo_stream` pod will then start, use the bootstrap tokens to connect,
and write the rotated refresh token to the `saxo_tokens` Postgres table.
After that first write the Vault `refresh_token` value is no longer needed
for normal operation.

---

## Token refresh cycle (automatic after first start)

```
Pod starts
  │
  ├─ Reads refresh_token from saxo_tokens DB table
  │   └─ Falls back to SAXO_REFRESH_TOKEN env var if table is empty
  │
  ▼
Calls Saxo token endpoint → receives new (access_token, refresh_token)
  │
  ▼
Writes new refresh_token to saxo_tokens table
  │
  └─ Repeats before SAXO_TOKEN_EXPIRES_AT is reached
```

---

## Recovery: if the refresh token is invalid

If the service logs `invalid_grant` and cannot refresh:

1. Go through Step 2 again to get a fresh `(access_token, refresh_token)` pair
2. Update Vault:
   ```bash
   vault kv patch secret/nexus/saxo \
     refresh_token="<NEW_REFRESH_TOKEN>" \
     access_token="<NEW_ACCESS_TOKEN>" \
     token_expires_at="<NEW_UNIX_TIMESTAMP>"
   ```
3. Delete the stale DB row so the service falls back to the Vault value:
   ```bash
   kubectl exec -n nexus nexus-postgres-0 -- \
     psql -U nexus -d backtest -c "DELETE FROM saxo_tokens;"
   ```
4. Restart the pod:
   ```bash
   kubectl rollout restart deployment/saxo-stream -n chronicle
   ```
