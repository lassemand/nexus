#!/usr/bin/env bash
#
# Builds the environment the agents need, then hands over to `conductor serve`.
#
# A fresh container has none of what a developer's Mac provides: no logged-in
# Claude Code, no agent definitions, no MCP servers, no git identity, no gh
# auth, no checkout. This runs on every container start and creates all of it
# on the volume mounted at /data.
#
# Idempotent by construction. Pod restarts are routine and the volume keeps its
# state, so every step either checks before acting or is safe to repeat. It
# never resets or cleans the repository or any worktree: agents may have
# uncommitted work there, and losing it silently is worse than failing loudly.
#
# Runs as uid 1000. Writes only under /data and /tmp. No sudo.
#
# `set -x` is deliberately absent and must stay absent: every secret this needs
# arrives in the environment, and tracing would copy all of them into the pod
# log.
#
# Written as functions with a `main` at the bottom so the parts worth testing —
# the JSON seeding, its idempotency and its corruption recovery — can be
# exercised directly by sourcing this file with CONDUCTOR_ENTRYPOINT_LIB=1.
set -euo pipefail

log() { printf '[entrypoint] %s\n' "$*"; }
die() { printf '[entrypoint] FATAL: %s\n' "$*" >&2; exit 1; }

# ── layout ───────────────────────────────────────────────────────────────────

init_layout() {
  DATA_DIR="${CONDUCTOR_DATA_DIR:-/data}"
  # HOME moves onto the volume so Claude's transcripts under
  # ~/.claude/projects/ outlive the container. `claude --resume` depends on it:
  # the registry persists a session id, and without the transcript that id
  # names a conversation the new container has never seen.
  export HOME="${DATA_DIR}/home"
  REPO_DIR="${DATA_DIR}/repo"
  WORKTREE_DIR="${DATA_DIR}/worktrees"
  STATE_DIR="${DATA_DIR}/state"
  CLAUDE_JSON="$HOME/.claude.json"
  SETTINGS_JSON="$HOME/.claude/settings.json"

  mkdir -p "$HOME" "$REPO_DIR" "$WORKTREE_DIR" "$STATE_DIR" "$HOME/.claude/agents"
  log "layout ready under ${DATA_DIR} (HOME=${HOME})"
}

# ── required environment ─────────────────────────────────────────────────────

check_env() {
  # Reported together rather than one per restart: a crash-looping pod is read
  # through its last log, and surfacing one missing variable at a time costs a
  # deploy cycle each.
  #
  # Accumulated as newline-delimited text rather than arrays, because an empty
  # array expanded under `set -u` is an error in bash 3.2 and that would make
  # this function impossible to exercise outside the container.
  local missing="" problems="" creds=0

  [[ -n "${GITHUB_TOKEN:-}" ]]   || missing="${missing} GITHUB_TOKEN"
  [[ -n "${LINEAR_API_KEY:-}" ]] || missing="${missing} LINEAR_API_KEY"

  # Exactly one, never both: which is used is a billing choice made when the
  # secret is written, and two would leave it ambiguous which applies.
  [[ -n "${ANTHROPIC_API_KEY:-}" ]]       && creds=$((creds + 1))
  [[ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]] && creds=$((creds + 1))

  [[ -n "$missing" ]] && problems="${problems}
missing:${missing}"
  case "$creds" in
    0) problems="${problems}
missing: exactly one of ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN" ;;
    1) ;;
    *) problems="${problems}
both ANTHROPIC_API_KEY and CLAUDE_CODE_OAUTH_TOKEN are set; set exactly one" ;;
  esac

  if [[ -n "$problems" ]]; then
    printf '[entrypoint] FATAL: environment is incomplete\n' >&2
    while IFS= read -r line; do
      [[ -n "$line" ]] && printf '[entrypoint]   - %s\n' "$line" >&2
    done <<<"$problems"
    printf '[entrypoint] these come from the conductor-secret ExternalSecret (Vault: nexus/conductor)\n' >&2
    return 1
  fi
  log "environment complete"
}

# ── git identity ─────────────────────────────────────────────────────────────

