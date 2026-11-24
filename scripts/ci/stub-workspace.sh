#!/usr/bin/env bash
# Builds a scratch copy of the repository in which `moon ci` can run every task
# without building or testing anything, so that what it runs shows what it
# selects. Used by scripts/ci/test-affected.sh and scripts/ci/moon-plan.sh.
#
#   <dest>/repo        a git repository (one commit) where the moon
#                      configuration, every Cargo.toml, Cargo.lock,
#                      .gitignore and scripts/ci/ are real, and every other
#                      tracked (or untracked, not ignored) file is empty;
#   <dest>/cargo-home  CARGO_HOME, whose bin/cargo is a stub (moon puts
#                      $CARGO_HOME/bin first on the PATH of Rust tasks);
#   <dest>/bin         stubs of cargo, docker and shellcheck, for the PATH.
#
# The stubs exit 0, and empty shell scripts succeed, so every task passes.
#
# Usage: scripts/ci/stub-workspace.sh <dest>   (dest must not exist or be empty)
set -euo pipefail

dest="${1:?usage: stub-workspace.sh <dest>}"
repo="$(git rev-parse --show-toplevel)"
copy="$dest/repo"
mkdir -p "$copy" "$dest/cargo-home/bin" "$dest/bin"

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

for tool in "$dest/cargo-home/bin/cargo" "$dest/bin/cargo" "$dest/bin/docker" "$dest/bin/shellcheck"; do
  printf '#!/bin/sh\nexit 0\n' > "$tool"
  chmod +x "$tool"
done

git -C "$copy" init -q -b main
git -C "$copy" add -A
git -C "$copy" -c user.name=stub -c user.email=stub@example.invalid commit -q -m base
