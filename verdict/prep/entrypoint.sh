#!/usr/bin/env bash
# The preparation Job's entrypoint: lets Claude Code write, test and build a
# kernel for one report, then uploads it to the analyst for acceptance.
#
# Environment (set by the analyst's launcher):
#   ANTHROPIC_API_KEY   the Claude API key
#   PREP_UPLOAD_URL     where to POST the result
#   PREP_UPLOAD_TOKEN   bearer token for that POST
#   PREP_INPUT          the JSON document described in verdict/prep/README.md
# Optional: PREP_MAX_TURNS (80), PREP_MAX_BUDGET_USD (15), PREP_MODEL (opus),
# MFN_BASE (https://mfn.se), CLAUDE_BIN (claude).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=workspace.sh
source "$here/workspace.sh"

CLAUDE_BIN="${CLAUDE_BIN:-claude}"
MFN_BASE="${MFN_BASE:-https://mfn.se}"
PREP_MODEL="${PREP_MODEL:-opus}"
PREP_MAX_TURNS="${PREP_MAX_TURNS:-80}"
PREP_MAX_BUDGET_USD="${PREP_MAX_BUDGET_USD:-15}"
FEED_PAGES="${FEED_PAGES:-6}"

log() { printf '%s prep: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
die() { log "error: $*"; exit 1; }

: "${PREP_UPLOAD_URL:?PREP_UPLOAD_URL is required}"
: "${PREP_UPLOAD_TOKEN:?PREP_UPLOAD_TOKEN is required}"
: "${PREP_INPUT:?PREP_INPUT is required}"

prepare_workspace
printf '%s' "$PREP_INPUT" | jq -e . > "$WORK/input.json" || die "PREP_INPUT is not valid JSON"
fixtures="$WORK/kernel/tests/fixtures"
mkdir -p "$fixtures"

# One release as the analyst's runtime will see it: the title, a blank line,
# then MFN's content.text. Downloaded here, not by the model: a page fetched
# through WebFetch is summarised, and a fixture has to be the exact text.
item_text() {
  jq -j '(.content.title // "") + "\n\n" + (.content.text // "")'
}

fetch_feed_page() {
  curl -fsS -g --retry 3 --max-time 60 -A 'nexus-analyst-prep' "$1"
}

slug="$(jq -r '.company.mfn_slug // empty' "$WORK/input.json")"
[[ -n "$slug" ]] || die "PREP_INPUT has no company.mfn_slug"
wanted="$(jq -r '.previous_reports[].news_id' "$WORK/input.json" | sort -u)"
remaining="$wanted"
url="$MFN_BASE/all/a/${slug}.json?limit=100"
for _ in $(seq 1 "$FEED_PAGES"); do
  [[ -n "$remaining" && -n "$url" ]] || break
  page="$(fetch_feed_page "$url")" || die "cannot read the company feed $url"
  while IFS= read -r id; do
    item="$(jq -c --arg id "$id" '.items // [] | map(select(.news_id == $id)) | first // empty' <<<"$page")"
    [[ -n "$item" ]] || continue
    lang="$(jq -r '.properties.lang // "xx"' <<<"$item")"
    item_text <<<"$item" > "$fixtures/${lang}__${id}.txt"
    remaining="$(grep -vxF "$id" <<<"$remaining" || true)"
  done <<<"$remaining"
  url="$(jq -r '.next_url // empty' <<<"$page")"
done
if [[ -n "$remaining" ]]; then
  log "previous reports not found in the feed: $(tr '\n' ' ' <<<"$remaining")"
fi

# One other company's report, for the "must abstain" fixture.
foreign_page="$(curl -fsS -g -G --retry 3 --max-time 60 -A 'nexus-analyst-prep' "$MFN_BASE/all/s/nordic.json" \
  --data-urlencode 'limit=100' --data-urlencode 'offset=0' \
  --data-urlencode 'filter=(and(or(.properties.tags@>["sub:report:interim"])))')" || die "cannot read the foreign sample"
foreign="$(jq -c --arg slug "$slug" '.items // [] | map(select(.author.slug != $slug and ((.content.text // "") | length) > 200)) | first // empty' <<<"$foreign_page")"
if [[ -n "$foreign" ]]; then
  flang="$(jq -r '.properties.lang // "xx"' <<<"$foreign")"
  fid="$(jq -r '.news_id' <<<"$foreign")"
  item_text <<<"$foreign" > "$fixtures/foreign__${flang}__${fid}.txt"
else
  log "no foreign report found: the abstain fixture is missing"
fi

log "starting Claude Code (model $PREP_MODEL, max turns $PREP_MAX_TURNS, budget $PREP_MAX_BUDGET_USD USD)"
cd "$WORK/kernel"
set +e
"$CLAUDE_BIN" -p "$(cat "$VERDICT_DIR/prep/PROMPT.md")" \
  --agent analyst \
  --model "$PREP_MODEL" \
  --output-format json \
  --max-turns "$PREP_MAX_TURNS" \
  --max-budget-usd "$PREP_MAX_BUDGET_USD" \
  --permission-mode dontAsk \
  --allowedTools Read Write Edit Glob Grep WebSearch WebFetch 'Bash(cargo *)' \
  --add-dir "$WORK" \
  --no-session-persistence \
  > "$WORK/claude-result.json" 2> "$WORK/claude-stderr.log"
claude_status=$?
set -e
[[ -s "$WORK/claude-result.json" ]] || echo '{}' > "$WORK/claude-result.json"
jq -e . "$WORK/claude-result.json" >/dev/null 2>&1 || echo '{}' > "$WORK/claude-result.json"
log "Claude Code exited with $claude_status; $(jq -r '"cost \(.total_cost_usd // "?") USD, \(.num_turns // "?") turns"' "$WORK/claude-result.json")"

upload() {
  local body="$1" code
  code="$(curl -sS --max-time 300 -o "$WORK/upload-response.txt" -w '%{http_code}' \
    -X POST -H "Authorization: Bearer ${PREP_UPLOAD_TOKEN}" -H 'Content-Type: application/json' \
    --data-binary "@${body}" "$PREP_UPLOAD_URL")" || die "the upload failed"
  log "upload answered HTTP $code: $(head -c 400 "$WORK/upload-response.txt")"
  [[ "$code" =~ ^2 ]] || exit 1
}

if [[ -f "$WORK/abort.json" ]]; then
  jq -e '.abort | strings' "$WORK/abort.json" >/dev/null || die "abort.json has no abort reason"
  log "the agent aborted: $(jq -r .abort "$WORK/abort.json")"
  upload "$WORK/abort.json"
  exit 0
fi

[[ "$claude_status" -eq 0 ]] || { tail -c 2000 "$WORK/claude-stderr.log" >&2; die "Claude Code failed"; }
[[ -f "$WORK/kernel.json" ]] || die "the agent wrote no /work/kernel.json"
jq -e . "$WORK/kernel.json" >/dev/null || die "/work/kernel.json is not valid JSON"
[[ -f "$WORK/kernel/src/lib.rs" ]] || die "the kernel has no src/lib.rs"

log "building the kernel for wasm32"
(cd "$WORK/kernel" && cargo build --offline --release --target wasm32-unknown-unknown) >&2 || die "the kernel does not build"
wasm="$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/verdict_kernel.wasm"
[[ -f "$wasm" ]] || die "no wasm at $wasm"
base64 -w0 "$wasm" > "$WORK/kernel.wasm.b64"

json_of_files() {
  # json_of_files <dir> <file...>: {"path relative to dir": "contents"} for each file
  local dir="$1"; shift
  local parts="$WORK/parts.$$"; rm -rf "$parts"; mkdir -p "$parts"
  local n=0 f rel
  for f in "$@"; do
    rel="${f#"$dir"/}"
    jq -n --arg path "$rel" --rawfile text "$f" '{($path): $text}' > "$parts/$n.json"
    n=$((n + 1))
  done
  if [[ "$n" -eq 0 ]]; then echo '{}'; else jq -s add "$parts"/*.json; fi
  rm -rf "$parts"
}

mapfile -t test_files < <(find "$WORK/kernel/tests" -path "$WORK/kernel/tests/fixtures" -prune -o -type f -name '*.rs' -print | sort)
json_of_files "$WORK/kernel/tests" "${test_files[@]}" > "$WORK/tests.json"

fixture_list="$WORK/fixture-list.json"
: > "$WORK/fixture-items.jsonl"
for f in "$fixtures"/*.txt; do
  [[ -e "$f" ]] || continue
  name="$(basename "$f" .txt)"
  lang="${name%%__*}"; rest="${name#*__}"
  foreign=false
  if [[ "$lang" == foreign ]]; then foreign=true; lang="${rest%%__*}"; rest="${rest#*__}"; fi
  jq -n -c --arg news_id "$rest" --arg lang "$lang" --argjson foreign "$foreign" --rawfile text "$f" \
    '{news_id: $news_id, lang: $lang, foreign: $foreign, text: $text}' >> "$WORK/fixture-items.jsonl"
done
jq -s '.' "$WORK/fixture-items.jsonl" > "$fixture_list"

jq -n \
  --rawfile source "$WORK/kernel/src/lib.rs" \
  --rawfile wasm "$WORK/kernel.wasm.b64" \
  --slurpfile tests "$WORK/tests.json" \
  --slurpfile kernel "$WORK/kernel.json" \
  --slurpfile fixtures "$fixture_list" \
  --slurpfile summary "$WORK/claude-result.json" \
  '{source: $source, tests: $tests[0], wasm: $wasm, kernel: $kernel[0], fixtures: $fixtures[0], transcript_summary: $summary[0]}' \
  > "$WORK/upload.json"

log "uploading the kernel ($(wc -c < "$WORK/upload.json") bytes)"
upload "$WORK/upload.json"
log "done"
