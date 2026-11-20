#!/usr/bin/env bash
#
# The first half of starting a review session. It runs as the agent uid (uid
# 1000), inside the tmux pane conductor created, and hands over to
# conductor-review-switch, which becomes the reviewer uid and runs
# conductor-review-session.
#
# Only what has to be done with the agent's own access happens here:
#   - refuse to start unless the entrypoint finished the reviewer identity;
#   - give the reviewer its own copy of the model login (subscription mode),
#     because it may not read the agent's home. API-key mode needs no file: the
#     key is passed through the environment allow-list of the switch.
#
# No `set -x`, ever.
set -euo pipefail

dir="${CONDUCTOR_REVIEW_IDENTITY_DIR:-}"
if [[ -z "$dir" || ! -f "${dir}/ready" ]]; then
  echo "review-launch: the reviewer identity is not ready (${dir:-unset})" >&2
  exit 1
fi
[[ $# -gt 0 ]] || { echo "review-launch: no command given" >&2; exit 1; }

review_home="${CONDUCTOR_REVIEW_HOME:-${CONDUCTOR_DATA_DIR:-/data}/review/home}"
credentials="${HOME}/.claude/.credentials.json"
if [[ -f "$credentials" ]]; then
  mkdir -p "${review_home}/.claude"
  chmod 0777 "${review_home}/.claude"
  tmp="$(mktemp "${review_home}/.claude/.credentials.XXXXXX")"
  cat "$credentials" > "$tmp"
  chmod 0666 "$tmp"
  mv -f "$tmp" "${review_home}/.claude/.credentials.json"
fi

exec "${CONDUCTOR_REVIEW_SWITCH:-/usr/local/libexec/conductor-review-switch}" "$@"
