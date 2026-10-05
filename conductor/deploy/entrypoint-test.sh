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
  grep -q "ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN" <<<"$out" \
    && ok "names the model credential" || bad "names the model credential" "$out"
  rm -rf "$SANDBOX"
)

(
  new_sandbox; init_layout >/dev/null
  export GITHUB_TOKEN=x LINEAR_API_KEY=y ANTHROPIC_API_KEY=a CLAUDE_CODE_OAUTH_TOKEN=b
  out="$(check_env 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && ok "both model credentials is rejected" || bad "both model credentials is rejected" "rc=$rc"
  grep -q "set exactly one" <<<"$out" && ok "explains the conflict" || bad "explains the conflict" "$out"
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
