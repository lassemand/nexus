#!/usr/bin/env bash
#
# The second half of starting a review session. It runs as the reviewer uid
# (1001), started by conductor-review-switch with a clean environment, and
# execs the command it was given (Claude).
#
# The session therefore has none of the agent's credentials and cannot read
# them: the agent's home, its GitHub App key and credential cache, and the
# environment of conductor are all uid 1000's. What it has is the reviewer
# identity directory (app.env, private-key.pem, gitconfig, mcp.json,
# settings.json), and the reviewer app's token is minted by the wrappers per
# call, not kept in the environment. The reviewer app's Contents permission is
# read-only, so a push fails at GitHub even if one were attempted.
#
# No `set -x`, ever.
set -euo pipefail

# What the entrypoint runs through conductor-review-switch to prove the uid
# switch works, before the identity is declared ready.
if [[ "${1:-}" == "--probe" ]]; then
  id -u
  exit 0
fi

dir="${CONDUCTOR_REVIEW_IDENTITY_DIR:-}"
if [[ -z "$dir" || ! -f "${dir}/ready" ]]; then
  echo "review-session: the reviewer identity is not ready (${dir:-unset})" >&2
  exit 1
fi
[[ $# -gt 0 ]] || { echo "review-session: no command given" >&2; exit 1; }
[[ "$(id -u)" != "0" ]] || { echo "review-session: refusing to run as root" >&2; exit 1; }

# shellcheck source=/dev/null
. "${dir}/app.env"

unset GITHUB_APP_PRIVATE_KEY GITHUB_TOKEN GH_TOKEN \
  REVIEW_GITHUB_APP_PRIVATE_KEY REVIEW_LINEAR_CLIENT_ID REVIEW_LINEAR_CLIENT_SECRET \
  LINEAR_API_KEY GITHUB_WEBHOOK_SECRET HOOKDECK_API_KEY DATABASE_URL

export GITHUB_APP_ID="$REVIEW_APP_ID"
export GITHUB_APP_INSTALLATION_ID="$REVIEW_INSTALLATION_ID"
export GITHUB_APP_PRIVATE_KEY_PATH="${dir}/private-key.pem"
export GIT_CONFIG_GLOBAL="${dir}/gitconfig"
unset REVIEW_APP_ID REVIEW_INSTALLATION_ID

mkdir -p "${HOME}/.cache/git-review"
chmod 0700 "${HOME}/.cache/git-review"

exec "$@"
