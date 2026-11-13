#!/usr/bin/env bash
# Sets up the writable workspace of a preparation: the kernel crate next to the
# SDK, a cargo home that builds offline from the vendored crates, and a copy of
# the pre-built target directory. Sourced by entrypoint.sh and smoke.sh.
#
# Every path can be overridden, so the same code runs in the image (the
# defaults) and in tests.

VERDICT_DIR="${VERDICT_DIR:-/verdict}"
WORK="${WORK:-/work}"
AGENT_SRC="${AGENT_SRC:-/home/prep/.claude/agents/analyst.md}"
CARGO_CONFIG_SRC="${CARGO_CONFIG_SRC:-/opt/cargo-config.toml}"
PREBUILT_TARGET="${PREBUILT_TARGET:-/opt/prebuilt/target}"

prepare_workspace() {
  mkdir -p "$WORK"
  rm -rf "$WORK/sdk" "$WORK/kernel"
  cp -a "$VERDICT_DIR/sdk" "$WORK/sdk"
  cp -a "$VERDICT_DIR/template" "$WORK/kernel"
  rm -rf "$WORK/sdk/target" "$WORK/kernel/target"

  export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
  export HOME="$WORK/home"
  mkdir -p "$HOME/.claude/agents"
  cp "$AGENT_SRC" "$HOME/.claude/agents/analyst.md"

  export CARGO_HOME="${PREP_CARGO_HOME:-$WORK/cargo}"
  mkdir -p "$CARGO_HOME"
  if [[ -f "$CARGO_CONFIG_SRC" ]]; then
    cp "$CARGO_CONFIG_SRC" "$CARGO_HOME/config.toml"
  fi

  export CARGO_TARGET_DIR="$WORK/target"
  if [[ -d "$PREBUILT_TARGET" ]]; then
    rm -rf "$CARGO_TARGET_DIR"
    cp -a "$PREBUILT_TARGET" "$CARGO_TARGET_DIR"
  fi
}
