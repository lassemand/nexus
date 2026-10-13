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

  GITHUB_APP_CREDENTIAL_HELPER="${CONDUCTOR_GITHUB_HELPER:-/usr/local/bin/github-app-credential-helper}"
  export GITHUB_APP_CREDENTIAL_HELPER

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
  local missing="" problems=""

  [[ -n "${LINEAR_API_KEY:-}" ]] || missing="${missing} LINEAR_API_KEY"

  # ── how to authenticate to GitHub ──
  #
  # App credentials are preferred and are what the developer's Mac already
  # uses. The App needs no long-lived secret: tokens are minted from the
  # private key, one hour at a time, by the credential helper for git and by
  # github-mcp-server for the MCP tools.
  #
  # A PAT still works, because the branch lookup in `conductor serve` takes a
  # plain token and because it is one less moving part when debugging.
  local has_app=""
  [[ -n "${GITHUB_APP_ID:-}" && -n "${GITHUB_APP_INSTALLATION_ID:-}" \
     && -n "${GITHUB_APP_PRIVATE_KEY:-}" ]] && has_app=1

  GITHUB_MODE="${CONDUCTOR_GITHUB_MODE:-}"
  if [[ -z "$GITHUB_MODE" ]]; then
    if [[ -n "$has_app" ]]; then
      GITHUB_MODE=app
    elif [[ -n "${GITHUB_TOKEN:-}" ]]; then
      GITHUB_MODE=token
    else
      problems="${problems}
missing GitHub credentials: set GITHUB_APP_ID + GITHUB_APP_INSTALLATION_ID + GITHUB_APP_PRIVATE_KEY (preferred), or GITHUB_TOKEN"
    fi
  fi

  case "$GITHUB_MODE" in
    app)
      [[ -n "$has_app" ]] || problems="${problems}
CONDUCTOR_GITHUB_MODE=app but GITHUB_APP_ID, GITHUB_APP_INSTALLATION_ID and GITHUB_APP_PRIVATE_KEY are not all set"
      ;;
    token)
      [[ -n "${GITHUB_TOKEN:-}" ]] || problems="${problems}
CONDUCTOR_GITHUB_MODE=token but GITHUB_TOKEN is not set"
      ;;
    "") ;;
    *)
      problems="${problems}
CONDUCTOR_GITHUB_MODE must be 'app' or 'token', got '${GITHUB_MODE}'"
      ;;
  esac
  [[ -n "$missing" ]] && problems="${problems}
missing:${missing}"

  # ── which model credential to use ──
  #
  # Both are supported, but never at the same time. Claude Code resolves
  # ANTHROPIC_API_KEY ahead of subscription credentials, so leaving both in the
  # environment would silently bill per-token while the operator believed the
  # subscription was in use — an expensive failure that looks like success. The
  # unused one is therefore unset rather than merely ignored.
  local has_sub="" has_key=""
  [[ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" || -n "${CLAUDE_CODE_CREDENTIALS_JSON:-}" \
     || -f "${HOME}/.claude/.credentials.json" ]] && has_sub=1
  [[ -n "${ANTHROPIC_API_KEY:-}" ]] && has_key=1

  AUTH_MODE="${CONDUCTOR_AUTH_MODE:-}"
  if [[ -z "$AUTH_MODE" ]]; then
    if [[ -n "$has_sub" && -n "$has_key" ]]; then
      problems="${problems}
both a subscription credential and ANTHROPIC_API_KEY are present; set CONDUCTOR_AUTH_MODE to 'subscription' or 'api_key' so the billing choice is explicit"
    elif [[ -n "$has_sub" ]]; then
      AUTH_MODE=subscription
    elif [[ -n "$has_key" ]]; then
      AUTH_MODE=api_key
    else
      problems="${problems}
missing a model credential: set CLAUDE_CODE_CREDENTIALS_JSON (subscription, renews itself), CLAUDE_CODE_OAUTH_TOKEN, or ANTHROPIC_API_KEY"
    fi
  fi

  case "$AUTH_MODE" in
    subscription)
      [[ -n "$has_sub" ]] || problems="${problems}
CONDUCTOR_AUTH_MODE=subscription but no subscription credential is present"
      ;;
    api_key)
      [[ -n "$has_key" ]] || problems="${problems}
