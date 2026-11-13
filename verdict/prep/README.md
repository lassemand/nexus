# Kernel preparation

What runs in the Kubernetes Job that prepares a kernel for one upcoming report. The image is
`infra/Dockerfile.analyst-prep`, released by `.github/workflows/release-analyst-prep.yml` as
`nexus-analyst-prep` (tags `analyst-prep-v*`). The service that starts the Job and checks what it
uploads is `analyst` (`analyst/`).

| File | What it is |
|---|---|
| `entrypoint.sh` | The Job's entrypoint: sets up `/work`, downloads the reports, runs Claude Code, builds, uploads. |
| `workspace.sh` | The workspace and cargo setup, shared with `smoke.sh`. |
| `PROMPT.md` | The task Claude Code is given. Its system prompt is the analyst agent guide (`infra/agents/analyst.md`, installed as agent `analyst`). |
| `smoke.sh` | Checks the image from the inside, no API key needed. |
| `test_entrypoint.sh` | Tests `entrypoint.sh` with a fake `claude` and a local MFN/upload server. Runs in CI. |

## What the entrypoint does

1. Copies the SDK and the template (`verdict/template`) to `/work/sdk` and `/work/kernel`, and the
   pre-built target directory to `/work/target`. `HOME`, `CARGO_HOME` and `CARGO_TARGET_DIR` all point
   into `/work`, the only writable path; cargo builds offline from the vendored crates.
2. Writes `PREP_INPUT` to `/work/input.json`.
3. Downloads the exact text of each previous report and of one other company's report from MFN's JSON
   feeds into `/work/kernel/tests/fixtures/` (`<lang>__<news_id>.txt`, `foreign__<lang>__<news_id>.txt`),
   in the form the analyst's runtime will see: the title, a blank line, `content.text`. The model never
   copies a fixture (a page fetched by `WebFetch` is a summary, not the text).
4. Runs Claude Code non-interactively (flags below) with the prompt in `PROMPT.md`.
5. Builds the kernel once more, and POSTs the result to `PREP_UPLOAD_URL`, or POSTs `{"abort": "..."}`
   when the agent wrote `/work/abort.json`. It exits non-zero when Claude Code failed, wrote no
   `kernel.json`, the kernel does not build or the upload is not answered with 2xx.

## Claude Code flags (pinned version 2.1.285)

```
claude -p "$(cat PROMPT.md)" --agent analyst --model opus --output-format json \
  --max-turns 80 --max-budget-usd 15 --permission-mode dontAsk \
  --allowedTools Read Write Edit Glob Grep WebSearch WebFetch 'Bash(cargo *)' \
  --add-dir /work --no-session-persistence
```

* `--agent analyst` works non-interactively. It reads `~/.claude/agents/analyst.md` and refuses an
  unknown name locally (`--agent 'x' not found. Available agents: ...`), so no `--append-system-prompt`
  fallback is needed. `HOME` is `/work/home` at runtime and the entrypoint installs the guide there.
* `--max-turns` is not in `claude --help` for this version but is accepted (the run starts and the
  limit is not rejected). `--max-budget-usd` is documented; it is added as a hard cost limit.
* `--permission-mode dontAsk` denies every tool that is not in `--allowedTools` instead of waiting for
  an answer nobody can give. `Bash(cargo *)` is the only command allowed.

## `PREP_INPUT`

```json
{
  "company": {"ticker": "", "exchange_mic": "", "name": "", "mfn_slug": "", "currency": "", "news_url": "https://mfn.se/all/a/<slug>"},
  "report": {"kind": "interim|annual", "fiscal_period": null, "expected_at": "RFC 3339", "expected_time_known": true},
  "languages": ["en", "sv"],
  "previous_reports": [{"news_id": "", "lang": "", "title": "", "published_at": "RFC 3339", "url": ""}],
  "notes": [{"note_id": "", "author": "", "created_at": "RFC 3339", "fiscal_period": null, "text": ""}],
  "prep_started_at": "RFC 3339"
}
```

`previous_reports` are the company's 8 latest report releases published before `prep_started_at`.

## The upload

`POST $PREP_UPLOAD_URL`, `Authorization: Bearer $PREP_UPLOAD_TOKEN`, JSON:

```json
{
  "source": "contents of src/lib.rs",
  "tests": {"kernel.rs": "contents"},
  "wasm": "base64",
  "kernel": {"...": "the agent's kernel.json, schema in PROMPT.md"},
  "fixtures": [{"news_id": "", "lang": "", "foreign": false, "text": ""}],
  "transcript_summary": {"...": "Claude Code's final JSON result: cost, turns, duration"}
}
```

or `{"abort": "already_released"}`. The analyst accepts or rejects it (acceptance, in a later PR).

## Checks

* `bash verdict/prep/test_entrypoint.sh` (needs bash, curl, jq, python3, cargo with the wasm32 target).
* `verdict/prep/smoke.sh` inside the image, as the release workflow does (see the header of the script).
