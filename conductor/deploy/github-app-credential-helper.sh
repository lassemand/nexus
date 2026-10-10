#!/usr/bin/env bash
#
# Git credential helper that mints a GitHub App installation token on demand.
#
# Why a helper rather than a token in the environment: an App installation
# token lives for one hour. A token minted at container start would work for
# the first agent run and then fail every push for the rest of the pod's life —
# the silent-expiry failure this whole deployment keeps trying to design out.
# Git invokes a credential helper for each authenticated operation, so the
# helper can always hand over something currently valid.
#
# This helper does no caching of its own. The entrypoint chains git's built-in
# `credential-cache` in front of it, which holds the token in a daemon's
# memory rather than on disk — so a live credential never lands on the
# PersistentVolume, which an earlier disk-cache version of this script did.
# Minting per call is cheap enough that the only real gains are latency and
# fewer network calls to fail on, and the daemon provides both.
#
# Installed by the entrypoint as:
#   git config --global credential.https://github.com.helper <this script>
#
# Git protocol: read key=value lines on stdin until a blank line, then print
# the credentials as key=value lines. Only `get` returns anything; `store` and
# `erase` exit quietly, because this helper holds no state — the chained
# `credential-cache` owns both, and git sends `erase` to it on rejection.
#
# No `set -x`, ever: the private key and the minted token both pass through here.
set -euo pipefail

APP_ID="${GITHUB_APP_ID:-}"
INSTALLATION_ID="${GITHUB_APP_INSTALLATION_ID:-}"
KEY_PATH="${GITHUB_APP_PRIVATE_KEY_PATH:-$HOME/.config/github-app/private-key.pem}"
API="${GITHUB_API_URL:-https://api.github.com}"

# Only `get` produces output. `store` and `erase` are stateless no-ops here:
# the chained credential-cache holds the token, and git sends `erase` to it
# when a credential is rejected.
[[ "${1:-}" == "get" ]] || exit 0

# Drain stdin. Git will wait on us otherwise, and the host it is asking about
# is worth checking: this helper must only ever answer for github.com.
host=""
while IFS= read -r line; do
  [[ -z "$line" ]] && break
  case "$line" in
    host=*) host="${line#host=}" ;;
  esac
done

# Declining by printing nothing is the correct protocol response, and keeps the
# token from being handed to some other host if a remote is ever changed.
[[ "$host" == "github.com" ]] || exit 0

for required in APP_ID INSTALLATION_ID; do
  if [[ -z "${!required}" ]]; then
    echo "github-app-credential-helper: ${required} is not set" >&2
    exit 1
  fi
done
if [[ ! -r "$KEY_PATH" ]]; then
  echo "github-app-credential-helper: cannot read ${KEY_PATH}" >&2
  exit 1
fi

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

now="$(date +%s)"
# iat backdated 60s to tolerate clock skew between the pod and GitHub; exp well
# inside the 10 minute maximum GitHub accepts for an App JWT.
header="$(printf '{"alg":"RS256","typ":"JWT"}' | b64url)"
payload="$(printf '{"iat":%d,"exp":%d,"iss":"%s"}' "$((now - 60))" "$((now + 540))" "$APP_ID" | b64url)"
signature="$(printf '%s.%s' "$header" "$payload" | openssl dgst -sha256 -sign "$KEY_PATH" | b64url)"
jwt="${header}.${payload}.${signature}"

response="$(curl -fsS -X POST \
  -H "Authorization: Bearer ${jwt}" \
  -H "Accept: application/vnd.github+json" \
  "${API}/app/installations/${INSTALLATION_ID}/access_tokens" 2>&1)" || {
    # Deliberately not echoing $response: it is an API error body, but this
    # path is one typo away from carrying a credential into the log.
    echo "github-app-credential-helper: minting an installation token failed" >&2
    exit 1
  }

token="$(printf '%s' "$response" | jq -r '.token // empty')"
if [[ -z "$token" ]]; then
  echo "github-app-credential-helper: response contained no token" >&2
  exit 1
fi

# x-access-token is the username GitHub expects for an installation token.
printf 'username=x-access-token\n'
printf 'password=%s\n' "$token"
