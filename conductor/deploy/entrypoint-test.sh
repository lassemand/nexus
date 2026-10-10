#!/usr/bin/env bash
#
# Tests the parts of the entrypoint that need no credentials.
#
# Sources entrypoint.sh as a library and drives its functions against throwaway
# directories. That covers the pieces most likely to be wrong and least likely
# to be noticed: environment validation, the JSON seeding, its idempotency, and
# recovery from a torn .claude.json.
#
# What it cannot cover, because all of it needs real secrets: `gh auth status`,
# `claude mcp add` / `mcp list` connectivity, and the `claude --print` smoke
# check. Those are run by hand against the built image.
#
# Needs only bash, git and jq, so it runs on a laptop as well as in CI.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Tallied through files, not variables. Each case runs in a subshell for
# isolation, so counters incremented inside one never reach the parent — which
# silently made this suite incapable of failing, however many checks broke.
TALLY="$(mktemp -d)"
trap 'rm -rf "$TALLY"' EXIT

ok()   { printf '%s\n' "$1" >> "${TALLY}/pass"; printf '  ok    %s\n' "$1"; }
bad()  { printf '%s\n' "$1" >> "${TALLY}/fail"; printf '  FAIL  %s\n' "$1"; [[ -n "${2:-}" ]] && printf '          %s\n' "$2"; }
check(){ if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1" "expected [$3], got [$2]"; fi; }

# A clean /data and a clean HOME per case, so nothing leaks between tests.
#
# Sets SANDBOX rather than printing it: called through command substitution the
# exports would land in a subshell and init_layout would fall back to the real
# /data, which is exactly what happened the first time this was written.
new_sandbox() {
  SANDBOX="$(mktemp -d)"
  export CONDUCTOR_DATA_DIR="$SANDBOX"
  # Never reach the network or a real credential by accident.
  unset GITHUB_TOKEN LINEAR_API_KEY ANTHROPIC_API_KEY CLAUDE_CODE_OAUTH_TOKEN
  export CLAUDE_CODE_VERSION="9.9.9-test"
}

# shellcheck source=conductor/deploy/entrypoint.sh
CONDUCTOR_ENTRYPOINT_LIB=1 . "${HERE}/entrypoint.sh"

# entrypoint.sh sets `set -e`, which is right for the real thing and fatal
# here: half these cases assert that a function *fails*, and errexit would end
# the test run at the first one.
set +e

printf '== environment validation ==\n'

(
  new_sandbox; init_layout >/dev/null
  out="$(check_env 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "nothing set is rejected" || bad "nothing set is rejected" "rc=$rc"
  # One message naming everything, not one variable per restart.
  grep -q "GITHUB_TOKEN" <<<"$out"   && ok "names GITHUB_TOKEN"   || bad "names GITHUB_TOKEN" "$out"
  grep -q "LINEAR_API_KEY" <<<"$out" && ok "names LINEAR_API_KEY" || bad "names LINEAR_API_KEY" "$out"
  grep -q "ANTHROPIC_API_KEY" <<<"$out" \
    && ok "names the model credential" || bad "names the model credential" "$out"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export GITHUB_TOKEN=x LINEAR_API_KEY=y ANTHROPIC_API_KEY=a CLAUDE_CODE_OAUTH_TOKEN=b
  out="$(check_env 2>&1)"; rc=$?
  # Ambiguous on purpose: guessing wrong silently bills per-token.
  [[ $rc -ne 0 ]] && ok "both credentials without a mode is rejected" \
    || bad "both credentials without a mode is rejected" "rc=$rc"
  grep -q "CONDUCTOR_AUTH_MODE" <<<"$out" && ok "points at CONDUCTOR_AUTH_MODE" \
    || bad "points at CONDUCTOR_AUTH_MODE" "$out"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  out="$(check_env 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "no model credential at all is rejected" || bad "no model credential at all is rejected"
  grep -q "CLAUDE_CODE_CREDENTIALS_JSON" <<<"$out" && ok "recommends the renewing option first" \
    || bad "recommends the renewing option first" "$out"
  rm -rf "$SANDBOX"
)

printf '== auth mode ==\n'

(
  new_sandbox; init_layout >/dev/null
  export GITHUB_TOKEN=x LINEAR_API_KEY=y ANTHROPIC_API_KEY=a CLAUDE_CODE_OAUTH_TOKEN=b
  export CONDUCTOR_AUTH_MODE=subscription
  check_env >/dev/null 2>&1 && ok "explicit subscription mode is accepted" || bad "explicit subscription mode is accepted"
  # The whole point: the API key must not be left where Claude Code can find it.
  check "ANTHROPIC_API_KEY is removed" "${ANTHROPIC_API_KEY:-unset}" "unset"
  check "mode is reported" "${AUTH_MODE:-}" "subscription"
  unset CONDUCTOR_AUTH_MODE
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export GITHUB_TOKEN=x LINEAR_API_KEY=y ANTHROPIC_API_KEY=a CLAUDE_CODE_OAUTH_TOKEN=b
  export CONDUCTOR_AUTH_MODE=api_key
  check_env >/dev/null 2>&1 && ok "explicit api_key mode is accepted" || bad "explicit api_key mode is accepted"
  check "the subscription token is removed" "${CLAUDE_CODE_OAUTH_TOKEN:-unset}" "unset"
  unset CONDUCTOR_AUTH_MODE
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export GITHUB_TOKEN=x LINEAR_API_KEY=y ANTHROPIC_API_KEY=a
  export CONDUCTOR_AUTH_MODE=nonsense
  out="$(check_env 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "an unknown mode is rejected" || bad "an unknown mode is rejected"
  grep -q "nonsense" <<<"$out" && ok "echoes the bad value" || bad "echoes the bad value" "$out"
  unset CONDUCTOR_AUTH_MODE
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export GITHUB_TOKEN=x LINEAR_API_KEY=y CLAUDE_CODE_CREDENTIALS_JSON='{"claudeAiOauth":{}}'
  check_env >/dev/null 2>&1 && ok "a credentials blob alone infers subscription" || bad "a credentials blob alone infers subscription"
  check "inferred mode" "${AUTH_MODE:-}" "subscription"
  rm -rf "$SANDBOX"
)

printf '== github mode ==\n'

(
  new_sandbox; init_layout >/dev/null
  export GITHUB_TOKEN=x LINEAR_API_KEY=y ANTHROPIC_API_KEY=a
  check_env >/dev/null 2>&1
  check "a token alone infers token mode" "${GITHUB_MODE:-}" "token"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export LINEAR_API_KEY=y ANTHROPIC_API_KEY=a
  export GITHUB_APP_ID=1 GITHUB_APP_INSTALLATION_ID=2 GITHUB_APP_PRIVATE_KEY=pem
  check_env >/dev/null 2>&1 && ok "app credentials alone are accepted" || bad "app credentials alone are accepted"
  check "and infer app mode" "${GITHUB_MODE:-}" "app"
  unset GITHUB_APP_ID GITHUB_APP_INSTALLATION_ID GITHUB_APP_PRIVATE_KEY
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export LINEAR_API_KEY=y ANTHROPIC_API_KEY=a
  export GITHUB_TOKEN=x GITHUB_APP_ID=1 GITHUB_APP_INSTALLATION_ID=2 GITHUB_APP_PRIVATE_KEY=pem
  check_env >/dev/null 2>&1
  # Unlike the model credential, there is no cost difference here — the App is
  # simply better, so preferring it silently is safe rather than presumptuous.
  check "the app wins when both are present" "${GITHUB_MODE:-}" "app"
  unset GITHUB_APP_ID GITHUB_APP_INSTALLATION_ID GITHUB_APP_PRIVATE_KEY
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export LINEAR_API_KEY=y ANTHROPIC_API_KEY=a
  out="$(check_env 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "no github credentials at all is rejected" || bad "no github credentials at all is rejected"
  grep -q "GITHUB_APP_ID" <<<"$out" && ok "names the app option first" || bad "names the app option first" "$out"
  grep -q "GITHUB_TOKEN" <<<"$out" && ok "and the token option" || bad "and the token option" "$out"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export LINEAR_API_KEY=y ANTHROPIC_API_KEY=a GITHUB_TOKEN=x
  export CONDUCTOR_GITHUB_MODE=app
  out="$(check_env 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "app mode without app credentials is rejected" || bad "app mode without app credentials is rejected"
  unset CONDUCTOR_GITHUB_MODE
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export LINEAR_API_KEY=y ANTHROPIC_API_KEY=a GITHUB_TOKEN=x
  export CONDUCTOR_GITHUB_MODE=sideways
  out="$(check_env 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "an unknown github mode is rejected" || bad "an unknown github mode is rejected"
  grep -q "sideways" <<<"$out" && ok "echoing the bad value" || bad "echoing the bad value" "$out"
  unset CONDUCTOR_GITHUB_MODE
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  GITHUB_MODE=app
  export GITHUB_APP_ID=1 GITHUB_APP_INSTALLATION_ID=2
  export GITHUB_APP_PRIVATE_KEY="not a private key at all"
  out="$(configure_gh 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "a malformed private key is rejected" || bad "a malformed private key is rejected" "$out"
  unset GITHUB_APP_ID GITHUB_APP_INSTALLATION_ID GITHUB_APP_PRIVATE_KEY
  rm -rf "$SANDBOX"
)

printf '== subscription credentials ==\n'

# Helper: a credentials file whose refresh token expires in N days.
creds_json() {
  python3 -c "
import json, time
ms = int((time.time() + $1 * 86400) * 1000)
print(json.dumps({'claudeAiOauth': {'accessToken': 'a', 'refreshToken': 'r',
                                    'expiresAt': ms, 'refreshTokenExpiresAt': ms}}))"
}

(
  new_sandbox; init_layout >/dev/null
  CLAUDE_CODE_CREDENTIALS_JSON="$(creds_json 21)"
  export CLAUDE_CODE_CREDENTIALS_JSON
  seed_claude_credentials >/dev/null 2>&1
  f="$HOME/.claude/.credentials.json"
  [[ -f "$f" ]] && ok "seeds the credentials file" || bad "seeds the credentials file"
  # GNU form first: on Linux `stat -f` is --file-system and exits 0 with an
  # unrelated value, so the BSD-first order silently compared garbage and only
  # failed inside the container.
  perms="$(stat -c '%a' "$f" 2>/dev/null || stat -f '%Lp' "$f")"
  check "written 0600, not world-readable" "$perms" "600"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  f="$HOME/.claude/.credentials.json"
  mkdir -p "$(dirname "$f")"
  # Stands in for a credential Claude Code has already refreshed.
  printf '%s' "$(creds_json 20)" | python3 -c "
import json,sys
d=json.load(sys.stdin); d['claudeAiOauth']['accessToken']='REFRESHED'; print(json.dumps(d))" > "$f"
  CLAUDE_CODE_CREDENTIALS_JSON="$(creds_json 21)"
  export CLAUDE_CODE_CREDENTIALS_JSON
  seed_claude_credentials >/dev/null 2>&1
  # Overwriting would roll the credential back to the seed and undo renewal.
  check "an existing file is never overwritten" \
    "$(jq -r '.claudeAiOauth.accessToken' "$f")" "REFRESHED"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  CLAUDE_CODE_CREDENTIALS_JSON="$(creds_json -1)"
  export CLAUDE_CODE_CREDENTIALS_JSON
  out="$(seed_claude_credentials 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "an expired refresh token is fatal" || bad "an expired refresh token is fatal" "$out"
  grep -q "setup-token" <<<"$out" && ok "and says how to fix it" || bad "and says how to fix it" "$out"
  grep -qE "expired [0-9]+h ago" <<<"$out" && ok "and how long ago, in hours not days" \
    || bad "and how long ago, in hours not days" "$out"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  CLAUDE_CODE_CREDENTIALS_JSON="$(creds_json 2)"
  export CLAUDE_CODE_CREDENTIALS_JSON
  out="$(seed_claude_credentials 2>&1)"
  grep -q "WARNING.*expires in" <<<"$out" && ok "warns before the refresh token expires" \
    || bad "warns before the refresh token expires" "$out"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export CLAUDE_CODE_CREDENTIALS_JSON='not json at all'
  out="$(seed_claude_credentials 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "a malformed credentials blob is rejected" || bad "a malformed credentials blob is rejected"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  f="$HOME/.claude/.credentials.json"
  mkdir -p "$(dirname "$f")"
  printf '{"claudeAiOauth": {tr' > "$f"
  CLAUDE_CODE_CREDENTIALS_JSON="$(creds_json 21)"
  export CLAUDE_CODE_CREDENTIALS_JSON
  seed_claude_credentials >/dev/null 2>&1
  jq empty "$f" >/dev/null 2>&1 && ok "a torn credentials file is recovered from the seed" \
    || bad "a torn credentials file is recovered from the seed"
  n=$(find "$HOME/.claude" -maxdepth 1 -name '.credentials.json.corrupt-*' | wc -l | tr -d ' ')
  check "and the broken one is kept" "$n" "1"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export CLAUDE_CODE_OAUTH_TOKEN=static-token
  out="$(seed_claude_credentials 2>&1)"
  grep -q "does not renew itself" <<<"$out" && ok "the static token path warns that it cannot renew" \
    || bad "the static token path warns that it cannot renew" "$out"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export GITHUB_TOKEN=x LINEAR_API_KEY=y ANTHROPIC_API_KEY=a
  check_env >/dev/null 2>&1 && ok "one model credential is accepted" || bad "one model credential is accepted"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export GITHUB_TOKEN=x LINEAR_API_KEY=y CLAUDE_CODE_OAUTH_TOKEN=b
  check_env >/dev/null 2>&1 && ok "the oauth token alone is accepted" || bad "the oauth token alone is accepted"
  rm -rf "$SANDBOX"
)

printf '== layout ==\n'

(
  new_sandbox
  init_layout >/dev/null
  for d in home repo worktrees state home/.claude/agents; do
    [[ -d "${SANDBOX}/${d}" ]] && ok "creates ${d}" || bad "creates ${d}"
  done
  check "HOME points at the volume" "$HOME" "${SANDBOX}/home"
  # Routine on every restart, so it must not care that the dirs exist.
  init_layout >/dev/null && ok "is idempotent" || bad "is idempotent"
  rm -rf "$SANDBOX"
)

printf '== claude configuration ==\n'

(
  new_sandbox; init_layout >/dev/null
  seed_claude_config >/dev/null
  check "onboarding suppressed" "$(jq -r '.hasCompletedOnboarding' "$CLAUDE_JSON")" "true"
  check "onboarding version recorded" "$(jq -r '.lastOnboardingVersion' "$CLAUDE_JSON")" "9.9.9-test"
  check "repo pre-trusted" \
    "$(jq -r --arg p "${SANDBOX}/repo" '.projects[$p].hasTrustDialogAccepted' "$CLAUDE_JSON")" "true"
  check "worktree root pre-trusted" \
    "$(jq -r --arg p "${SANDBOX}/worktrees" '.projects[$p].hasTrustDialogAccepted' "$CLAUDE_JSON")" "true"
  check "dangerous-mode prompt suppressed" \
    "$(jq -r '.skipDangerousModePermissionPrompt' "$SETTINGS_JSON")" "true"
  check "theme set so the picker never blocks" "$(jq -r '.theme' "$SETTINGS_JSON")" "dark"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  # The transcript bookkeeping in this file has to survive a restart, so the
  # seed must merge rather than overwrite.
  jq -n '{projects: {"/keep/me": {lastSessionId: "abc-123"}}, numStartups: 42}' > "$CLAUDE_JSON"
  jq -n '{theme: "light"}' > "$SETTINGS_JSON"
  seed_claude_config >/dev/null
  check "unrelated keys preserved" "$(jq -r '.numStartups' "$CLAUDE_JSON")" "42"
  check "existing project entries preserved" \
    "$(jq -r '.projects["/keep/me"].lastSessionId' "$CLAUDE_JSON")" "abc-123"
  check "an explicit theme is not overridden" "$(jq -r '.theme' "$SETTINGS_JSON")" "light"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  seed_claude_config >/dev/null
  first="$(jq -r '.firstStartTime' "$CLAUDE_JSON")"
  before="$(jq -S -c . "$CLAUDE_JSON")"
  seed_claude_config >/dev/null
  check "firstStartTime is not rewritten" "$(jq -r '.firstStartTime' "$CLAUDE_JSON")" "$first"
  check "second run changes nothing" "$(jq -S -c . "$CLAUDE_JSON")" "$before"
  rm -rf "$SANDBOX"
)

printf '== corruption recovery ==\n'

(
  new_sandbox; init_layout >/dev/null
  # What a torn concurrent write looks like.
  printf '{"hasCompletedOnboarding": tr' > "$CLAUDE_JSON"
  seed_claude_config >/dev/null 2>&1
  jq empty "$CLAUDE_JSON" >/dev/null 2>&1 && ok "unparseable file is replaced with valid JSON" \
    || bad "unparseable file is replaced with valid JSON"
  check "and reseeded" "$(jq -r '.hasCompletedOnboarding' "$CLAUDE_JSON")" "true"
  # Never discarded silently — it may be the only copy of the session map.
  count=$(find "$HOME" -maxdepth 1 -name '.claude.json.corrupt-*' | wc -l | tr -d ' ')
  check "the broken file is kept as a backup" "$count" "1"
  rm -rf "$SANDBOX"
)

printf '== github app credential helper ==\n'

# Exercises the helper end to end against a stand-in for GitHub's API: a
# throwaway RSA key signs the JWT, and the stub verifies that signature with
# the matching public key before returning a token. That covers the part most
# likely to be silently wrong — RS256 signing — without any real credential.
(
  new_sandbox
  HELPER="${HERE}/github-app-credential-helper.sh"
  port=$(( 20000 + RANDOM % 20000 ))
  openssl genrsa -out "$SANDBOX/k.pem" 2048 >/dev/null 2>&1
  openssl rsa -in "$SANDBOX/k.pem" -pubout -out "$SANDBOX/k.pub" >/dev/null 2>&1

  cat > "$SANDBOX/stub.py" <<'STUB'
import base64, http.server, json, os, subprocess, sys, tempfile, time
PUB = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        auth = self.headers.get("Authorization", "")
        ok = False
        if auth.startswith("Bearer "):
            try:
                h, p, sg = auth[7:].split(".")
                pad = lambda x: x + "=" * (-len(x) % 4)
                sig = base64.urlsafe_b64decode(pad(sg))
                sf = tempfile.NamedTemporaryFile(delete=False); sf.write(sig); sf.close()
                df = tempfile.NamedTemporaryFile(delete=False, mode="w"); df.write(f"{h}.{p}"); df.close()
                ok = subprocess.run(["openssl","dgst","-sha256","-verify",PUB,
                                     "-signature",sf.name,df.name],
                                    capture_output=True).returncode == 0
                os.unlink(sf.name); os.unlink(df.name)
            except Exception:
                ok = False
        if ok:
            open(os.environ["MINT_LOG"], "a").write("mint\n")
        exp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + 3600))
        body = json.dumps({"token": "ghs_STUB_TOKEN", "expires_at": exp}).encode() if ok else b'{"message":"bad jwt"}'
        self.send_response(201 if ok else 401)
        self.send_header("Content-Length", str(len(body))); self.end_headers()
        self.wfile.write(body)