CONDUCTOR_AUTH_MODE=api_key but ANTHROPIC_API_KEY is not set"
      ;;
    "") ;;
    *)
      problems="${problems}
CONDUCTOR_AUTH_MODE must be 'subscription' or 'api_key', got '${AUTH_MODE}'"
      ;;
  esac

  if [[ -n "$problems" ]]; then
    printf '[entrypoint] FATAL: environment is incomplete\n' >&2
    while IFS= read -r line; do
      [[ -n "$line" ]] && printf '[entrypoint]   - %s\n' "$line" >&2
    done <<<"$problems"
    printf '[entrypoint] these come from the conductor-secret ExternalSecret (Vault: nexus/conductor)\n' >&2
    return 1
  fi

  # Enforce the choice, so the other credential cannot take effect by accident.
  if [[ "$AUTH_MODE" == "subscription" ]]; then
    unset ANTHROPIC_API_KEY
  else
    unset CLAUDE_CODE_OAUTH_TOKEN CLAUDE_CODE_CREDENTIALS_JSON
  fi
  export AUTH_MODE GITHUB_MODE
  log "environment complete (auth mode: ${AUTH_MODE}, github mode: ${GITHUB_MODE})"
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
  if [[ "$GITHUB_MODE" == "app" ]]; then
    # The private key arrives as a secret property, so it has to be written
    # before anything can sign a JWT with it.
    local key_dir="$HOME/.config/github-app"
    mkdir -p "$key_dir"
    GITHUB_APP_PRIVATE_KEY_PATH="${key_dir}/private-key.pem"
    local tmp
    tmp="$(mktemp)"
    printf '%s' "$GITHUB_APP_PRIVATE_KEY" > "$tmp"
    if ! openssl rsa -in "$tmp" -noout -check >/dev/null 2>&1 \
       && ! openssl pkey -in "$tmp" -noout >/dev/null 2>&1; then
      rm -f "$tmp"
      die "GITHUB_APP_PRIVATE_KEY is not a usable private key"
    fi
    chmod 0600 "$tmp"
    mv "$tmp" "$GITHUB_APP_PRIVATE_KEY_PATH"
    export GITHUB_APP_PRIVATE_KEY_PATH

    # Git calls a credential helper per operation, so the one-hour token
    # lifetime stops mattering. A token in the environment would instead work
    # for the first agent run and fail every push afterwards.
    #
    # Two helpers, in order. git's own credential-cache answers first and
    # holds the token in a daemon's *memory*; on a miss it falls through to the
    # App helper below, and git stores the result back into the cache. That
    # keeps a live credential off the PersistentVolume, which a disk cache in
    # the helper would not.
    #
    # The timeout is deliberately under the token's hour: close enough to save
    # the repeated mints, far enough from expiry that a cached token cannot go
    # stale while an operation is using it.
    local cache_dir="$HOME/.cache/git"
    mkdir -p "$cache_dir"
    # credential-cache refuses to start if this directory is group- or
    # world-readable, since anyone who could read it could read the socket.
    chmod 0700 "$cache_dir"

    git config --global --replace-all \
      "credential.https://github.com.helper" \
      "cache --timeout=${CONDUCTOR_GIT_CREDENTIAL_TTL:-3300} --socket ${cache_dir}/credential-socket"
    git config --global --add \
      "credential.https://github.com.helper" "$GITHUB_APP_CREDENTIAL_HELPER"
    git config --global credential.https://github.com.useHttpPath false
    log "git authenticates through the GitHub App (installation ${GITHUB_APP_INSTALLATION_ID})"

    # Proves the key signs, the installation exists and the token mints — all
    # before an agent discovers otherwise mid-task.
    if ! printf 'protocol=https\nhost=github.com\n\n' \
         | "$GITHUB_APP_CREDENTIAL_HELPER" get >/dev/null 2>&1; then
      die "could not mint a GitHub App installation token; check GITHUB_APP_ID, GITHUB_APP_INSTALLATION_ID and the private key"
    fi
    log "GitHub App token minting verified"
  else
    # gh reads GITHUB_TOKEN from the environment. `setup-git` installs gh as
    # git's credential helper, so pushes authenticate without the token ever
    # appearing in a remote URL or in .git/config — which agents commit, and
    # which would leak it into the repository.
    gh auth setup-git
    if ! gh auth status >/dev/null 2>&1; then
      gh auth status >&2 || true
      die "gh auth status failed; check GITHUB_TOKEN's scopes (contents, pull requests, issues on ${CONDUCTOR_REPO:-lassemand/nexus})"
    fi
    log "git authenticates through GITHUB_TOKEN via gh"
  fi
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

