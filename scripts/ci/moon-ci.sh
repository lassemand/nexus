#!/usr/bin/env bash
# Runs, with `moon ci`, every task that the change can affect; see
# docs/guides/ci.md.
#
# The touched files are `git diff --name-only --no-renames` between the merge
# base of MOON_BASE (default origin/main) and MOON_HEAD (default HEAD), fed to
# moon on stdin. Dependents are deep, dependencies are not run:
#
#   moon ci --stdin --include-relations --downstream deep --upstream none
#
# When in doubt, every task runs (`moon ci --force`), never none:
#   * the base or head cannot be resolved, or has no merge base (a shallow
#     clone, a missing fetch);
#   * the diff is empty (a push to main compared with itself);
#   * moon fails before it starts any task.
#
# Usage: scripts/ci/moon-ci.sh [moon ci targets or options...]
# CHANGED_FILES=<file> replaces the git diff with that list (used by
# scripts/ci/test-affected.sh). MOON overrides the moon binary.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 2
moon="${MOON:-moon}"
report=.moon/cache/ciReport.json
changed="$(mktemp)"
trap 'rm -f "$changed"' EXIT

log() { echo "moon-ci: $*" >&2; }

touched_files() {
  if [[ -n "${CHANGED_FILES:-}" ]]; then
    cat -- "$CHANGED_FILES"
    return
  fi
  local base="${MOON_BASE:-origin/main}" head="${MOON_HEAD:-HEAD}" merge_base
  git rev-parse --verify --quiet "$base^{commit}" >/dev/null || { log "base $base not found"; return 1; }
  git rev-parse --verify --quiet "$head^{commit}" >/dev/null || { log "head $head not found"; return 1; }
  merge_base="$(git merge-base "$base" "$head")" || { log "no merge base of $base and $head (shallow clone?)"; return 1; }
  git diff --name-only --no-renames "$merge_base" "$head"
}

run_everything() {
  log "running every task: $1"
  shift
  env -u MOON_BASE -u MOON_HEAD "$moon" ci --force "$@"
}

ran_a_task() {
  [[ -s "$report" ]] && grep -q '"run-task"\|"RunTask"' "$report"
}

if ! touched_files > "$changed"; then
  run_everything "the touched files are unknown" "$@"
  exit
fi
if ! grep -q . "$changed"; then
  run_everything "no touched files" "$@"
  exit
fi

log "$(grep -c . "$changed") touched files"
rm -f "$report"
"$moon" ci --stdin --include-relations --downstream deep --upstream none "$@" < "$changed"
status=$?
if [[ $status -ne 0 ]] && ! ran_a_task; then
  run_everything "moon failed (exit $status) before running a task" "$@"
  exit
fi
exit "$status"
