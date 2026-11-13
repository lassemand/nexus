#!/usr/bin/env bash
# Tests entrypoint.sh without Docker and without the Claude API: a fake `claude`
# stands in for the model, a local HTTP server for MFN's feed and for the
# analyst's upload endpoint. Needs bash, curl, jq, python3 and cargo (with the
# wasm32-unknown-unknown target) and, unless PREP_CARGO_HOME points at a warm
# cargo home, network access for the crates.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'kill "${server_pid:-0}" 2>/dev/null || true; rm -rf "$tmp"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

mkdir -p "$tmp/www/all/a" "$tmp/www/all/s" "$tmp/uploads"
python3 - "$tmp/www" <<'PY'
import json, sys, os
root = sys.argv[1]
def item(news_id, lang, slug, title, text):
    return {"news_id": news_id, "properties": {"lang": lang, "tags": ["sub:report:interim"]},
            "author": {"slug": slug}, "content": {"title": title, "text": text,
            "publish_date": "2026-05-12T07:00:00Z"}}
page2 = {"items": [item("id-3", "sv", "acme", "Acme: Delårsrapport", "Nettoomsättningen var 5 (4).\n")], "next_url": None}
page1 = {"items": [item("id-1", "en", "acme", "Acme: Interim report", "Net sales were 5 (4).\n"),
                   item("id-2", "en", "acme", "Acme: Q1", "Net sales were 3 (2).\n"),
                   item("noise", "en", "acme", "Acme: Insider", "short")],
         "next_url": "http://127.0.0.1:PORT/all/a/acme.json?limit=100&offset=3"}
json.dump(page1, open(os.path.join(root, "all/a/acme.json"), "w"))
json.dump(page2, open(os.path.join(root, "all/a/acme.json.offset3"), "w"))
other = {"items": [item("acme-own", "en", "acme", "Acme again", "x" * 300),
                   item("other-9", "en", "other", "Other: Interim report", "Other Oyj revenue was 41.2 (38.0). " + "y" * 300)]}
json.dump(other, open(os.path.join(root, "all/s/nordic.json"), "w"))
PY

cat > "$tmp/server.py" <<'PY'
import http.server, json, os, sys, urllib.parse
root, uploads, mode_file = sys.argv[1], sys.argv[2], sys.argv[3]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        name = os.path.join(root, u.path.lstrip("/"))
        if "offset=3" in u.query: name += ".offset3"
        if not os.path.exists(name):
            self.send_response(404); self.end_headers(); return
        body = open(name, "rb").read().replace(b"PORT", str(self.server.server_port).encode())
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.end_headers(); self.wfile.write(body)
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0)); body = self.rfile.read(n)
        k = len(os.listdir(uploads))
        open(os.path.join(uploads, f"{k}.json"), "wb").write(body)
        open(os.path.join(uploads, f"{k}.auth"), "w").write(self.headers.get("Authorization", ""))
        status = int(open(mode_file).read().strip() or 200)
        self.send_response(status); self.end_headers(); self.wfile.write(b'{"status":"x"}')
srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
open(sys.argv[4], "w").write(str(srv.server_port))
srv.serve_forever()
PY
echo 200 > "$tmp/status"
python3 "$tmp/server.py" "$tmp/www" "$tmp/uploads" "$tmp/status" "$tmp/port" &
server_pid=$!
for _ in $(seq 1 50); do [[ -s "$tmp/port" ]] && break; sleep 0.1; done
port="$(cat "$tmp/port")"

cat > "$tmp/fake-claude" <<'SH'
#!/usr/bin/env bash
# A stand-in for `claude -p`: checks the flags it was started with, and either
# writes a kernel.json, aborts, or fails, as FAKE_MODE says.
args=" $* "
for must in "--agent analyst" "--model opus" "--output-format json" "--max-turns 80" "--permission-mode dontAsk" "--no-session-persistence" "'Bash(cargo *)'"; do
  want="${must//\'/}"
  [[ "$args" == *"$want"* ]] || { echo "fake claude: missing $must in: $*" >&2; exit 64; }
done
for tool in Read Write Edit Glob Grep WebSearch WebFetch; do
  [[ "$args" == *" $tool "* ]] || { echo "fake claude: tool $tool not allowed" >&2; exit 64; }
done
[[ "$PWD" == */kernel ]] || { echo "fake claude: started in $PWD, not the kernel crate" >&2; exit 64; }
[[ -f "$HOME/.claude/agents/analyst.md" ]] || { echo "fake claude: the analyst agent is not installed under HOME" >&2; exit 64; }
[[ -f /work-test-input-check ]] || true
case "${FAKE_MODE:-ok}" in
  abort) printf '{"abort":"already_released"}' > "$WORK/abort.json" ;;
  fail) echo "boom" >&2; exit 2 ;;
  nokernel) ;;
  ok) cat > "$WORK/kernel.json" <<'JSON'
{"lookup":"skeleton","fallback":"none","why_not_faster":null,"rung":1,"native_p50_us":1.0,"native_p99_us":2.0,
 "fallback_p50_us":1.0,"fallback_p99_us":2.0,"metrics":[],"decision_rule":"abstain","languages":["en","sv"],
 "fixtures":[],"notes_used":[],"model":"fake","generated_at":"2026-11-13T00:00:00Z"}