http.server.HTTPServer(("127.0.0.1", int(sys.argv[2])), H).serve_forever()
STUB

  : > "$SANDBOX/mints"
  MINT_LOG="$SANDBOX/mints" python3 "$SANDBOX/stub.py" "$SANDBOX/k.pub" "$port" >/dev/null 2>&1 &
  stub_pid=$!
  # Wait for the port rather than sleeping blind.
  for _ in $(seq 1 50); do
    (exec 3<>/dev/tcp/127.0.0.1/$port) 2>/dev/null && break
    sleep 0.1
  done

  out="$(printf 'protocol=https\nhost=github.com\n\n' | \
    GITHUB_APP_ID=12345 \
    GITHUB_APP_INSTALLATION_ID=67890 \
    GITHUB_APP_PRIVATE_KEY_PATH="$SANDBOX/k.pem" \
    GITHUB_API_URL="http://127.0.0.1:${port}" \
    bash "$HELPER" get 2>&1)"

  grep -q "^username=x-access-token$" <<<"$out" \
    && ok "returns the installation-token username" || bad "returns the installation-token username" "$out"
  grep -q "^password=ghs_STUB_TOKEN$" <<<"$out" \
    && ok "mints a token with a correctly signed RS256 JWT" \
    || bad "mints a token with a correctly signed RS256 JWT" "$out"

  # Must never answer for a host other than github.com, or a changed remote
  # would be handed a GitHub credential.
  other="$(printf 'protocol=https\nhost=gitlab.example.com\n\n' | \
    GITHUB_APP_ID=12345 GITHUB_APP_INSTALLATION_ID=67890 \
    GITHUB_APP_PRIVATE_KEY_PATH="$SANDBOX/k.pem" \
    GITHUB_API_URL="http://127.0.0.1:${port}" bash "$HELPER" get 2>&1)"
  [[ -z "$other" ]] && ok "declines any host but github.com" || bad "declines any host but github.com" "$other"

  for verb in store erase; do
    o="$(printf 'protocol=https\nhost=github.com\n\n' | \
      GITHUB_APP_TOKEN_CACHE_DIR="$SANDBOX/cache" bash "$HELPER" "$verb" 2>&1)"
    [[ -z "$o" ]] && ok "$verb prints nothing" || bad "$verb prints nothing" "$o"
  done

  # ── caching ──
  #
  # The stub counts requests, so this proves the second call does not reach
  # the API rather than merely returning the same string.
  helper_get() {
    printf 'protocol=https\nhost=github.com\n\n' | \
      GITHUB_APP_ID=12345 \
      GITHUB_APP_INSTALLATION_ID=67890 \
      GITHUB_APP_PRIVATE_KEY_PATH="$SANDBOX/k.pem" \
      GITHUB_API_URL="http://127.0.0.1:${port}" \
      GITHUB_APP_TOKEN_CACHE_DIR="$SANDBOX/cache" \
      bash "$HELPER" get 2>&1
  }

  rm -rf "$SANDBOX/cache"
  first="$(helper_get)"
  mints_after_first="$(cat "$SANDBOX/mints" 2>/dev/null | wc -l | tr -d ' ')"
  second="$(helper_get)"
  mints_after_second="$(cat "$SANDBOX/mints" 2>/dev/null | wc -l | tr -d ' ')"

  # Asserted first: without this, the equality below also holds when nothing
  # ever minted — the comparison would pass for the wrong reason.
  [[ "$mints_after_first" -ge 1 ]] && ok "the first call reaches the API" \
    || bad "the first call reaches the API" "mints=$mints_after_first"
  check "the second call is served from cache" "$mints_after_second" "$mints_after_first"
  check "and returns the same token" "$second" "$first"
  [[ -f "$SANDBOX/cache/installation-token.json" ]] && ok "cache file written" || bad "cache file written"
  perms="$(stat -c '%a' "$SANDBOX/cache/installation-token.json" 2>/dev/null \
    || stat -f '%Lp' "$SANDBOX/cache/installation-token.json")"
  check "cache is not world-readable" "$perms" "600"

  # Inside the refresh buffer the token must be replaced, not handed out as it
  # nears expiry — otherwise a long push could outlive it mid-operation.
  jq -n --arg t "stale" --argjson e "$(( $(date +%s) + 60 ))" \
    '{token: $t, expires_at_epoch: $e}' > "$SANDBOX/cache/installation-token.json"
  before="$(cat "$SANDBOX/mints" | wc -l | tr -d ' ')"
  near="$(helper_get)"
  after="$(cat "$SANDBOX/mints" | wc -l | tr -d ' ')"
  [[ "$after" -gt "$before" ]] && ok "a token inside the refresh buffer is re-minted" \
    || bad "a token inside the refresh buffer is re-minted"
  grep -q "stale" <<<"$near" && bad "the stale token was served" || ok "and the stale one is not served"

  # Git sends `erase` when the credential was rejected; keeping it cached would
  # turn a transient failure into a permanent one.
  printf 'protocol=https\nhost=github.com\n\n' | \
    GITHUB_APP_TOKEN_CACHE_DIR="$SANDBOX/cache" bash "$HELPER" erase >/dev/null 2>&1
  [[ ! -f "$SANDBOX/cache/installation-token.json" ]] && ok "erase drops the cache" \
    || bad "erase drops the cache"

  # A corrupt cache must fall through to minting, not fail the push.
  mkdir -p "$SANDBOX/cache"
  printf '{"token": tr' > "$SANDBOX/cache/installation-token.json"
  recovered="$(helper_get)"
  grep -q "^password=ghs_STUB_TOKEN$" <<<"$recovered" \
    && ok "a corrupt cache falls through to minting" || bad "a corrupt cache falls through to minting" "$recovered"

  # Reaped as well as killed, so bash's job-control notice does not land in
  # the middle of the test output.
  { kill "$stub_pid" && wait "$stub_pid"; } 2>/dev/null || true
  rm -rf "$SANDBOX"
)

