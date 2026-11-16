#!/usr/bin/env bash
#
# Starts a review session under the reviewer app's identity (Annette), then
# execs the command it was given (Claude).
#
# conductor prepends this to a review set's command line. Every other
# credential in the pod's environment is dropped, so the author app can never
# act from a review session; git, gh and the GitHub App credential helper see
# only the reviewer app, whose Contents permission is read-only, so a push
# fails at GitHub even if one were attempted.
#
# The identity directory is prepared by the entrypoint
# (`configure_review_identity`): app.env (the app and installation ids),
# private-key.pem, gitconfig (its own credential cache), mcp.json,
# settings.json, bot-login and, last, ready.
#
# No `set -x`, ever: a minted token passes through here.
set -euo pipefail

dir="${CONDUCTOR_REVIEW_IDENTITY_DIR:-}"
if [[ -z "$dir" || ! -f "${dir}/ready" ]]; then
  echo "review-session: the reviewer identity is not prepared (${dir:-unset})" >&2
  exit 1
fi
[[ $# -gt 0 ]] || { echo "review-session: no command given" >&2; exit 1; }

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

helper="${GITHUB_APP_CREDENTIAL_HELPER:-/usr/local/bin/github-app-credential-helper}"
token="$(printf 'protocol=https\nhost=github.com\n\n' | "$helper" get | sed -n 's/^password=//p')" || token=""
if [[ -z "$token" ]]; then
  echo "review-session: minting the reviewer app's token failed" >&2
  exit 1
fi
export GH_TOKEN="$token"
unset token

exec "$@"