configure_git() {
  local name="${CONDUCTOR_GIT_NAME:-nexus-conductor}"
  local email="${CONDUCTOR_GIT_EMAIL:-conductor@nexus.local}"
  git config --global user.name  "$name"
  git config --global user.email "$email"
  # Worktrees live outside the repository directory; without this git refuses
  # to operate on them when the owning uid looks unexpected after a remount.
  git config --global --replace-all safe.directory "$REPO_DIR"
  git config --global --add safe.directory "${WORKTREE_DIR}/*"
  log "git identity: ${name} <${email}>"
}

# ── gh auth ──────────────────────────────────────────────────────────────────

configure_gh() {
  # gh reads GITHUB_TOKEN from the environment. `setup-git` installs gh as
  # git's credential helper, so pushes authenticate without the token ever
  # appearing in a remote URL or in .git/config — which agents commit, and
  # which would leak it into the repository.
  gh auth setup-git
  if ! gh auth status >/dev/null 2>&1; then
    # Re-run visibly so the reason reaches the pod log, via gh itself, which
    # redacts the token.
    gh auth status >&2 || true
    die "gh auth status failed; check GITHUB_TOKEN's scopes (contents, pull requests, issues on ${CONDUCTOR_REPO:-lassemand/nexus})"
  fi
  log "gh authenticated"
}

# ── repository ───────────────────────────────────────────────────────────────

sync_repo() {
  local repo="${CONDUCTOR_REPO:-lassemand/nexus}"
  if [[ ! -d "${REPO_DIR}/.git" ]]; then
    log "cloning ${repo} into ${REPO_DIR}"
    # No token in the URL: gh's credential helper supplies it.
    git clone "https://github.com/${repo}.git" "$REPO_DIR"
  else
    log "updating existing checkout"
    git -C "$REPO_DIR" fetch origin --prune
    # Clears registrations for worktrees whose directories are gone, which a
    # `sessions close` or a wiped volume leaves behind. Never touches a
    # worktree that still exists.
    git -C "$REPO_DIR" worktree prune
  fi
  # Deliberately no `reset` and no `clean`, here or anywhere: an agent may
  # have uncommitted work in the repository or in any worktree.
  log "repository ready at ${REPO_DIR}"
}

# ── agent definitions ────────────────────────────────────────────────────────

