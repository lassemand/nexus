#!/usr/bin/env bash
# Proves which tasks moon selects for a set of touched files.
#
# For each case it runs scripts/ci/moon-ci.sh (the command CI runs) with a
# fixed list of touched files (CHANGED_FILES, given to `moon ci --stdin`), then
# reads the tasks moon actually ran from .moon/cache/ciReport.json and compares
# them with the exact expected set. Nothing is committed, nothing is built: it
# works in a scratch copy of the repository where the moon configuration, every
# Cargo.toml and Cargo.lock are real, every other tracked file is empty, and
# cargo, docker and shellcheck are stubs that succeed.
#
# Usage: scripts/ci/test-affected.sh   (needs moon at the version in .prototools)
set -euo pipefail

repo="$(git rev-parse --show-toplevel)"
moon="${MOON:-moon}"

want_version="$(sed -n 's/^moon *= *"\(.*\)"/\1/p' "$repo/.prototools")"
have_version="$("$moon" --version | awk '{print $2}')"
if [[ "$have_version" != "$want_version" ]]; then
  echo "moon $have_version found, .prototools pins $want_version" >&2
  exit 1
fi

work="$(mktemp -d)"
[[ -n "${KEEP:-}" ]] || trap 'rm -rf "$work"' EXIT; echo "work: $work" >&2
copy="$work/repo"
mkdir -p "$copy" "$work/cargo-home/bin" "$work/bin"

