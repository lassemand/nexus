#!/usr/bin/env bash
# Tells CI, before it sets anything up, what scripts/ci/moon-ci.sh will run.
#
# It lets moon-ci.sh choose as it will in the real run (the same MOON_BASE,
# MOON_CI_ALL and fallbacks), then, for the affected case, runs moon-ci.sh with
# those touched files in the stubbed copy of scripts/ci/stub-workspace.sh and
# reads what `moon ci` ran from its report. So the plan is what moon ci
# executes, not `moon query tasks --affected` (moon issue 2706). It runs twice:
# once as the main job does, once for `conductor/deploy:isolation`.
#
# Writes to $GITHUB_OUTPUT (and prints):
#   heavy=true      something other than root:fmt runs, so the job with
#                   Postgres, the system packages and the Rust cache is needed;
#   isolation=true  conductor/deploy:isolation runs;
#   mode=...        "affected" or "everything: <reason>".
# When anything goes wrong, both are true.
#
# Usage: scripts/ci/moon-plan.sh
set -uo pipefail

repo="$(git rev-parse --show-toplevel)"
cd "$repo" || exit 2
moon="${MOON:-moon}"
state=.moon/cache/moon-ci

emit() {
  echo "heavy=$1"
  echo "isolation=$2"
  echo "mode=$3"
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    { echo "heavy=$1"; echo "isolation=$2"; echo "mode=$3"; } >> "$GITHUB_OUTPUT"
  fi
  exit 0
}

if ! MOON_CI_DECIDE_ONLY=1 bash scripts/ci/moon-ci.sh >&2 || [[ ! -s "$state/mode" ]]; then
  emit true true "everything: moon-ci.sh could not choose"
fi
mode="$(cat "$state/mode")"
[[ "$mode" == affected ]] || emit true true "$mode"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp "$state/touched" "$work/touched"
if ! bash scripts/ci/stub-workspace.sh "$work" >&2; then
  emit true true "everything: the stubbed copy failed"
fi

# planned [VAR=value...] -- <moon-ci args...>: the targets moon runs in the
# stubbed copy (none when moon finds nothing affected and writes no report).
planned() {
  local vars=()
  while [[ $# -gt 0 && "$1" != "--" ]]; do vars+=("$1"); shift; done
  shift
  (
    cd "$work/repo" &&
      env -u MOON_BASE -u MOON_HEAD -u MOON_CI_ALL -u MOON_CI_IGNORE_CI_CHECKS -u MOON_CI_DECIDE_ONLY \
        CARGO_HOME="$work/cargo-home" PATH="$work/bin:$PATH" CHANGED_FILES="$work/touched" MOON="$moon" \
        "${vars[@]}" bash scripts/ci/moon-ci.sh "$@" >&2 || exit 1
    [[ -s .moon/cache/moon-ci/report.json ]] || exit 0
    python3 - .moon/cache/moon-ci/report.json <<'PY'
import json, sys
for a in json.load(open(sys.argv[1]))["actions"]:
    if a["node"]["action"] == "run-task":
        print(a["node"]["params"]["target"])
PY
  )
}

if ! main="$(planned --)"; then
  emit true true "everything: planning failed"
fi
if ! iso="$(planned MOON_CI_IGNORE_CI_CHECKS=1 -- conductor/deploy:isolation)"; then
  emit true true "everything: planning failed"
fi
echo "planned:" $main >&2

heavy=false
grep -qvx -e 'root:fmt' -e '' <<< "$main" && heavy=true
isolation=false
grep -qx 'conductor/deploy:isolation' <<< "$iso" && isolation=true
emit "$heavy" "$isolation" "$mode"