JSON
  ;;
esac
echo '{"type":"result","is_error":false,"total_cost_usd":0.42,"num_turns":7,"duration_ms":1234,"result":"done"}'
SH
chmod +x "$tmp/fake-claude"

input="$(jq -n '{company: {ticker: "ACM", exchange_mic: "XSTO", name: "Acme", mfn_slug: "acme", currency: "SEK",
   news_url: "https://mfn.se/all/a/acme"},
   report: {kind: "interim", fiscal_period: "Q2 2026", expected_at: "2026-08-19T07:00:00Z", expected_time_known: true},
   languages: ["en", "sv"],
   previous_reports: [{news_id: "id-1", lang: "en"}, {news_id: "id-2", lang: "en"}, {news_id: "id-3", lang: "sv"}, {news_id: "gone", lang: "en"}],
   notes: [], prep_started_at: "2026-08-18T19:00:00Z"}')"

run_entrypoint() {
  local mode="$1" work="$tmp/work-$1"
  rm -rf "$tmp/uploads"/*
  env -i PATH="$PATH" HOME="$tmp/realhome" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
    ${CARGO_HOME:+CARGO_HOME="$CARGO_HOME"} ${PREP_CARGO_HOME:+PREP_CARGO_HOME="$PREP_CARGO_HOME"} \
    VERDICT_DIR="$repo/verdict" WORK="$work" AGENT_SRC="$repo/infra/agents/analyst.md" \
    CARGO_CONFIG_SRC="$tmp/none" PREBUILT_TARGET="$tmp/none" \
    MFN_BASE="http://127.0.0.1:$port" CLAUDE_BIN="$tmp/fake-claude" FAKE_MODE="$mode" \
    ANTHROPIC_API_KEY=unused PREP_UPLOAD_URL="http://127.0.0.1:$port/upload" PREP_UPLOAD_TOKEN="secret-token" \
    PREP_INPUT="$input" bash "$repo/verdict/prep/entrypoint.sh" > "$tmp/out-$mode.log" 2>&1
}

echo "== a normal run uploads the kernel"
run_entrypoint ok || { cat "$tmp/out-ok.log" >&2; fail "entrypoint failed in the normal run"; }
[[ "$(ls "$tmp/uploads"/*.json | wc -l)" -eq 1 ]] || fail "expected one upload"
up="$tmp/uploads/0.json"
[[ "$(cat "$tmp/uploads/0.auth")" == "Bearer secret-token" ]] || fail "wrong Authorization header"
jq -e '.source | contains("export_kernel")' "$up" >/dev/null || fail "source is not the kernel's lib.rs"
jq -e '.wasm | length > 1000' "$up" >/dev/null || fail "no wasm"
[[ "$(jq -r .wasm "$up" | base64 -d | head -c4 | od -An -c | tr -d ' ')" == '\0asm' ]] || fail "wasm does not start with the magic number"
jq -e '.kernel.lookup == "skeleton"' "$up" >/dev/null || fail "kernel.json missing"
jq -e '.transcript_summary.total_cost_usd == 0.42 and .transcript_summary.num_turns == 7' "$up" >/dev/null || fail "no transcript summary"
jq -e '.tests | has("kernel.rs")' "$up" >/dev/null || fail "tests not uploaded"
jq -e '[.fixtures[] | select(.foreign == false) | .news_id + ":" + .lang] | sort == ["id-1:en","id-2:en","id-3:sv"]' "$up" >/dev/null || fail "previous reports not uploaded as fixtures: $(jq -c '[.fixtures[]|{news_id,lang,foreign}]' "$up")"
jq -e '[.fixtures[] | select(.foreign == true)] | length == 1 and .[0].news_id == "other-9" and .[0].lang == "en"' "$up" >/dev/null || fail "foreign fixture wrong"
jq -e '.fixtures[] | select(.news_id == "id-1") | .text == "Acme: Interim report\n\nNet sales were 5 (4).\n"' "$up" >/dev/null || fail "fixture text is not exact (title, blank line, text)"
grep -q "previous reports not found in the feed: gone" "$tmp/out-ok.log" || fail "a missing report was not reported"

echo "== an abort is uploaded as it is"
run_entrypoint abort || { cat "$tmp/out-abort.log" >&2; fail "entrypoint failed on abort"; }
[[ "$(jq -c . "$tmp/uploads/0.json")" == '{"abort":"already_released"}' ]] || fail "abort body wrong: $(cat "$tmp/uploads/0.json")"

echo "== a failing Claude Code fails the Job and uploads nothing"
if run_entrypoint fail; then fail "expected a failure"; fi
[[ -z "$(ls "$tmp/uploads")" ]] || fail "uploaded after a failure"

echo "== a run with no kernel.json fails the Job"
if run_entrypoint nokernel; then fail "expected a failure"; fi
[[ -z "$(ls "$tmp/uploads")" ]] || fail "uploaded without a kernel"

echo "== a rejected upload fails the Job"
echo 422 > "$tmp/status"
if run_entrypoint ok; then fail "expected a failure on HTTP 422"; fi
grep -q "HTTP 422" "$tmp/out-ok.log" || fail "the rejection was not logged"

echo "All entrypoint checks passed."