(cd "$repo" && git ls-files -z --cached --others --exclude-standard) |
  while IFS= read -r -d '' f; do
    [[ -e "$repo/$f" ]] || continue
    mkdir -p "$copy/$(dirname "$f")"
    case "$f" in
      .gitignore | .prototools | rust-toolchain.toml | .moon/* | moon.yml | */moon.yml | Cargo.toml | */Cargo.toml | Cargo.lock | scripts/ci/*)
        cp -p "$repo/$f" "$copy/$f" ;;
      *) : > "$copy/$f" ;;
    esac
  done

# moon puts $CARGO_HOME/bin first on the PATH of Rust tasks.
for tool in "$work/cargo-home/bin/cargo" "$work/bin/cargo" "$work/bin/docker" "$work/bin/shellcheck"; do
  printf '#!/bin/sh\nexit 0\n' > "$tool"
  chmod +x "$tool"
done

git -C "$copy" init -q -b main
git -C "$copy" add -A
git -C "$copy" -c user.name=test -c user.email=test@example.invalid commit -q -m base

export CARGO_HOME="$work/cargo-home"
export PATH="$work/bin:$PATH"
export MOON_COLOR=0 NO_COLOR=1
unset MOON_BASE MOON_HEAD GITHUB_BASE_REF GITHUB_HEAD_REF

failures=0

# ran <touched file>... : the sorted targets moon ci ran.
ran() {
  printf '%s\n' "$@" > "$work/changed"
  rm -f "$copy/.moon/cache/ciReport.json"
  if ! (cd "$copy" && CHANGED_FILES="$work/changed" MOON="$moon" bash scripts/ci/moon-ci.sh) > "$work/out" 2>&1; then
    echo "moon-ci.sh failed for: $*" >&2
    cat "$work/out" >&2
    return 1
  fi
  report_targets
}

report_targets() {
  python3 - "$copy/.moon/cache/ciReport.json" <<'PY'
import json, sys
report = json.load(open(sys.argv[1]))
targets = sorted(
    a["node"]["params"]["target"]
    for a in report["actions"]
    if a["node"]["action"] == "run-task" and a["status"] not in ("skipped", "invalid")
)
print("\n".join(targets))
PY
}

# check <name> <expected targets, space separated> -- <touched files>...
check() {
  local name="$1" expected="$2" got want
  shift 3
  want="$(tr ' ' '\n' <<< "$expected" | grep . | sort)"
  got="$(ran "$@")" || { failures=$((failures + 1)); return; }
  if [[ "$got" == "$want" ]]; then
    echo "ok   $name"
  else
    echo "FAIL $name"
    diff <(echo "$want") <(echo "$got") | sed -n 's/^</  missing:/p; s/^>/  extra:  /p'
    failures=$((failures + 1))
  fi
}

crate() { local c; for c in "$@"; do printf '%s:lint %s:test ' "$c" "$c"; done; }

fmt="root:fmt"
bench="verdict/bench:fmt verdict/bench:lint verdict/bench:lint-wasm verdict/bench:build verdict/bench:build-wasm verdict/bench:test"
template="verdict/template:fmt verdict/template:lint verdict/template:test verdict/template:build-wasm"
telemetry_extra="telemetry:test-kafka telemetry:test-collector-down"
wasm="verdict/kernels/sample:wasm"
deploy="conductor/deploy:shellcheck conductor/deploy:test"
prep="verdict/prep:syntax verdict/prep:test"
promtool="infra/charts/nexus/prometheus:rules"
rust_crates="model alpha chronicle signal insight cli conductor telemetry analyst verdict/sdk verdict/kernels/sample"

# verdict/bench depends on model only through analyst: deep dependents.
check "model: model and every crate above it" \
  "$fmt $(crate model alpha chronicle signal cli analyst) $bench" -- model/src/lib.rs

check "chronicle: chronicle and its dependents only" \
  "$fmt $(crate chronicle)" -- chronicle/src/nordic/live.rs

check "signal migration: signal, and analyst's and chronicle's tests" \
  "$fmt $(crate signal) analyst:test chronicle:test" -- signal/migrations/20990101000000_x.sql

check "recorded e2e events: chronicle and signal" \
  "$fmt $(crate chronicle signal)" -- chronicle/tests/fixtures/mfn/e2e/x.pb

check "docs: only fmt" "$fmt" -- docs/guides/x.md

check "Cargo.lock: every Rust task" \
  "$fmt $(crate $rust_crates) $telemetry_extra $bench $template $wasm verdict/prep:test" -- Cargo.lock

for global in Cargo.toml rust-toolchain.toml .cargo/config.toml .moon/workspace.yml .github/workflows/ci.yml; do
  check "$global: every Rust task" \
    "$fmt $(crate $rust_crates) $telemetry_extra $bench $template $wasm verdict/prep:test" -- "$global"
done

check "alert rules: only promtool" "$fmt $promtool" -- infra/charts/nexus/prometheus/rules/nexus.rules.yml

check "conductor/deploy script: deploy tasks, not conductor's Rust tests" \
  "$fmt $deploy" -- conductor/deploy/entrypoint.sh

check "conductor/deploy file read by conductor's tests" \
  "$fmt $deploy conductor:test" -- conductor/deploy/reviewer-settings.json

check "verdict-sdk: sdk, sample, WASM, bench, template, prep and the crates above it" \
  "$fmt $(crate verdict/sdk verdict/kernels/sample analyst) $wasm $bench $template verdict/prep:test" -- verdict/sdk/src/lib.rs

check "verdict template: template and prep" "$fmt $template verdict/prep:test" -- verdict/template/src/lib.rs

check "telemetry: every crate above it, deep" \
  "$fmt $(crate telemetry chronicle signal insight conductor analyst) $telemetry_extra $bench" -- telemetry/src/lib.rs

# The git path, without CHANGED_FILES: a branch that touches docs only.
git -C "$copy" checkout -q -b docs-change
mkdir -p "$copy/docs/guides"
echo change > "$copy/docs/guides/x.md"
git -C "$copy" add -A
git -C "$copy" -c user.name=test -c user.email=test@example.invalid commit -q -m docs
# check_git <name> <dir> <expected targets> [env assignments...]
check_git() {
  local name="$1" dir="$2" expected="$3" got want
  shift 3
  rm -f "$dir/.moon/cache/ciReport.json"
  if ! (cd "$dir" && env "$@" MOON="$moon" bash scripts/ci/moon-ci.sh) > "$work/out" 2>&1; then
    echo "FAIL $name: moon-ci.sh failed"
    cat "$work/out"
    failures=$((failures + 1))
    return
  fi
  want="$(tr ' ' '\n' <<< "$expected" | grep . | sort)"
  got="$(copy="$dir" report_targets)"
  if [[ "$got" == "$want" ]]; then
    echo "ok   $name"
  else
    echo "FAIL $name"
    diff <(echo "$want") <(echo "$got") | sed -n 's/^</  missing:/p; s/^>/  extra:  /p'
    failures=$((failures + 1))
  fi
}

check_git "git diff against main: only fmt" "$copy" "$fmt" MOON_BASE=main
check_git "git diff against main, on GitHub Actions: only fmt" "$copy" "$fmt" \
  MOON_BASE=main CI=true GITHUB_ACTIONS=true GITHUB_EVENT_NAME=pull_request GITHUB_BASE_REF=main GITHUB_HEAD_REF=docs-change

# When in doubt, everything (`moon ci --force`), never nothing.
all="$fmt $(crate $rust_crates) $telemetry_extra $bench $template $wasm $deploy $prep $promtool"

check_git "unknown base: every task" "$copy" "$all" MOON_BASE=does-not-exist

check_git "empty diff: every task" "$copy" "$all" MOON_BASE=docs-change

# A shallow clone of the branch with a shallow fetch of main, as on a CI
# runner: no merge base.
git clone -q --depth 1 --branch docs-change "file://$copy" "$work/shallow"
git -C "$work/shallow" fetch -q --depth 1 origin main:refs/remotes/origin/main
check_git "shallow clone without a merge base: every task" "$work/shallow" "$all" \
  GITHUB_ACTIONS=true GITHUB_BASE_REF=main GITHUB_HEAD_REF=docs-change

if [[ $failures -ne 0 ]]; then
  echo "$failures case(s) failed" >&2
  exit 1
fi
echo "all cases passed"
