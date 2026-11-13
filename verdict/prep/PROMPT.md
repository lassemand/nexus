You are preparing a kernel for ONE upcoming company report. Your system prompt is the
analyst agent guide: follow it, in particular its lookup ladder and speed rules.

A kernel is a small Rust crate, compiled to WASM, that will read this report's release
text the moment it is published and return the metrics it found and `buy`, `no_buy` or
`abstain`. The decision drives a simulated trade. Nothing here is predefined: which
metrics matter, how they are located and how they are read is your decision for this
company, taken from its own previous reports.

## Where things are

* `/work/input.json`: the task (the company, the report, the languages it publishes in, links to
  its previous 8 report releases, the user's notes, and `prep_started_at`).
* `/work/kernel`: your crate (the template). You may change ONLY `src/lib.rs`, files under
  `tests/`, and the `[dependencies]` of `Cargo.toml`, where you may add only `memchr = "2.8"`,
  `aho-corasick = "1.1"` and `regex = "1.12"`, default features, exactly these requirements:
  they are vendored and the build is offline. No build scripts, no macros crates, no other
  dependencies. Do not rename the crate.
* `/work/kernel/tests/fixtures/`: ALREADY DOWNLOADED, exact release texts as the runtime will
  see them (the title, a blank line, the text), named `<lang>__<news_id>.txt` for the company's
  previous reports and `foreign__<lang>__<news_id>.txt` for another company's report. Never
  retype or edit a fixture, and never copy a release into a file yourself.
* `cargo` is the only command you can run, for example `cargo test` and
  `cargo build --release --target wasm32-unknown-unknown` from `/work/kernel`.
* You can read pages with WebFetch and search with WebSearch. A fetched page is a summary, not
  the exact text: use it for research, never for fixtures or for deciding how a figure is laid out.

## What to do, in order

1. Read the agent guide (already in your system prompt) and `/work/input.json`.
2. **Read the previous reports** in `tests/fixtures/`, every language, newest first. **Never fetch,
   search for or use the target report itself.** If the company has already published it (an early
   release: check `company.news_url`, the company's latest releases, for a results release of
   `report.fiscal_period` or for this `report.kind` published after the previous reports), stop
   and write `{"abort":"already_released"}` to `/work/abort.json`, and nothing else.
3. **Research the expectations** for this report: analyst consensus, company guidance,
   management targets, last year's figures and the trend, and the KPIs this company's share price
   reacts to. Read every note in `input.json` (`notes`): each is the user's context for this report.
   Say, per note, how you used it (in `notes_used`).
4. **Choose 3 to 8 metrics** that decide whether this report is a success. For each, record why it
   matters, the expected value (or "beat last year", "above guidance" and so on) with its source
   URL and date, and **how it is located**, decided from the previous reports: a position (the
   title, a fixed line), a phrase in each language the company uses, a value pattern, or something
   else. Check your choice on every previous report in every language, not just one.
5. **Write the decision rule** as plain text first, then implement it, the lookups and the number
   reading in `analyze` in `src/lib.rs`. Read numbers exactly: `Decimal` only, in this company's
   format, parentheses and minus signs as the guide says. A missing or unverified metric means
   `Abstain` with the reason; never return a number you did not verify.
6. **Choose the lookup yourself**, by the guide's lookup ladder: the cheapest rung that every
   previous report in every language supports (a structural position or fixed prefix, then one
   `memmem` in a known window, then one Aho-Corasick pass, regex last). Implement a cheap
   verification and a broader fallback, and measure both with `verdict_sdk::harness::time_per_release`
   on the fixtures. Record both, and why a faster rung did not work.
7. **Add tests** in `tests/kernel.rs` using `verdict_sdk::harness`: `check_fixtures` on the last two
   reports of each language, asserting the current and prior value of every metric and the
   decision, and `assert_abstains` on the `foreign__` fixture. Keep the template's own foreign test
   passing or replace it with this one.
8. Run `cargo test` and `cargo build --release --target wasm32-unknown-unknown` until both pass.
9. **Write `/work/kernel.json`**, one JSON object, exactly these fields:

```json
{
  "lookup": "what the main lookup does, one or two sentences",
  "fallback": "what runs when its verification fails",
  "why_not_faster": "why a cheaper rung did not work; null when rung is 1",
  "rung": 1,
  "native_p50_us": 0.0,
  "native_p99_us": 0.0,
  "fallback_p50_us": 0.0,
  "fallback_p99_us": 0.0,
  "metrics": [
    {
      "key": "as in the kernel's KEYS and the Metric key",
      "label": "human name",
      "why": "why it matters for this report",
      "expected": {"value": "...", "unit": "...", "source_url": "...", "source_date": "YYYY-MM-DD"},
      "located_by": "free text: how the kernel finds it"
    }
  ],
  "decision_rule": "the rule in plain text",
  "languages": ["en", "sv"],
  "fixtures": [
    {
      "news_id": "...", "lang": "en", "foreign": false,
      "expected": [{"key": "...", "current": "123.4", "prior": "120.0"}],
      "expected_decision": "buy"
    }
  ],
  "notes_used": [{"note_id": "...", "how": "..."}],
  "model": "the model you are",
  "generated_at": "RFC 3339 instant"
}
```

   * `expected` of a metric is `null` when you found no figure you trust.
   * `fixtures` lists every fixture the tests assert: the last two reports in each language, with
     `foreign: false`, and the `foreign__` one with `foreign: true`, `expected: []` and
     `"expected_decision": "abstain"`. `news_id` and `lang` are those of the fixture's file name.
     `current` and `prior` are exact decimal strings, or `null` when the report has none.
   * `rung` is the ladder rung (1 to 4) of the main lookup; `why_not_faster` is required above 1.
   * Values in `kernel.json` are what the analyst checks by running your WASM on the fixtures: a
     claim that is not true rejects the kernel.

Finish by replying with one short paragraph: the metrics, the decision rule and the lookup.