# ── subscription credentials ─────────────────────────────────────────────────
#
# Claude Code renews its own subscription credential. ~/.claude/.credentials.json
# holds an accessToken (lifetime measured in hours), a refreshToken (weeks), and
# both expiry timestamps; Claude Code exchanges the refresh token when the access
# token expires and writes the new pair back.
#
# That is why CLAUDE_CODE_OAUTH_TOKEN is the wrong mechanism for a long-running
# pod: it is a static snapshot of a credential that expires in a couple of
# hours, and nothing updates the environment variable afterwards. Persisting the
# file instead — HOME is on the volume — means renewal happens by itself, and
# the only standing deadline is the refresh token's own expiry, which rolls
# forward every time it is used.
#
# Seeded write-if-absent, never overwritten: an existing file has been refreshed
# since it was seeded, and replacing it with the Vault copy would roll the
# credential back to a stale snapshot and undo the renewal.

seed_claude_credentials() {
  local creds="$HOME/.claude/.credentials.json"
  mkdir -p "$(dirname "$creds")"

  # A static token wins over the file, which is the reverse of what this
  # script originally did.
  #
  # The file-based credential rotates its refresh token on every refresh and
  # revokes the previous one, so two clients holding copies invalidate each
  # other. That is not hypothetical: seeding this pod from a snapshot of a
  # developer's live credential produced 383 crash-loops, because the laptop
  # kept refreshing and the pod's copy was dead on arrival — while its own
  # expiry timestamp still claimed two weeks of validity.
  #
  # A token supplied through the environment cannot rotate: there is nowhere
  # to write a replacement. That makes it safe to share, and safe across the
  # concurrent sessions in this pod, at the cost of no self-renewal.
  if [[ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]]; then
    # Any file here would be an older credential and might shadow the token,
    # so it is moved aside rather than left to compete. Renamed, not deleted:
    # it is the only copy of whatever session it belonged to.
    if [[ -f "$creds" ]]; then
      local superseded="${creds}.superseded-$(date +%s)"
      mv "$creds" "$superseded"
      log "moved an existing credentials file to ${superseded}; CLAUDE_CODE_OAUTH_TOKEN takes precedence"
    fi
    log "using CLAUDE_CODE_OAUTH_TOKEN (static; it cannot renew itself, so the smoke check below is what proves it still works)"
    return 0
  fi

  if [[ -f "$creds" ]]; then
    if jq empty "$creds" >/dev/null 2>&1; then
      log "using the existing credentials file on the volume"
      log "WARNING: a file credential rotates on refresh and revokes the previous one. Nothing else may hold a copy of it — not a laptop, not another deployment — or both will be invalidated. CLAUDE_CODE_OAUTH_TOKEN from 'claude setup-token' avoids this."
    else
      # Several Claude processes share this file, so a torn write is possible.
      local backup
      backup="${creds}.corrupt-$(date +%s)"
      mv "$creds" "$backup"
      log "WARNING: ${creds} was not valid JSON; moved to ${backup}"
    fi
  fi

  if [[ ! -f "$creds" ]]; then
    if [[ -n "${CLAUDE_CODE_CREDENTIALS_JSON:-}" ]]; then
      local tmp
      tmp="$(mktemp)"
      printf '%s' "$CLAUDE_CODE_CREDENTIALS_JSON" > "$tmp"
      if ! jq empty "$tmp" >/dev/null 2>&1; then
        rm -f "$tmp"
        die "CLAUDE_CODE_CREDENTIALS_JSON is not valid JSON"
      fi
      chmod 0600 "$tmp"
      mv "$tmp" "$creds"
      log "seeded subscription credentials from CLAUDE_CODE_CREDENTIALS_JSON"
      log "WARNING: this is a copy of a credential that rotates. If its origin refreshes, this copy is revoked. Prefer CLAUDE_CODE_OAUTH_TOKEN."
    else
      die "auth mode is subscription but neither CLAUDE_CODE_OAUTH_TOKEN nor CLAUDE_CODE_CREDENTIALS_JSON is set, and no credentials file exists on the volume"
    fi
  fi

  # Reported, never trusted. This is a timestamp inside the file: it says
  # nothing about whether the token has been revoked, and a revoked credential
  # happily claims weeks of remaining life. The smoke check is the only thing
  # that establishes the credential actually works.
  local expires_ms now_ms days
  expires_ms="$(jq -r '.claudeAiOauth.refreshTokenExpiresAt // empty' "$creds" 2>/dev/null || true)"
  if [[ -n "$expires_ms" && "$expires_ms" != "null" ]]; then
    now_ms=$(( $(date +%s) * 1000 ))
    days=$(( (expires_ms - now_ms) / 86400000 ))
    if (( expires_ms <= now_ms )); then
      local ago_h=$(( (now_ms - expires_ms) / 3600000 ))
      die "the credentials file claims its refresh token expired ${ago_h}h ago; re-run 'claude setup-token' and set CLAUDE_CODE_OAUTH_TOKEN"
    fi
    log "credentials file claims ${days} day(s) of refresh-token life (a claim, not a check — revocation is invisible here)"
  fi
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
  add_mcp linear "https://mcp.linear.app/mcp" "$LINEAR_API_KEY"

  claude mcp remove --scope user github >/dev/null 2>&1 || true
  if [[ "$GITHUB_MODE" == "app" ]]; then
    # The same shape the developer's Mac runs: a local stdio server holding the
    # App credentials, minting its own short-lived tokens. No bearer token to
    # store, so nothing long-lived lands in ~/.claude.json on the volume.
    claude mcp add --scope user github -- \
      github-mcp-server stdio \
        --app-id "$GITHUB_APP_ID" \
        --app-installation-id "$GITHUB_APP_INSTALLATION_ID" \
        --app-private-key-path "$GITHUB_APP_PRIVATE_KEY_PATH" \
        --toolsets all >/dev/null
  else
    add_mcp github "https://api.githubcopilot.com/mcp/" "$GITHUB_TOKEN"
  fi

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
  # An `if` rather than `[[ ]] &&`: at statement level a false test returns 1,
  # which `set -e` turns into an exit — so api_key mode would never have started.
  if [[ "${AUTH_MODE:-}" == "subscription" ]]; then
    seed_claude_credentials
  fi
  configure_mcp
  smoke_check
  log "starting conductor serve"
  exec conductor serve
}

# Sourced for testing rather than run: define the functions and stop.
if [[ "${CONDUCTOR_ENTRYPOINT_LIB:-0}" != "1" ]]; then
  main "$@"
fi