printf '== git identity ==\n'

(
  new_sandbox; init_layout >/dev/null
  configure_git >/dev/null
  check "default name"  "$(git config --global user.name)"  "nexus-conductor"
  check "default email" "$(git config --global user.email)" "conductor@nexus.local"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export CONDUCTOR_GIT_NAME="Someone" CONDUCTOR_GIT_EMAIL="someone@example.com"
  configure_git >/dev/null
  check "name is overridable"  "$(git config --global user.name)"  "Someone"
  check "email is overridable" "$(git config --global user.email)" "someone@example.com"
  unset CONDUCTOR_GIT_NAME CONDUCTOR_GIT_EMAIL
  rm -rf "$SANDBOX"
)

printf '== agent definitions ==\n'

(
  new_sandbox; init_layout >/dev/null
  mkdir -p "${SANDBOX}/agents-src"
  printf 'backend agent\n' > "${SANDBOX}/agents-src/backend.md"
  export CONDUCTOR_AGENT_DIR="${SANDBOX}/agents-src"
  install_agents >/dev/null
  [[ -f "$HOME/.claude/agents/backend.md" ]] && ok "copies definitions in" || bad "copies definitions in"
  # The ConfigMap is the source of truth, so a local edit must be overwritten.
  printf 'stale\n' > "$HOME/.claude/agents/backend.md"
  install_agents >/dev/null
  check "overwrites on every start" "$(cat "$HOME/.claude/agents/backend.md")" "backend agent"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export CONDUCTOR_AGENT_DIR="${SANDBOX}/nonexistent"
  # A missing ConfigMap mount must not take the whole pod down.
  out="$(install_agents 2>&1)"
  [[ $? -eq 0 ]] && ok "a missing mount is not fatal" || bad "a missing mount is not fatal"
  grep -q "WARNING" <<<"$out" && ok "but it is reported" || bad "but it is reported" "$out"
  unset CONDUCTOR_AGENT_DIR
  rm -rf "$SANDBOX"
)

passed=$( [[ -f "${TALLY}/pass" ]] && wc -l < "${TALLY}/pass" || echo 0 )
failed=$( [[ -f "${TALLY}/fail" ]] && wc -l < "${TALLY}/fail" || echo 0 )
printf '\n%d passed, %d failed\n' "$passed" "$failed"
if [[ "$failed" -ne 0 ]]; then
  printf 'failing checks:\n'
  sed 's/^/  - /' "${TALLY}/fail"
  exit 1
fi
# A suite that asserts nothing must not read as success.
if [[ "$passed" -eq 0 ]]; then
  printf 'no checks ran — treating as failure\n' >&2
  exit 1
fi
