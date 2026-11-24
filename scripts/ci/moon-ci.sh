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
# MOON_CI_ALL=<reason> runs every task (CI sets it on main and for a manual
# run). When in doubt, every task runs too (`moon ci --force`), never none:
#   * the base or head cannot be resolved, or has no merge base (a shallow
#     clone, a missing fetch);
#   * the diff is empty (a push to main compared with itself);
#   * moon fails before it starts any task.
#
# Usage: scripts/ci/moon-ci.sh [moon ci targets or options...]
#
# MOON_CI_IGNORE_CI_CHECKS=1 also runs tasks with `runInCI: false` (the review
# isolation job's `conductor/deploy:isolation`): `moon ci` has no such option,
# so it runs `moon exec --ci --on-failure continue --ignore-ci-checks` with the
# same flags. MOON_CI_DECIDE_ONLY=1 stops after choosing, without running moon
# (scripts/ci/moon-plan.sh). CHANGED_FILES=<file> replaces the git diff with
# that list (scripts/ci/test-affected.sh). MOON overrides the moon binary.
#
# It leaves, in .moon/cache/moon-ci/: `mode` ("affected", or "everything:
# <reason>"), `touched` (the touched files) and `report.json` (moon's report of
# the run), read by scripts/ci/moon-summary.py.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 2
moon="${MOON:-moon}"
state=.moon/cache/moon-ci
mkdir -p "$state"
changed="$state/touched"
rm -f "$changed" "$state/mode" "$state/report.json" .moon/cache/ciReport.json .moon/cache/runReport.json

if [[ -n "${MOON_CI_IGNORE_CI_CHECKS:-}" ]]; then
  base_cmd=("$moon" exec --ci --on-failure continue --summary detailed --ignore-ci-checks)
  affected=(--affected)
else
  base_cmd=("$moon" ci)
  affected=()
fi

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

keep_report() {
  local r
  for r in .moon/cache/ciReport.json .moon/cache/runReport.json; do
    [[ -s "$r" ]] && cp "$r" "$state/report.json"
  done
}

ran_a_task() {
  [[ -s "$state/report.json" ]] && grep -q '"run-task"' "$state/report.json"
}

run_everything() {
  log "running every task: $1"
  echo "everything: $1" > "$state/mode"
  [[ -n "${MOON_CI_DECIDE_ONLY:-}" ]] && exit 0
  shift
  rm -f .moon/cache/ciReport.json .moon/cache/runReport.json "$state/report.json"
  env -u MOON_BASE -u MOON_HEAD "${base_cmd[@]}" --force "$@"
  local status=$?
  keep_report
  exit "$status"
}

if [[ -n "${MOON_CI_ALL:-}" ]]; then
  run_everything "$MOON_CI_ALL" "$@"
fi
if ! touched_files > "$changed"; then
  run_everything "the touched files are unknown" "$@"
fi
if ! grep -q . "$changed"; then
  run_everything "no touched files" "$@"
fi

log "$(grep -c . "$changed") touched files"
echo "affected" > "$state/mode"
[[ -n "${MOON_CI_DECIDE_ONLY:-}" ]] && exit 0
"${base_cmd[@]}" "${affected[@]}" --stdin --include-relations --downstream deep --upstream none "$@" < "$changed"
status=$?
keep_report
if [[ $status -ne 0 ]] && ! ran_a_task; then
  run_everything "moon failed (exit $status) before running a task" "$@"
fi
exit "$status"
