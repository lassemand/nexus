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
# The token is cached on disk and reused until five minutes before it expires,
# matching what github-mcp-server does internally (refreshBuffer in
# internal/githubapp). Without the cache every `git fetch`, `git push` and
# `git ls-remote` would mint a fresh token, which with five concurrent agent
# sessions is a lot of avoidable API calls against the same rate limit the
# agents need for real work.
#
# No lock is taken around the cache. Two operations racing both mint, and the
# atomic rename means the loser is simply overwritten — a rare duplicate call,
# against a lock file that could be left stale by a killed process and then
# block every push. Correctness here does not need mutual exclusion: any token
# in the file is valid, whoever wrote it.
#
# Installed by the entrypoint as:
#   git config --global credential.https://github.com.helper <this script>
#
# Git protocol: read key=value lines on stdin until a blank line, then print
# the credentials as key=value lines. Only `get` returns anything. `store` is
# ignored, since the cache below is managed here rather than by git. `erase`
# drops the cache, because git sends it when the credential was rejected.
#
# No `set -x`, ever: the private key and the minted token both pass through here.
set -euo pipefail

APP_ID="${GITHUB_APP_ID:-}"
INSTALLATION_ID="${GITHUB_APP_INSTALLATION_ID:-}"
KEY_PATH="${GITHUB_APP_PRIVATE_KEY_PATH:-$HOME/.config/github-app/private-key.pem}"
API="${GITHUB_API_URL:-https://api.github.com}"

# `erase` is git telling us the credential was rejected. Dropping the cache
# then means the next call mints afresh instead of handing back the same bad
# token forever — the one case where a cache could turn a transient failure
# into a permanent one.
if [[ "${1:-}" == "erase" ]]; then
  rm -f "${GITHUB_APP_TOKEN_CACHE_DIR:-$HOME/.cache/github-app}/installation-token.json" 2>/dev/null || true
  exit 0
fi

# Only `get` produces output. `store` exits quietly: there is nothing for git
# to store, since the cache above is managed here.
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

CACHE_DIR="${GITHUB_APP_TOKEN_CACHE_DIR:-$HOME/.cache/github-app}"
CACHE="${CACHE_DIR}/installation-token.json"
# Five minutes, as github-mcp-server uses: long enough that a token cannot
# expire mid-operation after the check passed.
REFRESH_BUFFER="${GITHUB_APP_TOKEN_REFRESH_BUFFER:-300}"

emit() {
  printf 'username=x-access-token\n'
  printf 'password=%s\n' "$1"
}

# Serve from cache when the token has more than the buffer left to live.
if [[ -r "$CACHE" ]]; then
  cached_token="$(jq -r '.token // empty' "$CACHE" 2>/dev/null || true)"
  cached_expiry="$(jq -r '.expires_at_epoch // empty' "$CACHE" 2>/dev/null || true)"
  if [[ -n "$cached_token" && "$cached_expiry" =~ ^[0-9]+$ ]]; then
    if (( cached_expiry - $(date +%s) > REFRESH_BUFFER )); then
      emit "$cached_token"
      exit 0
    fi
  fi
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

# Cache it. `expires_at` is RFC 3339 from GitHub; stored as an epoch so the
# read path needs no date parsing, and the whole file is written through a
# temp file so a concurrent reader never sees half of it.
expires_at="$(printf '%s' "$response" | jq -r '.expires_at // empty')"
expiry_epoch=""
if [[ -n "$expires_at" ]]; then
  # GNU date first; BSD date needs the format spelled out. Either may fail on
  # an unexpected shape, and an uncacheable token is not worth failing over.
  expiry_epoch="$(date -u -d "$expires_at" +%s 2>/dev/null \
    || date -u -j -f '%Y-%m-%dT%H:%M:%SZ' "$expires_at" +%s 2>/dev/null \
    || true)"
fi
if [[ "$expiry_epoch" =~ ^[0-9]+$ ]]; then
  mkdir -p "$CACHE_DIR"
  chmod 0700 "$CACHE_DIR" 2>/dev/null || true
  cache_tmp="$(mktemp "${CACHE_DIR}/.token.XXXXXX")"
  chmod 0600 "$cache_tmp"
  jq -n --arg t "$token" --argjson e "$expiry_epoch" \
    '{token: $t, expires_at_epoch: $e}' > "$cache_tmp"
  mv "$cache_tmp" "$CACHE"
fi

emit "$token"
