#!/usr/bin/env bash
# Checks the preparation image from the inside, the way the Job runs it: as a
# non-root user, on a read-only root filesystem with /work and /tmp writable:
#
#   docker run --rm --read-only --user 1000:1000 \
#     --tmpfs /work:rw,exec,uid=1000,gid=1000,size=4g --tmpfs /tmp:rw,exec \
#     -e EXPECTED_CLAUDE=<pinned version> --entrypoint /verdict/prep/smoke.sh <image>
#
# Needs no Claude API key and spends nothing.
set -euo pipefail

: "${EXPECTED_CLAUDE:?set EXPECTED_CLAUDE to the pinned Claude Code version}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=workspace.sh
source "$here/workspace.sh"

ok() { echo "ok: $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

echo "--- claude is the pinned version"
out="$(claude --version)"
grep -qF "$EXPECTED_CLAUDE" <<<"$out" || fail "expected Claude Code $EXPECTED_CLAUDE, got: $out"
ok "$out"

echo "--- the agent guide is installed as agent analyst"
grep -q '^name: analyst$' "$AGENT_SRC" || fail "$AGENT_SRC has no 'name: analyst' frontmatter"
ok "$AGENT_SRC"

echo "--- the workspace and the offline build"
prepare_workspace
test -w "$WORK" || fail "$WORK is not writable"
test -f "$WORK/kernel/src/lib.rs" || fail "no kernel in $WORK/kernel"
test -d "$CARGO_TARGET_DIR" || fail "no pre-built target"
cd "$WORK/kernel"
start=$(date +%s)
cargo build --offline --release --target wasm32-unknown-unknown 2>&1 | tail -3
test -s "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/verdict_kernel.wasm" || fail "no wasm was built"
ok "wasm32 release build, offline, $(( $(date +%s) - start )) s"
cargo test --offline 2>&1 | grep -E '^test result' | head -3
ok "cargo test, offline"

echo "--- the crates a kernel may add are vendored"
cargo add --offline memchr@2.8 aho-corasick@1.1 regex@1.12 >/dev/null
cargo build --offline --release --target wasm32-unknown-unknown 2>&1 | tail -2
ok "memchr, aho-corasick and regex build offline"
cp "$VERDICT_DIR/template/Cargo.toml" "$WORK/kernel/Cargo.toml"

echo "--- the root filesystem is read-only except /work and /tmp"
if touch /verdict/.write-probe 2>/dev/null; then
  rm -f /verdict/.write-probe
  echo "note: /verdict is writable here (not run with --read-only)"
else
  ok "/verdict is read-only"
fi

echo "--- the guide reaches the model"
# With a dummy key the API refuses the request, so nothing is spent. Claude Code
# still records the system prompt of the conversation in its session transcript,
# which is what is checked: the analyst guide must be part of it.
rm -rf "$HOME/.claude/projects"
cd "$WORK"
ANTHROPIC_API_KEY=sk-ant-smoke-test-not-a-real-key timeout 120 \
  claude -p "say hi" --agent analyst --output-format json >/dev/null 2>&1 || true
mapfile -t transcripts < <(find "$HOME/.claude/projects" -name '*.jsonl' 2>/dev/null)
[[ "${#transcripts[@]}" -gt 0 ]] || fail "Claude Code wrote no session transcript"
marker="$(sed -n 's/^## \(The lookup ladder\)$/\1/p' "$AGENT_SRC" | head -1)"
[[ -n "$marker" ]] || fail "the guide has no 'The lookup ladder' heading to look for"
hits="$(jq -s --arg m "$marker" '[.[] | .attachment.systemPrompt? // empty | .[] | select(contains($m))] | length' "${transcripts[@]}")"
[[ "$hits" -ge 1 ]] || fail "the system prompt of the agent run does not contain the guide ('$marker')"
ok "the system prompt carries the guide ('$marker')"

echo "--- an unknown agent is refused before any model call"
unknown="$(ANTHROPIC_API_KEY=sk-ant-smoke-test-not-a-real-key timeout 60 claude -p x --agent no-such-agent 2>&1 || true)"
grep -q "not found" <<<"$unknown" || fail "claude did not refuse an unknown agent: $unknown"
ok "claude --agent checks the agent name"

echo "All smoke checks passed."