install_agents() {
  # Copied on every start so the ConfigMap stays the source of truth and an
  # edit there takes effect on the next restart.
  local src="${CONDUCTOR_AGENT_DIR:-/etc/conductor/agents}"
  if compgen -G "${src}/*.md" >/dev/null; then
    cp -f "${src}"/*.md "$HOME/.claude/agents/"
    log "agent definitions: $(cd "$HOME/.claude/agents" && printf '%s ' *.md)"
  else
    # Not fatal: failing here would make the whole pod unavailable over a
    # missing ConfigMap mount, and the dispatcher's --agent flag simply will
    # not resolve until it is fixed.
    log "WARNING: no agent definitions at ${src} — the dispatcher's --agent flag will not resolve"
  fi
}

# ── Claude configuration ─────────────────────────────────────────────────────
#
# The key names below are not guesses. They were read from a working Claude
# Code installation: `hasCompletedOnboarding` and `lastOnboardingVersion`
# suppress first-run onboarding, `projects.<abs path>.hasTrustDialogAccepted`
# suppresses the directory trust dialog, and `theme` plus
# `skipDangerousModePermissionPrompt` live in settings.json rather than
# .claude.json.

# Replaces a file that is not parseable JSON, keeping a copy. Several Claude
# processes write .claude.json concurrently — one per session — so a torn write
# is a question of when, not if, and crash-looping on it would take the pod
# down for something recoverable.
guard_json() {
  local path="$1"
  if [[ -f "$path" ]] && ! jq empty "$path" >/dev/null 2>&1; then
    local backup
    backup="${path}.corrupt-$(date +%s)"
    mv "$path" "$backup"
    log "WARNING: ${path} was not valid JSON; moved to ${backup} and reseeding"
  fi
  [[ -f "$path" ]] || echo '{}' > "$path"
}

seed_claude_config() {
  guard_json "$CLAUDE_JSON"

  # Merged into whatever is there rather than overwritten: this file also holds
  # the transcript bookkeeping that has to survive a restart.
  local version tmp
  version="${CLAUDE_CODE_VERSION:-$(claude --version 2>/dev/null | awk '{print $1}' || true)}"
  tmp="$(mktemp)"
  jq \
    --arg version "${version:-}" \
    --arg repo "$REPO_DIR" \
    --arg worktrees "$WORKTREE_DIR" \
    '
    .hasCompletedOnboarding = true
    | .lastOnboardingVersion = (if $version == "" then (.lastOnboardingVersion // "0.0.0") else $version end)
    | .firstStartTime = (.firstStartTime // (now | todate))
    | .projects = ((.projects // {})
        | .[$repo]      = ((.[$repo]      // {}) | .hasTrustDialogAccepted = true)
        | .[$worktrees] = ((.[$worktrees] // {}) | .hasTrustDialogAccepted = true))
    ' "$CLAUDE_JSON" > "$tmp"
  mv "$tmp" "$CLAUDE_JSON"

  mkdir -p "$(dirname "$SETTINGS_JSON")"
  guard_json "$SETTINGS_JSON"
  tmp="$(mktemp)"
  jq '
    .theme = (.theme // "dark")
    | .skipDangerousModePermissionPrompt = true
    ' "$SETTINGS_JSON" > "$tmp"
  mv "$tmp" "$SETTINGS_JSON"
  log "claude configuration seeded"
}

# ── MCP servers ──────────────────────────────────────────────────────────────

add_mcp() {
  local name="$1" url="$2" token="$3"
  # Removed before adding so a changed URL or a rotated token actually takes
  # effect; adding over an existing name is a no-op or an error depending on
  # version.
  claude mcp remove --scope user "$name" >/dev/null 2>&1 || true
  # The token reaches argv here, visible to `ps` inside this container, and
  # `claude mcp add` then stores it in ~/.claude.json on the volume. Both are
  # called out in the PR; there is no header-from-environment form.
  claude mcp add --scope user --transport http "$name" "$url" \
    --header "Authorization: Bearer ${token}" >/dev/null
}

configure_mcp() {
  # User scope, so every Claude process the dispatcher spawns inherits them.
  # The backend agent needs linear to move issues and github to work with PRs.
  add_mcp linear "https://mcp.linear.app/mcp"         "$LINEAR_API_KEY"
  add_mcp github "https://api.githubcopilot.com/mcp/" "$GITHUB_TOKEN"

  # Connectivity, not merely configuration. A wrong token, or an endpoint that
  # rejects this kind of credential, shows up here instead of as every agent
  # task quietly failing to reach Linear.
  local status name
  status="$(claude mcp list 2>&1 || true)"
  for name in linear github; do
    if ! grep -qiE "^${name}\b.*(connected|✓)" <<<"$status"; then
      printf '[entrypoint] claude mcp list:\n%s\n' "$status" >&2
      die "MCP server '${name}' is not connected"
    fi
  done
  log "MCP servers connected: linear, github"
}

# ── smoke check ──────────────────────────────────────────────────────────────

smoke_check() {
  # Proves the whole chain — credentials, configuration, model access — before
  # the pod reports ready. Without it a bad credential surfaces as every
  # dispatched task failing quietly, which reads as a dispatcher bug.
  if [[ "${CONDUCTOR_SKIP_SMOKE:-0}" == "1" ]]; then
    log "smoke check skipped (CONDUCTOR_SKIP_SMOKE=1)"
    return 0
  fi
  log "running smoke check"
  local reply
  reply="$(cd "$REPO_DIR" && timeout 60 claude --print --dangerously-skip-permissions \
      'Reply with exactly: ok' 2>&1 | tr -d '[:space:]')" || true
  [[ "$reply" == "ok" ]] || die "smoke check failed; claude replied: ${reply:-<nothing>}"
  log "smoke check passed"
}

# ── main ─────────────────────────────────────────────────────────────────────

main() {
  init_layout
  check_env || exit 1
  configure_git
  configure_gh
  sync_repo
  install_agents
  seed_claude_config
  configure_mcp
  smoke_check
  log "starting conductor serve"
  exec conductor serve
}

# Sourced for testing rather than run: define the functions and stop.
if [[ "${CONDUCTOR_ENTRYPOINT_LIB:-0}" != "1" ]]; then
  main "$@"
fi
