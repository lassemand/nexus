# `nexus saxo auth` — Saxo OAuth2 token registration

Performs the Saxo Bank OAuth2 authorization-code flow and registers the
resulting tokens directly with a running `saxo_stream` instance. Once the
command completes, `saxo_stream` starts streaming automatically — no manual
copy-paste into Vault or `.env` files needed.

## Prerequisites

1. A Saxo developer-portal app with:
   - **Client ID** and **Client Secret** (keep the secret out of your shell
     history — use `SAXO_CLIENT_SECRET` env var, not `--client-secret`).
   - A registered **Redirect URI** that matches `--redirect-uri`
     (e.g. `http://localhost:7878/callback`).

2. A running `saxo_stream` pod. Forward its health port to your machine:

   ```bash
   kubectl port-forward deploy/saxo-stream 8080:8080
   ```

## Usage

```bash
export SAXO_CLIENT_ID=<your-client-id>
export SAXO_CLIENT_SECRET=<your-client-secret>   # never pass as a flag

nexus saxo auth \
  --redirect-uri   http://localhost:7878/callback \
  --register-endpoint http://localhost:8080/tokens
```

`--auth-base` defaults to `https://sim.logonvalidation.net` (SIM environment).
For production pass `--auth-base https://live.logonvalidation.net`.

## What happens

1. The command builds the `/authorize` URL and opens it in your browser
   (or prints it for copy-paste if auto-open fails).
2. You log in and approve access in the browser.
3. Saxo redirects your browser to `http://localhost:7878/callback` with a
   one-time authorization code.
4. The command exchanges the code for an access token + refresh token and
   **POSTs both to `--register-endpoint`** (`saxo_stream`'s `/tokens`
   endpoint). `saxo_stream` receives the POST, persists the refresh token,
   and immediately starts the WebSocket stream.
5. The command prints the four token values to stdout as a permanent record:

   ```
   SAXO_REFRESH_TOKEN=<value>
   SAXO_ACCESS_TOKEN=<value>
   SAXO_REFRESH_TOKEN_EXPIRES_AT=<RFC3339>
   SAXO_ACCESS_TOKEN_EXPIRES_AT=<RFC3339>
   ```

   **In the normal case you do not need to act on this output** — `saxo_stream`
   has already unblocked. The output exists so you always have a copy of the
   tokens even if the registration POST failed (in which case a warning is
   printed to stderr).

## All flags

| Flag | Env var | Default | Notes |
|------|---------|---------|-------|
| `--client-id` | `SAXO_CLIENT_ID` | — | Required |
| `--client-secret` | `SAXO_CLIENT_SECRET` | — | Required; use env var |
| `--auth-base` | `SAXO_AUTH_BASE` | `https://sim.logonvalidation.net` | SIM or Live |
| `--redirect-uri` | `SAXO_REDIRECT_URI` | — | Must match app registration |
| `--callback-port` | `SAXO_CALLBACK_PORT` | `7878` | Must match port in `--redirect-uri` |
| `--register-endpoint` | `SAXO_REGISTER_ENDPOINT` | — | `saxo_stream` `/tokens` URL |

## Subsequent restarts

On every restart after the first, `saxo_stream` reads the refresh token from
the `oauth_tokens` database table and calls `SaxoAuth::refresh()` automatically.
Running `nexus saxo auth` again is only needed if the refresh token expires or
is revoked.

## Full help

```
nexus saxo auth --help
```
